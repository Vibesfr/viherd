//! Serveur HTTP : routage *.test -> PHP (FastCGI) / fichiers statiques / proxy Node + WebSocket.
//! Plus une petite API de contrôle sur 127.0.0.1:7777 pour la CLI.
use crate::config::{Global, Site};
use crate::php_pool::PhpPool;
use crate::supervisor::Supervisor;
use crate::mail::MailBox;
use anyhow::{anyhow, Result};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HOST, UPGRADE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::net::{TcpListener, TcpStream};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = BoxBody<Bytes, BoxError>;

pub struct State {
    pub global: RwLock<Global>,
    pub sites: RwLock<BTreeMap<String, Site>>,
    pub php: PhpPool,
    pub sup: Supervisor,
    pub mail: Arc<MailBox>,
    pub started: std::time::Instant,
    pub metrics: Metrics,
    pub dumps: Arc<crate::dumps::DumpStore>,
    /// Workers arrêtés à la main depuis l'interface : un reload ne les relance pas.
    pub paused: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Demande d'arrêt propre venue de l'API (bouton Quitter, script dev.ps1)
    pub quit: tokio::sync::Notify,
}

pub(crate) fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|e| match e {}).boxed()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn page(code: u16, title: &str, body: &str) -> Response<Body> {
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font-family:system-ui;max-width:720px;margin:4rem auto;padding:0 1rem\">\
         <h1>{title}</h1>{body}<p style=\"color:#888\">viherd</p></body>"
    );
    Response::builder()
        .status(code)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .body(full(html))
        .unwrap()
}

pub async fn serve(addr: SocketAddr, st: Arc<State>, api: bool) -> Result<()> {
    let listener = TcpListener::bind(addr).await.map_err(|e| anyhow!("impossible d'écouter sur {addr}: {e}"))?;
    tracing::info!("{} sur http://{addr}", if api { "API" } else { "Proxy" });
    loop {
        let (stream, peer) = listener.accept().await?;
        let st = st.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let st = st.clone();
                async move {
                    let r = if api { api_handle(req, st).await } else { handle(req, st, peer).await };
                    Ok::<_, Infallible>(r.unwrap_or_else(|e| {
                        tracing::error!("{e:#}");
                        page(502, "Erreur", &format!("<pre>{}</pre>", html_escape(&format!("{e:#}"))))
                    }))
                }
            });
            if let Err(e) = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).with_upgrades().await {
                tracing::debug!("connexion: {e}");
            }
        });
    }
}

async fn handle(req: Request<Incoming>, st: Arc<State>, peer: SocketAddr) -> Result<Response<Body>> {
    let host = req.headers().get(HOST).and_then(|h| h.to_str().ok()).unwrap_or("")
        .split(':').next().unwrap_or("").to_lowercase();
    let g = st.global.read().unwrap().clone();
    let name = host.strip_suffix(&format!(".{}", g.tld)).unwrap_or(&host).to_string();
    let site = {
        let sites = st.sites.read().unwrap();
        // foo.test, et aussi api.foo.test -> foo
        sites.get(&name).cloned().or_else(|| name.rsplit('.').next().and_then(|n| sites.get(n).cloned()))
    };
    let Some(site) = site else { return Ok(index_page(&st, &g)) };

    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let t0 = std::time::Instant::now();
    let (kind, r) = if let Some(r) = site.project.routes.iter().find(|r| path.starts_with(&r.path)) {
        ("route", proxy(req, r.port).await)
    } else if let Some(port) = site.project.proxy {
        ("proxy", proxy(req, port).await)
    } else {
        ("php", php_or_static(req, &site, &g, peer, &st).await)
    };
    let status = match &r { Ok(resp) => resp.status().as_u16(), Err(_) => 502 };
    st.metrics.record(&site.name, kind, &method, &path, status, t0.elapsed());
    r
}

/// Compteurs par site pour l'interface (trafic en direct, erreurs, latence).
#[derive(Default)]
pub struct Metrics(std::sync::Mutex<BTreeMap<String, SiteMetrics>>);

#[derive(Default, Clone, serde::Serialize)]
pub struct SiteMetrics {
    pub requests: u64,
    pub errors: u64,
    pub last_status: u16,
    pub last_kind: String,
    pub last_ms: u128,
    pub last_at: u64,
    /// Dernière page demandée (les fichiers statiques comme /build/app.js sont ignorés)
    pub last_method: String,
    pub last_path: String,
    /// Dernière réponse 5xx : chemin, statut et horodatage
    pub last_err_path: String,
    pub last_err_status: u16,
    pub last_err_at: u64,
    /// Horodatages (secondes) des 60 dernières requêtes, pour une sparkline
    pub recent: Vec<u64>,
}

