//! API JSON de l'interface graphique (servie par le démon sur http://127.0.0.1:7777/).
use crate::config::{Global, Project};
use crate::server::{full, json, pct_decode, Body, State};
use crate::redis::Reply;
use crate::{certs, daemon, db, env, herd, hosts, paths, services, share, supervisor, update, versions};
use anyhow::{anyhow, bail, Result};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::{Request, Response};
use serde_json::{json as j, Value};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

pub async fn handle(req: Request<Incoming>, st: Arc<State>) -> Result<Response<Body>> {
    match route(req, &st).await {
        Ok(r) => Ok(r),
        Err(e) => Ok(Response::builder()
            .status(400)
            .header(CONTENT_TYPE, "application/json")
            .body(full(serde_json::to_vec(&j!({ "error": format!("{e:#}") }))?))?),
    }
}

fn ok() -> Result<Response<Body>> {
    json(j!({ "ok": true }))
}

/// Réponse « fichier à enregistrer » (export CSV, dump SQL, sauvegarde).
fn attachment(bytes: Vec<u8>, filename: &str, mime: &str) -> Result<Response<Body>> {
    Ok(Response::builder()
        .header(CONTENT_TYPE, mime)
        .header("Content-Disposition", format!("attachment; filename=\"{}\"", filename.replace('"', "")))
        .header("Cache-Control", "no-store")
        .body(full(bytes))?)
}

async fn db_api(method: &str, path: &str, query: &str, body: &Value) -> Result<Option<Response<Body>>> {
    let q = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(pct_decode).unwrap_or_default();
    let b =|k: &str| body.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let svc = if method == "GET" { q("svc") } else { b("svc") };
    let db_name = if method == "GET" { q("db") } else { b("db") };
    let t = db::target(&svc)?;
    let r = match (method, path) {
        ("GET", "/api/db/databases") => json(j!(db::databases(&t).await?))?,
        ("GET", "/api/db/tables") => json(db::tables(&t, &db_name).await?)?,
        ("GET", "/api/db/columns") => json(db::columns(&t, &db_name, &q("table")).await?)?,
        ("POST", "/api/db/rows") => {
            let n = |k: &str, d: u64| body.get(k).and_then(|v| v.as_u64()).unwrap_or(d);
            json(db::rows(&t, &db_name, &b("table"), &b("where"), &b("order"), body.get("desc").and_then(|v| v.as_bool()).unwrap_or(false),
                n("limit", 50).min(1000), n("offset", 0)).await?)?
        }
        ("POST", "/api/db/query") => json(serde_json::to_value(db::run(&t, &db_name, &b("sql")).await?)?)?,
        ("POST", "/api/db/update") => {
            let obj = |k: &str| body.get(k).and_then(|v| v.as_object()).cloned().unwrap_or_default();
            json(j!({ "affected": db::update(&t, &db_name, &b("table"), &obj("pk"), &obj("set")).await? }))?
        }
        ("POST", "/api/db/delete") => {
            let pks = body.get("pks").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            json(j!({ "affected": db::delete(&t, &db_name, &b("table"), &pks).await? }))?
        }
        ("POST", "/api/db/insert") => {
            let vals = body.get("values").and_then(|v| v.as_object()).cloned().unwrap_or_default();
            json(j!({ "affected": db::insert(&t, &db_name, &b("table"), &vals).await? }))?
        }
        ("GET", "/api/db/export.csv") => {
            let table = q("table");
            attachment(db::table_csv(&t, &db_name, &table, &q("where")).await?.into_bytes(), &format!("{db_name}-{table}.csv"), "text/csv; charset=utf-8")?
        }
        ("GET", "/api/db/dump.sql") => {
            let tmp = std::env::temp_dir().join(format!("viherd-dump-{}-{db_name}.sql", std::process::id()));
            db::dump(&t, &db_name, &tmp).await?;
            let bytes = std::fs::read(&tmp)?;
            let _ = std::fs::remove_file(&tmp);
            let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M");
            attachment(bytes, &format!("{db_name}_{stamp}.sql"), "application/sql")?
        }
        ("GET", "/api/db/backups") => json(db::list_backups(&t, &db_name)?)?,
        ("POST", "/api/db/backups") => json(j!({ "file": db::create_backup(&t, &db_name, &b("label")).await? }))?,
        ("POST", "/api/db/backups/restore") => json(j!({ "safety": db::restore_backup(&t, &db_name, &b("file")).await? }))?,
        ("POST", "/api/db/backups/delete") => {
            std::fs::remove_file(db::backup_file(&t, &db_name, &b("file"))?)?;
            ok()?
        }
        ("GET", "/api/db/backups/download") => {
            let name = q("file");
            attachment(std::fs::read(db::backup_file(&t, &db_name, &name)?)?, &format!("{db_name}_{name}"), "application/sql")?
        }
        _ => return Ok(None),
    };
    Ok(Some(r))
}

fn site_of(st: &State, name: &str) -> Result<crate::config::Site> {
    st.sites.read().unwrap().get(name).cloned().ok_or_else(|| anyhow!("site inconnu: {name}"))
}

/// Chemin autorisé à être ouvert depuis l'interface : dans un site, ou le dossier des logs.
fn allowed_path(st: &State, p: &str) -> bool {
    let p = p.replace('/', "\\").to_lowercase();
    let logs = paths::logs().display().to_string().to_lowercase();
    p.starts_with(&logs) || st.sites.read().unwrap().values().any(|s| p.starts_with(&s.path.display().to_string().replace('/', "\\").to_lowercase()))
}

