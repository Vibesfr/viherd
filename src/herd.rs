//! Import depuis Laravel Herd (Windows) : dossiers parqués, sites liés, sites en HTTPS, version de PHP par site,
//! puis correction des .env qui pointent encore vers Herd (APP_URL, certificats de Reverb).
//! Lu dans %USERPROFILE%\.config\herd\config : valet\config.json, valet\Sites (liens), valet\Nginx\<site>.test.conf.
use crate::config::{self, Global, Project};
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".config").join("herd").join("config")
}
fn valet() -> PathBuf { config_dir().join("valet") }

#[derive(Debug, Serialize, Default)]
pub struct Plan {
    pub found: bool,
    pub tld: String,
    /// Dossiers parqués dans Herd
    pub parked: Vec<Entry>,
    /// Sites liés (`herd link`)
    pub links: Vec<Entry>,
    /// Sites servis en HTTPS par Herd
    pub secured: Vec<Entry>,
    /// Sites isolés sur une version de PHP (`herd isolate`)
    pub php: Vec<Entry>,
    /// Modifications proposées dans les .env (calculées pour l'import complet proposé)
    pub env: Vec<EnvFix>,
    /// Viherd tourne sur d'autres ports que 80 / 443 (souvent à cause de Herd)
    pub ports: Option<(u16, u16)>,
    /// Ce qui n'est pas importé, à refaire à la main
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Entry {
    /// Identifiant renvoyé par l'interface pour choisir cet élément
    pub id: String,
    pub label: String,
    pub detail: String,
    /// Déjà en place dans Viherd
    pub done: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct EnvFix {
    pub site: String,
    pub file: String,
    pub changes: Vec<Change>,
    /// Autres valeurs qui citent Herd, laissées telles quelles
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Change {
    pub key: String,
    pub from: String,
    pub to: String,
}

/// Ce que l'utilisateur a coché (identifiants des Entry / noms de site pour les .env).
#[derive(Debug, Default)]
pub struct Choice {
    pub parked: Vec<String>,
    pub links: Vec<String>,
    pub secured: Vec<String>,
    pub php: Vec<String>,
    pub env: Vec<String>,
    pub ports: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct Report {
    pub parked: usize,
    pub links: usize,
    pub secured: usize,
    pub php: usize,
    pub env: Vec<EnvFix>,
    pub notes: Vec<String>,
}

/// Lecture brute de la configuration de Herd.
struct Herd {
    tld: String,
    parked: Vec<PathBuf>,
    links: Vec<(String, PathBuf)>,
    secured: Vec<String>,
    php: Vec<(String, String)>,
    proxies: Vec<String>,
}

fn read_herd() -> Option<Herd> {
    let cfg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(valet().join("config.json")).ok()?).ok()?;
    let tld = cfg["tld"].as_str().unwrap_or("test").to_string();
    let sites_dir = valet().join("Sites");
    let parked = cfg["paths"].as_array().into_iter().flatten()
        .filter_map(|p| p.as_str()).map(PathBuf::from)
        .filter(|p| !same_path(p, &sites_dir) && p.is_dir())
        .collect();
    let links = std::fs::read_dir(&sites_dir).into_iter().flatten().flatten()
        .filter_map(|e| {
            let target = std::fs::read_link(e.path()).ok().or_else(|| config::canonical(e.path()).ok())?;
            Some((config::site_name(&e.file_name().to_string_lossy()), config::plain_path(&target)))
        })
        .collect();
    let (mut secured, mut php) = (vec![], vec![]);
    for e in std::fs::read_dir(valet().join("Nginx")).into_iter().flatten().flatten() {
        let file = e.file_name().to_string_lossy().to_string();
        let Some(site) = file.strip_suffix(&format!(".{tld}.conf")).map(config::site_name) else { continue };
        let conf = std::fs::read_to_string(e.path()).unwrap_or_default();
        if conf.lines().any(|l| l.trim_start().starts_with("listen") && l.contains(":443 ssl")) {
            secured.push(site.clone());
        }
        if let Some(v) = conf.lines().find_map(|l| l.trim().strip_prefix("# ISOLATED_PHP_VERSION=")) {
            php.push((site, v.trim().trim_start_matches("php").to_string()));
        }
    }
    let proxies = std::fs::read_dir(valet().join("Proxies")).into_iter().flatten().flatten()
        .map(|e| e.file_name().to_string_lossy().trim_end_matches(".conf").to_string()).collect();
    Some(Herd { tld, parked, links, secured, php, proxies })
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().trim_end_matches('\\').eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
}

/// Ce que l'import ferait, avec ce qui est déjà en place.
pub fn plan(g: &Global) -> Plan {
    let Some(h) = read_herd() else { return Plan::default() };
    let sites = config::discover_sites(g);
    let mut p = Plan { found: true, tld: h.tld.clone(), ..Default::default() };
    p.parked = h.parked.iter().map(|d| Entry {
        id: d.display().to_string(), label: d.display().to_string(), detail: "chaque sous-dossier devient un site".into(),
        done: g.parked.iter().any(|x| same_path(x, d)),
    }).collect();
    p.links = h.links.iter().map(|(n, d)| Entry {
        id: n.clone(), label: format!("{n}.{}", g.tld), detail: d.display().to_string(),
        done: sites.get(n).is_some_and(|s| same_path(&s.path, d)),
    }).collect();
    p.secured = h.secured.iter().map(|n| Entry {
        id: n.clone(), label: format!("{n}.{}", g.tld), detail: "HTTPS".into(), done: g.secured.contains(n),
    }).collect();
    p.php = h.php.iter().map(|(n, v)| Entry {
        id: n.clone(), label: format!("{n}.{}", g.tld), detail: format!("PHP {v}"),
        done: sites.get(n).is_some_and(|s| s.project.php.as_deref() == Some(v.as_str())),
    }).collect();
    if (g.http_port, g.https_port) != (80, 443) {
        p.ports = Some((g.http_port, g.https_port));
    }
    // .env : calculés comme si tout était importé
    let all = Choice {
        parked: p.parked.iter().map(|e| e.id.clone()).collect(),
        links: p.links.iter().map(|e| e.id.clone()).collect(),
        secured: p.secured.iter().map(|e| e.id.clone()).collect(),
        ports: p.ports.is_some(),
        ..Default::default()
    };
    let projected = merged(g, &h, &all);
    for s in config::discover_sites(&projected).values() {
        let fix = env_fix(&projected, &s.name, &s.path);
        if !fix.changes.is_empty() || !fix.warnings.is_empty() {
            p.env.push(fix);
        }
    }
    if h.tld != g.tld {
        p.notes.push(format!("Herd utilise .{} et Viherd .{} : les adresses changent.", h.tld, g.tld));
    }
    for n in &h.proxies {
        p.notes.push(format!("Proxy Herd « {n} » non importé : crée un dossier avec `proxy = <port>` dans viherd.toml."));
    }
    p
}

/// La configuration globale une fois les éléments cochés ajoutés.
fn merged(g: &Global, h: &Herd, c: &Choice) -> Global {
    let mut g = g.clone();
    for d in h.parked.iter().filter(|d| c.parked.contains(&d.display().to_string())) {
        if !g.parked.iter().any(|x| same_path(x, d)) { g.parked.push(d.clone()); }
    }
    for (n, d) in h.links.iter().filter(|(n, _)| c.links.contains(n)) {
        g.links.insert(n.clone(), d.clone());
    }
    for n in h.secured.iter().filter(|n| c.secured.contains(n)) {
        g.secured.insert(n.clone());
    }
    if c.ports {
        (g.http_port, g.https_port) = (80, 443);
    }
    g
}

/// Applique les éléments cochés.
pub fn apply(c: &Choice) -> Result<Report> {
    let h = read_herd().ok_or_else(|| anyhow::anyhow!("configuration de Herd introuvable dans {}", config_dir().display()))?;
    let old = Global::load()?;
    let mut g = merged(&old, &h, c);
    g.herd_imported = true;
    g.save()?;
    let mut r = Report {
        parked: g.parked.len() - old.parked.len(),
        links: c.links.iter().filter(|n| h.links.iter().any(|(x, _)| x == *n)).count(),
        secured: g.secured.len() - old.secured.len(),
        ..Default::default()
    };
    if c.ports && (old.http_port, old.https_port) != (80, 443) {
        r.notes.push("Ports 80 et 443 : quitte Herd puis redémarre Viherd pour les utiliser.".into());
    }
    let sites = config::discover_sites(&g);
    for (n, v) in h.php.iter().filter(|(n, _)| c.php.contains(n)) {
        let Some(s) = sites.get(n) else { r.notes.push(format!("{n} : site introuvable, version de PHP non appliquée.")); continue };
        let mut p = Project::load(&s.path)?;
        if p.php.as_deref() != Some(v) {
            p.php = Some(v.clone());
            p.save(&s.path)?;
            r.php += 1;
        }
    }
    for n in &c.env {
        let Some(s) = sites.get(n) else { continue };
        let fix = env_fix(&g, &s.name, &s.path);
        if fix.changes.is_empty() { continue; }
        write_env(&s.path, &fix.changes)?;
        r.env.push(fix);
    }
    Ok(r)
}

/// Valeurs du .env d'un site à corriger pour Viherd.
pub fn env_fix(g: &Global, site: &str, dir: &Path) -> EnvFix {
    let file = dir.join(".env");
    let mut fix = EnvFix { site: site.into(), file: file.display().to_string(), changes: vec![], warnings: vec![] };
    let Ok(text) = std::fs::read_to_string(&file) else { return fix };
    let host = format!("{site}.{}", g.tld);
    let mut tls: Option<(PathBuf, PathBuf)> = None;
    for (k, v) in text.lines().filter_map(parse_line) {
        let herd_path = { let l = v.to_lowercase().replace('\\', "/"); l.contains(".config/herd/") };
        match k.as_str() {
            "APP_URL" => {
                let target = crate::ui::site_url(g, site);
                let url_host = v.split("://").nth(1).unwrap_or("").split(['/', ':']).next().unwrap_or("").to_lowercase();
                if url_host == host && v.trim_end_matches('/') != target {
                    fix.changes.push(Change { key: k, from: v, to: target });
                }
            }
            "REVERB_TLS_CERT" | "REVERB_TLS_KEY" if herd_path => {
                // Reverb fait son propre TLS : on lui donne un certificat signé par l'autorité de Viherd
                let (crt, key) = match &tls {
                    Some(x) => x.clone(),
                    None => {
                        let x = (crate::certs::exports_dir().join(format!("{host}.crt")), crate::certs::exports_dir().join(format!("{host}.key")));
                        tls = Some(x.clone());
                        x
                    }
                };
                let to = if k == "REVERB_TLS_CERT" { crt } else { key }.display().to_string().replace('\\', "/");
                fix.changes.push(Change { key: k, from: v, to });
            }
            _ if herd_path => fix.warnings.push(format!("{k} pointe vers Herd ({v})")),
            _ => {}
        }
    }
    fix
}

/// `KEY=valeur` d'une ligne de .env (guillemets retirés), None pour un commentaire.
fn parse_line(line: &str) -> Option<(String, String)> {
    let l = line.trim();
    if l.starts_with('#') { return None; }
    let (k, v) = l.split_once('=')?;
    let v = v.trim();
    let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))).unwrap_or(v);
    Some((k.trim().to_string(), v.to_string()))
}