impl Metrics {
    pub fn record(&self, site: &str, kind: &str, method: &str, path: &str, status: u16, took: std::time::Duration) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut m = self.0.lock().unwrap();
        let e = m.entry(site.to_string()).or_default();
        e.requests += 1;
        let path: String = path.chars().take(200).collect();
        if status >= 500 {
            e.errors += 1;
            e.last_err_path = format!("{method} {path}");
            e.last_err_status = status;
            e.last_err_at = now;
        }
        let asset = path.rsplit('/').next().is_some_and(|f| f.contains('.') && !f.ends_with(".php"));
        if !asset || e.last_path.is_empty() || status >= 500 {
            e.last_method = method.to_string();
            e.last_path = path;
        }
        e.last_status = status;
        e.last_kind = kind.into();
        e.last_ms = took.as_millis();
        e.last_at = now;
        e.recent.push(now);
        if e.recent.len() > 60 { e.recent.remove(0); }
    }
    pub fn snapshot(&self) -> BTreeMap<String, SiteMetrics> {
        self.0.lock().unwrap().clone()
    }
}

fn index_page(st: &State, g: &Global) -> Response<Body> {
    let sites = st.sites.read().unwrap();
    let mut li = String::new();
    for (n, s) in sites.iter() {
        li += &format!(
            "<li><a href=\"http://{n}.{tld}\">{n}.{tld}</a> <small>{}</small></li>",
            html_escape(&s.path.display().to_string()), tld = g.tld
        );
    }
    page(404, "Sites viherd", &format!("<ul>{li}</ul>"))
}

pub(crate) fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn php_or_static(req: Request<Incoming>, site: &Site, g: &Global, peer: SocketAddr, st: &State) -> Result<Response<Body>> {
    let docroot = site.docroot();
    let (parts, body) = req.into_parts();
    let uri_path = pct_decode(parts.uri.path());
    let rel = uri_path.trim_start_matches('/').to_string();
    if rel.split('/').any(|s| s == "..") {
        return Ok(page(400, "Requête invalide", ""));
    }
    let file = docroot.join(rel.replace('/', "\\"));

    // Fichier statique
    if file.is_file() && file.extension().map_or(true, |e| e != "php") {
        let data = tokio::fs::read(&file).await?;
        let mime = mime_guess::from_path(&file).first_or_octet_stream();
        return Ok(Response::builder().header(CONTENT_TYPE, mime.as_ref()).body(full(data))?);
    }

    // Script PHP (front controller index.php par défaut, comme Laravel)
    let (script, script_name) = if file.is_file() {
        (file.clone(), format!("/{rel}"))
    } else if !rel.is_empty() && file.join("index.php").is_file() {
        (file.join("index.php"), format!("/{}/index.php", rel.trim_end_matches('/')))
    } else {
        (docroot.join("index.php"), "/index.php".to_string())
    };
    if !script.is_file() {
        return Ok(page(404, "Introuvable", &format!("<p>Aucun index.php dans {}</p>", html_escape(&docroot.display().to_string()))));
    }

    let body = body.collect().await?.to_bytes();
    let mut p: Vec<(String, String)> = vec![
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("SERVER_SOFTWARE".into(), "viherd".into()),
        ("SERVER_PROTOCOL".into(), "HTTP/1.1".into()),
        ("SERVER_NAME".into(), format!("{}.{}", site.name, g.tld)),
        ("SERVER_ADDR".into(), "127.0.0.1".into()),
        ("SERVER_PORT".into(), g.http_port.to_string()),
        ("REMOTE_ADDR".into(), peer.ip().to_string()),
        ("REMOTE_PORT".into(), peer.port().to_string()),
        ("REQUEST_METHOD".into(), parts.method.to_string()),
        ("REQUEST_URI".into(), parts.uri.path_and_query().map(|x| x.as_str()).unwrap_or("/").into()),
        ("QUERY_STRING".into(), parts.uri.query().unwrap_or("").into()),
        ("DOCUMENT_ROOT".into(), docroot.display().to_string()),
        ("SCRIPT_FILENAME".into(), script.display().to_string()),
        ("SCRIPT_NAME".into(), script_name.clone()),
        ("PHP_SELF".into(), script_name),
        ("REDIRECT_STATUS".into(), "200".into()),
        ("CONTENT_LENGTH".into(), body.len().to_string()),
    ];
    if let Some(ct) = parts.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        p.push(("CONTENT_TYPE".into(), ct.into()));
    }
    if let Some(d) = crate::env::dump_target(g) {
        p.push(("VAR_DUMPER_FORMAT".into(), d));
    }
    for (k, v) in parts.headers.iter() {
        let key = format!("HTTP_{}", k.as_str().to_uppercase().replace('-', "_"));
        if key != "HTTP_CONTENT_TYPE" && key != "HTTP_CONTENT_LENGTH" {
            p.push((key, v.to_str().unwrap_or("").into()));
        }
    }

    let r = st.php.request(site, g, &p, &body).await?;
    if !r.stderr.is_empty() {
        tracing::warn!("[{}] php: {}", site.name, r.stderr.trim());
    }
    let mut b = Response::builder().status(StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK));
    for (k, v) in &r.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    Ok(b.body(full(r.body))?)
}