async fn route(req: Request<Incoming>, st: &Arc<State>) -> Result<Response<Body>> {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let q = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(pct_decode).unwrap_or_default();
    let bytes = req.into_body().collect().await?.to_bytes();
    let body: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes)? };
    let s = |k: &str| body.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

    if path.starts_with("/api/db/") {
        if let Some(r) = db_api(&method, &path, &query, &body).await? {
            return Ok(r);
        }
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/api/state") => state(st),
        ("GET", "/api/logs") => {
            let key = q("key");
            let file = if key == "daemon" { paths::logs().join("viherd.log") } else { supervisor::log_path(&key) };
            let n = q("n").parse::<usize>().unwrap_or(300).clamp(50, 5000);
            json(j!({ "log": tail_lines(&file, n) }))
        }
        ("GET", "/api/dev") => {
            // mode dev : l'interface surveille la date de ui.html pour se recharger toute seule
            let stamp = crate::server::dev_ui_path()
                .and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64);
            json(j!({ "dev": stamp.is_some(), "stamp": stamp }))
        }
        ("POST", "/api/quit") => {
            // arrêt propre de tout Viherd (utilisé par dev.ps1 avant de recompiler)
            st.quit.notify_one();
            ok()
        }
        ("POST", "/api/reload") => {
            daemon::reload(st).await?;
            ok()
        }

        // ---------- versions isolées par site ----------
        ("POST", "/api/site/php") | ("POST", "/api/site/node") => {
            let site = site_of(st, &s("site"))?;
            let version = s("version");
            let mut p = Project::load(&site.path)?;
            let v = if version.is_empty() { None } else { Some(version) };
            if path.ends_with("php") {
                if let Some(v) = &v { versions::ensure_php(v).await?; }
                p.php = v;
            } else {
                if let Some(v) = &v { versions::ensure_node(v).await?; }
                p.node = v;
            }
            p.save(&site.path)?;
            daemon::reload(st).await?;
            ok()
        }
        // ---------- édition du viherd.toml : workers, routes, env, proxy, racine ----------
        ("POST", "/api/site/project") => {
            let site = site_of(st, &s("site"))?;
            let mut cur = serde_json::to_value(Project::load(&site.path)?)?;
            if let (Some(obj), Some(patch)) = (cur.as_object_mut(), body.get("patch").and_then(|p| p.as_object())) {
                for (k, v) in patch {
                    if ["workers", "routes", "env", "proxy", "root", "xdebug", "profile", "php_ini"].contains(&k.as_str()) {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }
            let p: Project = serde_json::from_value(cur).map_err(|e| anyhow!("configuration invalide: {e}"))?;
            let mut names = std::collections::HashSet::new();
            for w in &p.workers {
                if w.name.trim().is_empty() || w.command.trim().is_empty() { bail!("chaque worker doit avoir un nom et une commande"); }
                if !names.insert(w.name.clone()) { bail!("deux workers s'appellent « {} »", w.name); }
            }
            if p.needs_xdebug() {
                let php = site.php(&st.global.read().unwrap());
                versions::ensure_xdebug(&php).await?;
            }
            p.save(&site.path)?;
            daemon::reload(st).await?;
            ok()
        }

        // ---------- HTTPS et partage ----------
        ("POST", "/api/site/secure") => {
            let site = site_of(st, &s("site"))?;
            let on = body.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            if on && !certs::trusted_cached() {
                // certutil attend la confirmation de l'utilisateur dans une fenêtre Windows
                tokio::task::spawn_blocking(certs::trust).await??;
            }
            let mut g = Global::load()?;
            if on { g.secured.insert(site.name.clone()); } else { g.secured.remove(&site.name); }
            g.save()?;
            st.global.write().unwrap().secured = g.secured.clone();
            ok()
        }
        ("POST", "/api/certs/trust") => {
            tokio::task::spawn_blocking(certs::trust).await??;
            ok()
        }
        ("POST", "/api/certs/open") => {
            certs::Ca::load_or_create()?;
            std::process::Command::new("explorer").arg("/select,").arg(certs::ca_cert_path()).spawn()?;
            ok()
        }
        ("POST", "/api/site/share") => {
            let site = site_of(st, &s("site"))?;
            if body.get("on").and_then(|v| v.as_bool()).unwrap_or(true) {
                share::start(st, &site.name).await?;
            } else {
                share::stop(st, &site.name);
            }
            ok()
        }

        // ---------- workers / queues ----------
        ("POST", "/api/worker") => {
            let key = s("key");
            match s("action").as_str() {
                "start" | "restart" => daemon::start_worker(st, &key)?,
                "stop" => daemon::stop_worker(st, &key),
                a => bail!("action inconnue: {a}"),
            }
            ok()
        }
        ("POST", "/api/workers/all") => {
            // tout redémarrer / tout arrêter
            let keys: Vec<String> = st.sites.read().unwrap().values()
                .flat_map(|s| s.project.workers.iter().map(|w| daemon::worker_key(&s.name, &w.name)).collect::<Vec<_>>()).collect();
            match s("action").as_str() {
                "restart" => for k in &keys { daemon::start_worker(st, k)?; },
                "stop" => for k in &keys { daemon::stop_worker(st, k); },
                a => bail!("action inconnue: {a}"),
            }
            json(j!({ "ok": true, "count": keys.len() }))
        }
        ("GET", "/api/site/queue") => json(queue_state(&site_of(st, &q("site"))?).await?),
        ("POST", "/api/site/artisan") => {
            // commandes de queue autorisées depuis l'interface (liste fermée)
            let site = site_of(st, &s("site"))?;
            let args: Vec<String> = match s("cmd").as_str() {
                "retry-all" => vec!["queue:retry".into(), "all".into()],
                "retry" => {
                    let id = s("id");
                    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') { bail!("identifiant de job invalide"); }
                    vec!["queue:retry".into(), id]
                }
                "flush" => vec!["queue:flush".into()],
                "restart" => vec!["queue:restart".into()],
                c => bail!("commande non autorisée: {c}"),
            };
            let g = st.global.read().unwrap().clone();
            versions::ensure_php(&site.php(&g)).await?;
            let env = env::project_env(&g, &site.path, &site.project);
            let mut full_args = vec!["artisan".to_string()];
            full_args.extend(args);
            let mut cmd = tokio::process::Command::from(env::build_command("php", &full_args, &site.path, &env)?);
            cmd.stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
            let out = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output()).await??;
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            if !out.status.success() { bail!("{}", text.trim()); }
            json(j!({ "ok": true, "output": text.trim() }))
        }

        // ---------- débogage ----------
        ("GET", "/api/dumps") => json(serde_json::to_value(st.dumps.list())?),
        ("POST", "/api/dumps/clear") => {
            st.dumps.clear();
            ok()
        }
        ("POST", "/api/settings") => {
            let mut g = Global::load()?;
            if let Some(b) = body.get("dumps").and_then(|v| v.as_bool()) { g.dumps = b; }
            g.save()?;
            daemon::reload(st).await?; // relance php-cgi et workers avec le nouvel environnement
            ok()
        }
        ("GET", "/api/site/laravel-log") => {
            let site = site_of(st, &q("site"))?;
            match newest_log(&site.path.join("storage").join("logs")) {
                Some(f) => json(j!({ "file": f.display().to_string(), "text": tail_bytes(&f, 512 * 1024) })),
                None => json(j!({ "file": null, "text": "" })),
            }
        }
        ("POST", "/api/site/laravel-log/clear") => {
            let site = site_of(st, &s("site"))?;
            if let Some(f) = newest_log(&site.path.join("storage").join("logs")) {
                std::fs::write(f, "")?;
            }
            ok()
        }

        // ---------- ouvrir : explorateur, éditeur, terminal isolé ----------
        ("POST", "/api/open") => {
            let target = s("path");
            if !allowed_path(st, &target) { bail!("chemin non autorisé"); }
            std::process::Command::new("explorer").arg(&target).spawn()?;
            ok()
        }
        ("POST", "/api/open-editor") => {
            let target = s("path");
            if !allowed_path(st, &target) { bail!("chemin non autorisé"); }
            let line = body.get("line").and_then(|v| v.as_u64());
            let arg = match line { Some(l) => vec!["-g".to_string(), format!("{target}:{l}")], None => vec![target.clone()] };
            let ok_code = std::process::Command::new("cmd").args(["/C", "code"]).args(&arg)
                .creation_flags(CREATE_NO_WINDOW).stdout(Stdio::null()).stderr(Stdio::null())
                .status().map(|s| s.success()).unwrap_or(false);
            if !ok_code {
                bail!("VS Code introuvable (commande « code » absente du PATH)");
            }
            ok()
        }
        ("POST", "/api/open-terminal") => {
            let site = site_of(st, &s("site"))?;
            let g = st.global.read().unwrap().clone();
            // installe PHP / Node si besoin et répare un php.ini incomplet avant d'ouvrir la console
            versions::ensure_php(&site.php(&g)).await?;
            if let Some(n) = &site.project.node {
                versions::ensure_node(n).await?;
            }
            let banner = format!("echo Viherd ^| {} ^| PHP {} ^| Node {} && echo.", site.name, site.php(&g),
                site.project.node.clone().unwrap_or_else(|| g.default_node.clone()));
            std::process::Command::new("cmd").args(["/K", &banner])
                .current_dir(&site.path)
                .envs(env::project_env(&g, &site.path, &site.project))
                .creation_flags(CREATE_NEW_CONSOLE)
                .spawn()?;
            ok()
        }

        // ---------- choix d'un dossier (parquer, lier) ----------
        ("POST", "/api/fs/pick") => {
            // la fenêtre « Choisir un dossier » de Windows, ouverte par le démon (même session que l'utilisateur)
            let (title, start) = (s("title"), s("start"));
            let picked = tokio::task::spawn_blocking(move || {
                let mut d = rfd::FileDialog::new().set_title(if title.is_empty() { "Choisir un dossier" } else { &title });
                if !start.is_empty() && Path::new(&start).is_dir() { d = d.set_directory(&start); }
                d.pick_folder()
            }).await?;
            json(j!({ "path": picked.map(|p| crate::config::plain_path(&p).display().to_string()) }))
        }
        ("GET", "/api/fs/inspect") => json(inspect_dir(st, &q("path"))),
        ("GET", "/api/fs/suggest") => json(j!(suggest_dirs(&Global::load()?))),

        // ---------- projets ----------
        ("POST", "/api/project/new") => new_project(st, &s("dir"), &s("name"), &s("template")).await,
        ("POST", "/api/park") | ("POST", "/api/unpark") => {
            let mut g = Global::load()?;
            let p = s("path");
            if path.ends_with("unpark") {
                g.parked.retain(|x| x.display().to_string() != p);
            } else {
                let p = crate::config::canonical(&p).map_err(|_| anyhow!("dossier introuvable: {p}"))?;
                if !g.parked.contains(&p) { g.parked.push(p); }
            }
            g.save()?;
            daemon::reload(st).await?;
            ok()
        }
        ("POST", "/api/link") => {
            let mut g = Global::load()?;
            let p = crate::config::canonical(s("path")).map_err(|_| anyhow!("dossier introuvable"))?;
            let name = match s("name") {
                n if n.is_empty() => p.file_name().unwrap().to_string_lossy().to_string(),
                n => n,
            };
            g.links.insert(crate::config::site_name(&name), p);
            g.save()?;
            daemon::reload(st).await?;
            ok()
        }
        ("POST", "/api/unlink") => {
            let mut g = Global::load()?;
            g.links.remove(&s("name"));
            g.save()?;
            daemon::reload(st).await?;
            ok()
        }

        // ---------- services ----------
        ("POST", "/api/service") => {
            let name = s("name");
            let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            let mut g = Global::load()?;
            let svc = g.services.get_mut(&name).ok_or_else(|| anyhow!("service inconnu: {name}"))?;
            svc.enabled = enabled;
            let svc = svc.clone();
            g.save()?;
            // l'interface lit st.global : sans ça un service arrêté resterait affiché « activé »
            st.global.write().unwrap().services = g.services.clone();
            if enabled {
                if let Err(e) = services::start_one(&name, &svc, &st.sup).await {
                    // échec au démarrage (licence, téléchargement…) : on ne le laisse pas affiché « activé »
                    let mut g = Global::load()?;
                    if let Some(s) = g.services.get_mut(&name) { s.enabled = false; }
                    g.save()?;
                    st.global.write().unwrap().services = g.services.clone();
                    return Err(e);
                }
            } else {
                st.sup.stop(&format!("service:{name}"));
            }
            ok()
        }
        ("POST", "/api/service/license") => {
            // licence AIStor de MinIO : collée depuis l'interface, rangée dans ~/.viherd/licenses
            let content = s("content");
            if content.len() < 20 || content.len() > 64 * 1024 {
                bail!("licence invalide : colle le contenu complet du fichier minio.license");
            }
            let p = crate::config::minio_license();
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(&p, content)?;
            ok()
        }
        ("GET", "/api/service/databases") => {
            let name = q("name");
            let svc = st.global.read().unwrap().services.get(&name).cloned().ok_or_else(|| anyhow!("service inconnu: {name}"))?;
            json(j!({ "databases": services::list_databases(&name, &svc).await? }))
        }
        ("POST", "/api/service/databases") => {
            let name = s("name");
            let svc = st.global.read().unwrap().services.get(&name).cloned().ok_or_else(|| anyhow!("service inconnu: {name}"))?;
            services::create_database(&name, &svc, &s("database")).await?;
            ok()
        }
        ("POST", "/api/install") => {
            let v = s("version");
            match s("tool").as_str() {
                "php" => versions::install_php(&v).await?,
                "node" => versions::install_node(&v).await?,
                "composer" => versions::install_composer().await?,
                t => bail!("outil inconnu: {t}"),
            }
            ok()
        }
        // ---------- versions de PHP / Node : dernières publiées, version exacte installée, mise à jour ----------
        ("GET", "/api/versions") => {
            let avail = versions::available().await;
            let g = st.global.read().unwrap().clone();
            let mut out = serde_json::Map::new();
            for tool in ["php", "node"] {
                let dir = paths::home().join("bin").join(tool);
                let list = avail.as_ref().ok().and_then(|a| a[tool].as_array().cloned()).unwrap_or_default();
                let inst = installed(&dir);
                let mut rows: Vec<Value> = list.iter().map(|a| {
                    let v = a["v"].as_str().unwrap_or("").to_string();
                    j!({ "v": v, "latest": a["latest"], "lts": a["lts"], "installed": inst.contains(&v),
                         "current": inst.contains(&v).then(|| versions::installed_version(tool, &v)).flatten() })
                }).collect();
                // installées mais plus listées (anciennes branches) : affichées quand même
                for v in inst.iter().filter(|v| !list.iter().any(|a| a["v"] == v.as_str())) {
                    rows.push(j!({ "v": v, "latest": null, "lts": null, "installed": true, "current": versions::installed_version(tool, v) }));
                }
                out.insert(tool.into(), j!(rows));
            }
            out.insert("default_php".into(), j!(g.default_php));
            out.insert("default_node".into(), j!(g.default_node));
            out.insert("error".into(), j!(avail.err().map(|e| format!("{e:#}"))));
            json(Value::Object(out))
        }
        ("POST", "/api/runtime/update") => {
            let (tool, v) = (s("tool"), s("version"));
            if !["php", "node"].contains(&tool.as_str()) { bail!("outil inconnu: {tool}"); }
            // php-cgi et les workers verrouillent les fichiers : on arrête tout, on remplace, on relance
            st.php.stop_all().await;
            for k in st.sup.list().into_iter().filter(|k| k.starts_with("site:")) { st.sup.stop(&k); }
            for _ in 0..50 {
                let busy = st.sup.statuses().iter().any(|(k, s)| k.starts_with("site:") && matches!(s.state.as_str(), "running" | "starting" | "stopping"));
                if !busy { break; }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            let r = versions::update_runtime(&tool, &v).await;
            daemon::reload(st).await?;
            json(j!({ "ok": true, "version": r? }))
        }
        ("GET", "/api/php/extensions") => json(j!({ "php": q("php"), "extensions": crate::php_ext::list(&q("php"))? })),
        ("POST", "/api/php/extension") => {
            let on = body.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            crate::php_ext::set(&s("php"), &s("name"), on).await?;
            daemon::reload(st).await?; // php-cgi et workers relancés avec le nouveau php.ini
            ok()
        }

        ("POST", "/api/defaults") => {
            let mut g = Global::load()?;
            if !s("php").is_empty() { g.default_php = s("php"); }
            if !s("node").is_empty() { g.default_node = s("node"); }
            g.save()?;
            daemon::reload(st).await?;
            ok()
        }
        ("POST", "/api/hosts") => {
            let n = hosts::sync(&Global::load()?)?;
            json(j!({ "ok": true, "count": n }))
        }

        // ---------- mise à jour de Viherd ----------
        ("POST", "/api/update/check") => {
            let a = update::check().await?;
            *st.update.lock().unwrap() = a.clone();
            json(j!({ "update": a }))
        }
        ("POST", "/api/update/install") => {
            let a = st.update.lock().unwrap().clone().ok_or_else(|| anyhow!("aucune mise à jour disponible"))?;
            let setup = update::download(&a).await?;
            update::launch(&setup)?;
            ok()
        }

        // ---------- sauvegardes automatiques des bases ----------
        ("POST", "/api/backups/settings") => {
            let mut g = Global::load()?;
            if let Some(b) = body.get("enabled").and_then(|v| v.as_bool()) { g.backups.enabled = b; }
            if let Some(h) = body.get("hour").and_then(|v| v.as_u64()) { g.backups.hour = h.min(23) as u8; }
            if let Some(k) = body.get("keep").and_then(|v| v.as_u64()) { g.backups.keep = k.clamp(1, 365) as usize; }
            g.save()?;
            st.global.write().unwrap().backups = g.backups.clone();
            ok()
        }
        ("POST", "/api/backups/run") => {
            let keep = st.global.read().unwrap().backups.keep;
            json(j!({ "ok": true, "count": crate::backups::run_once(st, keep).await }))
        }

        // ---------- import depuis Herd ----------
        ("GET", "/api/herd") => json(serde_json::to_value(herd::plan(&Global::load()?))?),
        ("POST", "/api/herd/import") => {
            let pick = |k: &str| body.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
            let report = herd::apply(&herd::Choice {
                parked: pick("parked"), links: pick("links"), secured: pick("secured"), php: pick("php"), env: pick("env"),
                ports: body.get("ports").and_then(|v| v.as_bool()).unwrap_or(false),
            })?;
            daemon::reload(st).await?;
            json(serde_json::to_value(report)?)
        }
        ("POST", "/api/herd/dismiss") => {
            let mut g = Global::load()?;
            g.herd_imported = true;
            g.save()?;
            st.global.write().unwrap().herd_imported = true;
            ok()
        }

        // ---------- profilage (Xdebug profiler) ----------
        ("GET", "/api/site/profiles") => json(j!(crate::profiler::list(&site_of(st, &q("site"))?.path)?)),
        ("GET", "/api/site/profile") => json(crate::profiler::summary(&site_of(st, &q("site"))?.path, &q("file"))?),
        ("POST", "/api/site/profiles/clear") => {
            crate::profiler::clear(&site_of(st, &s("site"))?.path)?;
            ok()
        }
        _ => Err(anyhow!("route inconnue: {method} {path}")),
    }
}

