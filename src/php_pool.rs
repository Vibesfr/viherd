//! Un pool de php-cgi isolé PAR PROJET (sa version de PHP, son env, son dossier).
//! Sous Windows php-cgi ne traite qu'une requête à la fois : on en lance N par projet
//! et on distribue les requêtes sur les processus libres. Lancé à la demande, relancé s'il meurt.
use crate::config::{Global, Site};
use crate::fastcgi::{self, FcgiResponse};
use crate::{env, versions};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};

struct SitePool {
    _children: Vec<Child>, // kill_on_drop : supprimer le pool tue les processus
    free_tx: mpsc::UnboundedSender<u16>,
    free_rx: Mutex<mpsc::UnboundedReceiver<u16>>,
}

pub struct PhpPool {
    sites: Mutex<HashMap<PathBuf, Arc<SitePool>>>,
    next_port: AtomicU16,
}

impl PhpPool {
    pub fn new() -> Self {
        Self { sites: Mutex::new(HashMap::new()), next_port: AtomicU16::new(9100) }
    }

    pub async fn request(&self, site: &Site, g: &Global, params: &[(String, String)], body: &[u8]) -> Result<FcgiResponse> {
        match self.try_request(site, g, params, body).await {
            Ok(r) => Ok(r),
            Err(e) => {
                // Un php-cgi est probablement mort : on recrée le pool et on réessaie une fois
                tracing::warn!("[{}] php-cgi en erreur ({e}), redémarrage", site.name);
                self.sites.lock().await.remove(&site.path);
                self.try_request(site, g, params, body).await
            }
        }
    }

    async fn try_request(&self, site: &Site, g: &Global, params: &[(String, String)], body: &[u8]) -> Result<FcgiResponse> {
        let pool = self.pool_for(site, g).await?;
        let port = pool.free_rx.lock().await.recv().await.expect("pool fermé");
        let r = fastcgi::request(port, params, body).await;
        let _ = pool.free_tx.send(port);
        r
    }

    async fn pool_for(&self, site: &Site, g: &Global) -> Result<Arc<SitePool>> {
        let mut m = self.sites.lock().await;
        if let Some(p) = m.get(&site.path) {
            return Ok(p.clone());
        }
        let dir = versions::ensure_php(&site.php(g)).await?;
        let (free_tx, free_rx) = mpsc::unbounded_channel();
        let mut children = Vec::new();
        for _ in 0..g.php_processes.max(1) {
            let port = free_port(&self.next_port)?;
            let mut cmd = Command::new(dir.join("php-cgi.exe"));
            cmd.arg("-b").arg(format!("127.0.0.1:{port}"))
                .current_dir(&site.path)
                .envs(env::project_env(g, &site.path, &site.project))
                .env("PHP_FCGI_MAX_REQUESTS", "0")
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                .kill_on_drop(true);
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            let mut child = cmd.spawn()?;
            wait_ready(port, &mut child, &site.name).await?;
            children.push(child);
            let _ = free_tx.send(port);
        }
        tracing::info!("{} php-cgi {} démarrés pour {}", children.len(), site.php(g), site.name);
        let pool = Arc::new(SitePool { _children: children, free_tx, free_rx: Mutex::new(free_rx) });
        m.insert(site.path.clone(), pool.clone());
        Ok(pool)
    }

    pub async fn stop_all(&self) {
        self.sites.lock().await.clear();
    }

    /// Dossiers des sites dont le pool php-cgi est démarré (pour l'interface).
    pub fn active_sites(&self) -> Vec<PathBuf> {
        match self.sites.try_lock() {
            Ok(m) => m.keys().cloned().collect(),
            Err(_) => vec![],
        }
    }
}

/// Un port réellement libre : on part du compteur et on vérifie qu'on peut l'écouter.
/// (Un ancien php-cgi resté en vie sur un port nous ferait servir le mauvais projet.)
fn free_port(next: &AtomicU16) -> Result<u16> {
    for _ in 0..2000 {
        let mut p = next.fetch_add(1, Ordering::SeqCst);
        if p < 9100 || p > 19000 {
            next.store(9101, Ordering::SeqCst);
            p = 9100;
        }
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return Ok(p);
        }
    }
    bail!("aucun port libre pour php-cgi")
}

async fn wait_ready(port: u16, child: &mut Child, name: &str) -> Result<()> {
    for _ in 0..50 {
        // si notre php-cgi est mort (port pris entre-temps…), ce n'est pas lui qui répondrait
        if let Some(st) = child.try_wait()? {
            bail!("php-cgi s'est arrêté au démarrage pour {name} ({st})");
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("php-cgi n'a pas démarré pour {name}")
}
