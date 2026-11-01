//! Partage d'un site sur Internet (équivalent de `herd share`) via un tunnel Cloudflare « rapide » :
//! pas de compte, une adresse https://xxx.trycloudflare.com valable tant que le tunnel tourne.
use crate::config::Global;
use crate::download::{client, fetch};
use crate::paths;
use crate::server::State;
use crate::supervisor::{self, Job};
use anyhow::{anyhow, bail, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const URL: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-amd64.exe";

pub fn exe() -> PathBuf { paths::home().join("bin").join("cloudflared").join("cloudflared.exe") }
pub fn key(site: &str) -> String { format!("share:{site}") }

pub async fn ensure() -> Result<PathBuf> {
    let p = exe();
    if !p.exists() {
        println!("↓ {URL}");
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, fetch(URL).await?)?;
    }
    Ok(p)
}

/// Adresse publique annoncée par cloudflared dans son journal.
pub fn find_url(log: &str) -> Option<String> {
    log.split(|c: char| c.is_whitespace() || c == '|').find(|w| w.starts_with("https://") && w.ends_with(".trycloudflare.com") && !w.starts_with("https://api."))
        .map(str::to_string)
}

pub async fn start(st: &Arc<State>, site: &str) -> Result<()> {
    if !st.sites.read().unwrap().contains_key(site) { bail!("site inconnu: {site}"); }
    let bin = ensure().await?;
    let g = st.global.read().unwrap().clone();
    let k = key(site);
    let log = supervisor::log_path(&k);
    let _ = std::fs::remove_file(&log); // l'adresse change à chaque tunnel : on repart d'un journal vide
    st.sup.start(Job {
        key: k.clone(),
        program: bin.display().to_string(),
        args: ["tunnel", "--no-autoupdate", "--url", &format!("http://127.0.0.1:{}", g.http_port)].map(String::from).to_vec(),
        cwd: paths::home(),
        env: vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())],
        autorestart: false,
        graceful_stop: None,
    });
    // cloudflared met quelques secondes à obtenir son adresse : on la récupère en tâche de fond
    let (st, site) = (st.clone(), site.to_string());
    tokio::spawn(async move {
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if !st.sup.list().contains(&k) { return; }
            if let Some(url) = std::fs::read_to_string(&log).ok().as_deref().and_then(find_url) {
                let host = url.trim_start_matches("https://").to_string();
                let mut m = st.shares.lock().unwrap();
                m.retain(|_, s| s != &site);
                m.insert(host, site.clone());
                tracing::info!("{site} partagé sur {url}");
                return;
            }
        }
        tracing::warn!("{site} : cloudflared n'a pas annoncé d'adresse (voir {})", log.display());
    });
    Ok(())
}

pub fn stop(st: &State, site: &str) {
    st.sup.stop(&key(site));
    st.shares.lock().unwrap().retain(|_, s| s != site);
}

/// Adresse publique d'un site partagé, si le tunnel est prêt.
pub fn url_of(st: &State, site: &str) -> Option<String> {
    st.shares.lock().unwrap().iter().find(|(_, s)| *s == site).map(|(h, _)| format!("https://{h}"))
}

/// `viherd share` : démarre le partage dans le démon, affiche l'adresse, l'arrête au Ctrl+C.
pub async fn run_foreground(g: &Global, site: &str) -> Result<i32> {
    let api = format!("http://127.0.0.1:{}", g.api_port);
    let post = |on: bool| client().post(format!("{api}/api/site/share")).header("X-Viherd", "1")
        .json(&serde_json::json!({ "site": site, "on": on })).send();
    let r = post(true).await.map_err(|_| anyhow!("le démon ne tourne pas (`viherd start` ou l'appli)"))?;
    if !r.status().is_success() { bail!("{}", r.text().await.unwrap_or_default()); }
    println!("Ouverture du tunnel pour {site}.{}…", g.tld);
    let mut shown = false;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if shown { continue; }
                let st: serde_json::Value = client().get(format!("{api}/api/state")).send().await?.json().await?;
                let url = st["sites"].as_array().and_then(|a| a.iter().find(|s| s["name"] == site))
                    .and_then(|s| s["share"]["url"].as_str()).map(str::to_string);
                if let Some(u) = url {
                    println!("✔ {u}\n  (Ctrl+C pour arrêter le partage)");
                    shown = true;
                }
            }
        }
    }
    let _ = post(false).await;
    println!("\nPartage arrêté.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_tunnel_url_in_log() {
        let log = "2026-10-21T10:00:00Z INF |  https://calm-river-1234.trycloudflare.com  |\n";
        assert_eq!(super::find_url(log).as_deref(), Some("https://calm-river-1234.trycloudflare.com"));
        assert_eq!(super::find_url("INF Starting tunnel"), None);
    }
}