/// Type de projet d'un dossier, pour l'aperçu avant de parquer / lier.
fn project_kind(dir: &Path) -> &'static str {
    if dir.join("artisan").is_file() { "laravel" }
    else if dir.join("composer.json").is_file() || dir.join("index.php").is_file() || dir.join("public").join("index.php").is_file() { "php" }
    else if dir.join("package.json").is_file() { "node" }
    else if dir.join("index.html").is_file() || dir.join("public").join("index.html").is_file() { "static" }
    else { "folder" }
}

/// Ce que donnerait un dossier : son type, le nom de site, et ses sous-dossiers (pour parquer).
fn inspect_dir(st: &State, raw: &str) -> Value {
    let p = PathBuf::from(raw.trim().trim_matches('"'));
    if raw.trim().is_empty() || !p.is_dir() {
        return j!({ "exists": false });
    }
    let p = crate::config::canonical(&p).unwrap_or(p);
    let g = st.global.read().unwrap().clone();
    let sites = st.sites.read().unwrap().clone();
    let site_of = |d: &Path| sites.values().find(|s| s.path == d).map(|s| s.name.clone());
    let name = crate::config::site_name(&p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
    let mut children: Vec<Value> = std::fs::read_dir(&p).into_iter().flatten().flatten()
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            j!({ "name": crate::config::site_name(&n), "kind": project_kind(&e.path()), "site": site_of(&e.path()) })
        })
        .collect();
    children.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    j!({
        "exists": true,
        "path": p.display().to_string(),
        "name": name,
        "kind": project_kind(&p),
        // site déjà servi depuis ce dossier, et site qui porte déjà ce nom ailleurs
        "site": site_of(&p),
        "name_taken": sites.get(&name).filter(|s| s.path != p).map(|s| s.path.display().to_string()),
        "parked": g.parked.iter().any(|d| d == &p),
        "children": children,
    })
}

