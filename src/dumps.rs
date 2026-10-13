//! Débogage façon Herd : capture des dump()/dd().
//! Symfony VarDumper (utilisé par Laravel) sait envoyer ses dumps à un serveur TCP quand
//! VAR_DUMPER_FORMAT=tcp://127.0.0.1:9912. Chaque dump arrive sous la forme d'une ligne
//! base64(serialize([Data, contexte])). On la fait rendre en HTML dépliable par PHP lui-même,
//! avec le vendor/ du projet (render_dump.php), puis on la garde pour l'interface.
use crate::config::Global;
use crate::paths;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const MAX_DUMPS: usize = 300;

#[derive(Debug, Clone, Serialize)]
pub struct Dump {
    pub id: u64,
    pub received: u64,
    /// Dossier racine du projet (celui qui contient vendor/)
    pub root: Option<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
    /// Nom de la fonction appelante quand VarDumper le fournit (dump, dd…)
    pub name: Option<String>,
    /// « GET /users » pour une requête web, ou la ligne de commande pour un worker / artisan
    pub origin: Option<String>,
    pub html: Option<String>,
    pub text: Option<String>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct DumpStore {
    dumps: Mutex<Vec<Dump>>,
    next: Mutex<u64>,
}

impl DumpStore {
    pub fn list(&self) -> Vec<Dump> {
        let mut v = self.dumps.lock().unwrap().clone();
        v.reverse();
        v
    }
    pub fn count(&self) -> usize {
        self.dumps.lock().unwrap().len()
    }
    pub fn clear(&self) {
        self.dumps.lock().unwrap().clear();
    }
    fn push(&self, mut d: Dump) {
        d.id = {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            *n
        };
        let mut v = self.dumps.lock().unwrap();
        v.push(d);
        if v.len() > MAX_DUMPS {
            v.remove(0);
        }
    }
}

fn script() -> Result<std::path::PathBuf> {
    let p = paths::home().join("debug").join("render_dump.php");
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, include_str!("render_dump.php"))?;
    Ok(p)
}

/// PHP utilisé pour le rendu : la version par défaut, sinon n'importe quelle version installée.
fn php_exe(g: &Global) -> Option<std::path::PathBuf> {
    let def = paths::php_dir(&g.default_php).join("php.exe");
    if def.exists() {
        return Some(def);
    }
    std::fs::read_dir(paths::home().join("bin").join("php")).ok()?.flatten()
        .map(|e| e.path().join("php.exe")).find(|p| p.exists())
}

pub async fn serve(port: u16, store: Arc<DumpStore>) -> Result<()> {
    let script = script()?;
    let l = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| anyhow!("dumps: port {port} indisponible: {e}"))?;
    tracing::info!("Serveur de dumps sur 127.0.0.1:{port}");
    loop {
        let (s, _) = l.accept().await?;
        let (store, script) = (store.clone(), script.clone());
        tokio::spawn(async move {
            if let Err(e) = session(s, &store, &script).await {
                tracing::debug!("dumps: {e}");
            }
        });
    }
}

async fn session(s: TcpStream, store: &DumpStore, script: &std::path::Path) -> Result<()> {
    let mut lines = BufReader::new(s).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let d = render(&line, script).await.unwrap_or_else(|e| Dump {
            id: 0, received: now(), root: None, file: None, line: None, name: None, origin: None,
            html: None, text: None, error: Some(e.to_string()),
        });
        tracing::info!("◆ dump reçu{}", d.file.as_deref().map(|f| format!(" : {f}:{}", d.line.unwrap_or(0))).unwrap_or_default());
        store.push(d);
    }
    Ok(())
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn render(line: &str, script: &std::path::Path) -> Result<Dump> {
    let g = Global::load().unwrap_or_default();
    let php = php_exe(&g).ok_or_else(|| anyhow!("aucun PHP installé pour afficher le dump"))?;
    let mut child = tokio::process::Command::new(php)
        .arg(script)
        .env_remove("VAR_DUMPER_FORMAT")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    child.stdin.take().unwrap().write_all(line.as_bytes()).await?;
    let out = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output()).await??;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| anyhow!("rendu PHP illisible"))?;
    let ctx = &v["context"];
    let src = &ctx["source"];
    let origin = if let Some(uri) = ctx["request"]["uri"].as_str() {
        Some(format!("{} {}", ctx["request"]["method"].as_str().unwrap_or("GET"), uri))
    } else {
        ctx["cli"]["command_line"].as_str().map(String::from)
    };
    Ok(Dump {
        id: 0,
        received: ctx["timestamp"].as_f64().map(|t| t as u64).unwrap_or_else(now),
        root: v["root"].as_str().map(String::from),
        file: src["file"].as_str().map(String::from),
        line: src["line"].as_u64(),
        name: src["name"].as_str().map(String::from),
        origin,
        html: v["html"].as_str().map(String::from),
        text: v["text"].as_str().map(String::from),
        error: v["error"].as_str().map(String::from)
            .or_else(|| v["html"].is_null().then(|| "vendor/autoload.php introuvable : impossible d'afficher le contenu".into())),
    })
}
