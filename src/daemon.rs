//! Le démon : un seul processus Rust qui orchestre tout.
use crate::config::{self, Global, Site, Worker};
use crate::dumps::{self, DumpStore};
use crate::mail::{self, MailBox};
use crate::php_pool::PhpPool;
use crate::server::{self, State};
use crate::supervisor::{Job, Supervisor};
use crate::{env, services, versions};
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};

/// Mode CLI : tourne jusqu'à Ctrl+C.
pub async fn run() -> Result<()> {
    run_until(async {
        let _ = tokio::signal::ctrl_c().await;
        println!("\nArrêt...");
    })
    .await
}

/// Tourne jusqu'à ce que `shutdown` se termine (utilisé par l'appli de bureau).
pub async fn run_until(shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    let g = Global::load()?;
    let st = Arc::new(State {
        global: RwLock::new(g.clone()),
        sites: RwLock::new(BTreeMap::new()),
        php: PhpPool::new(),
        sup: Supervisor::default(),
        mail: Arc::new(MailBox::default()),
        started: std::time::Instant::now(),
        metrics: Default::default(),
        dumps: Arc::new(DumpStore::default()),
        paused: Mutex::new(HashSet::new()),
        quit: tokio::sync::Notify::new(),
        shares: Mutex::new(Default::default()),
    });
    let smtp_task = tokio::spawn(mail::serve(g.smtp_port, st.mail.clone()));
    let dump_task = tokio::spawn({
        let store = st.dumps.clone();
        let port = g.dump_port;
        async move {
            if let Err(e) = dumps::serve(port, store).await {
                tracing::error!("{e:#}");
            }
        }
    });
    let api = SocketAddr::from(([127, 0, 0, 1], g.api_port));
    let http = SocketAddr::from(([127, 0, 0, 1], g.http_port));
    let api_task = tokio::spawn(server::serve(api, st.clone(), true));
    // HTTPS : le port 443 peut être pris (IIS, Herd…) sans empêcher le reste de tourner
    let https = SocketAddr::from(([127, 0, 0, 1], g.https_port));
    let https_task = tokio::spawn({
        let st = st.clone();
        async move {
            if let Err(e) = server::serve_tls(https, st).await {
                tracing::error!("HTTPS indisponible : {e:#}");
            }
        }
    });

    // Services et découverte des sites en tâche de fond : les téléchargements (PHP, Node, MariaDB…)
    // ne doivent pas retarder l'ouverture des ports ni de l'interface.
    let bg = st.clone();
    tokio::spawn(async move {
        services::start_all(&Global::load().unwrap_or_default(), &bg.sup).await;
        if let Err(e) = reload(&bg).await {
            tracing::error!("chargement des sites: {e:#}");
        }
    });
    let result = tokio::select! {
        r = server::serve(http, st.clone(), false) => r,
        _ = shutdown => Ok(()),
        _ = st.quit.notified() => { tracing::info!("arrêt demandé via l'API"); Ok(()) }
    };
    api_task.abort();
    https_task.abort();
    smtp_task.abort();
    dump_task.abort();
    st.sup.stop_all();
    st.php.stop_all().await;
    // laisse aux bases de données le temps de s'arrêter proprement
    st.sup.wait_idle(std::time::Duration::from_secs(20)).await;
    result
}

pub fn worker_key(site: &str, worker: &str) -> String {
    format!("site:{site}:{worker}")
}

fn worker_job(g: &Global, s: &Site, w: &Worker) -> Option<Job> {
    let mut parts = env::split_command(&w.command).into_iter();
    let program = parts.next()?;
    Some(Job {
        key: worker_key(&s.name, &w.name),
        program,
        args: parts.collect(),
        cwd: s.path.clone(),
        env: env::project_env(g, &s.path, &s.project),
        autorestart: w.autorestart,
        graceful_stop: None,
    })
}

/// Démarre (ou redémarre) un worker précis à partir de la config du site.
pub fn start_worker(st: &State, key: &str) -> Result<()> {
    let g = st.global.read().unwrap().clone();
    let sites = st.sites.read().unwrap().clone();
    let (s, w) = sites.values()
        .find_map(|s| s.project.workers.iter().find(|w| worker_key(&s.name, &w.name) == key).map(|w| (s, w)))
        .ok_or_else(|| anyhow!("worker inconnu: {key}"))?;
    st.paused.lock().unwrap().remove(key);
    st.sup.start(worker_job(&g, s, w).ok_or_else(|| anyhow!("commande vide"))?);
    Ok(())
}

pub fn stop_worker(st: &State, key: &str) {
    st.paused.lock().unwrap().insert(key.to_string());
    st.sup.stop(key);
}

/// Relit la config, redécouvre les sites, relance les workers des projets.
pub async fn reload(st: &State) -> Result<()> {
    let g = Global::load()?;
    let sites = config::discover_sites(&g);

    let old: Vec<String> = st.sup.list().into_iter().filter(|k| k.starts_with("site:")).collect();
    for k in &old {
        st.sup.stop(k);
    }
    st.php.stop_all().await;

    let paused = st.paused.lock().unwrap().clone();
    let mut wanted = HashSet::new();
    for s in sites.values() {
        if s.project.workers.is_empty() {
            continue;
        }
        if let Err(e) = versions::ensure_php(&s.php(&g)).await {
            tracing::error!("{}: {e:#}", s.name);
        }
        if s.project.xdebug {
            if let Err(e) = versions::ensure_xdebug(&s.php(&g)).await {
                tracing::warn!("{}: Xdebug : {e:#}", s.name);
            }
        }
        if let Some(n) = &s.project.node {
            if let Err(e) = versions::ensure_node(n).await {
                tracing::error!("{}: {e:#}", s.name);
            }
        }
        for w in &s.project.workers {
            let key = worker_key(&s.name, &w.name);
            wanted.insert(key.clone());
            if paused.contains(&key) {
                continue;
            }
            if let Some(job) = worker_job(&g, s, w) {
                st.sup.start(job);
            }
        }
    }
    // Workers supprimés d'un viherd.toml : on oublie leur statut.
    for k in old {
        if !wanted.contains(&k) {
            st.sup.forget(&k);
        }
    }
    st.paused.lock().unwrap().retain(|k| wanted.contains(k));

    *st.global.write().unwrap() = g;
    *st.sites.write().unwrap() = sites;
    Ok(())
}
