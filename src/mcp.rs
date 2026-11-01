//! Serveur MCP (Model Context Protocol) sur stdio : `viherd mcp`.
//! Un assistant IA (Claude Code, Cursor…) y lit l'état des sites, les logs, les dumps, les files et les mails,
//! lance artisan dans l'environnement isolé du projet et interroge les bases — via l'API locale du démon.
use crate::config::{Global, Project};
use crate::download::client;
use crate::env;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL: &str = "2025-06-18";
/// Au-delà, la sortie est tronquée (le début est coupé : la fin d'un log est la plus utile)
const MAX_TEXT: usize = 40_000;

pub async fn serve() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() { continue; }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            write(&mut out, json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "JSON invalide" } })).await?;
            continue;
        };
        // notification (pas d'id) : rien à répondre
        let Some(id) = msg.get("id").cloned() else { continue };
        let reply = match handle(msg["method"].as_str().unwrap_or(""), &msg["params"]).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("{e:#}") } }),
        };
        write(&mut out, reply).await?;
    }
    Ok(())
}

async fn write(out: &mut tokio::io::Stdout, v: Value) -> Result<()> {
    out.write_all(format!("{v}\n").as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

async fn handle(method: &str, params: &Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "viherd", "version": crate::VERSION },
            "instructions": "Viherd sert les projets PHP/Laravel et Node de cette machine Windows en <site>.test. \
                Commence par `sites` pour connaître les noms de site, leurs versions de PHP/Node et l'état des workers.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = &params["arguments"];
            Ok(match call(name, args).await {
                Ok(text) => json!({ "content": [{ "type": "text", "text": clip(text) }] }),
                Err(e) => json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true }),
            })
        }
        m => bail!("méthode inconnue : {m}"),
    }
}

fn tools() -> Value {
    let site = json!({ "type": "string", "description": "Nom du site (sans .test), voir `sites`" });
    json!([
        { "name": "sites", "description": "Sites servis par Viherd : adresse, dossier, versions de PHP et Node, workers (état, redémarrages), dernière erreur HTTP.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "laravel_log", "description": "Fin du fichier de log Laravel le plus récent du site (storage/logs).",
          "inputSchema": { "type": "object", "properties": { "site": site, "lines": { "type": "integer", "description": "Nombre de lignes (200 par défaut)" } }, "required": ["site"] } },
        { "name": "logs", "description": "Sortie d'un worker (clé site:<site>:<worker>, voir `sites`), d'un service (service:mysql…) ou du démon Viherd (daemon).",
          "inputSchema": { "type": "object", "properties": { "key": { "type": "string" }, "lines": { "type": "integer" } }, "required": ["key"] } },
        { "name": "dumps", "description": "Derniers dump() / dd() capturés (fichier, ligne, valeur).",
          "inputSchema": { "type": "object", "properties": { "limit": { "type": "integer", "description": "20 par défaut" } } } },
        { "name": "queue", "description": "File Laravel du site : jobs en attente, en cours, différés, échoués (et état d'Horizon).",
          "inputSchema": { "type": "object", "properties": { "site": site }, "required": ["site"] } },
        { "name": "mails", "description": "Mails envoyés par les sites et capturés par Viherd (SMTP local).",
          "inputSchema": { "type": "object", "properties": { "limit": { "type": "integer" } } } },
        { "name": "artisan", "description": "Lance `php artisan …` dans le projet, avec sa version de PHP. Exemple d'arguments : [\"route:list\", \"--json\"].",
          "inputSchema": { "type": "object", "properties": { "site": site, "args": { "type": "array", "items": { "type": "string" } } }, "required": ["site", "args"] } },
        { "name": "db_query", "description": "Exécute une requête SQL sur une base MySQL/MariaDB ou PostgreSQL gérée par Viherd.",
          "inputSchema": { "type": "object", "properties": {
              "service": { "type": "string", "enum": ["mysql", "postgres"] }, "database": { "type": "string" }, "sql": { "type": "string" } },
            "required": ["service", "database", "sql"] } },
        { "name": "restart_worker", "description": "Redémarre un worker (clé site:<site>:<worker>), par exemple après avoir modifié un job.",
          "inputSchema": { "type": "object", "properties": { "key": { "type": "string" } }, "required": ["key"] } },
    ])
}

fn api() -> Result<String> {
    Ok(format!("http://127.0.0.1:{}", Global::load()?.api_port))
}

async fn get(path: &str, query: &[(&str, String)]) -> Result<Value> {
    let r = client().get(format!("{}{path}", api()?)).query(query).send().await
        .map_err(|_| anyhow!("Viherd ne tourne pas : lance l'appli ou `viherd start`"))?;
    answer(r).await
}

async fn post(path: &str, body: Value) -> Result<Value> {
    let r = client().post(format!("{}{path}", api()?)).header("X-Viherd", "1").json(&body).send().await
        .map_err(|_| anyhow!("Viherd ne tourne pas : lance l'appli ou `viherd start`"))?;
    answer(r).await
}

async fn answer(r: reqwest::Response) -> Result<Value> {
    let ok = r.status().is_success();
    let v: Value = r.json().await?;
    if !ok { bail!("{}", v["error"].as_str().unwrap_or("erreur de l'API Viherd")); }
    Ok(v)
}

