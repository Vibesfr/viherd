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

const PECL: &str = "https://downloads.php.net/~windows/pecl/releases/";
const PHP_RELEASES: &str = "https://downloads.php.net/~windows/releases/";

/// phpredis : téléchargé et activé une fois ; si l'utilisateur le désactive ensuite, on n'y touche plus.
async fn ensure_phpredis(php: &str, dir: &Path) -> Result<()> {
    if !dir.join("ext").join("php_redis.dll").exists() {
        install_pecl(php, dir, "redis").await?;
    }
    let ini = dir.join("php.ini");
    let src = std::fs::read_to_string(&ini)?;
    if !src.lines().any(|l| l.trim().trim_start_matches(';').trim() == "extension=redis") {
        std::fs::write(&ini, format!("{}\r\nextension=redis\r\n", src.trim_end()))?;
    }
    Ok(())
}

/// Télécharge une extension PECL (build NTS x64 pour cette version de PHP) : php_<nom>.dll dans ext/,
/// et ses éventuelles DLL compagnes (ImageMagick pour imagick, libyaml…) à côté de php.exe.
/// N'active rien dans php.ini.
pub async fn install_pecl(php: &str, dir: &Path, name: &str) -> Result<String> {
    let base = format!("{PECL}{name}/");
    let index = client().get(&base).send().await?.error_for_status()?.text().await?;
    // la dernière version n'a pas toujours de build pour un PHP tout neuf : on remonte un peu
    let mut tried = vec![];
    for version in stable_dirs(&index).into_iter().take(4) {
        let page = client().get(format!("{base}{version}/")).send().await?.error_for_status()?.text().await?;
        let Some(zip) = pecl_zip(&page, &format!("php_{name}-"), php) else { tried.push(version); continue };
        let tmp = dir.join("ext").join(format!(".{name}"));
        download_extract(&format!("{base}{version}/{zip}"), &tmp).await?;
        let res: Result<()> = (|| {
            let dll = format!("php_{name}.dll");
            let main = find_file(&tmp, &dll).ok_or_else(|| anyhow!("{dll} absent de {zip}"))?;
            std::fs::copy(main, dir.join("ext").join(&dll))?;
            for e in walk(&tmp) {
                let f = e.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                if f.ends_with(".dll") && !f.starts_with("php_") && !dir.join(&f).exists() {
                    std::fs::copy(&e, dir.join(&f))?;
                }
            }
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        res?;
        tracing::info!("extension {name} {version} installée pour PHP {php}");
        return Ok(version);
    }
    Err(anyhow!("pas de build Windows de {name} pour PHP {php} (versions essayées : {})", tried.join(", ")))
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().is_dir() { out.extend(walk(&e.path())); } else { out.push(e.path()); }
    }
    out
}

/// Versions stables d'un index PECL (« 6.3.0/ », sans RC ni beta), de la plus récente à la plus ancienne.
fn stable_dirs(index: &str) -> Vec<String> {
    let mut v: Vec<String> = index.split("href=\"").skip(1)
        .filter_map(|s| s.split('"').next()?.strip_suffix('/'))
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(str::to_string)
        .collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>()));
    v.dedup();
    v
}