/// Dossiers où l'on range souvent ses projets, s'ils existent et ne sont pas déjà parqués.
fn suggest_dirs(g: &Global) -> Vec<Value> {
    let Some(home) = dirs::home_dir() else { return vec![] };
    let mut out = vec![];
    for rel in ["Herd", "Sites", "Code", "code", "Projects", "Projets", "dev", "Dev", "www", "Documents\\GitHub", "source\\repos", "Desktop"] {
        let d = home.join(rel);
        if !d.is_dir() || g.parked.iter().any(|p| p.to_string_lossy().eq_ignore_ascii_case(&d.to_string_lossy())) { continue; }
        let projects = std::fs::read_dir(&d).into_iter().flatten().flatten()
            .filter(|e| e.path().is_dir() && project_kind(&e.path()) != "folder").count();
        if projects > 0 && !out.iter().any(|v: &Value| v["path"].as_str().is_some_and(|p| p.eq_ignore_ascii_case(&d.to_string_lossy()))) {
            out.push(j!({ "path": d.display().to_string(), "label": rel.replace('\\', "/"), "projects": projects }));
        }
    }
    out
}

/// Nouveau projet dans un dossier parqué : Laravel (composer create-project) ou vide.
async fn new_project(st: &Arc<State>, dir: &str, name: &str, template: &str) -> Result<Response<Body>> {
    let g = Global::load()?;
    let dir = PathBuf::from(dir);
    if !g.parked.iter().any(|p| p == &dir) { bail!("choisis un dossier parqué"); }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("nom invalide : lettres, chiffres, - et _ uniquement");
    }
    let target = dir.join(name);
    if target.exists() { bail!("{} existe déjà", target.display()); }

    if template != "laravel" {
        std::fs::create_dir_all(target.join("public"))?;
        std::fs::write(target.join("public").join("index.php"), format!("<?php\n\necho '<h1>{name}</h1><p>PHP ' . PHP_VERSION . ' — servi par Viherd</p>';\n"))?;
        Project { php: Some(g.default_php.clone()), node: Some(g.default_node.clone()), ..Default::default() }.save(&target)?;
        daemon::reload(st).await?;
        return json(j!({ "ok": true }));
    }

    // Laravel : peut prendre une minute, on répond tout de suite et on suit dans les logs.
    let key = format!("create:{name}");
    let log = supervisor::log_path(&key);
    std::fs::create_dir_all(paths::logs())?;
    std::fs::write(&log, format!("Création de {name} avec Laravel…\n"))?;
    let (st, name) = (st.clone(), name.to_string());
    tokio::spawn(async move {
        let res: Result<()> = async {
            versions::ensure_php(&g.default_php).await?;
            if !paths::composer_dir().join("composer.phar").exists() {
                versions::install_composer().await?;
            }
            let project = Project::default();
            let env = env::project_env(&g, &dir, &project);
            let args = ["create-project", "laravel/laravel", &name, "--prefer-dist", "--no-interaction"].map(String::from);
            let f = std::fs::OpenOptions::new().append(true).open(&log)?;
            let mut cmd = tokio::process::Command::from(env::build_command("composer", &args, &dir, &env)?);
            cmd.stdout(Stdio::from(f.try_clone()?)).stderr(Stdio::from(f)).stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
            let status = cmd.status().await?;
            if !status.success() { bail!("composer a échoué ({status})"); }
            Project { php: Some(g.default_php.clone()), node: Some(g.default_node.clone()), ..Default::default() }.save(&dir.join(&name))?;
            Ok(())
        }.await;
        let msg = match &res { Ok(()) => format!("\n✔ Projet {name} prêt.\n"), Err(e) => format!("\n✖ {e:#}\n") };
        let _ = std::fs::OpenOptions::new().append(true).open(&log).and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
        match res {
            Ok(()) => tracing::info!("projet {name} créé"),
            Err(e) => tracing::error!("création de {name}: {e:#}"),
        }
        let _ = daemon::reload(&st).await;
    });
    json(j!({ "ok": true, "key": key }))
}

