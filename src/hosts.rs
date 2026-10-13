//! Synchronise le fichier hosts de Windows avec les sites (nécessite un terminal administrateur).
use crate::config::{discover_sites, Global};
use anyhow::{Context, Result};

const HOSTS: &str = r"C:\Windows\System32\drivers\etc\hosts";

pub fn sync(g: &Global) -> Result<usize> {
    let cur = std::fs::read_to_string(HOSTS)?;
    let mut out = String::new();
    let mut skip = false;
    for l in cur.lines() {
        match l.trim() {
            "# viherd-start" => skip = true,
            "# viherd-end" => skip = false,
            _ if !skip => {
                out.push_str(l);
                out.push_str("\r\n");
            }
            _ => {}
        }
    }
    let sites = discover_sites(g);
    out.push_str("# viherd-start\r\n");
    for name in sites.keys() {
        out.push_str(&format!("127.0.0.1 {name}.{tld}\r\n::1 {name}.{tld}\r\n", tld = g.tld));
    }
    out.push_str("# viherd-end\r\n");
    std::fs::write(HOSTS, out).context("écriture du fichier hosts refusée : relance le terminal en administrateur")?;
    Ok(sites.len())
}
