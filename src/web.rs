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

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn only_the_panel_may_change_things() {
        assert!(allowed("192.168.0.20:8765", "http://192.168.0.20:8765").is_ok());
        assert!(allowed("soundya.local:8765", "http://soundya.local:8765").is_ok());
        assert!(allowed("192.168.0.20:8765", "").is_ok(), "curl, no Origin");
        assert!(allowed("station:8765", "http://station:8765").is_ok());
        assert!(allowed("[fe80::1]:8765", "http://[fe80::1]:8765").is_ok());
        // Another site in the same browser (CSRF).
        assert!(allowed("192.168.0.20:8765", "https://evil.example").is_err());
        // DNS rebinding: the attacker's name points at the speaker.
        assert!(allowed("evil.example:8765", "http://evil.example:8765").is_err());
        assert!(allowed("evil.example:8765", "").is_err());
    }
}

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
    let mut host = String::new();
    let mut origin = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("host") {
                host = v.trim().to_owned();
            } else if k.eq_ignore_ascii_case("origin") {
                origin = v.trim().to_owned();
            }
        }
    }
    // Bug reports carry a screenshot; everything else is small.
    let limit = if path.starts_with("/api/report") { 6 << 20 } else { 64 * 1024 };
    if length > limit {
        return reply(&conn, "413 Payload Too Large", "text/plain; charset=utf-8", "error: слишком большой запрос");
    }
    // Commands only from the panel itself: another site open in the browser
    // (CSRF) or a DNS-rebinding name must not uninstall or reconfigure the speaker.
    if method != "GET" {
        if let Err(why) = allowed(&host, &origin) {
            eprintln!("web: refused {method} {path}: {why}");
            return reply(&conn, "403 Forbidden", "text/plain; charset=utf-8", &format!("error: {why}"));
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
        ("POST", "/api/settings") => match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) if v.is_object() => ("200 OK", "text/plain; charset=utf-8", ask(tx, |r| Cmd::Settings(v, r), 15)),
            Ok(_) => ("400 Bad Request", "text/plain; charset=utf-8", "error: ожидается объект JSON".to_owned()),
            Err(e) => ("400 Bad Request", "text/plain; charset=utf-8", format!("error: {e}")),
        },
        // 0.4.40 page: plain text, anonymous. Now queued like a report (no logs, no id).
        ("POST", "/api/feedback") => {
            let req = crate::report::Request { kind: "feedback".into(), description: body.trim().to_owned(), ..Default::default() };
            match crate::report::submit(&crate::state_dir(), shared, &req) {
                Ok(_) => ("200 OK", "text/plain; charset=utf-8", "ok".to_owned()),
                Err(e) => ("200 OK", "text/plain; charset=utf-8", format!("error: {e}")),
            }
        }
        ("POST", "/api/report/preview") => match serde_json::from_str::<crate::report::Request>(&body) {
            Ok(req) => ("200 OK", "application/json", crate::report::preview(&crate::state_dir(), shared, &req).to_string()),
            Err(e) => ("400 Bad Request", "application/json", serde_json::json!({"error": e.to_string()}).to_string()),
        },
        ("POST", "/api/report") => match serde_json::from_str::<crate::report::Request>(&body) {
            Ok(req) => match crate::report::submit(&crate::state_dir(), shared, &req) {
                Ok(r) => ("200 OK", "application/json", serde_json::json!({"ok": true, "id": r.id, "status": r.status}).to_string()),
                Err(e) => ("200 OK", "application/json", serde_json::json!({"error": e}).to_string()),
            },
            Err(e) => ("400 Bad Request", "application/json", serde_json::json!({"error": e.to_string()}).to_string()),
        },
        ("POST", "/api/report/retry") => match crate::report::retry(&crate::state_dir(), body.trim()) {
            Ok(()) => ("200 OK", "application/json", serde_json::json!({"ok": true}).to_string()),
            Err(e) => ("200 OK", "application/json", serde_json::json!({"error": e}).to_string()),
        },
        ("GET", "/api/report/list") => ("200 OK", "application/json", crate::report::list_json(&crate::state_dir()).to_string()),
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

fn reply(conn: &TcpStream, status: &str, kind: &str, text: &str) -> std::io::Result<()> {
    let mut out = conn;
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )?;
    out.flush()
}

/// A name the speaker can honestly be reached by on the home network: an IP
/// address, localhost, a single-label name, a .local/.lan/.home/.internal/
/// .home.arpa name, or one listed in SOUNDYA_WEB_HOSTS (soundya.env).
fn local_host(host: &str) -> bool {
    let name = if host.starts_with('[') {
        return true; // IPv6 literal
    } else {
        host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host })
    }
    .trim_end_matches('.')
    .to_ascii_lowercase();
    if name.is_empty() || name.parse::<std::net::Ipv4Addr>().is_ok() || name == "localhost" || !name.contains('.') {
        return true;
    }
    if [".local", ".lan", ".home", ".internal", ".home.arpa", ".localdomain"].iter().any(|s| name.ends_with(s)) {
        return true;
    }
    std::env::var("SOUNDYA_WEB_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .any(|h| !h.is_empty() && h == name)
}

/// Checks a state-changing request: the page must come from this very address.
pub fn allowed(host: &str, origin: &str) -> Result<(), String> {
    if !local_host(host) {
        return Err(format!("адрес «{host}» не похож на адрес колонки в домашней сети (добавьте его в SOUNDYA_WEB_HOSTS в soundya.env)"));
    }
    if origin.is_empty() || origin == "null" && host.is_empty() {
        // Not a browser (curl, the PC tray), or a very old one.
        return Ok(());
    }
    let from = origin.split_once("://").map_or(origin, |(_, rest)| rest).trim_end_matches('/');
    if from.eq_ignore_ascii_case(host) {
        Ok(())
    } else {
        Err(format!("запрос со стороннего сайта ({origin}) отклонён"))
    }
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