/// Lit le .env d'un projet (KEY=VALUE, guillemets retirés, commentaires ignorés).
fn read_dotenv(dir: &Path) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    if let Ok(text) = std::fs::read_to_string(dir.join(".env")) {
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with('#') || l.is_empty() { continue; }
            if let Some((k, v)) = l.split_once('=') {
                let v = v.trim();
                let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))).unwrap_or(v);
                m.insert(k.trim().to_string(), v.to_string());
            }
        }
    }
    m
}

/// Base de données d'un projet Laravel d'après son .env (pour la file « database » et la table failed_jobs).
fn laravel_db(env: &std::collections::HashMap<String, String>) -> std::result::Result<(db::Target, String), String> {
    let driver = env.get("DB_CONNECTION").map(String::as_str).unwrap_or("sqlite");
    let (kind, svc) = match driver {
        "pgsql" => (db::Kind::Pg, "postgres"),
        "mysql" | "mariadb" => (db::Kind::My, "mysql"),
        other => return Err(format!("Base « {other} » non prise en charge pour le suivi de la file.")),
    };
    let port = env.get("DB_PORT").and_then(|p| p.parse().ok()).unwrap_or(if kind == db::Kind::Pg { 5432 } else { 3306 });
    Ok((db::Target { kind, svc: svc.into(), port }, env.get("DB_DATABASE").cloned().unwrap_or_default()))
}

