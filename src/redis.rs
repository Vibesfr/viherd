//! Client Redis minimal (protocole RESP2) : juste de quoi lire les files Laravel et Horizon.
use anyhow::{anyhow, bail, Result};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Nil,
    Int(i64),
    Str(String),
    Arr(Vec<Reply>),
}

impl Reply {
    pub fn int(&self) -> i64 {
        match self {
            Reply::Int(n) => *n,
            Reply::Str(s) => s.parse().unwrap_or(0),
            _ => 0,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self { Reply::Str(s) => Some(s), _ => None }
    }
    pub fn list(self) -> Vec<Reply> {
        match self { Reply::Arr(v) => v, _ => vec![] }
    }
}

pub struct Conn {
    io: BufReader<TcpStream>,
}

impl Conn {
    pub async fn connect(host: &str, port: u16, password: Option<&str>, db: u32) -> Result<Self> {
        let s = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect((host, port))).await
            .map_err(|_| anyhow!("Redis {host}:{port} ne répond pas"))??;
        let mut c = Self { io: BufReader::new(s) };
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            c.cmd(&["AUTH", p]).await?;
        }
        if db != 0 {
            c.cmd(&["SELECT", &db.to_string()]).await?;
        }
        Ok(c)
    }

    pub async fn cmd(&mut self, args: &[&str]) -> Result<Reply> {
        self.io.get_mut().write_all(&encode(args)).await?;
        tokio::time::timeout(Duration::from_secs(5), read_reply(&mut self.io)).await.map_err(|_| anyhow!("Redis : délai dépassé"))?
    }
}

pub fn encode(args: &[&str]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

pub fn read_reply<'a, R: AsyncBufRead + Unpin + Send>(r: &'a mut R) -> Pin<Box<dyn Future<Output = Result<Reply>> + Send + 'a>> {
    Box::pin(async move {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 { bail!("Redis : connexion fermée"); }
        let line = line.trim_end_matches(['\r', '\n']);
        let (kind, rest) = line.split_at(1.min(line.len()));
        match kind {
            "+" => Ok(Reply::Str(rest.into())),
            "-" => bail!("Redis : {rest}"),
            ":" => Ok(Reply::Int(rest.parse()?)),
            "$" => {
                let n: i64 = rest.parse()?;
                if n < 0 { return Ok(Reply::Nil); }
                let mut buf = vec![0; n as usize + 2];
                r.read_exact(&mut buf).await?;
                buf.truncate(n as usize);
                Ok(Reply::Str(String::from_utf8_lossy(&buf).into_owned()))
            }
            "*" => {
                let n: i64 = rest.parse()?;
                if n < 0 { return Ok(Reply::Nil); }
                let mut v = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    v.push(read_reply(r).await?);
                }
                Ok(Reply::Arr(v))
            }
            _ => bail!("Redis : réponse inattendue « {line} »"),
        }
    })
}

/// Str::slug($s, $sep) de Laravel, pour retrouver les préfixes par défaut (APP_NAME → laravel-database-).
pub fn laravel_slug(s: &str, sep: char) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with(sep) && !out.is_empty() {
            out.push(sep);
        }
    }
    out.trim_end_matches(sep).to_string()
}

/// Préfixe Redis par défaut : `slug-database-` depuis Laravel 11, `slug_database_` avant.
/// On regarde le config/database.php du projet plutôt que de deviner la version.
pub fn laravel_default_prefix(database_php: &str, app_name: &str) -> String {
    if database_php.contains("'-database-'") {
        format!("{}-database-", laravel_slug(app_name, '-'))
    } else {
        format!("{}_database_", laravel_slug(app_name, '_'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_nested_replies() {
        let mut data: &[u8] = b"*3\r\n:5\r\n$5\r\nhello\r\n*2\r\n$-1\r\n+OK\r\n";
        let r = read_reply(&mut data).await.unwrap();
        assert_eq!(r, Reply::Arr(vec![Reply::Int(5), Reply::Str("hello".into()), Reply::Arr(vec![Reply::Nil, Reply::Str("OK".into())])]));
        let mut err: &[u8] = b"-NOAUTH Authentication required\r\n";
        assert!(read_reply(&mut err).await.is_err());
    }

    #[test]
    fn encodes_commands() {
        assert_eq!(encode(&["LLEN", "q"]), b"*2\r\n$4\r\nLLEN\r\n$1\r\nq\r\n");
    }

    #[test]
    fn slugs_like_laravel() {
        assert_eq!(laravel_slug("Laravel", '_'), "laravel");
        assert_eq!(laravel_slug("Mon App 2", '_'), "mon_app_2");
        assert_eq!(laravel_slug("Mon App 2", '-'), "mon-app-2");
    }

    #[test]
    fn default_prefix_follows_laravel_version() {
        let l11 = "'prefix' => env('REDIS_PREFIX', Str::slug((string) env('APP_NAME', 'laravel')).'-database-'),";
        let l10 = "'prefix' => env('REDIS_PREFIX', Str::slug(env('APP_NAME', 'laravel'), '_').'_database_'),";
        assert_eq!(laravel_default_prefix(l11, "Mon App"), "mon-app-database-");
        assert_eq!(laravel_default_prefix(l10, "Mon App"), "mon_app_database_");
    }
}
