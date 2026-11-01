//! Isolation par projet : chaque projet a son propre PATH avec SA version de PHP et de Node.
use crate::config::{Global, Project};
use crate::paths;
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

pub fn project_env(g: &Global, root: &Path, project: &Project) -> Vec<(String, String)> {
    let php = paths::php_dir(project.php.as_deref().unwrap_or(&g.default_php));
    let node = paths::node_dir(project.node.as_deref().unwrap_or(&g.default_node));
    let sys = std::env::var("PATH").unwrap_or_default();
    let path = format!("{};{};{};{sys}", php.display(), node.display(), paths::composer_dir().display());
    let mut v = vec![
        ("PATH".to_string(), path),
        ("PHPRC".to_string(), php.display().to_string()),
        ("VIHERD_PROJECT".to_string(), root.display().to_string()),
    ];
    if let Some(d) = dump_target(g) {
        v.push(("VAR_DUMPER_FORMAT".to_string(), d));
    }
    if let Some(dir) = project_ini_dir(root, project, &php, &crate::profiler::dir(root)) {
        // php-cgi comme php CLI lisent les .ini de ce dossier après php.ini
        v.push(("PHP_INI_SCAN_DIR".to_string(), dir.display().to_string()));
    }
    v.extend(project.env.iter().map(|(k, val)| (k.clone(), val.clone())));
    v
}

/// Contenu du .ini propre au projet : Xdebug (débogage, profilage) et réglages `[php_ini]` du viherd.toml.
pub fn project_ini(project: &Project, php_dir: &Path, profiles: &Path) -> String {
    let mut out = String::from("; généré par Viherd depuis viherd.toml — ne pas modifier\r\n");
    let dll = php_dir.join("ext").join("php_xdebug.dll");
    if project.needs_xdebug() && dll.is_file() {
        let mut modes = vec![];
        if project.xdebug { modes.extend(["debug", "develop"]); }
        if project.profile { modes.push("profile"); }
        let mut lines = vec![
            format!("zend_extension=\"{}\"", dll.display()),
            format!("xdebug.mode={}", modes.join(",")),
            // avec le profiler, tout passe par un déclencheur (?XDEBUG_PROFILE=1, ?XDEBUG_SESSION=1)
            format!("xdebug.start_with_request={}", if project.profile { "trigger" } else { "yes" }),
            "xdebug.client_host=127.0.0.1".into(),
            "xdebug.client_port=9003".into(),
            // pas d'avertissement à chaque requête quand aucun éditeur n'écoute
            "xdebug.log_level=0".into(),
        ];
        if project.profile {
            lines.push(format!("xdebug.output_dir=\"{}\"", profiles.display()));
            lines.push("xdebug.profiler_output_name=cachegrind.out.%t.%R".into());
            lines.push("xdebug.use_compression=0".into());
        }
        for l in lines {
            out.push_str(&l);
            out.push_str("\r\n");
        }
    }
    for (k, val) in &project.php_ini {
        let val = if val.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c)) { val.clone() } else { format!("\"{}\"", val.replace('"', "")) };
        out.push_str(&format!("{}={val}\r\n", k.trim()));
    }
    out
}

/// Écrit le .ini du projet dans ~/.viherd/ini/<projet>/ et renvoie ce dossier (None s'il n'y a rien à régler).
fn project_ini_dir(root: &Path, project: &Project, php_dir: &Path, profiles: &Path) -> Option<PathBuf> {
    if !project.needs_xdebug() && project.php_ini.is_empty() {
        return None;
    }
    if project.profile {
        let _ = std::fs::create_dir_all(profiles); // Xdebug n'écrit pas dans un dossier absent
    }
    let dir = paths::home().join("ini").join(project_id(root));
    let file = dir.join("99-viherd.ini");
    let content = project_ini(project, php_dir, profiles);
    if std::fs::read_to_string(&file).ok().as_deref() != Some(content.as_str()) {
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, &content)) {
            tracing::warn!("{}: {e}", file.display());
            return None;
        }
    }
    Some(dir)
}

