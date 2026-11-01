//! Installeur de Viherd : une fenêtre sans bordure dont toute l'interface est en HTML (ui.html),
//! affichée par WebView2 comme l'appli. Installation par utilisateur, sans droits administrateur :
//! %LOCALAPPDATA%\Programs\Viherd, PATH, raccourcis, démarrage auto, entrée « Applications installées ».
//! `viherd-setup.exe --uninstall` (copié dans le dossier d'installation) désinstalle.
#![windows_subsystem = "windows"]

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

const CLI_EXE: &[u8] = include_bytes!(env!("VIHERD_CLI_EXE"));
const APP_EXE: &[u8] = include_bytes!(env!("VIHERD_APP_EXE"));
const UI: &str = include_str!("ui.html");

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const UNINSTALL_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Viherd";

enum UserEvent {
    /// Message pour l'interface : onEvent(<json>)
    Ui(Value),
    Exit,
}

fn main() {
    let uninstall = std::env::args().any(|a| a == "--uninstall");
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let (rgba, w, h) = viherd::icon::rgba(32);
    let window = WindowBuilder::new()
        .with_title(if uninstall { "Désinstaller Viherd" } else { "Installer Viherd" })
        .with_inner_size(tao::dpi::LogicalSize::new(860.0, 540.0))
        .with_resizable(false)
        .with_decorations(false)
        .with_undecorated_shadow(true)
        .with_window_icon(tao::window::Icon::from_rgba(rgba, w, h).ok())
        .build(&event_loop)
        .expect("fenêtre");

    let init = json!({
        "mode": if uninstall { "uninstall" } else { "install" },
        "version": viherd::VERSION,
        "dir": installed_dir().unwrap_or_else(default_dir).display().to_string(),
        "existing": installed_dir().map(|d| d.display().to_string()),
        "data": viherd::paths::home().display().to_string(),
        // sans créer l'autorité juste pour regarder
        "trusted": viherd::certs::ca_cert_path().exists() && viherd::certs::is_trusted(),
    });
    let ipc_proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(UI)
        .with_background_color((11, 13, 12, 255)) // pas de flash blanc avant le premier rendu
        .with_initialization_script(&format!("window.SETUP = {init};"))
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            let msg: Value = serde_json::from_str(req.body()).unwrap_or(Value::Null);
            handle_ipc(msg, ipc_proxy.clone());
        })
        .build(&window)
        .expect("WebView2 introuvable : installe « Microsoft Edge WebView2 Runtime »");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } | Event::UserEvent(UserEvent::Exit) => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::Ui(v)) => match v["window"].as_str() {
                Some("drag") => { let _ = window.drag_window(); }
                Some("minimize") => window.set_minimized(true),
                _ => { let _ = webview.evaluate_script(&format!("onEvent({v})")); }
            },
            _ => {}
        }
    });
}

/// Messages de l'interface (window.ipc.postMessage).
fn handle_ipc(msg: Value, proxy: EventLoopProxy<UserEvent>) {
    let exit = proxy.clone();
    let send = move |v: Value| { let _ = proxy.send_event(UserEvent::Ui(v)); };
    match msg["cmd"].as_str().unwrap_or("") {
        "drag" => send(json!({ "window": "drag" })),
        "minimize" => send(json!({ "window": "minimize" })),
        "close" => { let _ = exit.send_event(UserEvent::Exit); }
        "launch" => {
            let dir = PathBuf::from(msg["dir"].as_str().unwrap_or(""));
            let _ = Command::new(dir.join("viherd-app.exe")).spawn();
        }
        "install" | "uninstall" => {
            let opts = msg["opts"].clone();
            let uninstall = msg["cmd"] == "uninstall";
            std::thread::spawn(move || {
                let step = |id: &str, label: &str| send(json!({ "step": id, "label": label }));
                let r = if uninstall { do_uninstall(&opts, &step) } else { do_install(&opts, &step) };
                match r {
                    Ok(()) => send(json!({ "done": true })),
                    Err(e) => send(json!({ "error": format!("{e:#}") })),
                }
            });
        }
        _ => {}
    }
}

fn default_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| viherd::paths::home());
    base.join("Programs").join("Viherd")
}

