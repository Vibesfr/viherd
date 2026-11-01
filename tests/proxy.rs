//! Tests d'intégration du proxy : un vrai serveur Viherd sur un port libre, interrogé en HTTP brut
//! (pas de client qui normaliserait les chemins ou suivrait les redirections).
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use viherd::config::{Global, Project, Route, Site};
use viherd::server::{self, State};

fn temp_site(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("viherd-it-{name}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("public")).unwrap();
    std::fs::write(dir.join("public").join("hello.txt"), "bonjour").unwrap();
    std::fs::write(dir.join("public").join("index.php"), "<?php echo 'PHP ' . PHP_VERSION . ' ' . ($_SERVER['HTTPS'] ?? 'http') . ' ' . $_GET['q'];").unwrap();
    dir
}

async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port()
}

/// Démarre le proxy HTTP avec ces sites ; renvoie son adresse.
async fn start(g: Global, sites: Vec<Site>) -> (SocketAddr, Arc<State>) {
    let st = State::new(g);
    *st.sites.write().unwrap() = sites.into_iter().map(|s| (s.name.clone(), s)).collect();
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port().await));
    tokio::spawn(server::serve(addr, st.clone(), false));
    for _ in 0..50 {
        if TcpStream::connect(addr).await.is_ok() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (addr, st)
}

/// Requête GET brute ; renvoie (statut, en-têtes + corps).
async fn get(addr: SocketAddr, host: &str, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, text)
}

fn site(name: &str, path: PathBuf, project: Project) -> Site {
    Site { name: name.into(), path, project }
}

#[tokio::test]
async fn serves_static_files_by_host_and_subdomain() {
    let dir = temp_site("static");
    let (addr, _) = start(Global::default(), vec![site("demo", dir.clone(), Project::default())]).await;
    let (code, body) = get(addr, "demo.test", "/hello.txt").await;
    assert_eq!(code, 200);
    assert!(body.contains("content-type: text/plain") && body.ends_with("bonjour"), "{body}");
    // api.demo.test est servi par le site demo
    assert_eq!(get(addr, "api.demo.test", "/hello.txt").await.0, 200);
    // hôte inconnu : page listant les sites
    let (code, body) = get(addr, "inconnu.test", "/").await;
    assert_eq!(code, 404);
    assert!(body.contains("demo.test"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn refuses_path_traversal() {
    let dir = temp_site("traversal");
    let (addr, _) = start(Global::default(), vec![site("demo", dir.clone(), Project::default())]).await;
    assert_eq!(get(addr, "demo.test", "/../Cargo.toml").await.0, 400);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn secured_site_redirects_to_https() {
    let dir = temp_site("secure");
    let mut g = Global::default();
    g.secured.insert("demo".into());
    let (addr, _) = start(g, vec![site("demo", dir.clone(), Project::default())]).await;
    let (code, head) = get(addr, "demo.test", "/login?next=/a").await;
    assert_eq!(code, 307);
    assert!(head.to_lowercase().contains("location: https://demo.test/login?next=/a"), "{head}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn routes_are_proxied_to_their_port() {
    // backend minimal : répond toujours la même chose
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = backend.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = c.read(&mut buf).await.unwrap_or(0);
                let first = String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string();
                let body = format!("backend a reçu : {first}");
                let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            });
        }
    });
    let dir = temp_site("route");
    let project = Project { routes: vec![Route { path: "/app".into(), port }], ..Default::default() };
    let (addr, _) = start(Global::default(), vec![site("demo", dir.clone(), project)]).await;
    let (code, body) = get(addr, "demo.test", "/app/ws?x=1").await;
    assert_eq!(code, 200);
    assert!(body.ends_with("backend a reçu : GET /app/ws?x=1 HTTP/1.1"), "{body}");
    // hors de la route : fichier du site
    assert!(get(addr, "demo.test", "/hello.txt").await.1.ends_with("bonjour"));
    let _ = std::fs::remove_dir_all(dir);
}

/// Passe par un vrai php-cgi si PHP est installé par Viherd sur cette machine (ignoré en CI).
#[tokio::test]
async fn runs_php_through_fastcgi_when_available() {
    let g = Global::load().unwrap_or_default();
    let php = viherd::paths::php_dir(&g.default_php);
    if !php.join("php-cgi.exe").exists() {
        eprintln!("PHP {} absent de {} : test ignoré", g.default_php, php.display());
        return;
    }
    let dir = temp_site("php");
    let (addr, st) = start(g, vec![site("demo", dir.clone(), Project::default())]).await;
    let (code, body) = get(addr, "demo.test", "/une/route?q=ok").await;
    st.php.stop_all().await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("PHP ") && body.ends_with(" http ok"), "{body}");
    let _ = std::fs::remove_dir_all(dir);
}
