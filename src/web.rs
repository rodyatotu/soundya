//! Small web page for the local network: playback controls and proxy settings.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

use std::sync::Arc;

use crate::{settings::Settings, Cmd, Shared};

const PAGE: &str = include_str!("web.html");

pub fn serve(port: u16, tx: Sender<Cmd>, shared: Arc<Shared>) {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("web: cannot listen on port {port}: {e}");
            return;
        }
    };
    eprintln!("web: http://<speaker-ip>:{port}/");
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let tx = tx.clone();
            let shared = shared.clone();
            thread::spawn(move || {
                let _ = handle(conn, &tx, &shared);
            });
        }
    });
}

fn ask(tx: &Sender<Cmd>, make: impl FnOnce(Sender<String>) -> Cmd, wait: u64) -> String {
    let (rtx, rrx) = mpsc::channel();
    if tx.send(make(rtx)).is_err() {
        return "error: player is not running".into();
    }
    rrx.recv_timeout(Duration::from_secs(wait))
        .unwrap_or_else(|_| "error: timeout".into())
}

fn handle(conn: TcpStream, tx: &Sender<Cmd>, shared: &Shared) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(&conn);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0).min(64 * 1024);
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();

    // The cover of the playing track, fetched by the speaker (bypass/proxy).
    if method == "GET" && (path == "/cover" || path.starts_with("/cover?")) {
        let mut out = &conn;
        return match cover(shared) {
            Some((kind, bytes)) => {
                write!(
                    out,
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: max-age=86400\r\nConnection: close\r\n\r\n",
                    bytes.len()
                )?;
                out.write_all(&bytes)?;
                out.flush()
            }
            None => {
                write!(out, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
                out.flush()
            }
        };
    }
    let (status, kind, reply) = match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => ("200 OK", "text/html; charset=utf-8", PAGE.to_owned()),
        ("GET", "/api/status") => {
            let cached = shared.status_json.lock().unwrap().clone();
            let json = if cached.is_empty() {
                ask(tx, |r| Cmd::Control("json".into(), r), 30)
            } else {
                cached
            };
            ("200 OK", "application/json", json)
        }
        ("POST", "/api/cmd") => (
            "200 OK",
            "text/plain; charset=utf-8",
            ask(tx, |r| Cmd::Control(body.trim().to_owned(), r), 240),
        ),
        ("POST", "/api/settings") => match serde_json::from_str::<Settings>(&body) {
            Ok(s) => ("200 OK", "text/plain; charset=utf-8", ask(tx, |r| Cmd::Settings(s, r), 15)),
            Err(e) => ("400 Bad Request", "text/plain; charset=utf-8", format!("error: {e}")),
        },
        ("POST", "/api/feedback") => {
            let text = body.trim();
            match crate::stats::feedback(text) {
                Ok(()) => ("200 OK", "text/plain; charset=utf-8", "ok".to_owned()),
                Err(e) => ("200 OK", "text/plain; charset=utf-8", format!("error: {e}")),
            }
        }
        ("POST", "/api/test") => {
            let proxy = serde_json::from_str::<Settings>(&body).map(|s| s.proxy).unwrap_or_default();
            ("200 OK", "text/plain; charset=utf-8", ask(tx, |r| Cmd::TestProxy(proxy, r), 40))
        }
        _ => ("404 Not Found", "text/plain", "not found".into()),
    };
    let mut out = &conn;
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        reply.len()
    )?;
    out.write_all(reply.as_bytes())?;
    out.flush()
}

/// Fetches the new track's cover in the background.
pub fn prefetch_cover(shared: Arc<Shared>) {
    if shared.cover.lock().unwrap().is_some() {
        thread::spawn(move || {
            cover(&shared);
        });
    }
}

/// The last cover fetched: (its SoundCloud address, type, bytes).
static COVER: std::sync::Mutex<Option<(String, String, Vec<u8>)>> = std::sync::Mutex::new(None);

/// The playing track's cover, from SoundCloud's CDN through the same agent
/// as the music; kept until the track changes.
fn cover(shared: &Shared) -> Option<(String, Vec<u8>)> {
    let url = shared.cover.lock().unwrap().clone()?;
    if let Some((u, kind, bytes)) = COVER.lock().unwrap().as_ref() {
        if *u == url {
            return Some((kind.clone(), bytes.clone()));
        }
    }
    let proxy = shared.proxy.lock().unwrap().clone();
    let agent = crate::sc::build_agent(&proxy).ok()?;
    let resp = match agent.get(&url).timeout(Duration::from_secs(15)).call() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("web: cover: {e}");
            return None;
        }
    };
    let kind = resp.header("Content-Type").filter(|k| k.starts_with("image/")).unwrap_or("image/jpeg").to_owned();
    let mut bytes = Vec::new();
    resp.into_reader().take(3 << 20).read_to_end(&mut bytes).ok()?;
    *COVER.lock().unwrap() = Some((url, kind.clone(), bytes.clone()));
    Some((kind, bytes))
}
