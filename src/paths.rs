//! Emplacements des fichiers de Viherd (par défaut %USERPROFILE%\.viherd).
use std::path::PathBuf;

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("VIHERD_HOME") {
        return h.into();
    }
    dirs::home_dir().expect("dossier utilisateur introuvable").join(".viherd")
}
pub fn config_file() -> PathBuf { home().join("config.toml") }
pub fn php_dir(v: &str) -> PathBuf { home().join("bin").join("php").join(v) }
pub fn node_dir(v: &str) -> PathBuf { home().join("bin").join("node").join(v) }
pub fn composer_dir() -> PathBuf { home().join("bin").join("composer") }
pub fn service_dir(n: &str) -> PathBuf { home().join("services").join(n) }
pub fn data_dir(n: &str) -> PathBuf { home().join("data").join(n) }
pub fn logs() -> PathBuf { home().join("logs") }
