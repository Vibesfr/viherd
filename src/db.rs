//! Client de base de données intégré (façon phpMyAdmin / TablePlus) pour les services MySQL et PostgreSQL :
//! parcourir, filtrer, modifier, insérer, supprimer, requêtes SQL libres, exports et sauvegardes locales.
use crate::config::Global;
use crate::download::find_file;
use crate::paths;
use anyhow::{anyhow, bail, Result};
use mysql_async::prelude::*;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Pg,
    My,
}

pub struct Target {
    pub kind: Kind,
    pub svc: String,
    pub port: u16,
}

pub fn target(svc: &str) -> Result<Target> {
    let g = Global::load()?;
    let s = g.services.get(svc).ok_or_else(|| anyhow!("service inconnu: {svc}"))?;
    let kind = match svc {
        "postgres" => Kind::Pg,
        "mysql" => Kind::My,
        _ => bail!("{svc} n'est pas une base de données"),
    };
    Ok(Target { kind, svc: svc.into(), port: s.port })
}

#[derive(Serialize, Default)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// Lignes modifiées par les requêtes sans résultat (INSERT, UPDATE, DELETE…)
    pub affected: u64,
    pub ms: u128,
    pub truncated: bool,
}

const MAX_ROWS: usize = 5000;

/* ---------------------------------------------------------------- exécution brute */

