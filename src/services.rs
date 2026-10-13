//! Services globaux partagés par tous les projets : MySQL (MariaDB), PostgreSQL, Redis, MinIO, Meilisearch.
//! Téléchargés au premier démarrage (zip ou exécutable unique), initialisés, puis supervisés.
use crate::config::{Global, Service};
use crate::download::{download_extract, fetch, find_file};
use crate::paths;
use crate::supervisor::{Job, Supervisor};
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

pub async fn start_all(g: &Global, sup: &Supervisor) {
    for (name, svc) in &g.services {
        if svc.enabled {
            if let Err(e) = start_one(name, svc, sup).await {
                tracing::error!("service {name}: {e:#}");
            }
        }
    }
}

/// Télécharge le service s'il n'est pas encore là. Gère les zips et les exécutables seuls (MinIO, Meilisearch).
async fn ensure_downloaded(svc: &Service, dir: &Path) -> Result<()> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        return Ok(());
    }
    if svc.url.to_lowercase().ends_with(".exe") {
        println!("↓ {}", svc.url);
        std::fs::create_dir_all(dir)?;
        let name = svc.url.rsplit('/').next().unwrap_or("service.exe");
        std::fs::write(dir.join(name), fetch(&svc.url).await?)?;
    } else {
        download_extract(&svc.url, dir).await?;
    }
    Ok(())
}

fn exe(dir: &Path, names: &[&str]) -> Result<PathBuf> {
    names.iter().find_map(|n| find_file(dir, n)).ok_or_else(|| anyhow!("{} introuvable dans {}", names[0], dir.display()))
}

async fn run_once(program: &Path, args: &[String]) -> Result<()> {
    let st = tokio::process::Command::new(program).args(args).status().await?;
    if !st.success() {
        bail!("{} a échoué ({st})", program.display());
    }
    Ok(())
}

pub async fn start_one(name: &str, svc: &Service, sup: &Supervisor) -> Result<()> {
    if name == "minio" && !crate::config::minio_license().exists() {
        // inutile de télécharger 270 Mo si le service ne pourra pas démarrer
        bail!("MinIO AIStor exige une licence (gratuite sur min.io/pricing, offre « Free »). \
               Importe-la depuis Viherd : Services → MinIO → Licence. Sans compte, utilise plutôt RustFS.");
    }
    let dir = paths::service_dir(name);
    ensure_downloaded(svc, &dir).await?;
    let data = paths::data_dir(name);
    let d = data.display().to_string();
    let mut env = vec![("PATH".to_string(), std::env::var("PATH").unwrap_or_default())];
    let mut cwd = dir.clone();
    let port = svc.port.to_string();
    let (bin, args, graceful) = match name {
        "mysql" => {
            let bin = exe(&dir, &["mariadbd.exe", "mysqld.exe"])?;
            if !data.exists() {
                run_once(&exe(&dir, &["mariadb-install-db.exe", "mysql_install_db.exe"])?, &[format!("--datadir={d}")]).await?;
            }
            let stop = exe(&dir, &["mariadb-admin.exe", "mysqladmin.exe"]).ok()
                .map(|a| (a, vec!["-u".into(), "root".into(), "-h".into(), "127.0.0.1".into(), format!("--port={port}"), "shutdown".into()]));
            (bin, vec![format!("--datadir={d}"), format!("--port={port}"), "--bind-address=127.0.0.1".into(), "--console".into()], stop)
        }
        "postgres" => {
            let bin = exe(&dir, &["postgres.exe"])?;
            if !data.join("PG_VERSION").exists() {
                // utilisateur « postgres » sans mot de passe, accès local uniquement
                run_once(&exe(&dir, &["initdb.exe"])?, &["-U".into(), "postgres".into(), "-A".into(), "trust".into(), "-E".into(), "UTF8".into(), "-D".into(), d.clone()]).await?;
            }
            let stop = exe(&dir, &["pg_ctl.exe"]).ok().map(|c| (c, vec!["stop".into(), "-D".into(), d.clone(), "-m".into(), "fast".into(), "-w".into()]));
            (bin, vec!["-D".into(), d, "-p".into(), port, "-h".into(), "127.0.0.1".into()], stop)
        }
        "redis" => {
            std::fs::create_dir_all(&data)?;
            let stop = exe(&dir, &["redis-cli.exe"]).ok().map(|c| (c, vec!["-p".into(), port.clone(), "shutdown".into()]));
            (exe(&dir, &["redis-server.exe"])?, vec!["--port".into(), port, "--bind".into(), "127.0.0.1".into(), "--dir".into(), d], stop)
        }
        "rustfs" => {
            std::fs::create_dir_all(&data)?;
            (exe(&dir, &["rustfs.exe"])?, vec![
                "server".into(), "--address".into(), format!("127.0.0.1:{}", svc.port),
                "--console-enable".into(), "--console-address".into(), format!("127.0.0.1:{}", svc.port + 1),
                "--access-key".into(), "rustfsadmin".into(), "--secret-key".into(), "rustfsadmin".into(),
                "--region".into(), "us-east-1".into(), d], None)
        }
        "minio" => {
            // MinIO AIStor : plus de binaire communautaire, la licence (gratuite) est obligatoire.
            let license = crate::config::minio_license();
            if !license.exists() {
                bail!("MinIO AIStor exige une licence (gratuite sur min.io/pricing, offre « Free »). \
                       Importe-la depuis Viherd : Services → MinIO → Licence. Sans compte, utilise plutôt RustFS.");
            }
            std::fs::create_dir_all(&data)?;
            env.push(("MINIO_ROOT_USER".into(), "minioadmin".into()));
            env.push(("MINIO_ROOT_PASSWORD".into(), "minioadmin".into()));
            (exe(&dir, &["minio.exe"])?, vec!["server".into(), d, "--license".into(), license.display().to_string(),
                "--address".into(), format!("127.0.0.1:{}", svc.port), "--console-address".into(), format!("127.0.0.1:{}", svc.port + 1)], None)
        }
        "meilisearch" => {
            std::fs::create_dir_all(&data)?;
            cwd = data.clone(); // Meilisearch écrit ses dumps et snapshots dans son dossier courant
            (exe(&dir, &["meilisearch-windows-amd64.exe", "meilisearch.exe"])?, vec![
                "--http-addr".into(), format!("127.0.0.1:{}", svc.port), "--db-path".into(), data.join("data.ms").display().to_string(),
                "--env".into(), "development".into(), "--no-analytics".into(), "--master-key".into(), "viherd-local-master-key".into()], None)
        }
        other => bail!("service inconnu: {other}"),
    };
    sup.start(Job { key: format!("service:{name}"), program: bin.display().to_string(), args, cwd, env, autorestart: true, graceful_stop: graceful });
    Ok(())
}

