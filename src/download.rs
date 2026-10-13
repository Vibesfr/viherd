//! Téléchargement + extraction de zips (PHP, Node, MariaDB, Redis...).
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("viherd/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("client http")
}

pub async fn fetch(url: &str) -> Result<bytes::Bytes> {
    Ok(client().get(url).send().await?.error_for_status()?.bytes().await?)
}

/// Télécharge un zip et l'extrait dans `dest`. Si le zip contient un seul dossier racine, il est "aplati".
pub async fn download_extract(url: &str, dest: &Path) -> Result<()> {
    println!("↓ {url}");
    let data = fetch(url).await?;
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || extract_flatten(&data, &dest)).await??;
    Ok(())
}

fn extract_flatten(data: &[u8], dest: &Path) -> Result<()> {
    let parent = dest.parent().unwrap();
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.tmp", dest.file_name().unwrap().to_string_lossy()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    zip::ZipArchive::new(std::io::Cursor::new(data))?.extract(&tmp)?;
    let entries: Vec<_> = std::fs::read_dir(&tmp)?.flatten().collect();
    let src = if entries.len() == 1 && entries[0].path().is_dir() { entries[0].path() } else { tmp.clone() };
    let _ = std::fs::remove_dir_all(dest);
    std::fs::rename(&src, dest)?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

pub fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if e.file_name().to_string_lossy().eq_ignore_ascii_case(name) {
            return Some(p);
        }
    }
    None
}
