//! Profilage : le profiler d'Xdebug écrit un fichier cachegrind par requête déclenchée (?XDEBUG_PROFILE=1)
//! dans ~/.viherd/profiles/<projet>/ ; Viherd le résume (temps total, fonctions les plus coûteuses).
use crate::paths;
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Dossier des profils d'un projet (même identifiant que son .ini généré).
pub fn dir(root: &Path) -> PathBuf {
    paths::home().join("profiles").join(crate::env::project_id(root))
}

/// Profils du projet, du plus récent au plus ancien.
pub fn list(root: &Path) -> Result<Vec<Value>> {
    let mut v: Vec<(u64, Value)> = std::fs::read_dir(dir(root)).into_iter().flatten().flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("cachegrind.out."))
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let md = e.metadata().ok();
            let at = md.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            (at, json!({ "file": name, "request": request_of(&name), "size": md.map(|m| m.len()).unwrap_or(0), "created": at }))
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in v.iter().skip(KEEP) {
        let _ = std::fs::remove_file(dir(root).join(old["file"].as_str().unwrap_or("")));
    }
    Ok(v.into_iter().take(KEEP).map(|(_, x)| x).collect())
}

pub fn clear(root: &Path) -> Result<()> {
    let d = dir(root);
    if d.is_dir() {
        std::fs::remove_dir_all(&d)?;
    }
    Ok(())
}

/// cachegrind.out.<horodatage>.<REQUEST_URI avec / ? & → _> : l'adresse demandée, sans le déclencheur.
/// Approximatif : un « _ » du chemin d'origine redevient aussi « / ».
fn request_of(name: &str) -> String {
    let rest = name.trim_start_matches("cachegrind.out.");
    let mut uri = rest.split_once('.').map(|(_, r)| r).unwrap_or("").trim_end_matches(".gz").to_string();
    for trigger in ["XDEBUG_PROFILE", "XDEBUG_TRIGGER"] {
        if let Some(i) = uri.find(trigger) {
            let after = i + trigger.len();
            let end = uri[after..].find('_').map_or(uri.len(), |j| after + j);
            uri.replace_range(i.saturating_sub(1)..end, "");
        }
    }
    let uri = uri.replace('_', "/");
    if uri.is_empty() { "/".into() } else { uri }
}

/// Un profil pèse souvent plusieurs Mo : on ne garde que les plus récents.
const KEEP: usize = 30;

#[derive(Debug, Serialize, Clone, Default)]
pub struct Func {
    pub name: String,
    pub file: String,
    pub calls: u64,
    pub self_ms: f64,
    pub incl_ms: f64,
}

pub fn summary(root: &Path, file: &str) -> Result<Value> {
    if file.contains(['/', '\\']) || file.contains("..") || !file.starts_with("cachegrind.out.") {
        bail!("profil invalide");
    }
    let path = dir(root).join(file);
    let text = std::fs::read_to_string(&path)?;
    let (total_ms, cmd, mut funcs) = parse(&text);
    funcs.sort_by(|a, b| b.self_ms.total_cmp(&a.self_ms));
    funcs.truncate(40);
    Ok(json!({ "file": file, "request": request_of(file), "script": cmd, "total_ms": total_ms, "functions": funcs }))
}