/// Jobs échoués de la table failed_jobs : nombre total et les plus récents.
async fn db_failed(t: &db::Target, dbn: &str) -> (u64, Vec<Value>) {
    let n = db::run(t, dbn, "SELECT COUNT(*) FROM failed_jobs").await.ok()
        .and_then(|r| r.rows.first().and_then(|x| x[0].clone())).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let recent = db::run(t, dbn, "SELECT uuid, queue, LEFT(exception, 400), failed_at, LEFT(payload, 600) FROM failed_jobs ORDER BY failed_at DESC LIMIT 8").await
        .map(|r| r.rows).unwrap_or_default();
    (n, recent.iter().map(|r| failed_json(r[0].as_deref(), r[1].as_deref(), r[2].as_deref(), r[3].as_deref(), r[4].as_deref())).collect())
}

fn failed_json(uuid: Option<&str>, queue: Option<&str>, error: Option<&str>, at: Option<&str>, payload: Option<&str>) -> Value {
    // nom lisible du job à partir du payload JSON (displayName)
    let job = payload.and_then(|p| p.split("\"displayName\":\"").nth(1)).and_then(|x| x.split('"').next()).map(|s| s.replace("\\\\", "\\"));
    j!({ "uuid": uuid, "queue": queue, "error": error.map(|e| e.lines().next().unwrap_or("").to_string()), "failed_at": at, "job": job })
}

