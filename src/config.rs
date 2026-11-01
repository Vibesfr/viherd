//! Configuration globale (~/.viherd/config.toml) et par projet (viherd.toml).
use crate::paths;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const PROJECT_FILE: &str = "viherd.toml";
pub const MINIO_AISTOR_URL: &str = "https://dl.min.io/aistor/minio/release/windows-amd64/minio.exe";

/// Licence AIStor de MinIO (gratuite, à demander sur min.io) déposée par l'utilisateur.
pub fn minio_license() -> PathBuf {
    paths::home().join("licenses").join("minio.license")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Global {
    pub tld: String,
    pub http_port: u16,
    /// Port HTTPS des sites sécurisés (`viherd secure`)
    pub https_port: u16,
    pub api_port: u16,
    pub smtp_port: u16,
    /// Nombre de php-cgi par projet (requêtes traitées en parallèle)
    pub php_processes: usize,
    pub default_php: String,
    pub default_node: String,
    pub parked: Vec<PathBuf>,
    pub links: BTreeMap<String, PathBuf>,
    /// Sites servis en HTTPS (le HTTP redirige vers HTTPS)
    pub secured: BTreeSet<String>,
    pub services: BTreeMap<String, Service>,
    /// Intercepter dump()/dd() (Symfony VarDumper) et les afficher dans Viherd
    pub dumps: bool,
    pub dump_port: u16,
    /// Sauvegarde quotidienne de toutes les bases MySQL / PostgreSQL
    pub backups: AutoBackups,
    /// Import depuis Herd déjà fait (ou proposition écartée)
    pub herd_imported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoBackups {
    pub enabled: bool,
    /// Heure de la journée (0-23) à partir de laquelle la sauvegarde du jour est faite
    pub hour: u8,
    /// Nombre de sauvegardes automatiques gardées par base
    pub keep: usize,
}

impl Default for AutoBackups {
    fn default() -> Self { Self { enabled: false, hour: 12, keep: 7 } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub enabled: bool,
    pub url: String,
    pub port: u16,
}

impl Default for Global {
    fn default() -> Self {
        let mut services = BTreeMap::new();
        services.insert("mysql".into(), Service {
            enabled: false,
            url: "https://archive.mariadb.org/mariadb-11.4.3/winx64-packages/mariadb-11.4.3-winx64.zip".into(),
            port: 3306,
        });
        services.insert("redis".into(), Service {
            enabled: false,
            url: "https://github.com/redis-windows/redis-windows/releases/download/7.4.1/Redis-7.4.1-Windows-x64-msys2.zip".into(),
            port: 6379,
        });
        services.insert("postgres".into(), Service {
            enabled: false,
            url: "https://get.enterprisedb.com/postgresql/postgresql-16.4-1-windows-x64-binaries.zip".into(),
            port: 5432,
        });
        // Stockage S3 sans compte : RustFS (compatible S3, remplaçant de MinIO, licence Apache 2.0).
        services.insert("rustfs".into(), Service {
            enabled: false,
            url: "https://github.com/rustfs/rustfs/releases/latest/download/rustfs-windows-x86_64-latest.zip".into(),
            port: 9000,
        });
        // MinIO n'a plus de binaire communautaire : seule l'édition AIStor existe, elle exige une licence (gratuite).
        services.insert("minio".into(), Service {
            enabled: false,
            url: MINIO_AISTOR_URL.into(),
            port: 9200,
        });
        services.insert("meilisearch".into(), Service {
            enabled: false,
            url: "https://github.com/meilisearch/meilisearch/releases/latest/download/meilisearch-windows-amd64.exe".into(),
            port: 7700,
        });
        Self {
            tld: "test".into(),
            http_port: 80,
            https_port: 443,
            api_port: 7777,
            smtp_port: 2525,
            php_processes: 4,
            default_php: "8.3".into(),
            default_node: "22".into(),
            parked: vec![],
            links: BTreeMap::new(),
            secured: BTreeSet::new(),
            services,
            dumps: true,
            dump_port: 9912,
            backups: AutoBackups::default(),
            herd_imported: false,
        }
    }
}

impl Global {
    pub fn load() -> Result<Self> {
        let p = paths::config_file();
        if !p.exists() {
            return Ok(Self::default());
        }
        let mut g: Self = toml::from_str(&std::fs::read_to_string(p)?)?;
        // Une config écrite par une ancienne version ne connaît pas les nouveaux services : on les ajoute.
        for (k, v) in Self::default().services {
            g.services.entry(k).or_insert(v);
        }
        // L'ancien binaire communautaire de MinIO a été retiré (HTTP 410) : on bascule sur AIStor,
        // sur un port qui ne gêne pas RustFS (9000).
        if let Some(m) = g.services.get_mut("minio") {
            if m.url.contains("/server/minio/release/") {
                m.url = MINIO_AISTOR_URL.into();
                if m.port == 9000 {
                    m.port = 9200;
                }
            }
        }
        // Répare les chemins enregistrés au format interne de Windows (\\?\C:\...) par les anciennes versions.
        for p in g.parked.iter_mut() {
            *p = plain_path(p);
        }
        for p in g.links.values_mut() {
            *p = plain_path(p);
        }
        g.parked.dedup();
        Ok(g)
    }
    pub fn save(&self) -> Result<()> {
        let p = paths::config_file();
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Retire le préfixe « verbatim » de Windows (\\?\C:\x → C:\x, \\?\UNC\srv\x → \\srv\x).
pub fn plain_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p.to_path_buf(),
    }
}

/// Chemin absolu propre d'un dossier existant (sans le préfixe \\?\ que renvoie canonicalize sous Windows).
pub fn canonical(p: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p).map(|c| plain_path(&c))
}

/// viherd.toml à la racine d'un projet : versions isolées, workers, routes websocket...
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub php: Option<String>,
    pub node: Option<String>,
    /// Dossier public (par défaut `public/` s'il existe)
    pub root: Option<String>,
    /// Tout le site est proxifié vers une app Node (ex: 3000)
    pub proxy: Option<u16>,
    /// Active Xdebug (mode debug, port 9003) pour PHP, les workers et le terminal du projet
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub xdebug: bool,
    /// Profiler d'Xdebug, déclenché par requête avec ?XDEBUG_PROFILE=1
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub profile: bool,
    /// Préfixes proxifiés (HTTP + WebSocket), ex: /app -> Reverb sur 8080
    pub routes: Vec<Route>,
    pub workers: Vec<Worker>,
    pub env: BTreeMap<String, String>,
    /// Réglages php.ini propres au projet, ex : memory_limit = "1G"
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub php_ini: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub path: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    pub name: String,
    pub command: String,
    #[serde(default = "yes")]
    pub autorestart: bool,
}
fn yes() -> bool { true }

impl Project {
    /// Xdebug doit être chargé (débogage ou profilage)
    pub fn needs_xdebug(&self) -> bool { self.xdebug || self.profile }

    pub fn load(dir: &Path) -> Result<Self> {
        let p = dir.join(PROJECT_FILE);
        if !p.exists() {
            return Ok(Self::default());
        }
        Ok(toml::from_str(&std::fs::read_to_string(p)?)?)
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::write(dir.join(PROJECT_FILE), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn find_project_root(start: &Path) -> PathBuf {
    let mut d = Some(start);
    while let Some(x) = d {
        if x.join(PROJECT_FILE).exists() {
            return x.to_path_buf();
        }
        d = x.parent();
    }
    start.to_path_buf()
}

#[derive(Debug, Clone, Serialize)]
pub struct Site {
    pub name: String,
    pub path: PathBuf,
    pub project: Project,
}

impl Site {
    pub fn php(&self, g: &Global) -> String {
        self.project.php.clone().unwrap_or_else(|| g.default_php.clone())
    }
    pub fn docroot(&self) -> PathBuf {
        if let Some(r) = &self.project.root {
            return self.path.join(r);
        }
        let p = self.path.join("public");
        if p.is_dir() { p } else { self.path.clone() }
    }
}

pub fn site_name(s: &str) -> String {
    s.to_lowercase().replace([' ', '_'], "-")
}

pub fn discover_sites(g: &Global) -> BTreeMap<String, Site> {
    let mut m = BTreeMap::new();
    let mut add = |name: String, path: PathBuf| {
        let project = Project::load(&path).unwrap_or_else(|e| {
            tracing::warn!("{}: viherd.toml invalide: {e}", path.display());
            Project::default()
        });
        m.insert(name.clone(), Site { name, path, project });
    };
    for dir in &g.parked {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    add(site_name(&e.file_name().to_string_lossy()), e.path());
                }
            }
        }
    }
    for (n, p) in &g.links {
        add(n.clone(), p.clone());
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_parses_and_omits_defaults() {
        let p: Project = toml::from_str(
            "php = \"8.3\"\nxdebug = true\n[[workers]]\nname = \"queue\"\ncommand = \"php artisan queue:work\"\n[php_ini]\nmemory_limit = \"1G\"\n",
        ).unwrap();
        assert_eq!(p.php.as_deref(), Some("8.3"));
        assert!(p.xdebug && p.workers[0].autorestart);
        assert_eq!(p.php_ini["memory_limit"], "1G");
        // un projet sans Xdebug ni php_ini n'écrit pas ces clés dans viherd.toml
        let out = toml::to_string_pretty(&Project { php: Some("8.2".into()), ..Default::default() }).unwrap();
        assert!(!out.contains("xdebug") && !out.contains("php_ini"));
    }

    #[test]
    fn old_global_config_gets_new_defaults() {
        let g: Global = toml::from_str("tld = \"test\"\nhttp_port = 8888\n").unwrap();
        assert_eq!((g.http_port, g.https_port), (8888, 443));
        assert!(g.secured.is_empty());
    }

    #[test]
    fn plain_path_strips_verbatim_prefix() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\Sites\app")), PathBuf::from(r"C:\Sites\app"));
        assert_eq!(plain_path(Path::new(r"\\?\UNC\srv\share")), PathBuf::from(r"\\srv\share"));
    }

    #[test]
    fn site_names_are_normalized() {
        assert_eq!(site_name("Mon Super_App"), "mon-super-app");
    }
}
