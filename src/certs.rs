//! HTTPS local : une autorité de certification propre à la machine (~/.viherd/certs/ca.*),
//! approuvée dans le magasin Windows de l'utilisateur, qui signe à la volée un certificat par site.
use crate::paths;
use crate::server::State;
use anyhow::{anyhow, bail, Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const CA_NAME: &str = "Viherd Local CA";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn dir() -> PathBuf { paths::home().join("certs") }
pub fn ca_cert_path() -> PathBuf { dir().join("ca.crt") }
fn ca_key_path() -> PathBuf { dir().join("ca.key") }

/// Paramètres de l'autorité, toujours identiques : re-signer avec la même clé redonne le même émetteur.
fn ca_params() -> Result<CertificateParams> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.distinguished_name.push(DnType::CommonName, CA_NAME);
    p.distinguished_name.push(DnType::OrganizationName, "Viherd");
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    p.not_before = rcgen::date_time_ymd(2024, 1, 1);
    p.not_after = rcgen::date_time_ymd(2049, 12, 31);
    Ok(p)
}

/// L'autorité (certificat + clé), créée au premier appel.
pub struct Ca {
    /// Sert d'émetteur pour signer (re-signé en mémoire : sa signature change à chaque fois)
    cert: rcgen::Certificate,
    key: KeyPair,
    /// Le certificat tel qu'il est enregistré et approuvé dans Windows (DER)
    der: Vec<u8>,
}

impl Ca {
    pub fn load_or_create() -> Result<Self> {
        let key = match std::fs::read_to_string(ca_key_path()) {
            Ok(pem) => KeyPair::from_pem(&pem).context("clé de l'autorité illisible")?,
            Err(_) => {
                let key = KeyPair::generate()?;
                std::fs::create_dir_all(dir())?;
                std::fs::write(ca_key_path(), key.serialize_pem())?;
                let _ = std::fs::remove_file(ca_cert_path());
                key
            }
        };
        let cert = ca_params()?.self_signed(&key)?;
        let der = match std::fs::read(ca_cert_path()) {
            Ok(d) => d,
            Err(_) => {
                std::fs::write(ca_cert_path(), cert.der())?;
                cert.der().to_vec()
            }
        };
        Ok(Self { cert, key, der })
    }

    /// Certificat d'un site : `nom.test` et ses sous-domaines (`api.nom.test`).
    pub fn issue(&self, host: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut p = CertificateParams::new(vec![host.to_string(), format!("*.{host}")])?;
        p.distinguished_name.push(DnType::CommonName, host);
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        // Les navigateurs refusent les certificats valables plus de 398 jours
        let now = chrono::Local::now().date_naive();
        let end = now + chrono::Duration::days(397);
        let ymd = |d: chrono::NaiveDate| {
            use chrono::Datelike;
            rcgen::date_time_ymd(d.year(), d.month() as u8, d.day() as u8)
        };
        p.not_before = ymd(now - chrono::Duration::days(1));
        p.not_after = ymd(end);
        let key = KeyPair::generate()?;
        let cert = p.signed_by(&key, &self.cert, &self.key)?;
        Ok((cert.der().to_vec(), key.serialize_der()))
    }

    pub fn der(&self) -> &[u8] { &self.der }
}

/// Empreinte SHA-1 du certificat de l'autorité, telle que l'attend certutil.
fn thumbprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn certutil(args: &[&str]) -> Result<std::process::Output> {
    Ok(std::process::Command::new("certutil").args(args).creation_flags(CREATE_NO_WINDOW).output()?)
}

/// L'autorité est-elle dans les « Autorités de certification racines » de l'utilisateur ?
pub fn is_trusted() -> bool {
    let Ok(ca) = Ca::load_or_create() else { return false };
    certutil(&["-user", "-store", "Root", &thumbprint(ca.der())]).is_ok_and(|o| o.status.success())
}

/// Version mise en cache de is_trusted() pour l'interface (certutil prend ~100 ms).
pub fn trusted_cached() -> bool {
    static CACHE: Mutex<Option<bool>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap();
    if c.is_none() || TRUST_CHANGED.swap(false, std::sync::atomic::Ordering::SeqCst) {
        *c = Some(is_trusted());
    }
    c.unwrap()
}
static TRUST_CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ajoute l'autorité au magasin de l'utilisateur (Windows affiche une confirmation, pas besoin d'être admin).
pub fn trust() -> Result<()> {
    Ca::load_or_create()?;
    if is_trusted() {
        return Ok(()); // déjà dans le magasin : pas de nouvelle fenêtre Windows
    }
    let p = ca_cert_path().display().to_string();
    let o = certutil(&["-user", "-addstore", "Root", &p])?;
    TRUST_CHANGED.store(true, std::sync::atomic::Ordering::SeqCst);
    if !o.status.success() || !is_trusted() {
        bail!("l'autorité n'a pas été approuvée (confirmation refusée ?) : {}", String::from_utf8_lossy(&o.stdout).trim());
    }
    Ok(())
}

/// Retire l'autorité du magasin (désinstallation).
pub fn untrust() -> Result<()> {
    let ca = Ca::load_or_create()?;
    certutil(&["-user", "-delstore", "Root", &thumbprint(ca.der())])?;
    TRUST_CHANGED.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// Choisit (et fabrique au besoin) le certificat selon le nom demandé par le navigateur (SNI).
pub struct Resolver {
    ca: Ca,
    st: Arc<State>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("Resolver") }
}

impl Resolver {
    fn certified(&self, site: &str, tld: &str) -> Result<Arc<CertifiedKey>> {
        if let Some(c) = self.cache.lock().unwrap().get(site) {
            return Ok(c.clone());
        }
        let (cert, key) = self.ca.issue(&format!("{site}.{tld}"))?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key));
        let signer = rustls::crypto::ring::sign::any_supported_type(&key).map_err(|e| anyhow!("{e}"))?;
        let ck = Arc::new(CertifiedKey::new(vec![CertificateDer::from(cert), CertificateDer::from(self.ca.der().to_vec())], signer));
        self.cache.lock().unwrap().insert(site.to_string(), ck.clone());
        Ok(ck)
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = hello.server_name()?.to_lowercase();
        let tld = self.st.global.read().unwrap().tld.clone();
        let site = crate::server::site_for_host(&self.st, &host, &tld)?;
        self.certified(&site.name, &tld).map_err(|e| tracing::error!("certificat {host}: {e:#}")).ok()
    }
}

pub fn server_config(st: Arc<State>) -> Result<rustls::ServerConfig> {
    let resolver = Resolver { ca: Ca::load_or_create()?, st, cache: Mutex::new(HashMap::new()) };
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_is_stable_and_signs_site_certs() {
        let tmp = std::env::temp_dir().join(format!("viherd-certs-{}", std::process::id()));
        std::env::set_var("VIHERD_HOME", &tmp);
        let a = Ca::load_or_create().unwrap();
        let b = Ca::load_or_create().unwrap();
        // même clé + mêmes paramètres : le certificat rechargé est le même émetteur
        assert_eq!(thumbprint(a.der()), thumbprint(b.der()));
        let (cert, key) = a.issue("demo.test").unwrap();
        assert!(!cert.is_empty() && !key.is_empty());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
