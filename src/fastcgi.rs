//! Client FastCGI minimal pour parler à php-cgi.exe.
use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const BEGIN_REQUEST: u8 = 1;
const END_REQUEST: u8 = 3;
const PARAMS: u8 = 4;
const STDIN: u8 = 5;
const STDOUT: u8 = 6;
const STDERR: u8 = 7;

pub struct FcgiResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub stderr: String,
}

fn record(t: u8, content: &[u8], out: &mut Vec<u8>) {
    let len = content.len();
    let pad = (8 - len % 8) % 8;
    out.extend_from_slice(&[1, t, 0, 1, (len >> 8) as u8, len as u8, pad as u8, 0]);
    out.extend_from_slice(content);
    out.resize(out.len() + pad, 0);
}

fn nv_len(n: usize, out: &mut Vec<u8>) {
    if n < 128 {
        out.push(n as u8);
    } else {
        out.extend_from_slice(&((n as u32) | 0x8000_0000).to_be_bytes());
    }
}

pub async fn request(port: u16, params: &[(String, String)], body: &[u8]) -> Result<FcgiResponse> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await?;
    let mut out = Vec::with_capacity(body.len() + 4096);
    record(BEGIN_REQUEST, &[0, 1, 0, 0, 0, 0, 0, 0], &mut out);
    let mut p = Vec::new();
    for (k, v) in params {
        nv_len(k.len(), &mut p);
        nv_len(v.len(), &mut p);
        p.extend_from_slice(k.as_bytes());
        p.extend_from_slice(v.as_bytes());
    }
    for c in p.chunks(65535) {
        record(PARAMS, c, &mut out);
    }
    record(PARAMS, &[], &mut out);
    for c in body.chunks(65535) {
        record(STDIN, c, &mut out);
    }
    record(STDIN, &[], &mut out);
    s.write_all(&out).await?;

    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let mut h = [0u8; 8];
        s.read_exact(&mut h).await?;
        let len = u16::from_be_bytes([h[4], h[5]]) as usize;
        let mut c = vec![0u8; len + h[6] as usize];
        s.read_exact(&mut c).await?;
        c.truncate(len);
        match h[1] {
            STDOUT => stdout.extend(c),
            STDERR => stderr.extend(c),
            END_REQUEST => break,
            _ => {}
        }
    }

    let (head_end, sep) = match find(&stdout, b"\r\n\r\n") {
        Some(i) => (i, 4),
        None => match find(&stdout, b"\n\n") {
            Some(i) => (i, 2),
            None => (0, 0),
        },
    };
    let mut status = 200;
    let mut headers = Vec::new();
    for line in String::from_utf8_lossy(&stdout[..head_end]).lines() {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("status") {
                status = v.split_whitespace().next().and_then(|c| c.parse().ok()).unwrap_or(200);
            } else {
                headers.push((k, v));
            }
        }
    }
    Ok(FcgiResponse {
        status,
        headers,
        body: stdout[head_end + sep..].to_vec(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}