/// Identifiant stable d'un projet pour ses fichiers générés : <dossier>-<4 octets du hash du chemin>.
pub fn project_id(root: &Path) -> String {
    let id = ring::digest::digest(&ring::digest::SHA256, root.display().to_string().to_lowercase().as_bytes());
    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    format!("{name}-{}", id.as_ref()[..4].iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Symfony VarDumper (donc dump()/dd() de Laravel) envoie les dumps à ce serveur TCP au lieu de les afficher.
/// S'il ne répond pas (démon arrêté), VarDumper retombe tout seul sur l'affichage normal.
pub fn dump_target(g: &Global) -> Option<String> {
    g.dumps.then(|| format!("tcp://127.0.0.1:{}", g.dump_port))
}

/// Cherche un exécutable dans un PATH donné (et non celui du processus courant).
pub fn resolve(program: &str, path: &str) -> Option<PathBuf> {
    for d in path.split(';').filter(|d| !d.is_empty()) {
        for ext in [".exe", ".cmd", ".bat", ""] {
            let p = Path::new(d).join(format!("{program}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Découpe une commande en respectant les guillemets : `php artisan x --name="a b"`.
pub fn split_command(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has = true;
            }
            None if c.is_whitespace() => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

pub fn build_command(program: &str, args: &[String], cwd: &Path, env: &[(String, String)]) -> Result<std::process::Command> {
    let path = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()).unwrap_or("");
    let exe = resolve(program, path)
        .ok_or_else(|| anyhow!("`{program}` introuvable (essaie `viherd install ...`)"))?;
    let ext = exe.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut cmd = if ext == "cmd" || ext == "bat" {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(&exe);
        c
    } else {
        std::process::Command::new(&exe)
    };
    cmd.args(args).current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands_with_quotes() {
        assert_eq!(split_command(r#"php artisan x --name="a b" 'c d'"#), ["php", "artisan", "x", "--name=a b", "c d"]);
        assert!(split_command("   ").is_empty());
    }

    #[test]
    fn project_ini_lists_settings_and_quotes_values() {
        let mut p = Project::default();
        p.php_ini.insert("memory_limit".into(), "1G".into());
        p.php_ini.insert("date.timezone".into(), "Europe/Paris".into());
        p.php_ini.insert("error_log".into(), r"C:\logs\php err.log".into());
        let none = Path::new(r"C:\nowhere");
        let ini = project_ini(&p, none, none);
        assert!(ini.contains("memory_limit=1G\r\n"));
        assert!(ini.contains("date.timezone=Europe/Paris\r\n"));
        assert!(ini.contains(r#"error_log="C:\logs\php err.log""#));
        // Xdebug demandé mais DLL absente : pas de zend_extension cassée
        p.xdebug = true;
        assert!(!project_ini(&p, none, none).contains("zend_extension"));
    }

    #[test]
    fn profiler_switches_xdebug_to_trigger_mode() {
        let php = std::env::temp_dir().join(format!("viherd-php-{}", std::process::id()));
        std::fs::create_dir_all(php.join("ext")).unwrap();
        std::fs::write(php.join("ext").join("php_xdebug.dll"), b"").unwrap();
        let p = Project { xdebug: true, profile: true, ..Default::default() };
        let ini = project_ini(&p, &php, Path::new(r"C:\profiles\app"));
        assert!(ini.contains("xdebug.mode=debug,develop,profile\r\n"));
        assert!(ini.contains("xdebug.start_with_request=trigger\r\n"));
        assert!(ini.contains(r#"xdebug.output_dir="C:\profiles\app""#));
        let only_debug = project_ini(&Project { xdebug: true, ..Default::default() }, &php, Path::new("x"));
        assert!(only_debug.contains("start_with_request=yes") && !only_debug.contains("output_dir"));
        let _ = std::fs::remove_dir_all(php);
    }
}
