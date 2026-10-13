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
    v.extend(project.env.iter().map(|(k, val)| (k.clone(), val.clone())));
    v
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
