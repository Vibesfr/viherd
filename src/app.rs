//! Appli de bureau : fenêtre (WebView2), icône dans la barre des tâches et démon,
//! le tout dans UN seul processus. Fermer la fenêtre la cache ; « Quitter » arrête tout.
#![windows_subsystem = "windows"]

use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use viherd::config::Global;
use viherd::{daemon, paths};
use wry::WebViewBuilder;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

enum UserEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    DaemonFailed(String),
    /// Le démon s'est arrêté proprement (ex : /api/quit) : on ferme l'appli.
    DaemonStopped,
}

fn main() {
    // Logs dans ~/.viherd/logs/viherd.log (pas de console en mode fenêtre)
    let _ = std::fs::create_dir_all(paths::logs());
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(paths::logs().join("viherd.log")) {
        let _ = tracing_subscriber::fmt().with_target(false).with_ansi(false).with_writer(Mutex::new(f)).try_init();
    }

    let g = Global::load().unwrap_or_default();
    let url = format!("http://127.0.0.1:{}/", g.api_port);
    let api_up = |timeout: Duration| TcpStream::connect_timeout(&([127, 0, 0, 1], g.api_port).into(), timeout).is_ok();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Si un démon tourne déjà (ex: `viherd start` dans un terminal), on ne fait qu'afficher son interface.
    let external = api_up(Duration::from_millis(300));
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon_thread = (!external).then(|| {
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("runtime tokio");
            let r = rt.block_on(daemon::run_until(async {
                let _ = shutdown_rx.await;
            }));
            match r {
                Err(e) => {
                    tracing::error!("{e:#}");
                    let _ = proxy.send_event(UserEvent::DaemonFailed(format!("{e:#}")));
                }
                Ok(()) => {
                    let _ = proxy.send_event(UserEvent::DaemonStopped);
                }
            }
        })
    });
    if !external {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) && !api_up(Duration::from_millis(100)) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let (rgba, w, h) = make_icon();
    let window = WindowBuilder::new()
        .with_title("Viherd")
        .with_inner_size(tao::dpi::LogicalSize::new(1000.0, 680.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(720.0, 480.0))
        .with_window_icon(tao::window::Icon::from_rgba(rgba.clone(), w, h).ok())
        .build(&event_loop)
        .expect("fenêtre");
    let webview = WebViewBuilder::new().with_url(&url).build(&window).expect("WebView2 introuvable : installe « Microsoft Edge WebView2 Runtime »");

    // Barre des tâches
    let open = MenuItem::new("Ouvrir Viherd", true, None);
    let sites = MenuItem::new(format!("Sites (*.{}) dans le navigateur", g.tld), true, None);
    let logs = MenuItem::new("Ouvrir le dossier des logs", true, None);
    let autostart = CheckMenuItem::new("Lancer au démarrage de Windows", true, autostart_enabled(), None);
    let quit = MenuItem::new(if external { "Fermer la fenêtre" } else { "Quitter (arrête les sites)" }, true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[&open, &sites, &logs, &PredefinedMenuItem::separator(), &autostart, &PredefinedMenuItem::separator(), &quit]);
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(if external { "Viherd (démon externe)" } else { "Viherd" })
        .with_icon(tray_icon::Icon::from_rgba(rgba, w, h).expect("icône"))
        .build()
        .expect("tray");
    {
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| { let _ = p.send_event(UserEvent::Tray(e)); }));
        MenuEvent::set_event_handler(Some(move |e| { let _ = proxy.send_event(UserEvent::Menu(e)); }));
    }

    let mut shutdown_tx = Some(shutdown_tx);
    let mut daemon_thread = daemon_thread;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let show = |window: &tao::window::Window| {
            window.set_visible(true);
            window.set_minimized(false);
            window.set_focus();
        };
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => window.set_visible(false),
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }))
            | Event::UserEvent(UserEvent::Tray(TrayIconEvent::DoubleClick { .. })) => show(&window),
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == open.id() {
                    show(&window);
                } else if e.id == sites.id() {
                    let port = if g.http_port == 80 { String::new() } else { format!(":{}", g.http_port) };
                    open_external(&format!("http://viherd.{}{port}/", g.tld));
                } else if e.id == logs.id() {
                    open_external(&paths::logs().display().to_string());
                } else if e.id == autostart.id() {
                    set_autostart(autostart.is_checked());
                } else if e.id == quit.id() {
                    if let Some(tx) = shutdown_tx.take() {
                        let _ = tx.send(());
                    }
                    if let Some(t) = daemon_thread.take() {
                        let _ = t.join(); // laisse le temps d'arrêter php-cgi, workers et services
                    }
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::DaemonFailed(msg)) => {
                let _ = webview.load_html(&error_html(&msg, g.http_port));
                show(&window);
            }
            Event::UserEvent(UserEvent::DaemonStopped) => {
                if let Some(t) = daemon_thread.take() {
                    let _ = t.join();
                }
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

fn error_html(msg: &str, port: u16) -> String {
    let msg = msg.replace('&', "&amp;").replace('<', "&lt;");
    let hint = if msg.contains(&format!(":{port}:")) {
        format!(
            "<p>Le port <b>{port}</b> est déjà utilisé par un autre programme (souvent Laravel Herd, IIS ou Apache). \
             Quitte-le, ou change <code>http_port</code> dans <code>{}</code> puis relance Viherd.</p>",
            paths::config_file().display()
        )
    } else {
        String::new()
    };
    format!(
        "<!doctype html><meta charset=utf-8><body style=\"font-family:Segoe UI,system-ui;max-width:640px;margin:4rem auto;padding:0 1rem;color:#1c1c1a\">\
         <h1 style=\"color:#b3403a\">Le démon n'a pas pu démarrer</h1><pre style=\"white-space:pre-wrap;background:#f6f6f4;padding:1rem;border-radius:8px\">{msg}</pre>{hint}\
         <p style=\"color:#75746f\">Logs : {}</p></body>",
        paths::logs().join("viherd.log").display()
    )
}

fn open_external(target: &str) {
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", target]).creation_flags(CREATE_NO_WINDOW).spawn();
}

fn autostart_enabled() -> bool {
    std::process::Command::new("reg").args(["query", RUN_KEY, "/v", "Viherd"])
        .creation_flags(CREATE_NO_WINDOW).output().map(|o| o.status.success()).unwrap_or(false)
}

fn set_autostart(on: bool) {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let mut c = std::process::Command::new("reg");
    if on {
        c.args(["add", RUN_KEY, "/v", "Viherd", "/t", "REG_SZ", "/d", &format!("\"{exe}\""), "/f"]);
    } else {
        c.args(["delete", RUN_KEY, "/v", "Viherd", "/f"]);
    }
    let _ = c.creation_flags(CREATE_NO_WINDOW).status();
}

/// Icône 32×32 dessinée en code : pastille verte avec un « V » blanc.
fn make_icon() -> (Vec<u8>, u32, u32) {
    let n = 32u32;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
            let inside = fx * fx + fy * fy <= 15.5 * 15.5;
            // deux segments du V : (9,9)->(16,24) et (23,9)->(16,24)
            let v = seg_dist(fx + 16.0, fy + 16.0, 9.5, 9.0, 16.0, 24.0) < 2.2
                || seg_dist(fx + 16.0, fy + 16.0, 22.5, 9.0, 16.0, 24.0) < 2.2;
            let rgba = match (inside, v) {
                (false, _) => [0, 0, 0, 0],
                (true, true) => [255, 255, 255, 255],
                (true, false) => [47, 111, 78, 255],
            };
            px.extend_from_slice(&rgba);
        }
    }
    (px, n, n)
}

fn seg_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
}