async fn call(name: &str, a: &Value) -> Result<String> {
    let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
    let n = |k: &str, d: u64| a[k].as_u64().unwrap_or(d);
    match name {
        "sites" => {
            let st = get("/api/state", &[]).await?;
            let sites: Vec<Value> = st["sites"].as_array().into_iter().flatten().map(|x| json!({
                "name": x["name"], "url": x["url"], "path": x["path"], "php": x["php"], "node": x["node"],
                "laravel": x["laravel"], "https": x["secure"], "xdebug": x["xdebug"],
                "workers": x["workers"].as_array().into_iter().flatten().map(|w| json!({
                    "key": w["key"], "command": w["command"], "state": w["state"], "restarts": w["restarts"], "error": w["error"] })).collect::<Vec<_>>(),
                "last_error": (x["metrics"]["last_err_status"].as_u64().unwrap_or(0) > 0).then(|| json!({
                    "status": x["metrics"]["last_err_status"], "request": x["metrics"]["last_err_path"] })),
            })).collect();
            Ok(serde_json::to_string_pretty(&json!({ "tld": st["tld"], "sites": sites,
                "services": st["services"].as_array().into_iter().flatten().filter(|x| x["enabled"] == true)
                    .map(|x| json!({ "name": x["name"], "port": x["port"], "state": x["state"] })).collect::<Vec<_>>() }))?)
        }
        "laravel_log" => {
            let v = get("/api/site/laravel-log", &[("site", s("site"))]).await?;
            let text = v["text"].as_str().unwrap_or("");
            if text.is_empty() { return Ok("Aucun log Laravel.".into()); }
            Ok(last_lines(text, n("lines", 200) as usize))
        }
        "logs" => {
            let v = get("/api/logs", &[("key", s("key")), ("n", n("lines", 200).to_string())]).await?;
            Ok(v["log"].as_str().unwrap_or("").to_string())
        }
        "dumps" => {
            let v = get("/api/dumps", &[]).await?;
            let list: Vec<Value> = v.as_array().into_iter().flatten().take(n("limit", 20) as usize).cloned().collect();
            Ok(serde_json::to_string_pretty(&list)?)
        }
        "queue" => Ok(serde_json::to_string_pretty(&get("/api/site/queue", &[("site", s("site"))]).await?)?),
        "mails" => {
            let v = get("/mail.json", &[]).await?;
            let list: Vec<Value> = v.as_array().into_iter().flatten().take(n("limit", 20) as usize)
                .map(|m| json!({ "id": m["id"], "subject": m["subject"], "from": m["from"], "to": m["to"] })).collect();
            Ok(serde_json::to_string_pretty(&list)?)
        }
        "artisan" => artisan(&s("site"), a["args"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()).await,
        "db_query" => {
            let v = post("/api/db/query", json!({ "svc": s("service"), "db": s("database"), "sql": s("sql") })).await?;
            Ok(serde_json::to_string_pretty(&v)?)
        }
        "restart_worker" => {
            post("/api/worker", json!({ "action": "restart", "key": s("key") })).await?;
            Ok(format!("{} redémarré", s("key")))
        }
        t => bail!("outil inconnu : {t}"),
    }
}

async fn artisan(site: &str, args: Vec<String>) -> Result<String> {
    let st = get("/api/state", &[]).await?;
    let path = st["sites"].as_array().into_iter().flatten().find(|x| x["name"] == site)
        .and_then(|x| x["path"].as_str()).map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow!("site inconnu : {site}"))?;
    if !path.join("artisan").exists() { bail!("{site} n'est pas un projet Laravel"); }
    let g = Global::load()?;
    let project = Project::load(&path)?;
    crate::versions::ensure_php(project.php.as_deref().unwrap_or(&g.default_php)).await?;
    let mut full = vec!["artisan".to_string()];
    full.extend(args);
    full.extend(["--no-interaction".into(), "--no-ansi".into()]);
    let mut cmd = tokio::process::Command::from(env::build_command("php", &full, &path, &env::project_env(&g, &path, &project))?);
    cmd.stdin(std::process::Stdio::null()).creation_flags(0x0800_0000);
    let o = tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output()).await
        .map_err(|_| anyhow!("artisan n'a pas terminé en 2 minutes"))??;
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    if !o.status.success() { bail!("code {} :\n{}", o.status.code().unwrap_or(-1), text.trim()); }
    Ok(text.trim().to_string())
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn clip(text: String) -> String {
    if text.len() <= MAX_TEXT { return text; }
    let mut cut = text.len() - MAX_TEXT;
    while !text.is_char_boundary(cut) { cut += 1; }
    format!("[… {} caractères coupés]\n{}", cut, &text[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_the_mcp_handshake() {
        let init = handle("initialize", &json!({ "protocolVersion": "2025-03-26" })).await.unwrap();
        assert_eq!(init["protocolVersion"], "2025-03-26");
        assert_eq!(init["serverInfo"]["name"], "viherd");
        let tools = handle("tools/list", &Value::Null).await.unwrap();
        let names: Vec<&str> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"artisan") && names.contains(&"sites"));
        assert!(handle("nope", &Value::Null).await.is_err());
        let r = handle("tools/call", &json!({ "name": "inconnu", "arguments": {} })).await.unwrap();
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn clips_from_the_start() {
        let s = "é".repeat(MAX_TEXT);
        let c = clip(s);
        assert!(c.starts_with("[…") && c.ends_with('é'));
    }
}
