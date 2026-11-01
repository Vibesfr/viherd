//! Sauvegardes automatiques : une fois par jour, à partir de l'heure choisie, chaque base MySQL / PostgreSQL
//! est exportée dans ~/.viherd/backups/<service>/<base>/ (étiquette « auto ») ; les plus anciennes sont supprimées.
use crate::db;
use crate::paths;
use crate::server::State;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

const LABEL: &str = "auto";

fn marker() -> std::path::PathBuf { paths::home().join("backups").join(".last-auto") }

/// Boucle du démon : vérifie toutes les 10 minutes si la sauvegarde du jour est due.
pub async fn run(st: Arc<State>) {
    loop {
        tokio::time::sleep(Duration::from_secs(600)).await;
        let cfg = st.global.read().unwrap().backups.clone();
        let now = chrono::Local::now();
        let today = now.format("%Y-%m-%d").to_string();
        if !due(cfg.enabled, cfg.hour, now.format("%H").to_string().parse().unwrap_or(0), &today, std::fs::read_to_string(marker()).ok().as_deref()) {
            continue;
        }
        let n = run_once(&st, cfg.keep).await;
        tracing::info!("sauvegarde automatique : {n} base(s)");
        let _ = std::fs::create_dir_all(marker().parent().unwrap());
        let _ = std::fs::write(marker(), &today);
    }
}

/// La sauvegarde du jour est-elle à faire ?
fn due(enabled: bool, hour: u8, now_hour: u8, today: &str, last: Option<&str>) -> bool {
    enabled && now_hour >= hour && last.map(str::trim) != Some(today)
}

/// Sauvegarde toutes les bases des services démarrés ; renvoie le nombre de bases sauvegardées.
pub async fn run_once(st: &State, keep: usize) -> usize {
    let statuses = st.sup.statuses();
    let mut n = 0;
    for svc in ["mysql", "postgres"] {
        if statuses.get(&format!("service:{svc}")).is_none_or(|s| s.state != "running") {
            continue;
        }
        let Ok(t) = db::target(svc) else { continue };
        let dbs = match db::databases(&t).await {
            Ok(d) => d,
            Err(e) => { tracing::warn!("sauvegarde automatique {svc} : {e:#}"); continue; }
        };
        for name in dbs.iter().filter(|d| d.as_str() != "postgres") {
            match db::create_backup(&t, name, LABEL).await {
                Ok(_) => { n += 1; let _ = prune(&t, name, keep); }
                Err(e) => tracing::warn!("sauvegarde automatique {svc}/{name} : {e:#}"),
            }
        }
    }
    n
}

/// Ne garde que les `keep` sauvegardes automatiques les plus récentes (les sauvegardes manuelles restent).
fn prune(t: &db::Target, name: &str, keep: usize) -> Result<()> {
    let dir = db::backup_dir(t, name)?;
    let mut autos: Vec<String> = std::fs::read_dir(&dir)?.flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|f| f.ends_with(&format!("__{LABEL}.sql")))
        .collect();
    autos.sort_by(|a, b| b.cmp(a)); // horodatage en tête du nom : du plus récent au plus ancien
    for old in autos.iter().skip(keep.max(1)) {
        std::fs::remove_file(dir.join(old))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::due;

    #[test]
    fn daily_backup_is_due_once_after_the_hour() {
        assert!(due(true, 12, 13, "2026-11-01", Some("2026-10-31")));
        assert!(due(true, 12, 12, "2026-11-01", None));
        assert!(!due(true, 12, 11, "2026-11-01", None));
        assert!(!due(true, 12, 18, "2026-11-01", Some("2026-11-01\n")));
        assert!(!due(false, 0, 23, "2026-11-01", None));
    }
}
