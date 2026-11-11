//! Small web page for the local network: playback controls and proxy settings.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

use std::sync::Arc;

use crate::{
    sc::{Client, Kind},
    settings::Settings,
    Cmd, Shared,
};

#[cfg(test)]
mod listen_tests {
    #[test]
    fn listeners_come_and_go() {
        assert_eq!(super::listen("tabA", true), 1);
        assert_eq!(super::listen("tabB", true), 2);
        assert_eq!(super::listen("tabA", true), 2, "a heartbeat is not a new listener");
        assert_eq!(super::listen("tabA", false), 1);
        let g = super::LISTEN_GEN.load(std::sync::atomic::Ordering::Acquire);
        super::stop_listeners();
        assert_eq!(super::listeners(), 0);
        assert_eq!(super::LISTEN_GEN.load(std::sync::atomic::Ordering::Acquire), g + 1);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_panel_installs_on_a_phone() {
        let m: serde_json::Value = serde_json::from_str(super::manifest()).unwrap();
        assert_eq!(m["display"], "standalone");
        assert_eq!(m["start_url"], "/");
        assert!(m["icons"][0]["src"].as_str().unwrap().starts_with("data:image/png;base64,iVBOR"));
        assert!(super::PAGE.contains(r#"<link rel="manifest" href="/manifest.webmanifest">"#));
    }

    #[test]
    fn refusal_reaches_a_client_sending_an_archive() {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let len = 3 << 20;
        let client = std::thread::spawn(move || {
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let body = vec![b'x'; len];
            let _ = c.write_all(&body);
            let mut out = String::new();
            c.read_to_string(&mut out).map(|_| out)
        });
        let (conn, _) = l.accept().unwrap();
        let mut r = super::BufReader::new(&conn);
        super::reply_unread(&conn, &mut r, len, "401 Unauthorized", "error: нужен PIN панели").unwrap();
        drop(r);
        drop(conn);
        let got = client.join().unwrap().expect("the answer, not a connection reset");
        assert!(got.starts_with("HTTP/1.1 401") && got.contains("нужен PIN"), "{got}");
    }

    #[test]
    fn head_lines_are_limited() {
        use std::io::Write as _;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let w = std::thread::spawn(move || {
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let _ = c.write_all(b"GET / HTTP/1.1\r\n");
            let _ = c.write_all(&vec![b'a'; 3 * super::MAX_LINE as usize]);
        });
        let (conn, _) = l.accept().unwrap();
        let mut r = super::BufReader::new(&conn);
        let mut line = String::new();
        assert!(super::read_line_limited(&mut r, &mut line).unwrap());
        assert_eq!(line, "GET / HTTP/1.1\r\n");
        let mut long = String::new();
        assert!(!super::read_line_limited(&mut r, &mut long).unwrap(), "an endless line is refused");
        assert_eq!(long.len() as u64, super::MAX_LINE);
        w.join().unwrap();
    }

    #[test]
    fn only_soundcloud_covers_are_fetched() {
        assert_eq!(super::art_url("/art/i1/artworks-abc_D-t300x300.jpg").as_deref(), Some("https://i1.sndcdn.com/artworks-abc_D-t300x300.jpg"));
        assert_eq!(super::art_url("/art/i9/avatars-x-large.png").as_deref(), Some("https://i9.sndcdn.com/avatars-x-large.png"));
        // The same picture from the other CDN names, its own first.
        assert_eq!(
            super::art_hosts("https://i3.sndcdn.com/artworks-a-t300x300.jpg"),
            [
                "https://i3.sndcdn.com/artworks-a-t300x300.jpg",
                "https://i1.sndcdn.com/artworks-a-t300x300.jpg",
                "https://i2.sndcdn.com/artworks-a-t300x300.jpg",
                "https://i4.sndcdn.com/artworks-a-t300x300.jpg"
            ]
        );
        assert_eq!(super::art_hosts("https://example.com/a.jpg"), ["https://example.com/a.jpg"]);
        for bad in ["/art/i1/", "/art/x1/a.jpg", "/art/i10/a.jpg", "/art/i1/a.jpg?x=1", "/art/i1/../a.jpg", "/art/i1/a/b.jpg",
                    "/art/i1/a.html", "/art/i1/a%2F.jpg", "/art/i1@evil.com/a.jpg", "/art/i1"] {
            assert!(super::art_url(bad).is_none(), "{bad}");
        }
    }


    #[test]
    fn developer_notes_live_on_the_speaker() {
        // In the browser only, they were lost on another address or browser (34 notes once).
        let dir = std::env::temp_dir().join(format!("soundya-dev-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(super::dev_notes("GET", "", &dir).2, "{}");
        let notes = r#"{"notes":[{"id":1,"text":"уменьшить кнопку"}],"goal":"цель"}"#;
        assert_eq!(super::dev_notes("POST", notes, &dir).0, "200 OK");
        assert_eq!(super::dev_notes("GET", "", &dir).2, notes);
        assert_eq!(super::dev_notes("POST", "[1]", &dir).0, "400 Bad Request");
        assert_eq!(super::dev_notes("POST", "не json", &dir).0, "400 Bad Request");
        assert_eq!(super::dev_notes("GET", "", &dir).2, notes, "a bad request keeps what was there");
        let _ = std::fs::remove_dir_all(&dir);
    }

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

/// The panel; in the developer build with the feedback mode (src/devpage.rs).
#[cfg(not(feature = "dev"))]
fn page() -> &'static str {
    static MODDED_PAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MODDED_PAGE.get_or_init(|| {
        crate::mods::inject_loader(PAGE)
    })
}

/// The web app manifest: «На экран Домой» on a phone opens the panel as an app of its own,
/// without the browser's address bar. Its icon is the page's own apple-touch-icon.
fn manifest() -> &'static str {
    static M: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let icon = PAGE
            .split_once(r#"<link rel="apple-touch-icon" href=""#)
            .and_then(|(_, rest)| rest.split_once('"'))
            .map_or("", |(href, _)| href);
        serde_json::json!({
            "name": "soundya",
            "short_name": "soundya",
            "start_url": "/",
            "scope": "/",
            "display": "standalone",
            "background_color": "#141019",
            "theme_color": "#ff5500",
            "icons": [{ "src": icon, "sizes": "180x180", "type": "image/png", "purpose": "any" }],
        })
        .to_string()
    })
}

#[cfg(feature = "dev")]
fn page() -> &'static str {
    static PAGE_DEV: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PAGE_DEV.get_or_init(|| {
        crate::devpage::inject(
            PAGE,
            &serde_json::json!({
                "app": "panel",
                "title": "панель колонки",
                "version": crate::VERSION,
                "device": crate::stats::model(),
                "log": "/api/dev/log",
                // On the speaker, not in the browser: the same notes from any browser and address.
                "store": "/api/dev/notes",
                "h2c": "/dev/html2canvas.js",
            }),
        )
    })
}

/// The developer build's feedback notes, kept on the speaker (dev-notes.json in its
/// folder): GET gives them («{}» when none), POST replaces them with a JSON object.
#[cfg(any(feature = "dev", test))]
fn dev_notes(method: &str, body: &str, dir: &std::path::Path) -> (&'static str, &'static str, String) {
    let file = dir.join("dev-notes.json");
    if method == "GET" {
        return ("200 OK", "application/json", crate::fsx::read_string(&file).unwrap_or_else(|| "{}".into()));
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) if v.is_object() => {
            let tmp = dir.join("dev-notes.json.tmp");
            match crate::settings::write_synced(&tmp, body.as_bytes()).and_then(|()| std::fs::rename(&tmp, &file)) {
                Ok(()) => ("200 OK", "text/plain; charset=utf-8", "ok".into()),
                Err(e) => ("500 Internal Server Error", "text/plain; charset=utf-8", format!("error: {e}")),
            }
        }
        _ => ("400 Bad Request", "text/plain; charset=utf-8", "error: ожидается объект JSON".into()),
    }
}

pub fn serve(port: u16, tx: Sender<Cmd>, shared: Arc<Shared>, client: Client) {
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
            // A thread per connection, but not without end: a slow or hostile
            // client on the network must not use up the speaker's memory.
            if OPEN.load(std::sync::atomic::Ordering::Acquire) >= MAX_CONNECTIONS {
                let _ = reply(&conn, "503 Service Unavailable", "text/plain; charset=utf-8", "error: занято, повторите");
                continue;
            }
            OPEN.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let tx = tx.clone();
            let shared = shared.clone();
            let client = client.clone();
            thread::spawn(move || {
                let _ = handle(conn, &tx, &shared, &client);
                OPEN.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            });
        }
    });
}