/// Lit un fichier cachegrind d'Xdebug : coût propre (lignes sous fn=) et coût inclusif (+ lignes après calls=).
pub fn parse(text: &str) -> (f64, String, Vec<Func>) {
    // Xdebug 3 compte en dizaines de nanosecondes, Xdebug 2 en microsecondes
    let mut unit_ms = 0.001;
    let (mut files, mut names): (HashMap<String, String>, HashMap<String, String>) = Default::default();
    let mut funcs: HashMap<String, Func> = HashMap::new();
    let (mut cur, mut cur_file, mut callee) = (String::new(), String::new(), None::<String>);
    let mut after_calls = false;
    let (mut total, mut cmd) = (0.0, String::new());
    // « (12) nom » à la première apparition, « (12) » ensuite
    let resolve = |map: &mut HashMap<String, String>, v: &str| -> String {
        let v = v.trim();
        match v.strip_prefix('(').and_then(|r| r.split_once(')')) {
            Some((id, rest)) if !rest.trim().is_empty() => { map.insert(id.to_string(), rest.trim().to_string()); rest.trim().to_string() }
            Some((id, _)) => map.get(id).cloned().unwrap_or_default(),
            None => v.to_string(),
        }
    };
    for line in text.lines() {
        if let Some(e) = line.strip_prefix("events:") {
            unit_ms = if e.contains("(10ns)") { 0.00001 } else { 0.001 };
        } else if let Some(c) = line.strip_prefix("cmd:") {
            cmd = c.trim().to_string();
        } else if let Some(s) = line.strip_prefix("summary:") {
            total = s.split_whitespace().next().and_then(|n| n.parse::<f64>().ok()).unwrap_or(0.0) * unit_ms;
        } else if let Some(f) = line.strip_prefix("fl=") {
            cur_file = resolve(&mut files, f);
        } else if let Some(f) = line.strip_prefix("fn=") {
            cur = resolve(&mut names, f);
            let e = funcs.entry(cur.clone()).or_insert_with(|| Func { name: cur.clone(), ..Default::default() });
            if e.file.is_empty() { e.file = cur_file.clone(); }
        } else if line.starts_with("cfl=") || line.starts_with("cfi=") {
            let _ = resolve(&mut files, &line[4..]);
        } else if let Some(f) = line.strip_prefix("cfn=") {
            callee = Some(resolve(&mut names, f));
        } else if let Some(c) = line.strip_prefix("calls=") {
            let n: u64 = c.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(1);
            if let Some(name) = &callee {
                funcs.entry(name.clone()).or_insert_with(|| Func { name: name.clone(), ..Default::default() }).calls += n;
            }
            after_calls = true;
        } else if line.starts_with(|c: char| c.is_ascii_digit() || c == '+' || c == '-' || c == '*') && !cur.is_empty() {
            let cost = line.split_whitespace().nth(1).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) * unit_ms;
            let f = funcs.get_mut(&cur).unwrap();
            if after_calls {
                f.incl_ms += cost;
                after_calls = false;
            } else {
                f.self_ms += cost;
                f.incl_ms += cost;
            }
        }
    }
    for f in funcs.values_mut() {
        if f.calls == 0 { f.calls = 1; } // {main} n'est appelé par personne
        f.self_ms = (f.self_ms * 1000.0).round() / 1000.0;
        f.incl_ms = (f.incl_ms * 1000.0).round() / 1000.0;
    }
    if total == 0.0 {
        total = funcs.values().map(|f| f.incl_ms).fold(0.0, f64::max);
    }
    ((total * 1000.0).round() / 1000.0, cmd, funcs.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "version: 1\ncreator: xdebug 3.5.3\ncmd: C:\\app\\public\\index.php\npart: 1\npositions: line\n\nevents: Time_(10ns) Memory_(bytes)\n\n\
fl=(1) C:\\app\\src\\Slow.php\nfn=(1) App\\Slow->run\n10 300000 0\n\n\
fl=(2) php:internal\nfn=(2) php::usleep\n1 100000 0\n\n\
fl=(1)\nfn=(1)\n12 50000 0\ncfl=(2)\ncfn=(2)\ncalls=2 0 0\n13 100000 0\n\n\
fl=(3) C:\\app\\public\\index.php\nfn=(3) {main}\n1 1000 0\ncfl=(1)\ncfn=(1)\ncalls=1 0 0\n5 450000 0\n\nsummary: 451000 2048\n";

    #[test]
    fn parses_self_and_inclusive_costs() {
        let (total, cmd, f) = parse(SAMPLE);
        let get = |n: &str| f.iter().find(|x| x.name == n).unwrap().clone();
        assert_eq!(total, 4.51);
        assert_eq!(cmd, r"C:\app\public\index.php");
        let run = get(r"App\Slow->run");
        assert_eq!((run.self_ms, run.incl_ms, run.calls), (3.5, 4.5, 1));
        assert_eq!(run.file, r"C:\app\src\Slow.php");
        assert_eq!(get("php::usleep").calls, 2);
        assert_eq!(get("{main}").incl_ms, 4.51);
    }

    #[test]
    fn request_comes_from_the_file_name() {
        assert_eq!(request_of("cachegrind.out.1793563000._orders_42"), "/orders/42");
        assert_eq!(request_of("cachegrind.out.1793563000."), "/");
        assert_eq!(request_of("cachegrind.out.1793564978._login_XDEBUG_PROFILE=1"), "/login");
        assert_eq!(request_of("cachegrind.out.1793564978._search_q=a_XDEBUG_PROFILE=1_page=2"), "/search/q=a/page=2");
        assert_eq!(request_of("cachegrind.out.1793564978._XDEBUG_PROFILE=1"), "/");
    }
}
