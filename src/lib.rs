//! Viherd : environnement de dev PHP/Node local, façon Laravel Herd, en Rust.
//! Cette bibliothèque est partagée par la CLI (`viherd`) et l'appli de bureau (`viherd-app`).
pub mod certs;
pub mod config;
pub mod daemon;
pub mod db;
pub mod download;
pub mod dumps;
pub mod env;
pub mod fastcgi;
pub mod hosts;
pub mod icon;
pub mod mail;
pub mod paths;
pub mod php_pool;
pub mod redis;
pub mod server;
pub mod services;
pub mod share;
pub mod supervisor;
pub mod ui;
pub mod versions;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