/// Reverse proxy HTTP + WebSocket (Reverb, Vite HMR, app Node...).
async fn proxy(mut req: Request<Incoming>, port: u16) -> Result<Response<Body>> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await
        .map_err(|e| anyhow!("backend 127.0.0.1:{port} injoignable (le worker tourne ?) : {e}"))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let client_upgrade = if req.headers().contains_key(UPGRADE) { Some(hyper::upgrade::on(&mut req)) } else { None };
    let mut resp = sender.send_request(req).await?;
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(cu) = client_upgrade {
            let bu = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                if let (Ok(a), Ok(b)) = tokio::join!(cu, bu) {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b)).await;
                }
            });
        }
    }
    Ok(resp.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed()))
}

pub(crate) fn json(v: serde_json::Value) -> Result<Response<Body>> {
    Ok(Response::builder().header(CONTENT_TYPE, "application/json").body(full(serde_json::to_vec(&v)?))?)
}

async fn api_handle(req: Request<Incoming>, st: Arc<State>) -> Result<Response<Body>> {
    // Anti-CSRF : un site web ouvert dans le navigateur pourrait envoyer un POST vers 127.0.0.1.
    // Toute action exige l'en-tête X-Viherd (un en-tête personnalisé déclenche un pré-vol CORS
    // que nous ne validons jamais pour une autre origine) et une origine locale si elle est fournie.
    if req.method() != hyper::Method::GET {
        let has_header = req.headers().contains_key("x-viherd");
        let origin_ok = req.headers().get("origin").and_then(|o| o.to_str().ok())
            .map_or(true, |o| o.starts_with("http://127.0.0.1:") || o.starts_with("http://localhost:"));
        if !has_header || !origin_ok {
            return Ok(Response::builder().status(403).body(full("requête refusée"))?);
        }
    }
    if req.uri().path().starts_with("/api/") {
        return crate::ui::handle(req, st).await;
    }
    match (req.method().as_str(), req.uri().path()) {
        ("GET", "/") => Ok(Response::builder()
            .header(CONTENT_TYPE, "text/html; charset=utf-8")
            // l'interface est intégrée au binaire : jamais de version périmée en cache après une mise à jour
            .header("Cache-Control", "no-store")
            .body(full(ui_html()))?),
        ("GET", "/status") => {
            let sites = st.sites.read().unwrap().clone();
            json(serde_json::json!({ "sites": sites, "jobs": st.sup.list() }))
        }
        ("POST", "/reload") => {
            crate::daemon::reload(&st).await?;
            json(serde_json::json!({ "ok": true }))
        }
        ("GET", "/mail") => Ok(mail_page(&st)),
        ("GET", "/mail.json") => json(serde_json::to_value(st.mail.list())?),
        ("POST", "/mail/clear") => {
            st.mail.clear();
            Ok(Response::builder().status(303).header("Location", "/mail").body(full(""))?)
        }
        ("GET", p) if p.starts_with("/mail/") => match p[6..].parse().ok().and_then(|id| st.mail.get(id)) {
            Some(m) => Ok(Response::builder()
                .header(CONTENT_TYPE, "text/plain; charset=utf-8")
                .body(full(m.raw))?),
            None => Ok(page(404, "Mail introuvable", "")),
        },
        _ => Ok(page(404, "Not found", "")),
    }
}

fn mail_page(st: &State) -> Response<Body> {
    let mut rows = String::new();
    for m in st.mail.list() {
        rows += &format!(
            "<tr><td><a href=\"/mail/{}\">{}</a></td><td>{}</td><td>{}</td></tr>",
            m.id, html_escape(&m.subject), html_escape(&m.from), html_escape(&m.to.join(", "))
        );
    }
    page(200, "Mails capturés", &format!(
        "<form method=post action=/mail/clear><button>Tout vider</button></form>\
         <table cellpadding=6><tr><th>Sujet</th><th>De</th><th>À</th></tr>{rows}</table>"
    ))
}

/// Mode dev : `VIHERD_DEV=1` sert ui.html depuis le dossier source (relu à chaque requête),
/// et l'interface se recharge toute seule quand le fichier change. Sinon : version intégrée au binaire.
pub fn dev_ui_path() -> Option<std::path::PathBuf> {
    if std::env::var("VIHERD_DEV").map_or(true, |v| v.is_empty() || v == "0") {
        return None;
    }
    let p = std::env::var("VIHERD_UI").map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("ui.html"));
    p.exists().then_some(p)
}

fn ui_html() -> String {
    dev_ui_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_else(|| include_str!("ui.html").to_string())
}