/// Réécrit les clés modifiées (copie de l'original dans .env.viherd-backup la première fois).
fn write_env(dir: &Path, changes: &[Change]) -> Result<()> {
    let file = dir.join(".env");
    let text = std::fs::read_to_string(&file)?;
    let backup = dir.join(".env.viherd-backup");
    if !backup.exists() {
        std::fs::write(&backup, &text)?;
    }
    // le fichier visé (…/certs/sites/<hôte>.crt ou .key) doit exister avant que Reverb ne le lise
    if let Some(host) = changes.iter().find(|c| c.key.starts_with("REVERB_TLS_")).and_then(|c| Path::new(&c.to).file_stem().map(|s| s.to_string_lossy().to_string())) {
        crate::certs::export_site(&host)?;
    }
    let out = rewrite(&text, changes);
    std::fs::write(&file, out)?;
    Ok(())
}

fn rewrite(text: &str, changes: &[Change]) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(|line| {
        match parse_line(line).and_then(|(k, _)| changes.iter().find(|c| c.key == k)) {
            Some(c) => {
                let quoted = line.split_once('=').is_some_and(|(_, v)| v.trim().starts_with('"'));
                if quoted { format!("{}=\"{}\"", c.key, c.to) } else { format!("{}={}", c.key, c.to) }
            }
            None => line.to_string(),
        }
    }).collect();
    if text.ends_with('\n') { lines.push(String::new()); }
    lines.join(eol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_fix_targets_this_site_only() {
        let dir = std::env::temp_dir().join(format!("viherd-herd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "APP_URL=http://shop.test\r\nREVERB_TLS_CERT=\"C:/Users/x/.config/herd/config/valet/Certificates/shop.test.crt\"\r\nVIEW_PATH=C:\\Users\\x\\.config\\herd\\bin\r\n").unwrap();
        let mut g = Global { http_port: 80, https_port: 443, ..Default::default() };
        g.secured.insert("shop".into());
        let f = env_fix(&g, "shop", &dir);
        assert_eq!(f.changes[0].to, "https://shop.test");
        assert!(f.changes[1].to.ends_with("certs/sites/shop.test.crt"));
        assert_eq!(f.warnings.len(), 1);
        // une APP_URL vers un autre hôte (localhost:8000) n'est pas touchée
        std::fs::write(dir.join(".env"), "APP_URL=http://localhost:8000\n").unwrap();
        assert!(env_fix(&g, "shop", &dir).changes.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rewrite_keeps_quotes_and_line_endings() {
        let c = [Change { key: "APP_URL".into(), from: String::new(), to: "https://a.test".into() }];
        assert_eq!(rewrite("A=1\r\nAPP_URL=\"http://a.test\"\r\n", &c), "A=1\r\nAPP_URL=\"https://a.test\"\r\n");
        assert_eq!(rewrite("APP_URL=http://a.test", &c), "APP_URL=https://a.test");
    }
}