#[cfg(test)]
fn latest_stable_dir(index: &str) -> Option<String> {
    stable_dirs(index).into_iter().next()
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

/// Marque d'un php.ini déjà complété : ensuite, les extensions sont celles choisies par l'utilisateur.
const INI_MARK: &str = "; viherd: extensions initialisées";

/// Active (décommente) les extensions de PHP_EXTS qui seraient encore commentées dans php.ini — une seule fois.
fn enable_missing_exts(dir: &Path) -> Result<()> {
    let ini = dir.join("php.ini");
    let src = std::fs::read_to_string(&ini)?;
    if src.contains(INI_MARK) {
        return Ok(());
    }
    let mut changed = true; // ajoute au moins la marque
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
        std::fs::write(&ini, format!("{}\r\n{INI_MARK}\r\n", out.join("\r\n").trim_end()))?;
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

async fn php_releases() -> Result<serde_json::Map<String, serde_json::Value>> {
    let rel: serde_json::Value = client().get(format!("{PHP_RELEASES}releases.json")).send().await?.error_for_status()?.json().await?;
    rel.as_object().cloned().ok_or_else(|| anyhow!("releases.json invalide"))
}

/// Zip NTS x64 (le bon pour php-cgi en FastCGI) et numéro complet d'une version de PHP.
fn php_zip(obj: &serde_json::Map<String, serde_json::Value>, v: &str) -> Result<(String, String)> {
    let entry = obj.get(v).and_then(|e| e.as_object()).ok_or_else(|| {
        anyhow!("PHP {v} introuvable. Disponibles : {}", obj.keys().cloned().collect::<Vec<_>>().join(", "))
    })?;
    let zip = entry.iter()
        .find(|(k, _)| k.starts_with("nts-") && k.ends_with("-x64"))
        .and_then(|(_, x)| x["zip"]["path"].as_str())
        .ok_or_else(|| anyhow!("pas de build NTS x64 pour PHP {v}"))?;
    Ok((zip.to_string(), entry.get("version").and_then(|x| x.as_str()).unwrap_or(v).to_string()))
}

pub async fn install_php(v: &str) -> Result<()> {
    let (zip, full) = php_zip(&php_releases().await?, v)?;
    let dir = paths::php_dir(v);
    download_extract(&format!("{PHP_RELEASES}{zip}"), &dir).await?;
    write_php_ini(&dir)?;
    println!("✔ PHP {full} installé dans {}", dir.display());
    Ok(())
}

/// Dernières versions publiées : PHP (releases.json) et Node (index.json), gardées une heure en mémoire.
pub async fn available() -> Result<serde_json::Value> {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>> = std::sync::Mutex::new(None);
    if let Some((at, v)) = CACHE.lock().unwrap().as_ref() {
        if at.elapsed().as_secs() < 3600 { return Ok(v.clone()); }
    }
    let php_rel = php_releases().await?;
    let mut php: Vec<serde_json::Value> = php_rel.keys()
        .filter_map(|k| php_zip(&php_rel, k).ok().map(|(_, full)| serde_json::json!({ "v": k, "latest": full.split('-').next().unwrap_or(&full) })))
        .collect();
    php.sort_by_key(|x| std::cmp::Reverse(num(x["v"].as_str().unwrap_or(""))));
    let idx: Vec<serde_json::Value> = client().get("https://nodejs.org/dist/index.json").send().await?.error_for_status()?.json().await?;
    let node = node_majors(&idx);
    let v = serde_json::json!({ "php": php, "node": node });
    *CACHE.lock().unwrap() = Some((std::time::Instant::now(), v.clone()));
    Ok(v)
}

fn num(v: &str) -> Vec<u32> {
    v.trim_start_matches('v').split(['.', '-']).map(|n| n.parse().unwrap_or(0)).collect()
}

/// Dernière version stable de chaque majeure de Node (avec un zip Windows x64), LTS indiquées.
fn node_majors(idx: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = vec![];
    for r in idx {
        let ver = r["version"].as_str().unwrap_or("");
        if ver.contains('-') || !r["files"].as_array().is_some_and(|f| f.iter().any(|x| x == "win-x64-zip")) { continue; }
        let major = ver.trim_start_matches('v').split('.').next().unwrap_or("").to_string();
        if major.parse::<u32>().unwrap_or(0) < 18 || out.iter().any(|o| o["v"] == major.as_str()) { continue; }
        out.push(serde_json::json!({ "v": major, "latest": ver.trim_start_matches('v'), "lts": r["lts"].as_str() }));
    }
    out
}

/// Version exacte installée (8.4.26, 24.11.1), lue auprès du binaire.
pub fn installed_version(tool: &str, v: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let (exe, args): (PathBuf, &[&str]) = match tool {
        "php" => (paths::php_dir(v).join("php.exe"), &["-n", "-r", "echo PHP_VERSION;"]),
        _ => (paths::node_dir(v).join("node.exe"), &["--version"]),
    };
    let o = std::process::Command::new(exe).args(args).creation_flags(0x0800_0000).output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().trim_start_matches('v').to_string();
    (o.status.success() && !s.is_empty()).then_some(s)
}

/// Remplace une version installée par la dernière de sa branche (8.4.26 → 8.4.27, Node 24.x → dernière 24).
/// PHP garde son php.ini et les DLL ajoutées (PECL, Xdebug). Les processus qui l'utilisent doivent être arrêtés.
pub async fn update_runtime(tool: &str, v: &str) -> Result<String> {
    let dir = if tool == "php" { paths::php_dir(v) } else { paths::node_dir(v) };
    if !dir.exists() { anyhow::bail!("{tool} {v} n'est pas installé"); }
    let fresh = dir.with_file_name(format!(".{v}.new"));
    let old = dir.with_file_name(format!(".{v}.old"));
    let _ = std::fs::remove_dir_all(&fresh);
    let _ = std::fs::remove_dir_all(&old);
    let full = if tool == "php" {
        let (zip, full) = php_zip(&php_releases().await?, v)?;
        download_extract(&format!("{PHP_RELEASES}{zip}"), &fresh).await?;
        // réglages et extensions ajoutées : repris de l'ancienne installation
        std::fs::copy(dir.join("php.ini"), fresh.join("php.ini"))?;
        for (sub, from) in [("ext", dir.join("ext")), ("", dir.clone())] {
            for e in std::fs::read_dir(&from).into_iter().flatten().flatten() {
                let name = e.file_name();
                let to = if sub.is_empty() { fresh.join(&name) } else { fresh.join(sub).join(&name) };
                if e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")) && !to.exists() {
                    std::fs::copy(e.path(), to)?;
                }
            }
        }
        full
    } else {
        let idx: Vec<serde_json::Value> = client().get("https://nodejs.org/dist/index.json").send().await?.error_for_status()?.json().await?;
        let latest = node_majors(&idx).into_iter().find(|n| n["v"] == v)
            .and_then(|n| n["latest"].as_str().map(String::from)).ok_or_else(|| anyhow!("Node {v} introuvable"))?;
        download_extract(&format!("https://nodejs.org/dist/v{latest}/node-v{latest}-win-x64.zip"), &fresh).await?;
        latest
    };
    std::fs::rename(&dir, &old).map_err(|e| anyhow!("{tool} {v} est encore utilisé (terminal, éditeur ?) : {e}"))?;
    if let Err(e) = std::fs::rename(&fresh, &dir) {
        let _ = std::fs::rename(&old, &dir);
        return Err(anyhow!("mise à jour de {tool} {v} : {e}"));
    }
    let _ = std::fs::remove_dir_all(&old);
    tracing::info!("{tool} {v} mis à jour en {full}");
    Ok(full)
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
              "post_max_size=256M", "max_execution_time=120", INI_MARK] {
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
