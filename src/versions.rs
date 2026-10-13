//! Installation des versions de PHP / Node / Composer (une copie par version, partagée entre projets).
use crate::download::{client, download_extract, fetch};
use crate::paths;
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

const PHP_EXTS: &[&str] = &[
    "curl", "fileinfo", "gd", "intl", "mbstring", "openssl", "pdo_mysql", "mysqli", "pdo_sqlite",
    "sqlite3", "pdo_pgsql", "pgsql", "sodium", "zip", "exif", "bcmath",
];

pub async fn ensure_php(v: &str) -> Result<PathBuf> {
    let dir = paths::php_dir(v);
    if !dir.join("php-cgi.exe").exists() {
        install_php(v).await?;
    } else {
        // Un php.ini écrit par une ancienne version de Viherd peut manquer d'extensions (ex : pdo_pgsql).
        if let Err(e) = enable_missing_exts(&dir) {
            tracing::warn!("php.ini {}: {e}", dir.display());
        }
    }
    Ok(dir)
}

/// Active (décommente) les extensions de PHP_EXTS qui seraient encore commentées dans php.ini.
fn enable_missing_exts(dir: &Path) -> Result<()> {
    let ini = dir.join("php.ini");
    let src = std::fs::read_to_string(&ini)?;
    let mut changed = false;
    let out: Vec<String> = src.lines().map(|l| {
        if let Some(name) = l.trim().strip_prefix(";extension=") {
            if PHP_EXTS.contains(&name) && dir.join("ext").join(format!("php_{name}.dll")).exists() {
                changed = true;
                return format!("extension={name}");
            }
        }
        l.to_string()
    }).collect();
    if changed {
        std::fs::write(&ini, out.join("\r\n"))?;
        tracing::info!("php.ini mis à jour : extensions manquantes activées ({})", dir.display());
    }
    Ok(())
}

pub async fn ensure_node(v: &str) -> Result<PathBuf> {
    let dir = paths::node_dir(v);
    if !dir.join("node.exe").exists() {
        install_node(v).await?;
    }
    Ok(dir)
}

pub async fn install_php(v: &str) -> Result<()> {
    let rel: serde_json::Value = client()
        .get("https://windows.php.net/downloads/releases/releases.json")
        .send().await?.error_for_status()?.json().await?;
    let obj = rel.as_object().ok_or_else(|| anyhow!("releases.json invalide"))?;
    let entry = obj.get(v).and_then(|e| e.as_object()).ok_or_else(|| {
        anyhow!("PHP {v} introuvable. Disponibles : {}", obj.keys().cloned().collect::<Vec<_>>().join(", "))
    })?;
    // Build NTS x64 : la bonne pour php-cgi en FastCGI
    let zip = entry.iter()
        .find(|(k, _)| k.starts_with("nts-") && k.ends_with("-x64"))
        .and_then(|(_, x)| x["zip"]["path"].as_str())
        .ok_or_else(|| anyhow!("pas de build NTS x64 pour PHP {v}"))?;
    let dir = paths::php_dir(v);
    download_extract(&format!("https://windows.php.net/downloads/releases/{zip}"), &dir).await?;
    write_php_ini(&dir)?;
    println!("✔ PHP {} installé dans {}", entry.get("version").and_then(|x| x.as_str()).unwrap_or(v), dir.display());
    Ok(())
}

fn write_php_ini(dir: &Path) -> Result<()> {
    let src = std::fs::read_to_string(dir.join("php.ini-development"))?;
    let mut out: Vec<String> = src.lines().map(|l| {
        let t = l.trim();
        if t.starts_with(";extension_dir = \"ext\"") {
            return format!("extension_dir = \"{}\"", dir.join("ext").display());
        }
        if let Some(name) = t.strip_prefix(";extension=") {
            if PHP_EXTS.contains(&name) && dir.join("ext").join(format!("php_{name}.dll")).exists() {
                return format!("extension={name}");
            }
        }
        l.to_string()
    }).collect();
    for l in ["", "; --- viherd ---", "memory_limit=512M", "upload_max_filesize=256M",
              "post_max_size=256M", "max_execution_time=120"] {
        out.push(l.to_string());
    }
    std::fs::write(dir.join("php.ini"), out.join("\r\n"))?;
    Ok(())
}

pub async fn install_node(major: &str) -> Result<()> {
    let idx: Vec<serde_json::Value> = client()
        .get("https://nodejs.org/dist/index.json")
        .send().await?.error_for_status()?.json().await?;
    let m = major.trim_start_matches('v');
    let prefix = format!("v{m}.");
    let exact = format!("v{m}");
    let version = idx.iter()
        .filter(|e| e["files"].as_array().is_some_and(|f| f.iter().any(|x| x.as_str() == Some("win-x64-zip"))))
        .filter_map(|e| e["version"].as_str())
        .find(|s| s.starts_with(&prefix) || *s == exact)
        .ok_or_else(|| anyhow!("Node {major} introuvable"))?;
    let dir = paths::node_dir(major);
    download_extract(&format!("https://nodejs.org/dist/{version}/node-{version}-win-x64.zip"), &dir).await?;
    println!("✔ Node {version} installé dans {}", dir.display());
    Ok(())
}

pub async fn install_composer() -> Result<()> {
    let dir = paths::composer_dir();
    std::fs::create_dir_all(&dir)?;
    let phar = fetch("https://getcomposer.org/download/latest-stable/composer.phar").await?;
    std::fs::write(dir.join("composer.phar"), phar)?;
    std::fs::write(dir.join("composer.bat"), "@php \"%~dp0composer.phar\" %*\r\n")?;
    println!("✔ Composer installé dans {}", dir.display());
    Ok(())
}