/// État de la file Laravel d'un site : drivers « database » et « redis » (avec Horizon s'il est installé).
async fn queue_state(site: &crate::config::Site) -> Result<Value> {
    let env = read_dotenv(&site.path);
    let conn = env.get("QUEUE_CONNECTION").cloned().unwrap_or_else(|| "sync".into());
    if !site.path.join("artisan").exists() {
        return Ok(j!({ "connection": null, "supported": false, "reason": "Pas un projet Laravel" }));
    }
    match conn.as_str() {
        "database" => {}
        "redis" => return redis_queue_state(site, &env).await,
        "sync" => return Ok(j!({ "connection": conn, "supported": false, "reason": "Les jobs s'exécutent tout de suite (QUEUE_CONNECTION=sync) : rien n'est mis en file." })),
        _ => return Ok(j!({ "connection": conn, "supported": false, "reason": "Driver non suivi par Viherd." })),
    }
    let (t, dbn) = match laravel_db(&env) {
        Ok(x) => x,
        Err(reason) => return Ok(j!({ "connection": conn, "supported": false, "reason": reason })),
    };
    let pending = match db::run(&t, &dbn, "SELECT queue, COUNT(*), SUM(CASE WHEN reserved_at IS NULL THEN 0 ELSE 1 END) FROM jobs GROUP BY queue ORDER BY 2 DESC").await {
        Ok(r) => r,
        Err(e) => return Ok(j!({ "connection": conn, "supported": false, "reason": format!("Base injoignable : {e}") })),
    };
    let (failed, recent_failed) = db_failed(&t, &dbn).await;
    let num = |v: &Option<String>| v.as_deref().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    Ok(j!({
        "connection": conn, "supported": true, "database": dbn, "engine": t.svc,
        "queues": pending.rows.iter().map(|r| j!({ "queue": r[0], "pending": num(&r[1]), "running": num(&r[2]) })).collect::<Vec<_>>(),
        "failed": failed,
        "recent_failed": recent_failed,
    }))
}

/// File Redis : clés {prefix}queues:{nom} (liste en attente, :reserved en cours, :delayed différés).
async fn redis_queue_state(site: &crate::config::Site, env: &std::collections::HashMap<String, String>) -> Result<Value> {
    let get = |k: &str, d: &str| env.get(k).filter(|v| !v.is_empty() && v.as_str() != "null").cloned().unwrap_or_else(|| d.to_string());
    let app_name = get("APP_NAME", "laravel");
    let prefix = env.get("REDIS_PREFIX").cloned().unwrap_or_else(|| {
        let database_php = std::fs::read_to_string(site.path.join("config").join("database.php")).unwrap_or_default();
        crate::redis::laravel_default_prefix(&database_php, &app_name)
    });
    // Horizon garde le slug avec « _ » dans toutes les versions
    let app = crate::redis::laravel_slug(&app_name, '_');
    let (host, port) = (get("REDIS_HOST", "127.0.0.1"), get("REDIS_PORT", "6379").parse().unwrap_or(6379));
    let password = env.get("REDIS_PASSWORD").filter(|p| p.as_str() != "null").cloned();
    let dbi: u32 = get("REDIS_DB", "0").parse().unwrap_or(0);
    let mut c = match crate::redis::Conn::connect(&host, port, password.as_deref(), dbi).await {
        Ok(c) => c,
        Err(e) => return Ok(j!({ "connection": "redis", "supported": false, "reason": format!("{e:#} — active le service Redis dans Viherd.") })),
    };
    let qp = format!("{prefix}queues:");
    let mut names: Vec<String> = c.cmd(&["KEYS", &format!("{qp}*")]).await?.list().iter()
        .filter_map(|k| k.str()).filter_map(|k| k.strip_prefix(&qp))
        .map(|q| q.trim_end_matches(":reserved").trim_end_matches(":delayed").trim_end_matches(":notify").to_string())
        .collect();
    names.push(get("REDIS_QUEUE", "default"));
    names.sort();
    names.dedup();
    let mut queues = Vec::new();
    for q in &names {
        let base = format!("{qp}{q}");
        let pending = c.cmd(&["LLEN", &base]).await?.int();
        let running = c.cmd(&["ZCARD", &format!("{base}:reserved")]).await?.int();
        let delayed = c.cmd(&["ZCARD", &format!("{base}:delayed")]).await?.int();
        queues.push(j!({ "queue": q, "pending": pending, "running": running, "delayed": delayed }));
    }
    queues.sort_by_key(|q| -(q["pending"].as_i64().unwrap_or(0)));

    // Horizon : ses propres clés ({app}_horizon:), état du master et jobs échoués
    let horizon_installed = site.path.join("vendor").join("laravel").join("horizon").is_dir();
    let mut horizon = j!({ "installed": horizon_installed });
    let (mut failed, mut recent_failed) = (0u64, Vec::new());
    if horizon_installed {
        let hp = env.get("HORIZON_PREFIX").cloned().unwrap_or_else(|| format!("{app}_horizon:"));
        let masters = c.cmd(&["SMEMBERS", &format!("{hp}masters")]).await.map(Reply::list).unwrap_or_default();
        let paused = match masters.first().and_then(|r| r.str()) {
            Some(name) => c.cmd(&["HGET", &format!("{hp}master:{name}"), "status"]).await.ok()
                .is_some_and(|r| r.str() == Some("paused")),
            None => false,
        };
        horizon["running"] = j!(!masters.is_empty());
        horizon["paused"] = j!(paused);
        failed = c.cmd(&["ZCARD", &format!("{hp}failed_jobs")]).await.map(|r| r.int() as u64).unwrap_or(0);
        // scores négatifs : le plus récent en premier
        for id in c.cmd(&["ZRANGE", &format!("{hp}failed_jobs"), "0", "7"]).await.map(Reply::list).unwrap_or_default() {
            let Some(id) = id.str() else { continue };
            let f = c.cmd(&["HMGET", &format!("{hp}{id}"), "queue", "exception", "failed_at", "payload"]).await?.list();
            let s = |i: usize| f.get(i).and_then(|r| r.str());
            let at = s(2).and_then(|t| t.split('.').next()?.parse::<i64>().ok())
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string());
            recent_failed.push(failed_json(Some(id), s(0), s(1), at.as_deref(), s(3)));
        }
    } else if let Ok((t, dbn)) = laravel_db(env) {
        (failed, recent_failed) = db_failed(&t, &dbn).await;
    }
    Ok(j!({
        "connection": "redis", "supported": true, "engine": "redis",
        "database": format!("{host}:{port} · db {dbi}"),
        "queues": queues, "failed": failed, "recent_failed": recent_failed, "horizon": horizon,
    }))
}