async fn pg_client(t: &Target, db: &str) -> Result<tokio_postgres::Client> {
    let mut cfg = tokio_postgres::Config::new();
    cfg.host("127.0.0.1").port(t.port).user("postgres").dbname(if db.is_empty() { "postgres" } else { db });
    cfg.connect_timeout(std::time::Duration::from_secs(5));
    let (client, conn) = cfg.connect(tokio_postgres::NoTls).await
        .map_err(|e| anyhow!("connexion PostgreSQL impossible ({e}). Le service est-il démarré ?"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(client)
}

async fn my_conn(t: &Target, db: &str) -> Result<mysql_async::Conn> {
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(t.port)
        .user(Some("root"))
        .db_name(if db.is_empty() { None } else { Some(db.to_string()) });
    mysql_async::Conn::new(opts).await
        .map_err(|e| anyhow!("connexion MySQL impossible ({e}). Le service est-il démarré ?"))
}

fn my_val(v: mysql_async::Value) -> Option<String> {
    use mysql_async::Value::*;
    match v {
        NULL => None,
        Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        Int(i) => Some(i.to_string()),
        UInt(u) => Some(u.to_string()),
        Float(f) => Some(f.to_string()),
        Double(d) => Some(d.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

/// Exécute une ou plusieurs requêtes et renvoie le dernier jeu de résultats (protocole texte : tous types).
pub async fn run(t: &Target, db: &str, sql: &str) -> Result<ResultSet> {
    let t0 = Instant::now();
    let mut out = ResultSet::default();
    match t.kind {
        Kind::Pg => {
            let c = pg_client(t, db).await?;
            let msgs = c.simple_query(sql).await.map_err(|e| anyhow!(pg_err(&e)))?;
            let (mut cols, mut rows, mut has_set): (Vec<String>, Vec<Vec<Option<String>>>, bool) = (vec![], vec![], false);
            for m in msgs {
                match m {
                    tokio_postgres::SimpleQueryMessage::RowDescription(d) => {
                        cols = d.iter().map(|c| c.name().to_string()).collect();
                        rows = vec![];
                        has_set = true;
                    }
                    tokio_postgres::SimpleQueryMessage::Row(r) => {
                        if cols.is_empty() {
                            cols = r.columns().iter().map(|c| c.name().to_string()).collect();
                            has_set = true;
                        }
                        if rows.len() < MAX_ROWS {
                            rows.push((0..r.len()).map(|i| r.get(i).map(String::from)).collect());
                        } else {
                            out.truncated = true;
                        }
                    }
                    tokio_postgres::SimpleQueryMessage::CommandComplete(n) => {
                        if has_set {
                            out.columns = std::mem::take(&mut cols);
                            out.rows = std::mem::take(&mut rows);
                            has_set = false;
                        } else {
                            out.affected += n;
                        }
                    }
                    _ => {}
                }
            }
        }
        Kind::My => {
            let mut c = my_conn(t, db).await?;
            {
                let mut res = c.query_iter(sql).await?;
                loop {
                    let cols = res.columns();
                    let set: Vec<mysql_async::Row> = res.collect().await?;
                    match cols {
                        Some(cs) if !cs.is_empty() => {
                            out.columns = cs.iter().map(|c| c.name_str().into_owned()).collect();
                            out.truncated = set.len() > MAX_ROWS;
                            out.rows = set.into_iter().take(MAX_ROWS).map(|r| r.unwrap().into_iter().map(my_val).collect()).collect();
                        }
                        _ => out.affected += res.affected_rows(),
                    }
                    if res.is_empty() {
                        break;
                    }
                }
            }
            let _ = c.disconnect().await;
        }
    }
    out.ms = t0.elapsed().as_millis();
    Ok(out)
}

fn pg_err(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => format!("{}{}", d.message(), d.detail().map(|x| format!(" — {x}")).unwrap_or_default()),
        None => e.to_string(),
    }
}

/* ---------------------------------------------------------------- échappement */

pub fn ident(k: Kind, s: &str) -> String {
    match k {
        Kind::Pg => format!("\"{}\"", s.replace('"', "\"\"")),
        Kind::My => format!("`{}`", s.replace('`', "``")),
    }
}

pub fn lit(k: Kind, v: Option<&str>) -> String {
    match v {
        None => "NULL".into(),
        Some(v) => match k {
            Kind::Pg => format!("'{}'", v.replace('\'', "''")),
            Kind::My => format!("'{}'", v.replace('\\', "\\\\").replace('\'', "''")),
        },
    }
}

fn table_ref(k: Kind, table: &str) -> String {
    match k {
        Kind::Pg => format!("\"public\".{}", ident(k, table)),
        Kind::My => ident(k, table),
    }
}

fn json_str(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn where_eq(k: Kind, cond: &Map<String, Value>) -> Result<String> {
    if cond.is_empty() {
        bail!("ligne non identifiable");
    }
    Ok(cond.iter().map(|(c, v)| match json_str(v) {
        None => format!("{} IS NULL", ident(k, c)),
        Some(s) => format!("{} = {}", ident(k, c), lit(k, Some(&s))),
    }).collect::<Vec<_>>().join(" AND "))
}

/* ---------------------------------------------------------------- navigation */

pub async fn databases(t: &Target) -> Result<Vec<String>> {
    let sql = match t.kind {
        Kind::Pg => "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY datname",
        Kind::My => "SELECT schema_name FROM information_schema.schemata WHERE schema_name NOT IN ('information_schema','performance_schema','mysql','sys') ORDER BY 1",
    };
    Ok(run(t, "", sql).await?.rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
}

pub async fn tables(t: &Target, db: &str) -> Result<Value> {
    let sql = match t.kind {
        // n_live_tup suit les insertions quasi en direct, reltuples seulement après un ANALYZE
        Kind::Pg => "SELECT c.relname, COALESCE(s.n_live_tup, GREATEST(c.reltuples, 0))::bigint, pg_total_relation_size(c.oid), \
                     (SELECT count(*) FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped) FROM pg_class c \
                     JOIN pg_namespace n ON n.oid = c.relnamespace LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid \
                     WHERE n.nspname = 'public' AND c.relkind IN ('r','p') ORDER BY 1",
        Kind::My => "SELECT t.table_name, t.table_rows, COALESCE(t.data_length, 0) + COALESCE(t.index_length, 0), \
                     (SELECT count(*) FROM information_schema.columns c WHERE c.table_schema = t.table_schema AND c.table_name = t.table_name) \
                     FROM information_schema.tables t WHERE t.table_schema = DATABASE() AND t.table_type = 'BASE TABLE' ORDER BY 1",
    };
    let r = run(t, db, sql).await?;
    let num = |v: Option<&Option<String>>| v.cloned().flatten().and_then(|s| s.parse::<u64>().ok());
    let mut tables: Vec<(String, Option<u64>, Option<u64>, Option<u64>)> = r.rows.iter()
        .filter_map(|row| Some((row[0].clone()?, num(row.get(1)), num(row.get(2)), num(row.get(3))))).collect();

    // Les compteurs du moteur ne sont que des estimations (0 tant que PostgreSQL n'a pas analysé la table,
    // approximatifs pour InnoDB) : on compte pour de vrai les tables raisonnables, en une requête par lot.
    const EXACT_UNDER: u64 = 200_000;
    let small: Vec<usize> = (0..tables.len()).filter(|&i| tables[i].1.unwrap_or(0) < EXACT_UNDER).collect();
    for chunk in small.chunks(80) {
        let sql = chunk.iter().map(|&i| format!("SELECT {}, COUNT(*) FROM {}", lit(t.kind, Some(&tables[i].0)), table_ref(t.kind, &tables[i].0)))
            .collect::<Vec<_>>().join(" UNION ALL ");
        if let Ok(res) = run(t, db, &sql).await {
            for row in res.rows {
                if let (Some(name), Some(n)) = (row[0].as_deref(), num(row.get(1))) {
                    if let Some(tb) = tables.iter_mut().find(|x| x.0 == name) {
                        tb.1 = Some(n);
                    }
                }
            }
        }
    }
    Ok(Value::Array(tables.into_iter().map(|(name, rows, size, columns)| json!({
        "name": name, "rows": rows, "size": size, "columns": columns,
    })).collect()))
}

pub async fn columns(t: &Target, db: &str, table: &str) -> Result<Value> {
    let tl = lit(t.kind, Some(table));
    let sql = match t.kind {
        Kind::Pg => format!(
            "SELECT a.attname, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, pg_get_expr(d.adbin, d.adrelid), \
             EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = a.attrelid AND i.indisprimary AND a.attnum = ANY(i.indkey)), '' \
             FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = (SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'public' AND c.relname = {tl}) \
             AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"),
        Kind::My => format!(
            "SELECT column_name, column_type, is_nullable = 'YES', column_default, column_key = 'PRI', extra \
             FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = {tl} ORDER BY ordinal_position"),
    };
    let truthy = |v: &Option<String>| matches!(v.as_deref(), Some("t") | Some("1") | Some("true"));
    let r = run(t, db, &sql).await?;
    Ok(Value::Array(r.rows.into_iter().map(|row| json!({
        "name": row[0], "type": row[1], "nullable": truthy(&row[2]), "default": row[3], "pk": truthy(&row[4]), "extra": row[5],
    })).collect()))
}

pub async fn rows(t: &Target, db: &str, table: &str, filter: &str, order: &str, desc: bool, limit: u64, offset: u64) -> Result<Value> {
    let tr = table_ref(t.kind, table);
    let wh = if filter.trim().is_empty() { String::new() } else { format!(" WHERE {filter}") };
    let ord = if order.is_empty() { String::new() } else { format!(" ORDER BY {} {}", ident(t.kind, order), if desc { "DESC" } else { "ASC" }) };
    let data = run(t, db, &format!("SELECT * FROM {tr}{wh}{ord} LIMIT {limit} OFFSET {offset}")).await?;
    let total = run(t, db, &format!("SELECT COUNT(*) FROM {tr}{wh}")).await?
        .rows.first().and_then(|r| r[0].clone()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    Ok(json!({ "columns": data.columns, "rows": data.rows, "total": total, "ms": data.ms }))
}

/* ---------------------------------------------------------------- modifications */

pub async fn update(t: &Target, db: &str, table: &str, pk: &Map<String, Value>, set: &Map<String, Value>) -> Result<u64> {
    if set.is_empty() {
        bail!("rien à modifier");
    }
    let sets = set.iter().map(|(c, v)| format!("{} = {}", ident(t.kind, c), lit(t.kind, json_str(v).as_deref()))).collect::<Vec<_>>().join(", ");
    let sql = format!("UPDATE {} SET {sets} WHERE {}", table_ref(t.kind, table), where_eq(t.kind, pk)?);
    let n = run(t, db, &sql).await?.affected;
    if n > 1 {
        tracing::warn!("UPDATE a touché {n} lignes sur {table}");
    }
    Ok(n)
}

pub async fn delete(t: &Target, db: &str, table: &str, pks: &[Value]) -> Result<u64> {
    if pks.is_empty() {
        bail!("aucune ligne sélectionnée");
    }
    let conds = pks.iter().map(|p| p.as_object().ok_or_else(|| anyhow!("clé invalide")).and_then(|m| where_eq(t.kind, m)).map(|w| format!("({w})")))
        .collect::<Result<Vec<_>>>()?;
    run(t, db, &format!("DELETE FROM {} WHERE {}", table_ref(t.kind, table), conds.join(" OR "))).await.map(|r| r.affected)
}

pub async fn insert(t: &Target, db: &str, table: &str, values: &Map<String, Value>) -> Result<u64> {
    let tr = table_ref(t.kind, table);
    let sql = if values.is_empty() {
        match t.kind { Kind::Pg => format!("INSERT INTO {tr} DEFAULT VALUES"), Kind::My => format!("INSERT INTO {tr} () VALUES ()") }
    } else {
        let cols = values.keys().map(|c| ident(t.kind, c)).collect::<Vec<_>>().join(", ");
        let vals = values.values().map(|v| lit(t.kind, json_str(v).as_deref())).collect::<Vec<_>>().join(", ");
        format!("INSERT INTO {tr} ({cols}) VALUES ({vals})")
    };
    run(t, db, &sql).await.map(|r| r.affected)
}

/* ---------------------------------------------------------------- exports */

pub fn to_csv(rs: &ResultSet) -> String {
    let cell = |s: &str| if s.contains([',', '"', '\n', '\r', ';']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
    let mut out = String::from("\u{feff}"); // BOM : Excel lit l'UTF-8 correctement
    out += &rs.columns.iter().map(|c| cell(c)).collect::<Vec<_>>().join(",");
    out += "\r\n";
    for r in &rs.rows {
        out += &r.iter().map(|v| v.as_deref().map(cell).unwrap_or_default()).collect::<Vec<_>>().join(",");
        out += "\r\n";
    }
    out
}

pub async fn table_csv(t: &Target, db: &str, table: &str, filter: &str) -> Result<String> {
    let wh = if filter.trim().is_empty() { String::new() } else { format!(" WHERE {filter}") };
    Ok(to_csv(&run(t, db, &format!("SELECT * FROM {}{wh}", table_ref(t.kind, table))).await?))
}

fn tool(t: &Target, names: &[&str]) -> Result<PathBuf> {
    let dir = paths::service_dir(&t.svc);
    for n in names {
        let p = dir.join("bin").join(n);
        if p.exists() {
            return Ok(p);
        }
    }
    names.iter().find_map(|n| find_file(&dir, n)).ok_or_else(|| anyhow!("{} introuvable", names[0]))
}

async fn run_tool(prog: PathBuf, args: Vec<String>, stdin_file: Option<PathBuf>) -> Result<()> {
    let mut c = tokio::process::Command::new(prog);
    c.args(&args).env("PGCLIENTENCODING", "UTF8").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    c.stdin(match &stdin_file {
        Some(f) => std::process::Stdio::from(std::fs::File::open(f)?),
        None => std::process::Stdio::null(),
    });
    #[cfg(windows)]
    c.creation_flags(0x0800_0000);
    let out = c.output().await?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Écrit un dump SQL complet de la base dans `file`.
pub async fn dump(t: &Target, db: &str, file: &std::path::Path) -> Result<()> {
    let f = file.display().to_string();
    match t.kind {
        Kind::Pg => run_tool(tool(t, &["pg_dump.exe"])?, vec![
            "-h".into(), "127.0.0.1".into(), "-p".into(), t.port.to_string(), "-U".into(), "postgres".into(),
            "--clean".into(), "--if-exists".into(), "--no-owner".into(), "--encoding=UTF8".into(), "-f".into(), f, db.into()], None).await,
        Kind::My => run_tool(tool(t, &["mariadb-dump.exe", "mysqldump.exe"])?, vec![
            "-h".into(), "127.0.0.1".into(), format!("--port={}", t.port), "-u".into(), "root".into(),
            "--single-transaction".into(), "--routines".into(), "--triggers".into(), "--default-character-set=utf8mb4".into(),
            format!("--result-file={f}"), db.into()], None).await,
    }
}

/// Remplace entièrement la base par le contenu d'un dump.
pub async fn restore(t: &Target, db: &str, file: &std::path::Path) -> Result<()> {
    match t.kind {
        Kind::Pg => {
            run(t, "postgres", &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", ident(t.kind, db))).await?;
            run(t, "postgres", &format!("CREATE DATABASE {} ENCODING 'UTF8'", ident(t.kind, db))).await?;
            run_tool(tool(t, &["psql.exe"])?, vec![
                "-h".into(), "127.0.0.1".into(), "-p".into(), t.port.to_string(), "-U".into(), "postgres".into(),
                "-d".into(), db.into(), "-q".into(), "-f".into(), file.display().to_string()], None).await
        }
        Kind::My => {
            run(t, "", &format!("DROP DATABASE IF EXISTS {}", ident(t.kind, db))).await?;
            run(t, "", &format!("CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci", ident(t.kind, db))).await?;
            run_tool(tool(t, &["mariadb.exe", "mysql.exe"])?, vec![
                "-h".into(), "127.0.0.1".into(), format!("--port={}", t.port), "-u".into(), "root".into(),
                "--default-character-set=utf8mb4".into(), db.into()], Some(file.to_path_buf())).await
        }
    }
}

/* ---------------------------------------------------------------- sauvegardes locales */

pub fn backup_dir(t: &Target, db: &str) -> Result<PathBuf> {
    if db.is_empty() || db.contains(['/', '\\', '.', ':']) {
        bail!("nom de base invalide");
    }
    Ok(paths::home().join("backups").join(&t.svc).join(db))
}

/// Fichier de sauvegarde existant, sans possibilité de sortir du dossier.
pub fn backup_file(t: &Target, db: &str, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") || !name.ends_with(".sql") {
        bail!("sauvegarde invalide");
    }
    let p = backup_dir(t, db)?.join(name);
    if !p.is_file() {
        bail!("sauvegarde introuvable");
    }
    Ok(p)
}

pub async fn create_backup(t: &Target, db: &str, label: &str) -> Result<String> {
    let dir = backup_dir(t, db)?;
    std::fs::create_dir_all(&dir)?;
    let clean: String = label.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else if c == ' ' { '-' } else { '_' }).take(60).collect();
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let name = if clean.trim_matches(['-', '_']).is_empty() { format!("{stamp}.sql") } else { format!("{stamp}__{clean}.sql") };
    let file = dir.join(&name);
    if let Err(e) = dump(t, db, &file).await {
        let _ = std::fs::remove_file(&file);
        return Err(e);
    }
    tracing::info!("sauvegarde {}/{db} : {name}", t.svc);
    Ok(name)
}

pub fn list_backups(t: &Target, db: &str) -> Result<Value> {
    let dir = backup_dir(t, db)?;
    let mut v: Vec<Value> = std::fs::read_dir(&dir).map(|rd| rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let md = e.metadata().ok();
            let label = name.trim_end_matches(".sql").split_once("__").map(|(_, l)| l.replace('-', " ")).unwrap_or_default();
            json!({
                "file": name,
                "label": label,
                "size": md.as_ref().map(|m| m.len()).unwrap_or(0),
                "created": md.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0),
            })
        }).collect()).unwrap_or_default();
    v.sort_by(|a, b| b["file"].as_str().cmp(&a["file"].as_str()));
    Ok(Value::Array(v))
}

/// Restaure une sauvegarde après en avoir fait une de l'état actuel (filet de sécurité).
pub async fn restore_backup(t: &Target, db: &str, name: &str) -> Result<String> {
    let file = backup_file(t, db, name)?;
    let safety = create_backup(t, db, "avant-restauration").await.unwrap_or_default();
    restore(t, db, &file).await?;
    tracing::info!("base {}/{db} restaurée depuis {name}", t.svc);
    Ok(safety)
}
