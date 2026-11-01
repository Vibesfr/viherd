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
        // 404 aussi pour un dépôt privé : ne pas annoncer « à jour » à tort
        let repo = client().get(format!("https://api.github.com/repos/{REPO}")).send().await?;
        if repo.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("le dépôt GitHub {REPO} est privé ou introuvable : mises à jour indisponibles");
        }
        return Ok(None); // dépôt public sans release
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
    // notes de toutes les versions sautées (0.2.1 → 0.2.4 : celles de 0.2.2, 0.2.3 et 0.2.4)
    let notes = match client().get(format!("https://api.github.com/repos/{REPO}/releases?per_page=30"))
        .header("Accept", "application/vnd.github+json").send().await.and_then(|r| r.error_for_status())
    {
        Ok(r) => r.json::<Vec<serde_json::Value>>().await.map(|all| combined_notes(&all, crate::VERSION)).unwrap_or_default(),
        Err(_) => String::new(),
    };
    Ok(Some(Available {
        version,
        url: url.to_string(),
        page: rel["html_url"].as_str().unwrap_or("").to_string(),
        notes: if notes.is_empty() { rel["body"].as_str().unwrap_or("").to_string() } else { notes }.chars().take(12_000).collect(),
    }))
}

/// Notes des releases plus récentes que `current`, de la plus récente à la plus ancienne,
/// chacune sous un titre « Version X.Y.Z » (sans le pied de page « --- » ni le titre « Nouveautés »).
fn combined_notes(releases: &[serde_json::Value], current: &str) -> String {
    let mut list: Vec<(&str, &str)> = releases.iter()
        .filter(|r| r["draft"] != true && r["prerelease"] != true)
        .filter_map(|r| Some((r["tag_name"].as_str()?.trim_start_matches('v'), r["body"].as_str().unwrap_or(""))))
        .filter(|(v, _)| newer(v, current))
        .collect();
    list.sort_by(|a, b| if newer(a.0, b.0) { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater });
    list.iter().map(|(v, body)| {
        let body = body.split("\n---").next().unwrap_or("").replace("## Nouveautés", "");
        format!("## Version {v}\n\n{}", body.trim())
    }).collect::<Vec<_>>().join("\n\n")
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
    use super::{combined_notes, newer};

    #[test]
    fn combines_notes_of_skipped_versions() {
        let rel = |v: &str, body: &str| serde_json::json!({ "tag_name": v, "body": body, "draft": false, "prerelease": false });
        let all = [
            rel("v0.2.2", "## Nouveautés\n\n### Dossiers\n- Parcourir\n\n---\n\nInstaller…"),
            rel("v0.2.3", "## Nouveautés\n\n### Icône"),
            rel("v0.2.1", "## Nouveautés\n\n### Tables"),
            serde_json::json!({ "tag_name": "v0.3.0", "body": "brouillon", "draft": true }),
        ];
        let n = combined_notes(&all, "0.2.1");
        assert_eq!(n, "## Version 0.2.3\n\n### Icône\n\n## Version 0.2.2\n\n### Dossiers\n- Parcourir");
    }

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