fn newest_log(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

fn tail_bytes(f: &Path, max: u64) -> String {
    let Ok(mut file) = std::fs::File::open(f) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(max)));
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn tail_lines(f: &Path, n: usize) -> String {
    // ~200 octets par ligne de journal en moyenne : on lit juste ce qu'il faut
    let text = tail_bytes(f, (n as u64 * 400).max(256 * 1024));
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn installed(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).map(|rd| {
        rd.flatten()
            .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }).unwrap_or_default();
    v.sort();
    v
}

/// Adresse d'un site : https si sécurisé, avec le port s'il n'est pas standard.
pub fn site_url(g: &Global, name: &str) -> String {
    let (scheme, port, std) = if g.secured.contains(name) { ("https", g.https_port, 443) } else { ("http", g.http_port, 80) };
    if port == std { format!("{scheme}://{name}.{}", g.tld) } else { format!("{scheme}://{name}.{}:{port}", g.tld) }
}

fn state(st: &State) -> Result<Response<Body>> {
    let g = st.global.read().unwrap().clone();
    let sites = st.sites.read().unwrap().clone();
    let statuses = st.sup.statuses();
    let paused = st.paused.lock().unwrap().clone();
    let metrics = st.metrics.snapshot();
    let php_active = st.php.active_sites();
    let job = |key: &str| {
        let s = statuses.get(key).cloned().unwrap_or_default();
        let state = if paused.contains(key) && s.state != "running" { "paused".to_string() }
            else if s.state.is_empty() { "stopped".to_string() } else { s.state.clone() };
        j!({ "key": key, "state": state, "running": state == "running", "pid": s.pid, "restarts": s.restarts,
             "since": s.since, "last_exit": s.last_exit, "error": s.error })
    };
    let sites: Vec<Value> = sites.values().map(|s| {
        let share_v = j!({ "state": job(&share::key(&s.name))["state"], "url": share::url_of(st, &s.name) });
        let mut wv = Vec::new();
        for w in &s.project.workers {
            let mut v = job(&daemon::worker_key(&s.name, &w.name));
            v["name"] = j!(w.name);
            v["command"] = j!(w.command);
            v["autorestart"] = j!(w.autorestart);
            wv.push(v);
        }
        j!({
            "name": s.name,
            "php_running": php_active.contains(&s.path),
            "metrics": metrics.get(&s.name).cloned().unwrap_or_default(),
            "url": site_url(&g, &s.name),
            "secure": g.secured.contains(&s.name),
            "share": share_v,
            "xdebug": s.project.xdebug,
            "profile": s.project.profile,
            "php_ini": s.project.php_ini,
            "path": s.path.display().to_string(),
            "docroot": s.docroot().display().to_string(),
            "root": s.project.root,
            "routes": s.project.routes.iter().map(|r| j!({ "path": r.path, "port": r.port })).collect::<Vec<_>>(),
            "env": s.project.env,
            "php": s.php(&g),
            "php_pinned": s.project.php.is_some(),
            "node": s.project.node.clone().unwrap_or_else(|| g.default_node.clone()),
            "node_pinned": s.project.node.is_some(),
            "proxy": s.project.proxy,
            "laravel": s.path.join("artisan").exists(),
            "workers": wv,
            "linked": g.links.contains_key(&s.name),
        })
    }).collect();
    let services: Vec<Value> = g.services.iter().map(|(n, svc)| {
        let mut v = job(&format!("service:{n}"));
        v["name"] = j!(n);
        v["enabled"] = j!(svc.enabled);
        v["port"] = j!(svc.port);
        v["installed"] = j!(paths::service_dir(n).exists());
        if n == "minio" {
            v["license"] = j!(crate::config::minio_license().exists());
        }
        v
    }).collect();
    json(j!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime": st.started.elapsed().as_secs(),
        "api_port": g.api_port,
        "php_processes": g.php_processes,
        "logs_dir": paths::logs().display().to_string(),
        "tld": g.tld,
        "http_port": g.http_port,
        "https_port": g.https_port,
        "ca_trusted": certs::trusted_cached(),
        "ca_path": certs::ca_cert_path().display().to_string(),
        "smtp_port": g.smtp_port,
        "dump_port": g.dump_port,
        "dumps_enabled": g.dumps,
        "dumps": st.dumps.count(),
        "default_php": g.default_php,
        "default_node": g.default_node,
        "parked": g.parked.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "sites": sites,
        "services": services,
        "installed": {
            "php": installed(&paths::home().join("bin").join("php")),
            "node": installed(&paths::home().join("bin").join("node")),
            "composer": paths::composer_dir().join("composer.phar").exists(),
        },
        "mails": st.mail.list().len(),
        "update": st.update.lock().unwrap().clone(),
        "backups": g.backups,
        // proposer l'import tant qu'il n'a pas été fait ou écarté
        "herd": !g.herd_imported && herd::config_dir().is_dir(),
    }))
}
