//! Superviseur de processus : workers de queue, scheduler, Reverb (websockets), Vite, services (MySQL, Redis…).
//! Redémarrage automatique avec temporisation en cas de crash en boucle, statut détaillé pour l'interface,
//! logs dans ~/.viherd/logs, arrêt de tout l'arbre de processus.
use crate::{env, paths};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::watch;

pub struct Job {
    pub key: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub autorestart: bool,
    /// Commande d'arrêt propre (ex : pg_ctl stop) exécutée avant de tuer le processus.
    pub graceful_stop: Option<(PathBuf, Vec<String>)>,
}

/// État d'un job tel qu'affiché dans l'interface.
#[derive(Clone, Debug, Default, Serialize)]
pub struct JobStatus {
    /// starting | running | restarting | crashloop | failed | exited | stopped
    pub state: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    /// Horodatage (s) du dernier démarrage
    pub since: u64,
    pub last_exit: Option<i32>,
    pub error: Option<String>,
    #[serde(skip)]
    gen: u64,
}

type Status = Arc<Mutex<HashMap<String, JobStatus>>>;

#[derive(Default)]
pub struct Supervisor {
    jobs: Mutex<HashMap<String, watch::Sender<bool>>>,
    status: Status,
    gen: AtomicU64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Met à jour le statut seulement si c'est toujours la même « génération » du job
/// (un ancien job qui s'arrête ne doit pas écraser le statut du nouveau).
fn set(st: &Status, key: &str, gen: u64, f: impl FnOnce(&mut JobStatus)) {
    let mut m = st.lock().unwrap();
    if let Some(s) = m.get_mut(key) {
        if s.gen == gen {
            f(s);
        }
    }
}

impl Supervisor {
    pub fn start(&self, job: Job) {
        self.stop(&job.key);
        let gen = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = watch::channel(false);
        self.jobs.lock().unwrap().insert(job.key.clone(), tx);
        self.status.lock().unwrap().insert(job.key.clone(), JobStatus { state: "starting".into(), gen, ..Default::default() });
        tokio::spawn(run(job, rx, self.status.clone(), gen));
    }
    pub fn stop(&self, key: &str) {
        if let Some(tx) = self.jobs.lock().unwrap().remove(key) {
            let _ = tx.send(true);
        }
    }
    pub fn stop_all(&self) {
        for (_, tx) in self.jobs.lock().unwrap().drain() {
            let _ = tx.send(true);
        }
    }
    /// Oublie le statut d'un job qui n'existe plus (worker supprimé d'un viherd.toml).
    pub fn forget(&self, key: &str) {
        if !self.jobs.lock().unwrap().contains_key(key) {
            self.status.lock().unwrap().remove(key);
        }
    }
    pub fn list(&self) -> Vec<String> {
        let mut v: Vec<String> = self.jobs.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
    pub fn statuses(&self) -> HashMap<String, JobStatus> {
        self.status.lock().unwrap().clone()
    }
    /// Attend que plus aucun job ne tourne (arrêts propres en cours), au plus `max`.
    pub async fn wait_idle(&self, max: Duration) {
        let t0 = Instant::now();
        while t0.elapsed() < max {
            let busy = self.status.lock().unwrap().values()
                .any(|s| matches!(s.state.as_str(), "running" | "starting" | "stopping" | "restarting"));
            if !busy {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

pub fn log_path(key: &str) -> PathBuf {
    paths::logs().join(format!("{}.log", key.replace([':', '/', '\\'], "_")))
}

async fn run(job: Job, mut stop: watch::Receiver<bool>, status: Status, gen: u64) {
    let log = log_path(&job.key);
    let mut quick_fails = 0u32;
    loop {
        let t0 = Instant::now();
        match spawn_once(&job, &log) {
            Err(e) => {
                tracing::warn!("[{}] démarrage impossible: {e}", job.key);
                set(&status, &job.key, gen, |s| { s.state = "failed".into(); s.error = Some(e.to_string()); s.pid = None; });
            }
            Ok(mut child) => {
                let pid = child.id();
                tracing::info!("[{}] démarré (pid {:?})", job.key, pid);
                set(&status, &job.key, gen, |s| { s.state = "running".into(); s.pid = pid; s.since = now(); s.error = None; });
                let stopped = tokio::select! {
                    st = child.wait() => {
                        let code = st.ok().and_then(|s| s.code());
                        tracing::warn!("[{}] terminé (code {code:?})", job.key);
                        set(&status, &job.key, gen, |s| { s.last_exit = code; s.pid = None; s.state = "restarting".into(); });
                        false
                    }
                    _ = stop.changed() => true,
                };
                if stopped {
                    set(&status, &job.key, gen, |s| s.state = "stopping".into());
                    // Arrêt propre d'abord (bases de données), on ne force qu'en dernier recours.
                    let mut exited = false;
                    if let Some((prog, args)) = &job.graceful_stop {
                        let mut c = Command::new(prog);
                        c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                        #[cfg(windows)]
                        c.creation_flags(0x0800_0000);
                        let _ = tokio::time::timeout(Duration::from_secs(15), c.status()).await;
                        exited = tokio::time::timeout(Duration::from_secs(15), child.wait()).await.is_ok();
                        if exited {
                            tracing::info!("[{}] arrêté proprement", job.key);
                        }
                    }
                    if !exited {
                        if let Some(pid) = pid {
                            kill_tree(pid).await;
                        }
                        let _ = child.wait().await;
                    }
                    set(&status, &job.key, gen, |s| { s.state = "stopped".into(); s.pid = None; });
                    return;
                }
            }
        }
        if *stop.borrow() {
            set(&status, &job.key, gen, |s| s.state = "stopped".into());
            return;
        }
        if !job.autorestart {
            set(&status, &job.key, gen, |s| s.state = "exited".into());
            return;
        }
        // Crash en boucle : on espace les tentatives au lieu de marteler la machine.
        quick_fails = if t0.elapsed() < Duration::from_secs(5) { quick_fails + 1 } else { 0 };
        let delay = if quick_fails >= 5 { 30 } else { 2 };
        set(&status, &job.key, gen, |s| {
            s.restarts += 1;
            if quick_fails >= 5 { s.state = "crashloop".into(); }
        });
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
            _ = stop.changed() => {
                set(&status, &job.key, gen, |s| s.state = "stopped".into());
                return;
            }
        }
    }
}

fn spawn_once(job: &Job, log: &PathBuf) -> Result<Child> {
    std::fs::create_dir_all(paths::logs())?;
    let f = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let f2 = f.try_clone()?;
    let mut cmd = Command::from(env::build_command(&job.program, &job.args, &job.cwd, &job.env)?);
    cmd.stdin(Stdio::null()).stdout(Stdio::from(f)).stderr(Stdio::from(f2));
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    Ok(cmd.spawn()?)
}

/// Tue le processus ET ses enfants (ex: cmd.exe -> npm -> node).
pub async fn kill_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().await;
}