/// Client en ligne de commande d'une base (mariadb.exe / psql.exe) avec ses arguments de connexion.
fn db_client(name: &str, svc: &Service) -> Result<(PathBuf, Vec<String>)> {
    let dir = paths::service_dir(name);
    let port = svc.port.to_string();
    match name {
        "mysql" => Ok((exe(&dir, &["mariadb.exe", "mysql.exe"])?, vec!["-u".into(), "root".into(), "-h".into(), "127.0.0.1".into(), format!("--port={port}"), "-N".into(), "-B".into()])),
        "postgres" => Ok((exe(&dir, &["psql.exe"])?, vec!["-h".into(), "127.0.0.1".into(), "-p".into(), port, "-U".into(), "postgres".into(), "-d".into(), "postgres".into(), "-At".into(), "-v".into(), "ON_ERROR_STOP=1".into()])),
        _ => bail!("{name} n'a pas de bases à gérer"),
    }
}

async fn db_exec(name: &str, svc: &Service, sql: &str) -> Result<String> {
    let (prog, mut args) = db_client(name, svc)?;
    args.push(if name == "mysql" { "-e".into() } else { "-c".into() });
    args.push(sql.into());
    let mut c = tokio::process::Command::new(prog);
    c.args(&args).env("PGCLIENTENCODING", "UTF8").stdin(std::process::Stdio::null());
    #[cfg(windows)]
    c.creation_flags(0x0800_0000);
    let out = c.output().await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("{}", if err.is_empty() { "le service ne répond pas (est-il démarré ?)".into() } else { err });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Bases de données existantes (hors bases système).
pub async fn list_databases(name: &str, svc: &Service) -> Result<Vec<String>> {
    let sql = match name {
        "mysql" => "SHOW DATABASES",
        _ => "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY datname",
    };
    let system = ["information_schema", "performance_schema", "mysql", "sys"];
    Ok(db_exec(name, svc, sql).await?.lines().map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !system.contains(&l.as_str())).collect())
}

pub async fn create_database(name: &str, svc: &Service, db: &str) -> Result<()> {
    if db.is_empty() || !db.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("nom de base invalide : lettres, chiffres, _ et - uniquement");
    }
    let sql = match name {
        "mysql" => format!("CREATE DATABASE IF NOT EXISTS `{db}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"),
        _ => {
            if list_databases(name, svc).await?.iter().any(|x| x == db) { return Ok(()); }
            format!("CREATE DATABASE \"{db}\" ENCODING 'UTF8'")
        }
    };
    db_exec(name, svc, &sql).await.map(|_| ())
}
