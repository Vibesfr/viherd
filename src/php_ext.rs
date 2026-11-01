//! Extensions PHP d'une version installée : celles livrées avec PHP (ext/php_*.dll) et un choix d'extensions PECL
//! courantes, téléchargées à la demande. L'état se lit et s'écrit dans le php.ini de la version.
use crate::{paths, versions};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

/// Extensions PECL proposées (toutes publiées en build Windows NTS x64 sur downloads.php.net).
pub const PECL: &[(&str, &str)] = &[
    ("apcu", "Cache en mémoire partagée (APCu)"),
    ("redis", "Client Redis (phpredis), le défaut de Laravel"),
    ("imagick", "Images avec ImageMagick (Intervention, Spatie Media Library)"),
    ("mongodb", "Client MongoDB"),
    ("pcov", "Couverture de code rapide pour PHPUnit / Pest"),
    ("igbinary", "Sérialisation binaire compacte (cache, sessions)"),
    ("msgpack", "Sérialisation MessagePack"),
    ("memcache", "Client Memcached"),
    ("sqlsrv", "SQL Server"),
    ("pdo_sqlsrv", "SQL Server pour PDO / Eloquent"),
    ("yaml", "Lecture et écriture de YAML"),
    ("xlswriter", "Écriture rapide de fichiers Excel"),
    ("ds", "Structures de données (Vector, Map, Set…)"),
    ("ast", "Arbre syntaxique de PHP (outils d'analyse)"),
];

/// Extensions chargées autrement que par `extension=` ou gérées ailleurs.
const ZEND: &[&str] = &["opcache"];
const HIDDEN: &[&str] = &["xdebug", "dl_test", "zend_test"];

#[derive(Debug, Serialize)]
pub struct Ext {
    pub name: String,
    pub enabled: bool,
    pub installed: bool,
    /// Description pour les extensions PECL
    pub about: Option<&'static str>,
    pub pecl: bool,
}

/// Nom normalisé d'une valeur `extension=` : « php_gd.dll », "gd", C:\…\php_gd.dll → gd.
fn ext_name(v: &str) -> String {
    let v = v.trim().trim_matches('"').trim_matches('\'');
    let file = v.rsplit(['\\', '/']).next().unwrap_or(v);
    file.trim_end_matches(".dll").trim_start_matches("php_").to_lowercase()
}

/// Extensions actives d'un php.ini.
pub fn enabled_in(ini: &str) -> BTreeSet<String> {
    ini.lines().filter_map(|l| {
        let l = l.trim();
        let v = l.strip_prefix("extension=").or_else(|| l.strip_prefix("zend_extension="))
            .or_else(|| l.strip_prefix("extension =")).or_else(|| l.strip_prefix("zend_extension ="))?;
        Some(ext_name(v))
    }).collect()
}

/// Active ou désactive une extension : décommente / commente sa ligne, ou en ajoute une à la fin.
pub fn set_in(ini: &str, name: &str, on: bool) -> String {
    let key = if ZEND.contains(&name) { "zend_extension" } else { "extension" };
    let matches = |l: &str| {
        let t = l.trim().trim_start_matches(';').trim();
        t.split_once('=').is_some_and(|(k, v)| (k.trim() == "extension" || k.trim() == "zend_extension") && ext_name(v) == name)
    };
    let mut lines: Vec<String> = ini.lines().map(String::from).collect();
    let mut done = false;
    for l in lines.iter_mut() {
        if !matches(l) { continue; }
        let active = !l.trim_start().starts_with(';');
        if on && !active && !done {
            *l = l.trim_start().trim_start_matches(';').trim_start().to_string();
            done = true;
        } else if on && active {
            done = true;
        } else if !on && active {
            *l = format!(";{}", l.trim_start());
        }
    }
    if on && !done {
        lines.push(format!("{key}={name}"));
    }
    let eol = if ini.contains("\r\n") { "\r\n" } else { "\n" };
    lines.join(eol) + eol
}

