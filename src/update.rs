//! Mise à jour : dernière release GitHub de Viherd, vérifiée au démarrage du démon puis toutes les 6 h.
//! L'installeur téléchargé (viherd-setup.exe) arrête Viherd, remplace les fichiers et peut le relancer.
use crate::download::{client, fetch};
use crate::paths;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::path::PathBuf;

const REPO: &str = "Vibesfr/viherd";
const ASSET: &str = "viherd-setup.exe";

#[derive(Debug, Clone, Serialize)]
pub struct Available {
    pub version: String,
    /// Lien direct vers viherd-setup.exe
    pub url: String,
    /// Page de la release (notes complètes)
    pub page: String,
    pub notes: String,
}

/// Nouvelle version disponible, ou None si Viherd est à jour (ou s'il n'y a pas encore de release).
pub async fn check() -> Result<Option<Available>> {
    let r = client().get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json").send().await?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let rel: serde_json::Value = r.error_for_status()?.json().await?;
    let version = rel["tag_name"].as_str().unwrap_or("").trim_start_matches('v').to_string();
    if !newer(&version, crate::VERSION) {
        return Ok(None);
    }
    let url = rel["assets"].as_array().into_iter().flatten()
        .find(|a| a["name"] == ASSET)
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or_else(|| anyhow!("la release v{version} n'a pas de {ASSET}"))?;
    Ok(Some(Available {
        version,
        url: url.to_string(),
        page: rel["html_url"].as_str().unwrap_or("").to_string(),
        notes: rel["body"].as_str().unwrap_or("").chars().take(4000).collect(),
    }))
}

/// `a` est-elle plus récente que `b` ? (comparaison numérique 1.10.0 > 1.9.3, suffixes -beta ignorés)
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.split(['-', '+']).next().unwrap_or("").split('.').map(|n| n.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parts(a), parts(b));
    (0..a.len().max(b.len())).map(|i| (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0)))
        .find(|(x, y)| x != y).is_some_and(|(x, y)| x > y)
}

/// Télécharge l'installeur de cette version dans ~/.viherd/updates (réutilisé s'il est déjà là).
pub async fn download(a: &Available) -> Result<PathBuf> {
    let dir = paths::home().join("updates");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("viherd-setup-{}.exe", a.version));
    if !file.exists() {
        let data = fetch(&a.url).await?;
        let part = file.with_extension("part");
        std::fs::write(&part, data)?;
        std::fs::rename(&part, &file)?;
    }
    Ok(file)
}

/// Lance l'installeur (il arrête lui-même Viherd avant de remplacer les fichiers).
pub fn launch(setup: &std::path::Path) -> Result<()> {
    std::process::Command::new(setup).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn compares_versions_numerically() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.10.0", "1.9.3"));
        assert!(newer("0.1.1", "0.1"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0-beta", "0.1.0"));
        assert!(!newer("", "0.1.0"));
    }
}
