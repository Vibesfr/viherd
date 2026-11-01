use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use viherd::config::{self, find_project_root, site_name, Global, Project};
use viherd::{certs, daemon, download, env, herd, hosts, paths, share, update, versions};

#[derive(Parser)]
#[command(name = "viherd", version, about = "Environnement de dev PHP/Node local (à la Herd), en Rust")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Lance le démon (proxy HTTP, PHP-FastCGI, workers, services)
    Start,
    /// Ouvre l'interface graphique dans le navigateur
    Ui,
    /// Tous les sous-dossiers de ce dossier deviennent des sites <nom>.test
    Park { path: Option<PathBuf> },
    /// Lie le dossier courant comme site <nom>.test
    Link { name: Option<String> },
    Unlink { name: String },
    /// Liste les sites
    Sites,
    /// Crée un viherd.toml dans le projet courant
    Init,
    /// Installe une version de PHP / Node, ou Composer
    Install {
        #[command(subcommand)]
        what: Install,
    },
    /// Fixe la version de php/node pour le projet courant (ex: `viherd use php 8.2`)
    Use { tool: String, version: String },
    /// Active/désactive un service global (mysql, redis)
    Service { name: String, state: String },
    /// Exécute une commande avec le PHP/Node isolé du projet
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    #[command(disable_help_flag = true)]
    Php { #[arg(trailing_var_arg = true, allow_hyphen_values = true)] args: Vec<String> },
    #[command(disable_help_flag = true)]
    Node { #[arg(trailing_var_arg = true, allow_hyphen_values = true)] args: Vec<String> },
    #[command(disable_help_flag = true)]
    Npm { #[arg(trailing_var_arg = true, allow_hyphen_values = true)] args: Vec<String> },
    #[command(disable_help_flag = true)]
    Npx { #[arg(trailing_var_arg = true, allow_hyphen_values = true)] args: Vec<String> },
    #[command(disable_help_flag = true)]
    Composer { #[arg(trailing_var_arg = true, allow_hyphen_values = true)] args: Vec<String> },
    #[command(disable_help_flag = true)]
    Artisan { #[arg(trailing_var_arg = true, allow_hyphen_values = true)] args: Vec<String> },
    /// Sert le site en HTTPS (le site du dossier courant par défaut)
    Secure { name: Option<String> },
    /// Repasse le site en HTTP
    Unsecure { name: Option<String> },
    /// Approuve l'autorité de certification locale de Viherd dans Windows
    Trust,
    /// Partage le site sur une adresse publique temporaire (Cloudflare Tunnel) — Ctrl+C pour arrêter
    Share { name: Option<String> },
    /// Écrit les sites dans le fichier hosts (admin)
    Hosts,
    /// État du démon (sites, workers)
    Status,
    /// Recharge la config dans le démon (après avoir modifié un viherd.toml)
    Reload,
    /// Installe la dernière version de Viherd publiée sur GitHub
    Update,
    /// Importe la configuration de Laravel Herd (sites, HTTPS, versions de PHP, .env)
    Herd {
        /// Applique sans demander (sinon : affiche seulement ce qui serait fait)
        #[arg(long)]
        yes: bool,
    },
    /// Serveur MCP sur stdio pour les assistants IA (ex : `claude mcp add viherd -- viherd mcp`)
    Mcp,
}

#[derive(Subcommand)]
enum Install {
    Php { version: String },
    Node { version: String },
    Composer,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // Le démon écrit aussi dans ~/.viherd/logs/viherd.log (page « Activité » de l'interface).
    let log_file = matches!(cli.cmd, Cmd::Start).then(|| {
        let _ = std::fs::create_dir_all(paths::logs());
        std::fs::OpenOptions::new().create(true).append(true).open(paths::logs().join("viherd.log")).ok()
    }).flatten();
    match log_file {
        Some(f) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt;
            tracing_subscriber::fmt().with_target(false).with_ansi(false)
                .with_writer(std::io::stderr.and(std::sync::Mutex::new(f))).init();
        }
        // MCP : stdout est réservé aux messages JSON-RPC
        None if matches!(cli.cmd, Cmd::Mcp) => tracing_subscriber::fmt().with_target(false).with_ansi(false).with_writer(std::io::stderr).init(),
        None => tracing_subscriber::fmt().with_target(false).init(),
    }
    match run(cli).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("✖ {e:#}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    match cli.cmd {
        Cmd::Start => daemon::run().await?,
        Cmd::Ui => {
            // Lance l'appli de bureau si elle est à côté, sinon ouvre le navigateur.
            let app = std::env::current_exe()?.with_file_name("viherd-app.exe");
            if app.exists() {
                std::process::Command::new(app).spawn()?;
            } else {
                let url = format!("http://127.0.0.1:{}/", Global::load()?.api_port);
                std::process::Command::new("cmd").args(["/C", "start", "", &url]).status()?;
                println!("→ {url} (le démon doit tourner : `viherd start`)");
            }
        }
        Cmd::Park { path } => {
            let mut g = Global::load()?;
            let p = config::canonical(path.unwrap_or(cwd))?;
            if !g.parked.contains(&p) {
                g.parked.push(p.clone());
            }
            g.save()?;
            println!("✔ {} parqué", p.display());
            notify_reload().await;
        }
        Cmd::Link { name } => {
            let mut g = Global::load()?;
            let name = site_name(&name.unwrap_or_else(|| cwd.file_name().unwrap().to_string_lossy().into()));
            g.links.insert(name.clone(), cwd);
            g.save()?;
            println!("✔ http://{name}.{}", g.tld);
            notify_reload().await;
        }
        Cmd::Unlink { name } => {
            let mut g = Global::load()?;
            g.links.remove(&name);
            g.save()?;
            notify_reload().await;
        }
        Cmd::Sites => {
            let g = Global::load()?;
            for (n, s) in config::discover_sites(&g) {
                println!("{n}.{:<8} php {:<5} {}", g.tld, s.php(&g), s.path.display());
            }
        }
        Cmd::Init => {
            let p = cwd.join(config::PROJECT_FILE);
            if p.exists() {
                bail!("viherd.toml existe déjà");
            }
            std::fs::write(&p, include_str!("../viherd.example.toml"))?;
            println!("✔ {} créé", p.display());
        }
        Cmd::Install { what } => match what {
            Install::Php { version } => versions::install_php(&version).await?,
            Install::Node { version } => versions::install_node(&version).await?,
            Install::Composer => versions::install_composer().await?,
        },
        Cmd::Use { tool, version } => {
            let root = find_project_root(&cwd);
            let mut p = Project::load(&root)?;
            match tool.as_str() {
                "php" => { versions::ensure_php(&version).await?; p.php = Some(version); }
                "node" => { versions::ensure_node(&version).await?; p.node = Some(version); }
                _ => bail!("outil inconnu: {tool} (php|node)"),
            }
            p.save(&root)?;
            println!("✔ {} mis à jour", root.join(config::PROJECT_FILE).display());
            notify_reload().await;
        }
        Cmd::Service { name, state } => {
            let mut g = Global::load()?;
            let Some(s) = g.services.get_mut(&name) else { bail!("service inconnu: {name}") };
            let enabled = matches!(state.as_str(), "on" | "enable" | "start" | "true");
            s.enabled = enabled;
            g.save()?;
            println!("✔ {name}: {} (relance `viherd start` pour appliquer)", if enabled { "activé" } else { "désactivé" });
        }
        Cmd::Run { args } => return exec(&args[0], &args[1..], true).await,
        Cmd::Php { args } => return exec("php", &args, false).await,
        Cmd::Node { args } => return exec("node", &args, false).await,
        Cmd::Npm { args } => return exec("npm", &args, false).await,
        Cmd::Npx { args } => return exec("npx", &args, false).await,
        Cmd::Composer { args } => return exec("composer", &args, false).await,
        Cmd::Artisan { args } => {
            let mut a = vec!["artisan".to_string()];
            a.extend(args);
            return exec("php", &a, false).await;
        }
        Cmd::Secure { name } => {
            let mut g = Global::load()?;
            let name = site_or_cwd(&g, name, &cwd)?;
            if !certs::is_trusted() {
                println!("Windows va demander d'approuver l'autorité « Viherd Local CA »…");
                certs::trust()?;
            }
            g.secured.insert(name.clone());
            g.save()?;
            let port = if g.https_port == 443 { String::new() } else { format!(":{}", g.https_port) };
            println!("✔ https://{name}.{}{port}  (pense à APP_URL dans le .env)", g.tld);
            notify_reload().await;
        }
        Cmd::Unsecure { name } => {
            let mut g = Global::load()?;
            let name = site_or_cwd(&g, name, &cwd)?;
            g.secured.remove(&name);
            g.save()?;
            println!("✔ http://{name}.{}", g.tld);
            notify_reload().await;
        }
        Cmd::Trust => {
            certs::trust()?;
            println!("✔ autorité approuvée ({})", certs::ca_cert_path().display());
            println!("  Firefox : Paramètres → Certificats → Importer ce fichier, ou security.enterprise_roots.enabled = true");
        }
        Cmd::Share { name } => {
            let g = Global::load()?;
            let name = site_or_cwd(&g, name, &cwd)?;
            return share::run_foreground(&g, &name).await;
        }
        Cmd::Hosts => {
            let n = hosts::sync(&Global::load()?)?;
            println!("✔ {n} site(s) écrits dans le fichier hosts");
        }
        Cmd::Status => {
            let g = Global::load()?;
            let v: serde_json::Value = download::client()
                .get(format!("http://127.0.0.1:{}/status", g.api_port))
                .send().await.map_err(|_| anyhow::anyhow!("le démon ne tourne pas (`viherd start`)"))?
                .json().await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Reload => {
            if !notify_reload().await {
                bail!("le démon ne tourne pas (`viherd start`)");
            }
            println!("✔ rechargé");
        }
        Cmd::Update => match update::check().await? {
            None => println!("✔ Viherd v{} est à jour", viherd::VERSION),
            Some(a) => {
                println!("↓ Viherd v{} ({})", a.version, a.page);
                let setup = update::download(&a).await?;
                update::launch(&setup)?;
                println!("✔ installeur lancé : suis les étapes dans sa fenêtre");
            }
        },
        Cmd::Herd { yes } => return import_herd(yes).await,
        Cmd::Mcp => viherd::mcp::serve().await?,
    }
    Ok(0)
}

/// `viherd herd` : montre ce qui serait importé ; `--yes` importe tout ce qui n'est pas déjà en place.
async fn import_herd(yes: bool) -> Result<i32> {
    let p = herd::plan(&Global::load()?);
    if !p.found {
        bail!("configuration de Herd introuvable dans {}", herd::config_dir().display());
    }
    let todo = |l: &[herd::Entry]| l.iter().filter(|e| !e.done).map(|e| e.id.clone()).collect::<Vec<_>>();
    let show = |title: &str, l: &[herd::Entry]| {
        if l.is_empty() { return; }
        println!("{title}");
        for e in l {
            println!("  {} {}  {}", if e.done { "✔" } else { "+" }, e.label, e.detail);
        }
    };
    show("Dossiers parqués", &p.parked);
    show("Sites liés", &p.links);
    show("HTTPS", &p.secured);
    show("Versions de PHP", &p.php);
    for f in &p.env {
        println!(".env de {}", f.site);
        for c in &f.changes { println!("  {} : {} → {}", c.key, c.from, c.to); }
        for w in &f.warnings { println!("  ! {w}"); }
    }
    if let Some((h, s)) = p.ports { println!("Ports : Viherd utilise {h}/{s}, Herd 80/443"); }
    for n in &p.notes { println!("! {n}"); }
    if !yes {
        println!("\nRien n'a été modifié. Relance avec --yes pour importer (ou passe par l'interface : Machine locale → Importer depuis Herd).");
        return Ok(0);
    }
    let r = herd::apply(&herd::Choice {
        parked: todo(&p.parked), links: todo(&p.links), secured: todo(&p.secured), php: todo(&p.php),
        env: p.env.iter().filter(|f| !f.changes.is_empty()).map(|f| f.site.clone()).collect(),
        ports: false,
    })?;
    println!("✔ {} dossier(s), {} lien(s), {} site(s) HTTPS, {} version(s) de PHP, {} .env corrigé(s)", r.parked, r.links, r.secured, r.php, r.env.len());
    for n in &r.notes { println!("! {n}"); }
    notify_reload().await;
    Ok(0)
}

/// Lance un binaire avec l'environnement isolé du projet courant.
async fn exec(program: &str, args: &[String], ensure_all: bool) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let root = find_project_root(&cwd);
    let project = Project::load(&root)?;
    let g = Global::load()?;
    let php = project.php.clone().unwrap_or_else(|| g.default_php.clone());
    let node = project.node.clone().unwrap_or_else(|| g.default_node.clone());
    if ensure_all || matches!(program, "php" | "composer") {
        versions::ensure_php(&php).await?;
        if project.needs_xdebug() {
            versions::ensure_xdebug(&php).await?;
        }
    }
    if ensure_all || matches!(program, "node" | "npm" | "npx") {
        versions::ensure_node(&node).await?;
    }
    if program == "composer" && !paths::composer_dir().join("composer.phar").exists() {
        versions::install_composer().await?;
    }
    let envs = env::project_env(&g, &root, &project);
    let status = env::build_command(program, args, &cwd, &envs)?.status()?;
    Ok(status.code().unwrap_or(1))
}

/// Le site nommé, ou celui du dossier courant (lié ou dans un dossier parqué).
fn site_or_cwd(g: &Global, name: Option<String>, cwd: &std::path::Path) -> Result<String> {
    let sites = config::discover_sites(g);
    if let Some(n) = name {
        let n = site_name(n.trim_end_matches(&format!(".{}", g.tld)));
        return if sites.contains_key(&n) { Ok(n) } else { bail!("site inconnu: {n}") };
    }
    let root = find_project_root(cwd);
    sites.values().find(|s| s.path == root || cwd.starts_with(&s.path)).map(|s| s.name.clone())
        .ok_or_else(|| anyhow::anyhow!("ce dossier n'est pas un site Viherd (`viherd link` ou `viherd park`)"))
}

async fn notify_reload() -> bool {
    let Ok(g) = Global::load() else { return false };
    download::client()
        .post(format!("http://127.0.0.1:{}/reload", g.api_port))
        .header("X-Viherd", "1")
        .send().await
        .is_ok_and(|r| r.status().is_success())
}
