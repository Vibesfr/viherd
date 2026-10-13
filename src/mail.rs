//! Capteur de mails : un serveur SMTP local qui garde les mails en mémoire (rien n'est envoyé).
//! Laravel : MAIL_MAILER=smtp MAIL_HOST=127.0.0.1 MAIL_PORT=2525. Interface : http://127.0.0.1:7777/mail
use anyhow::Result;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const MAX_MAILS: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Mail {
    pub id: u64,
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub received: u64,
    pub raw: String,
}

#[derive(Default)]
pub struct MailBox {
    mails: Mutex<Vec<Mail>>,
    next_id: Mutex<u64>,
}

impl MailBox {
    pub fn list(&self) -> Vec<Mail> {
        let mut v = self.mails.lock().unwrap().clone();
        v.reverse();
        v
    }
    pub fn get(&self, id: u64) -> Option<Mail> {
        self.mails.lock().unwrap().iter().find(|m| m.id == id).cloned()
    }
    pub fn clear(&self) {
        self.mails.lock().unwrap().clear();
    }
    fn push(&self, from: String, to: Vec<String>, raw: String) {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let subject = raw.lines()
            .take_while(|l| !l.is_empty())
            .find_map(|l| l.strip_prefix("Subject:").or_else(|| l.strip_prefix("subject:")))
            .unwrap_or("(sans sujet)").trim().to_string();
        let received = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        tracing::info!("✉ mail capturé : {subject} -> {}", to.join(", "));
        let mut m = self.mails.lock().unwrap();
        m.push(Mail { id, from, to, subject, received, raw });
        if m.len() > MAX_MAILS {
            m.remove(0);
        }
    }
}

pub async fn serve(port: u16, mb: Arc<MailBox>) -> Result<()> {
    let l = TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("SMTP (capture des mails) sur 127.0.0.1:{port}");
    loop {
        let (s, _) = l.accept().await?;
        let mb = mb.clone();
        tokio::spawn(async move {
            if let Err(e) = session(s, &mb).await {
                tracing::debug!("smtp: {e}");
            }
        });
    }
}

async fn session(s: TcpStream, mb: &MailBox) -> Result<()> {
    let (r, mut w) = s.into_split();
    let mut r = BufReader::new(r);
    w.write_all(b"220 viherd ESMTP\r\n").await?;
    let (mut from, mut to) = (String::new(), Vec::new());
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let cmd = line.trim_end().to_string();
        let upper = cmd.to_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            b"250-viherd\r\n250-8BITMIME\r\n250 SIZE 52428800\r\n"
        } else if upper.starts_with("HELO") || upper.starts_with("NOOP") {
            b"250 OK\r\n"
        } else if upper.starts_with("MAIL FROM:") {
            from = addr(&cmd[10..]);
            to.clear();
            b"250 OK\r\n"
        } else if upper.starts_with("RCPT TO:") {
            to.push(addr(&cmd[8..]));
            b"250 OK\r\n"
        } else if upper == "DATA" {
            w.write_all(b"354 Fin avec <CRLF>.<CRLF>\r\n").await?;
            let mut raw = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).await? == 0 {
                    return Ok(());
                }
                let l = line.trim_end_matches(['\r', '\n']);
                if l == "." {
                    break;
                }
                raw.push_str(l.strip_prefix('.').filter(|_| l.starts_with("..")).unwrap_or(l));
                raw.push('\n');
            }
            mb.push(std::mem::take(&mut from), std::mem::take(&mut to), raw);
            b"250 OK queued\r\n"
        } else if upper == "RSET" {
            from.clear();
            to.clear();
            b"250 OK\r\n"
        } else if upper == "QUIT" {
            w.write_all(b"221 Bye\r\n").await?;
            return Ok(());
        } else {
            b"502 Command not implemented\r\n"
        };
        w.write_all(reply).await?;
    }
}

fn addr(s: &str) -> String {
    s.trim().split_whitespace().next().unwrap_or("").trim_matches(['<', '>']).to_string()
}
