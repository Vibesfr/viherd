//! Installation des versions de PHP / Node / Composer (une copie par version, partagée entre projets).
use crate::download::{client, download_extract, fetch, find_file};
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
    // phpredis : client Redis par défaut de Laravel. Pas bloquant s'il manque (hors ligne…)
    if let Err(e) = ensure_phpredis(v, &dir).await {
        tracing::warn!("phpredis pour PHP {v} : {e:#}");
    }
    Ok(dir)
}

const PECL_REDIS: &str = "https://downloads.php.net/~windows/pecl/releases/redis/";

/// Télécharge php_redis.dll (PECL, build NTS x64) et l'active dans php.ini.
async fn ensure_phpredis(php: &str, dir: &Path) -> Result<()> {
    let dll = dir.join("ext").join("php_redis.dll");
    if !dll.exists() {
        let index = client().get(PECL_REDIS).send().await?.error_for_status()?.text().await?;
        let version = latest_stable_dir(&index).ok_or_else(|| anyhow!("aucune version sur {PECL_REDIS}"))?;
        let page = client().get(format!("{PECL_REDIS}{version}/")).send().await?.error_for_status()?.text().await?;
        let zip = pecl_zip(&page, "php_redis-", php).ok_or_else(|| anyhow!("pas de build phpredis {version} pour PHP {php}"))?;
        let tmp = dir.join("ext").join(".phpredis");
        download_extract(&format!("{PECL_REDIS}{version}/{zip}"), &tmp).await?;
        let found = find_file(&tmp, "php_redis.dll").ok_or_else(|| anyhow!("php_redis.dll absent de {zip}"));
        let copied = found.and_then(|f| Ok(std::fs::copy(f, &dll)?));
        let _ = std::fs::remove_dir_all(&tmp);
        copied?;
        tracing::info!("phpredis {version} installé pour PHP {php}");
    }
    let ini = dir.join("php.ini");
    let src = std::fs::read_to_string(&ini)?;
    if !src.lines().any(|l| l.trim() == "extension=redis") {
        std::fs::write(&ini, format!("{}\r\nextension=redis\r\n", src.trim_end()))?;
    }
    Ok(())
}

/// Dossier de la version stable la plus récente dans un index PECL (« 6.3.0/ », sans RC ni beta).
fn latest_stable_dir(index: &str) -> Option<String> {
    index.split("href=\"").skip(1)
        .filter_map(|s| s.split('"').next()?.strip_suffix('/'))
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .max_by_key(|v| v.split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>())
        .map(str::to_string)
}

/// Zip NTS x64 pour cette version de PHP, ex : php_redis-6.3.0-8.4-nts-vs17-x64.zip
fn pecl_zip(page: &str, prefix: &str, php: &str) -> Option<String> {
    let needle = format!("-{php}-nts-");
    page.split("href=\"").skip(1)
        .filter_map(|s| s.split('"').next())
        .map(|h| h.rsplit('/').next().unwrap_or(h))
        .find(|f| f.starts_with(prefix) && f.contains(&needle) && f.ends_with("-x64.zip"))
        .map(str::to_string)
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

/// Télécharge php_xdebug.dll (build NTS x64 correspondant à la version de PHP) depuis xdebug.org.
pub async fn ensure_xdebug(php: &str) -> Result<PathBuf> {
    let dir = ensure_php(php).await?;
    let dll = dir.join("ext").join("php_xdebug.dll");
    if dll.exists() {
        return Ok(dll);
    }
    let page = client().get("https://xdebug.org/download").send().await?.error_for_status()?.text().await?;
    let file = xdebug_file(&page, php).ok_or_else(|| anyhow!("pas de build Xdebug pour PHP {php} sur xdebug.org"))?;
    let data = fetch(&format!("https://xdebug.org/files/{file}")).await?;
    std::fs::write(&dll, data)?;
    tracing::info!("Xdebug installé pour PHP {php} ({file})");
    Ok(dll)
}

/// Nom du fichier stable le plus récent pour cette version de PHP, ex : php_xdebug-3.5.3-8.3-nts-vs16-x86_64.dll
pub fn xdebug_file(page: &str, php: &str) -> Option<String> {
    let needle = format!("-{php}-nts-");
    page.split("/files/").skip(1)
        .filter_map(|s| s.split(['\'', '"']).next())
        .find(|f| f.starts_with("php_xdebug-") && f.contains(&needle) && f.ends_with("-x86_64.dll")
            && !["alpha", "beta", "RC"].iter().any(|x| f.contains(x)))
        .map(str::to_string)
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

#[cfg(test)]
mod tests {
    #[test]
    fn picks_stable_nts_xdebug_build() {
        let page = "<a href='/files/php_xdebug-3.6.0alpha1-8.3-nts-vs16-x86_64.dll'>\
                    <a href='/files/php_xdebug-3.5.3-8.3-ts-vs16-x86_64.dll'>\
                    <a href='/files/php_xdebug-3.5.3-8.3-nts-vs16-x86_64.dll'>\
                    <a href='/files/php_xdebug-3.5.3-8.4-nts-vs17-x86_64.dll'>";
        assert_eq!(super::xdebug_file(page, "8.3").as_deref(), Some("php_xdebug-3.5.3-8.3-nts-vs16-x86_64.dll"));
        assert_eq!(super::xdebug_file(page, "8.4").as_deref(), Some("php_xdebug-3.5.3-8.4-nts-vs17-x86_64.dll"));
        assert_eq!(super::xdebug_file(page, "7.4"), None);
    }

    #[test]
    fn picks_phpredis_build() {
        let index = r#"<a href="5.3.7RC1/">5.3.7RC1/</a> <a href="6.10.0/">6.10.0/</a> <a href="6.3.0/">6.3.0/</a> <a href="../">..</a>"#;
        assert_eq!(super::latest_stable_dir(index).as_deref(), Some("6.10.0"));
        let page = r#"<a href="/~windows/pecl/releases/redis/6.3.0/php_redis-6.3.0-8.4-ts-vs17-x64.zip">
                      <a href="/~windows/pecl/releases/redis/6.3.0/php_redis-6.3.0-8.4-nts-vs17-x64.zip">"#;
        assert_eq!(super::pecl_zip(page, "php_redis-", "8.4").as_deref(), Some("php_redis-6.3.0-8.4-nts-vs17-x64.zip"));
        assert_eq!(super::pecl_zip(page, "php_redis-", "8.3"), None);
    }
}