fn reg(args: &[&str]) -> Result<String> {
    let o = Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).output()?;
    if !o.status.success() {
        bail!("reg {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Dossier d'une installation existante (clé de désinstallation).
fn installed_dir() -> Option<PathBuf> {
    let out = reg(&["query", UNINSTALL_KEY, "/v", "InstallLocation"]).ok()?;
    let line = out.lines().find(|l| l.contains("InstallLocation"))?;
    let v = line.split("REG_SZ").nth(1)?.trim();
    (!v.is_empty()).then(|| PathBuf::from(v))
}

fn powershell(script: &str) -> Result<()> {
    let o = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW).output()?;
    if !o.status.success() {
        bail!("{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

fn ps_quote(s: &str) -> String { format!("'{}'", s.replace('\'', "''")) }

fn shortcuts() -> Vec<PathBuf> {
    let mut v = vec![];
    if let Some(d) = dirs_desktop() { v.push(d.join("Viherd.lnk")); }
    if let Ok(a) = std::env::var("APPDATA") {
        v.push(PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs\Viherd.lnk"));
    }
    v
}

fn dirs_desktop() -> Option<PathBuf> {
    std::env::var("USERPROFILE").ok().map(|p| PathBuf::from(p).join("Desktop"))
}

fn make_shortcut(lnk: &Path, target: &Path) -> Result<()> {
    if let Some(p) = lnk.parent() { std::fs::create_dir_all(p)?; }
    powershell(&format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({});$s.TargetPath={};$s.WorkingDirectory={};$s.Description='Viherd';$s.Save()",
        ps_quote(&lnk.display().to_string()), ps_quote(&target.display().to_string()),
        ps_quote(&target.parent().unwrap().display().to_string())
    ))
}

/// Ajoute ou retire un dossier du PATH de l'utilisateur (prévient Windows du changement).
fn user_path(dir: &Path, add: bool) -> Result<()> {
    let d = ps_quote(&dir.display().to_string());
    powershell(&format!(
        "$d={d};$p=[Environment]::GetEnvironmentVariable('Path','User');$l=@(($p -split ';') | Where-Object {{ $_ -and $_.TrimEnd('\\') -ne $d.TrimEnd('\\') }});\
         if ({}) {{ $l += $d }};[Environment]::SetEnvironmentVariable('Path',($l -join ';'),'User')",
        if add { "$true" } else { "$false" }
    ))
}

/// Arrête proprement un Viherd en cours (sinon ses fichiers sont verrouillés).
fn stop_running() {
    let port = viherd::config::Global::load().map(|g| g.api_port).unwrap_or(7777);
    if let Ok(mut s) = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)) {
        let _ = s.write_all(b"POST /api/quit HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Viherd: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = s.read(&mut [0; 256]);
    }
    let running = || {
        Command::new("tasklist").args(["/FI", "IMAGENAME eq viherd-app.exe", "/NH"]).creation_flags(CREATE_NO_WINDOW)
            .output().map(|o| String::from_utf8_lossy(&o.stdout).contains("viherd-app.exe")).unwrap_or(false)
    };
    let t0 = Instant::now();
    while running() && t0.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(300));
    }
    if running() {
        let _ = Command::new("taskkill").args(["/IM", "viherd-app.exe", "/T", "/F"]).creation_flags(CREATE_NO_WINDOW).status();
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn opt(opts: &Value, k: &str) -> bool { opts[k].as_bool().unwrap_or(false) }

fn do_install(opts: &Value, step: &impl Fn(&str, &str)) -> Result<()> {
    let dir = PathBuf::from(opts["dir"].as_str().unwrap_or("").trim());
    if !dir.is_absolute() { bail!("choisis un dossier d'installation complet (ex : C:\\Programmes\\Viherd)"); }

    step("stop", "Arrêt de Viherd s'il tourne");
    stop_running();

    step("files", "Copie des fichiers");
    std::fs::create_dir_all(&dir).with_context(|| format!("impossible de créer {}", dir.display()))?;
    let app = dir.join("viherd-app.exe");
    std::fs::write(dir.join("viherd.exe"), CLI_EXE).context("viherd.exe est verrouillé (un terminal l'utilise ?)")?;
    std::fs::write(&app, APP_EXE).context("viherd-app.exe est verrouillé")?;
    let me = std::env::current_exe()?;
    let setup = dir.join("viherd-setup.exe");
    if !same_file(&me, &setup) {
        std::fs::copy(&me, &setup)?;
    }

    if opt(opts, "path") {
        step("path", "Ajout de viherd au PATH");
        user_path(&dir, true)?;
    }
    step("shortcuts", "Raccourcis");
    let [desktop, start] = &shortcuts()[..] else { bail!("dossiers Bureau / Menu Démarrer introuvables") };
    make_shortcut(start, &app)?;
    if opt(opts, "desktop") { make_shortcut(desktop, &app)?; } else { let _ = std::fs::remove_file(desktop); }

    if opt(opts, "autostart") {
        reg(&["add", RUN_KEY, "/v", "Viherd", "/t", "REG_SZ", "/d", &format!("\"{}\"", app.display()), "/f"])?;
    } else {
        let _ = reg(&["delete", RUN_KEY, "/v", "Viherd", "/f"]);
    }

    step("register", "Inscription dans « Applications installées »");
    let size_kb = ((CLI_EXE.len() + APP_EXE.len()) / 1024 + std::fs::metadata(&setup).map(|m| m.len() as usize / 1024).unwrap_or(0)).to_string();
    for (name, kind, val) in [
        ("DisplayName", "REG_SZ", "Viherd".to_string()),
        ("DisplayVersion", "REG_SZ", viherd::VERSION.to_string()),
        ("Publisher", "REG_SZ", "Viherd".to_string()),
        ("InstallLocation", "REG_SZ", dir.display().to_string()),
        ("DisplayIcon", "REG_SZ", app.display().to_string()),
        ("UninstallString", "REG_SZ", format!("\"{}\" --uninstall", setup.display())),
        ("NoModify", "REG_DWORD", "1".into()),
        ("NoRepair", "REG_DWORD", "1".into()),
        ("EstimatedSize", "REG_DWORD", size_kb.clone()),
    ] {
        reg(&["add", UNINSTALL_KEY, "/v", name, "/t", kind, "/d", &val, "/f"])?;
    }

    // Déjà approuvée : pas de nouvelle fenêtre Windows. Refusée : l'installation reste réussie.
    if opt(opts, "https") && !viherd::certs::is_trusted() {
        step("https", "Approbation de l'autorité HTTPS (confirme dans la fenêtre Windows)");
        if viherd::certs::trust().is_err() {
            step("https-skip", "HTTPS local non approuvé : lance « viherd trust » quand tu veux");
        }
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) { (Ok(x), Ok(y)) => x == y, _ => false }
}

fn do_uninstall(opts: &Value, step: &impl Fn(&str, &str)) -> Result<()> {
    let dir = installed_dir().or_else(|| std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)))
        .context("installation introuvable")?;

    step("stop", "Arrêt de Viherd");
    stop_running();

    step("shortcuts", "Raccourcis et démarrage automatique");
    for l in shortcuts() { let _ = std::fs::remove_file(l); }
    let _ = reg(&["delete", RUN_KEY, "/v", "Viherd", "/f"]);

    step("path", "Nettoyage du PATH");
    user_path(&dir, false)?;

    if viherd::certs::ca_cert_path().exists() {
        step("https", "Retrait de l'autorité HTTPS");
        let _ = viherd::certs::untrust();
    }
    if opt(opts, "data") {
        step("data", "Suppression des données (~/.viherd)");
        std::fs::remove_dir_all(viherd::paths::home()).context("des fichiers de ~/.viherd sont encore utilisés")?;
    }

    step("files", "Suppression des fichiers");
    let _ = reg(&["delete", UNINSTALL_KEY, "/f"]);
    for f in ["viherd.exe", "viherd-app.exe"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    // l'installeur ne peut pas s'effacer lui-même : cmd s'en charge juste après sa fermeture,
    // et ne retire le dossier que s'il est vide (jamais de suppression récursive d'un dossier choisi à la main)
    let _ = Command::new("cmd")
        .args(["/C", &format!("ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"", dir.join("viherd-setup.exe").display(), dir.display())])
        .creation_flags(CREATE_NO_WINDOW).spawn();
    Ok(())
}