/// Browsers listening right now («Слушать здесь»): tab id → last heartbeat.
static LISTENERS: std::sync::Mutex<Option<std::collections::HashMap<String, std::time::Instant>>> = std::sync::Mutex::new(None);
/// Bumped by «Вернуть звук на колонку»: every listening tab stops.
pub static LISTEN_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const LISTEN_TTL: Duration = Duration::from_secs(12);
static STREAMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_STREAMS: usize = 6;

/// How many browsers listen now (expired heartbeats dropped).
pub fn listeners() -> usize {
    let mut g = LISTENERS.lock().unwrap();
    let map = g.get_or_insert_with(Default::default);
    map.retain(|_, at| at.elapsed() < LISTEN_TTL);
    map.len()
}

/// A tab starts (`on`), keeps (`on` again within 12 s) or stops listening.
pub fn listen(id: &str, on: bool) -> usize {
    let id: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).take(40).collect();
    {
        let mut g = LISTENERS.lock().unwrap();
        let map = g.get_or_insert_with(Default::default);
        if on && !id.is_empty() {
            map.insert(id, std::time::Instant::now());
        } else {
            map.remove(&id);
        }
    }
    listeners()
}

/// «Вернуть звук на колонку».
pub fn stop_listeners() {
    LISTENERS.lock().unwrap().get_or_insert_with(Default::default).clear();
    LISTEN_GEN.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

static OPEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_CONNECTIONS: usize = 48;

fn ask(tx: &Sender<Cmd>, make: impl FnOnce(Sender<String>) -> Cmd, wait: u64) -> String {
    let (rtx, rrx) = mpsc::channel();
    if tx.send(make(rtx)).is_err() {
        return "error: player is not running".into();
    }
    rrx.recv_timeout(Duration::from_secs(wait))
        .unwrap_or_else(|_| "error: timeout".into())
}

const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 64;

/// One line of the request head, at most MAX_LINE bytes. False: the line is longer.
fn read_line_limited(reader: &mut BufReader<&TcpStream>, line: &mut String) -> std::io::Result<bool> {
    let n = reader.by_ref().take(MAX_LINE).read_line(line)?;
    Ok(!(n as u64 == MAX_LINE && !line.ends_with('\n')))
}

/// A refusal before the body was read. Small bodies are read away first: closing
/// with unread data sends a reset, and the app would show «связь оборвалась»
/// instead of the reason (for example «нужен PIN»).
/// Read away into nothing (no memory), up to an update archive, for at most 20 s.
fn reply_unread(conn: &TcpStream, reader: &mut BufReader<&TcpStream>, length: usize, status: &str, text: &str) -> std::io::Result<()> {
    if length <= crate::update::UPLOAD_MAX {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut left = length as u64;
        let mut chunk = [0u8; 16 * 1024];
        while left > 0 && std::time::Instant::now() < deadline {
            match reader.read(&mut chunk[..left.min(16 * 1024) as usize]) {
                Ok(0) | Err(_) => break,
                Ok(n) => left -= n as u64,
            }
        }
    }
    reply(conn, status, "text/plain; charset=utf-8", text)
}

fn handle(conn: TcpStream, tx: &Sender<Cmd>, shared: &Shared, client: &Client) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(&conn);
    // Lines and their number are limited: a line without an end must not fill the
    // speaker's memory (48 connections × unlimited before).
    let mut request = String::new();
    if !read_line_limited(&mut reader, &mut request)? {
        return reply(&conn, "431 Request Header Fields Too Large", "text/plain; charset=utf-8", "error: слишком длинный запрос");
    }
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    let mut length = 0usize;
    let mut host = String::new();
    let mut origin = String::new();
    let mut cookie = String::new();
    let mut headers = 0;
    let mut raw_headers = String::new();
    loop {
        let mut line = String::new();
        headers += 1;
        if headers > MAX_HEADERS || !read_line_limited(&mut reader, &mut line)? {
            return reply(&conn, "431 Request Header Fields Too Large", "text/plain; charset=utf-8", "error: слишком длинный запрос");
        }
        if line.is_empty() || line == "\r\n" || line == "\n" {
            break;
        }
        raw_headers.push_str(&line);
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("host") {
                host = v.trim().to_owned();
            } else if k.eq_ignore_ascii_case("origin") {
                origin = v.trim().to_owned();
            } else if k.eq_ignore_ascii_case("cookie") {
                cookie = v.trim().to_owned();
            }
        }
    }
    // Bug reports carry a screenshot; everything else is small.
    // Developer notes carry screenshots too (only the developer build answers that path).
    let limit = if path.starts_with("/api/report") {
        6 << 20
    } else if cfg!(feature = "dev") && path == "/api/dev/notes" {
        8 << 20
    } else if path == "/api/update/upload" {
        crate::update::UPLOAD_MAX
    } else if path == "/api/mods/upload" {
        2 << 20
    } else {
        64 * 1024
    };
    if length > limit {
        return reply(&conn, "413 Payload Too Large", "text/plain; charset=utf-8", "error: слишком большой запрос");
    }
    // Commands only from the panel itself: another site open in the browser
    // (CSRF) or a DNS-rebinding name must not uninstall or reconfigure the speaker.
    if method != "GET" {
        if let Err(why) = allowed(&host, &origin) {
            eprintln!("web: refused {method} {path}: {why}");
            return reply_unread(&conn, &mut reader, length, "403 Forbidden", &format!("error: {why}"));
        }
    } else if !local_host(&host) {
        // Reading too: a DNS-rebinding name would let any site read /api/status
        // (the PC's address, Telegram ids, team emails).
        eprintln!("web: refused GET {path}: host {host}");
        return reply(&conn, "403 Forbidden", "text/plain; charset=utf-8", &format!("error: адрес «{host}» не похож на адрес колонки в домашней сети"));
    }
    // Without the PIN's session the body is not even read (a 6 MB report from anyone).
    if method != "GET" && path != "/api/login" && path != "/api/duck" && crate::auth::enabled() && !crate::auth::session_ok(&cookie) {
        return reply_unread(&conn, &mut reader, length, "401 Unauthorized", "error: нужен PIN панели");
    }
    // An update archive from the Mac/Windows app (its «Обновить» when the speaker's ADB
    // is closed): a few MB straight to a file; installed only after «Алиса, да» at the speaker.
    if method == "POST" && path == "/api/update/upload" {
        if length == 0 || length > crate::update::UPLOAD_MAX {
            return reply(&conn, "413 Payload Too Large", "text/plain; charset=utf-8", "error: архив пустой или слишком большой");
        }
        if let Err(why) = crate::update::begin_upload() {
            return reply_unread(&conn, &mut reader, length, "409 Conflict", &format!("error: {why}"));
        }
        struct Slot;
        impl Drop for Slot {
            fn drop(&mut self) {
                crate::update::end_upload();
            }
        }
        let _slot = Slot;
        conn.set_read_timeout(Some(Duration::from_secs(30)))?;
        let saved = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(crate::update::UPLOAD)?;
            let n = std::io::copy(&mut (&mut reader).take(length as u64), &mut f)?;
            if n as usize != length {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, format!("пришло {n} из {length} байт")));
            }
            f.sync_all()
        })();
        if let Err(e) = saved {
            let _ = std::fs::remove_file(crate::update::UPLOAD);
            return reply(&conn, "400 Bad Request", "text/plain; charset=utf-8", &format!("error: архив дошёл не целиком: {e}"));
        }
        eprintln!("web: update archive received ({length} bytes), asking at the speaker");
        return match shared.updates.offer_uploaded() {
            Ok(text) => reply(&conn, "200 OK", "text/plain; charset=utf-8", &format!("OK: {text}")),
            Err(e) => reply(&conn, "400 Bad Request", "text/plain; charset=utf-8", &format!("error: {e}")),
        };
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();
    // The optional PIN: logging in, and every change needs the session.
    if method == "POST" && path == "/api/login" {
        return match crate::auth::login(&crate::state_dir(), body.trim()) {
            Ok(token) => {
                let mut out = &conn;
                write!(
                    out,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nSet-Cookie: {}={token}; Max-Age=31536000; Path=/; HttpOnly; SameSite=Strict\r\nContent-Length: 2\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nok",
                    crate::auth::COOKIE
                )?;
                out.flush()
            }
            Err(e) => reply(&conn, "403 Forbidden", "text/plain; charset=utf-8", &format!("error: {e}")),
        };
    }

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
    // Covers of playlists and recent tracks: /art/i1/artworks-…-t300x300.jpg is
    // https://i1.sndcdn.com/artworks-…-t300x300.jpg, fetched by the speaker (bypass/proxy).
    if method == "GET" && path.starts_with("/art/") {
        let mut out = &conn;
        let found = art_url(&path).and_then(|url| art(shared, &url));
        return match found {
            Some((kind, bytes)) => {
                write!(
                    out,
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: max-age=604800\r\nConnection: close\r\n\r\n",
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
    // «Слушать здесь»: the playing track piped to the browser through the
    // speaker (it plays even where SoundCloud is blocked for the browser's
    // network). `id` must be the track playing now; `t` (ms) starts it there:
    // an HLS stream from the segment holding `t` (nothing before it is
    // downloaded); MP3 and ADTS then resync on frame headers, so the bytes up to
    // `t` are skipped (by the bitrate SoundCloud uses: 128 / 160 kbit/s); MP4
    // starts at the segment (a progressive MP4 file at 0: `stream_seekable`).
    if method == "GET" && (path == "/api/stream" || path.starts_with("/api/stream?")) {
        let mut out = &conn;
        let query = path.split_once('?').map_or("", |(_, q)| q);
        let param = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).unwrap_or("");
        let track = shared.now_track.lock().unwrap().clone();
        let Some(track) = track else {
            return reply(&conn, "404 Not Found", "text/plain", "nothing playing");
        };
        if !param("id").is_empty() && param("id") != track.id.to_string() {
            return reply(&conn, "409 Conflict", "text/plain", "another track is playing");
        }
        if STREAMS.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_STREAMS {
            STREAMS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            return reply(&conn, "503 Service Unavailable", "text/plain; charset=utf-8", "слишком много слушателей");
        }
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                STREAMS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
        let _release = Release;
        // Not past the end (the whole track would be read just to be thrown away),
        // and no overflow in the byte count below.
        let t: u64 = param("t").parse::<u64>().unwrap_or(0).min(track.duration.unwrap_or(4 * 3600 * 1000));
        match client.open_from(&track, t) {
            Ok((mut src, kind, started)) => {
                let (mime, bytes_per_ms) = match kind {
                    Kind::Mp4 => ("audio/mp4", 0),
                    Kind::Adts => ("audio/aac", 20),
                    _ => ("audio/mpeg", 16),
                };
                let skip = t.saturating_sub(started).saturating_mul(bytes_per_ms);
                if skip > 0 {
                    let _ = std::io::copy(&mut (&mut src).take(skip), &mut std::io::sink());
                }
                let offset = if bytes_per_ms > 0 { t } else { started };
                write!(
                    out,
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nX-Soundya-Offset: {offset}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
                )?;
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match src.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) if out.write_all(&buf[..n]).is_ok() => {}
                        _ => break,
                    }
                }
                return out.flush();
            }
            Err(e) => {
                eprintln!("web: stream: {e}");
                return reply(&conn, "502 Bad Gateway", "text/plain", "cannot open the track");
            }
        }
    }

    // Universal Mod Dynamic API Gateway
    if let Some(mgr) = crate::mods::get_manager() {
        if let Some((sock_path, target_path)) = mgr.find_socket_for_route(&path) {
            let _ = mgr.proxy_unix(&sock_path, &method, &target_path, &raw_headers, &body, &conn);
            return Ok(());
        }
    }

    let (status, kind, reply) = match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => ("200 OK", "text/html; charset=utf-8", page().to_owned()),
        // «/?app=1» (the Mac app's window) and the like: the same page.
        ("GET", p) if p.starts_with("/?") => ("200 OK", "text/html; charset=utf-8", page().to_owned()),
        ("GET", "/manifest.webmanifest") => ("200 OK", "application/manifest+json", manifest().to_owned()),
        // The developer build: its scripts and the end of the log (secrets cut out;
        // POST, so only the panel itself and, with a PIN, only after it).
        #[cfg(feature = "dev")]
        ("GET", p) if crate::devpage::script(p).is_some() => {
            ("200 OK", "text/javascript; charset=utf-8", crate::devpage::script(p).unwrap_or_default().to_owned())
        }
        #[cfg(feature = "dev")]
        (m @ ("GET" | "POST"), "/api/dev/notes") => dev_notes(m, &body, &crate::state_dir()),
        #[cfg(feature = "dev")]
        ("POST", "/api/dev/log") => (
            "200 OK",
            "text/plain; charset=utf-8",
            crate::report::log_tail(&crate::state_dir(), body.trim().parse().unwrap_or(80).clamp(1, 400)),
        ),
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
        // Smart Ducking events from devices (the PC agent, a phone automation…).
        ("POST", "/api/duck") => match serde_json::from_str::<crate::ducking::Request>(&body) {
            Ok(req) => match crate::ducking::event(&req) {
                Ok(v) => ("200 OK", "application/json", v.to_string()),
                Err(e) => ("400 Bad Request", "application/json", serde_json::json!({"error": e}).to_string()),
            },
            Err(e) => ("400 Bad Request", "application/json", serde_json::json!({"error": e.to_string()}).to_string()),
        },
        // Set or remove the PIN (behind the current one); this browser stays signed in.
        ("POST", "/api/pin") => {
            let pin = body.trim().to_owned();
            let r = ask(tx, |r| Cmd::Control(if pin.is_empty() { "pin off".into() } else { format!("pin set {pin}") }, r), 15);
            if r.starts_with("OK") && !pin.is_empty() {
                if let Ok(token) = crate::auth::login(&crate::state_dir(), &pin) {
                    let mut out = &conn;
                    write!(
                        out,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nSet-Cookie: {}={token}; Max-Age=31536000; Path=/; HttpOnly; SameSite=Strict\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{r}",
                        crate::auth::COOKIE,
                        r.len()
                    )?;
                    return out.flush();
                }
            }
            ("200 OK", "text/plain; charset=utf-8", r)
        }
        // «Слушать здесь»: {"id": "<tab>", "on": true} every few seconds; «"all": "stop"» brings the sound back.
        ("POST", "/api/listen") => {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if v["all"] == "stop" {
                stop_listeners();
            } else {
                listen(v["id"].as_str().unwrap_or(""), v["on"].as_bool().unwrap_or(false));
            }
            let n = listeners();
            shared.duck.set_listening(n > 0);
            ("200 OK", "application/json", serde_json::json!({"listeners": n, "gen": LISTEN_GEN.load(std::sync::atomic::Ordering::Acquire)}).to_string())
        }
        ("GET", "/api/community") => (
            "200 OK",
            "application/json",
            crate::stats::community(shared.stats.load(std::sync::atomic::Ordering::Acquire)).to_string(),
        ),
        ("GET", "/api/report/list") => ("200 OK", "application/json", crate::report::list_json(&crate::state_dir()).to_string()),
        ("POST", "/api/test") => {
            let proxy = serde_json::from_str::<Settings>(&body).map(|s| s.proxy).unwrap_or_default();
            ("200 OK", "text/plain; charset=utf-8", ask(tx, |r| Cmd::TestProxy(proxy, r), 40))
        }

        // Mod static assets: /mods/...
        _ if (method == "GET" || method == "HEAD") && path.starts_with("/mods/") => {
            let rel = path.trim_start_matches("/mods/");
            if let Some(mgr) = crate::mods::get_manager() {
                if let Some((status, mime, bytes)) = mgr.serve_static(rel, &method) {
                    let mut out = &conn;
                    write!(
                        out,
                        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )?;
                    if method != "HEAD" {
                        out.write_all(&bytes)?;
                    }
                    return out.flush();
                } else {
                    return reply(&conn, "404 Not Found", "text/plain", "mod file not found");
                }
            } else {
                return reply(&conn, "500 Internal Server Error", "text/plain", "mods not initialized");
            }
        }

        // Mod manager endpoints
        ("GET", "/api/mods") => {
            if let Some(mgr) = crate::mods::get_manager() {
                ("200 OK", "application/json", mgr.list_mods_json())
            } else {
                ("500 Internal Server Error", "text/plain", "mods not initialized".into())
            }
        }
        ("POST", "/api/mods/toggle") => {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                if let (Some(id), Some(enabled)) = (val["id"].as_str(), val["enabled"].as_bool()) {
                    if let Some(mgr) = crate::mods::get_manager() {
                        mgr.toggle_mod(id, enabled);
                        ("200 OK", "application/json", r#"{"status":"ok"}"#.into())
                    } else {
                        ("500 Internal Server Error", "text/plain", "mods not initialized".into())
                    }
                } else {
                    ("400 Bad Request", "text/plain", "missing id or enabled".into())
                }
            } else {
                ("400 Bad Request", "text/plain", "invalid json".into())
            }
        }
        ("POST", "/api/mods/delete") => {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(id) = val["id"].as_str() {
                    if let Some(mgr) = crate::mods::get_manager() {
                        mgr.delete_mod(id);
                        ("200 OK", "application/json", r#"{"status":"ok"}"#.into())
                    } else {
                        ("500 Internal Server Error", "text/plain", "mods not initialized".into())
                    }
                } else {
                    ("400 Bad Request", "text/plain", "missing id".into())
                }
            } else {
                ("400 Bad Request", "text/plain", "invalid json".into())
            }
        }
        ("POST", "/api/mods/reset") => {
            if let Some(mgr) = crate::mods::get_manager() {
                mgr.reset_all();
                ("200 OK", "application/json", r#"{"status":"ok","message":"all mods reset"}"#.into())
            } else {
                ("500 Internal Server Error", "text/plain", "mods not initialized".into())
            }
        }
        ("POST", "/api/mods/upload") => {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                let filename = val["filename"].as_str().unwrap_or("");
                let bytes = if let Some(b64) = val["base64"].as_str() {
                    crate::mods::decode_base64(b64)
                } else if let Some(content) = val["content"].as_str() {
                    Some(content.as_bytes().to_vec())
                } else {
                    None
                };

                if let (false, Some(data)) = (filename.is_empty(), bytes) {
                    if let Some(mgr) = crate::mods::get_manager() {
                        match mgr.handle_upload(filename, &data) {
                            Ok(msg) => ("200 OK", "application/json", serde_json::json!({"status": "ok", "message": msg}).to_string()),
                            Err(e) => ("500 Internal Server Error", "text/plain", e),
                        }
                    } else {
                        ("500 Internal Server Error", "text/plain", "mods not initialized".into())
                    }
                } else {
                    ("400 Bad Request", "text/plain", "missing filename or valid content/base64".into())
                }
            } else {
                ("400 Bad Request", "text/plain", "invalid json".into())
            }
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

/// /art/<i0…i9>/<file>.jpg|png → the image on SoundCloud's CDN; anything else is refused
/// (the speaker fetches only covers, never an address given by the page).
fn art_url(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/art/")?;
    let (host, file) = rest.split_once('/')?;
    let host_ok = host.len() == 2 && host.starts_with('i') && host.as_bytes()[1].is_ascii_digit();
    let file_ok = !file.is_empty()
        && file.len() <= 160
        && file.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !file.contains("..")
        && (file.ends_with(".jpg") || file.ends_with(".png"));
    (host_ok && file_ok).then(|| format!("https://{host}.sndcdn.com/{file}"))
}

/// Covers fetched lately (address, type, bytes), newest last; at most ~3 MB.
static ARTS: std::sync::Mutex<Vec<(String, String, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());

/// One agent for the covers (kept while the proxy setting is the same): its connections
/// to the CDN are reused, a shelf of covers no longer makes a TLS handshake per picture.
fn art_agent(shared: &Shared) -> Option<ureq::Agent> {
    // Keyed by the bypass strategy too: switching it changes the agent but not the proxy.
    type Kept = Option<(String, Option<crate::bypass::Strategy>, ureq::Agent)>;
    static AGENT: std::sync::Mutex<Kept> = std::sync::Mutex::new(None);
    let proxy = shared.proxy.lock().unwrap().clone();
    let strategy = crate::bypass::active();
    let mut g = AGENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((p, st, a)) = &*g {
        if *p == proxy && *st == strategy {
            return Some(a.clone());
        }
    }
    let a = crate::sc::build_agent(&proxy).ok()?;
    *g = Some((proxy, strategy, a.clone()));
    Some(a)
}

/// The same picture is on i1…i4.sndcdn.com; the provider may filter one name and not
/// another (owner's speaker: i1 stuck for 15 s, i2 and i4 answered in 0.1 s).
fn art_hosts(url: &str) -> Vec<String> {
    let Some(rest) = url.strip_prefix("https://") else { return vec![url.to_owned()] };
    let Some((host, path)) = rest.split_once('/') else { return vec![url.to_owned()] };
    let n = host.strip_suffix(".sndcdn.com").and_then(|h| h.strip_prefix('i')).filter(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit());
    let Some(n) = n else { return vec![url.to_owned()] };
    let mut out = vec![url.to_owned()];
    for d in ["1", "2", "3", "4"] {
        if d != n {
            out.push(format!("https://i{d}.sndcdn.com/{path}"));
        }
    }
    out
}

/// Pictures that could not be fetched lately: asked again only after a while
/// (every status poll asked again, and each try could hang for 15 s).
static ART_MISSES: std::sync::Mutex<Vec<(String, std::time::Instant)>> = std::sync::Mutex::new(Vec::new());
/// Cover downloads under way: spare CDN names join only while few are, so a shelf of
/// covers does not crowd out the music's own connections (the track hung at 0:00).
static ART_INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const ART_SPARE_BELOW: usize = 6;
const ART_MISS_FOR: Duration = Duration::from_secs(120);

/// A cover from the CDN: its own host first; when it has not answered in 1.5 s the next
/// one starts alongside, and so on — the first picture wins. (One try that hung held a
/// cover for 13 s on the owner's speaker, while the next name answered in 0.1 s.)
/// (type, bytes up to `max`); one line in the log when none gave it.
fn fetch_art(agent: &ureq::Agent, url: &str, max: u64) -> Option<(String, Vec<u8>)> {
    {
        let mut misses = ART_MISSES.lock().unwrap();
        misses.retain(|(_, at)| at.elapsed() < ART_MISS_FOR);
        if misses.iter().any(|(u, _)| u == url) {
            return None;
        }
    }
    let (tx, rx) = std::sync::mpsc::channel::<Result<(String, Vec<u8>), String>>();
    let hosts = art_hosts(url);
    let mut started = 0;
    let mut done = 0;
    let mut last = String::new();
    let start = |u: String| {
        let (agent, tx) = (agent.clone(), tx.clone());
        ART_INFLIGHT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        thread::spawn(move || {
            struct Done;
            impl Drop for Done {
                fn drop(&mut self) {
                    ART_INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
            }
            let _done = Done;
            let r = match agent.get(&u).timeout(Duration::from_secs(8)).call() {
                Ok(resp) => {
                    let kind = resp.header("Content-Type").filter(|k| k.starts_with("image/")).unwrap_or("image/jpeg").to_owned();
                    let mut bytes = Vec::new();
                    match resp.into_reader().take(max).read_to_end(&mut bytes) {
                        Ok(_) if !bytes.is_empty() => Ok((kind, bytes)),
                        Ok(_) => Err("empty".to_owned()),
                        Err(e) => Err(e.to_string()),
                    }
                }
                Err(ureq::Error::Status(code, _)) => Err(format!("{code}")),
                Err(e) => {
                    crate::dns::failed(&u);
                    Err(e.to_string())
                }
            };
            let _ = tx.send(r);
        });
    };
    start(hosts[0].clone());
    started += 1;
    while done < started {
        match rx.recv_timeout(Duration::from_millis(1500)) {
            Ok(Ok(found)) => return Some(found),
            Ok(Err(e)) => {
                done += 1;
                let gone = e == "404";
                last = e;
                if gone {
                    break;
                }
                if started < hosts.len() {
                    start(hosts[started].clone());
                    started += 1;
                }
            }
            Err(_) if started < hosts.len() && ART_INFLIGHT.load(std::sync::atomic::Ordering::Acquire) < ART_SPARE_BELOW => {
                start(hosts[started].clone());
                started += 1;
            }
            Err(_) => {}
        }
    }
    eprintln!("web: cover not loaded from i1–i4.sndcdn.com: {}", last.chars().take(160).collect::<String>());
    let mut misses = ART_MISSES.lock().unwrap();
    misses.push((url.to_owned(), std::time::Instant::now()));
    if misses.len() > 200 {
        misses.remove(0);
    }
    None
}

fn art(shared: &Shared, url: &str) -> Option<(String, Vec<u8>)> {
    if let Some((_, kind, bytes)) = ARTS.lock().unwrap().iter().find(|a| a.0 == url) {
        return Some((kind.clone(), bytes.clone()));
    }
    let agent = art_agent(shared)?;
    let (kind, bytes) = fetch_art(&agent, url, 512 << 10)?;
    let mut arts = ARTS.lock().unwrap();
    arts.push((url.to_owned(), kind.clone(), bytes.clone()));
    while arts.iter().map(|a| a.2.len()).sum::<usize>() > 3 << 20 {
        arts.remove(0);
    }
    Some((kind, bytes))
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
    // The covers' own agent (kept connections, the bypass) and the other CDN hosts:
    // a new agent per cover went to the provider's stub again and hung for 15 s.
    let agent = art_agent(shared)?;
    let (kind, bytes) = fetch_art(&agent, &url, 3 << 20)?;
    *COVER.lock().unwrap() = Some((url, kind.clone(), bytes.clone()));
    Some((kind, bytes))
}