pub fn list(php: &str) -> Result<Vec<Ext>> {
    let dir = paths::php_dir(php);
    if !dir.join("php.exe").exists() { bail!("PHP {php} n'est pas installé"); }
    let ini = std::fs::read_to_string(dir.join("php.ini")).unwrap_or_default();
    let on = enabled_in(&ini);
    let dlls: BTreeSet<String> = std::fs::read_dir(dir.join("ext")).into_iter().flatten().flatten()
        .map(|e| e.file_name().to_string_lossy().to_lowercase())
        .filter(|f| f.starts_with("php_") && f.ends_with(".dll"))
        .map(|f| ext_name(&f))
        .collect();
    let mut out: Vec<Ext> = PECL.iter().map(|(n, about)| Ext {
        name: n.to_string(), enabled: on.contains(*n), installed: dlls.contains(*n), about: Some(about), pecl: true,
    }).collect();
    for n in &dlls {
        if PECL.iter().any(|(p, _)| p == n) || HIDDEN.contains(&n.as_str()) { continue; }
        out.push(Ext { name: n.clone(), enabled: on.contains(n), installed: true, about: None, pecl: false });
    }
    Ok(out)
}

/// Active / désactive ; une extension PECL absente est d'abord téléchargée.
pub async fn set(php: &str, name: &str, on: bool) -> Result<()> {
    let dir = paths::php_dir(php);
    if !dir.join("php.exe").exists() { bail!("PHP {php} n'est pas installé"); }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { bail!("nom d'extension invalide"); }
    if HIDDEN.contains(&name) { bail!("{name} se règle par projet (réglages du site)"); }
    let dll = dir.join("ext").join(format!("php_{name}.dll"));
    if on && !dll.exists() {
        if !PECL.iter().any(|(p, _)| *p == name) { bail!("extension inconnue : {name}"); }
        versions::install_pecl(php, &dir, name).await?;
    }
    let ini = dir.join("php.ini");
    let text = std::fs::read_to_string(&ini)?;
    std::fs::write(&ini, set_in(&text, name, on))?;
    if on { check_loads(&dir, name)?; }
    Ok(())
}

/// PHP démarre-t-il avec l'extension ? Sinon on la désactive aussitôt et on rend l'erreur lisible.
fn check_loads(dir: &Path, name: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let o = std::process::Command::new(dir.join("php.exe")).args(["-c", &dir.join("php.ini").display().to_string(), "-m"])
        .creation_flags(0x0800_0000).output()?;
    let mods = String::from_utf8_lossy(&o.stdout).to_lowercase();
    let loaded = mods.lines().any(|l| l.trim() == name || l.trim().replace(' ', "_") == name || (name == "opcache" && l.contains("opcache")));
    if !loaded {
        let ini = dir.join("php.ini");
        let text = std::fs::read_to_string(&ini)?;
        std::fs::write(&ini, set_in(&text, name, false))?;
        let err = String::from_utf8_lossy(&o.stderr);
        let first = format!("{}{}", err, String::from_utf8_lossy(&o.stdout)).lines()
            .find(|l| l.to_lowercase().contains("warning") || l.to_lowercase().contains("unable")).unwrap_or("").trim().to_string();
        bail!("PHP ne charge pas {name} : désactivée. {first}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_toggles_extensions() {
        let ini = ";extension=gd\r\nextension=intl\r\nextension=\"php_redis.dll\"\r\n;zend_extension=opcache\r\n";
        let on = enabled_in(ini);
        assert!(on.contains("intl") && on.contains("redis") && !on.contains("gd"));
        let a = set_in(ini, "gd", true);
        assert!(a.contains("\r\nextension=intl") && a.starts_with("extension=gd\r\n"));
        let b = set_in(ini, "redis", false);
        assert!(b.contains(";extension=\"php_redis.dll\""));
        let c = set_in(ini, "opcache", true);
        assert!(c.contains("\r\nzend_extension=opcache\r\n"));
        let d = set_in(ini, "apcu", true);
        assert!(d.ends_with("extension=apcu\r\n"));
        // rien ne bouge si l'état est déjà le bon
        assert_eq!(set_in(ini, "intl", true), ini);
    }
}
