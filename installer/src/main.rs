//! soundya installer for Windows and macOS: one file with the speaker build
//! inside. It opens a step-by-step wizard in the browser (served from
//! 127.0.0.1 only), finds the speaker on the network, uploads soundya with its
//! own ADB client (src/adb.rs — no adb to install), runs the speaker's
//! install.sh (backup, check, rollback) and walks through the first settings.
//! On Windows it can also set up this PC (pc/install.ps1) and fill the PC's
//! address, key and MAC into the speaker by itself; on a Mac the same binary
//! is the PC agent (`soundya agent`, agent.rs) started by launchd.

#![cfg_attr(windows, windows_subsystem = "console")]

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod agent;
#[cfg(target_os = "macos")]
mod macwin;
#[cfg(feature = "dev")]
mod dev;
#[cfg(feature = "dev")]
#[path = "../../src/devpage.rs"]
mod devpage;
#[path = "../../src/sha256.rs"]
#[allow(dead_code)]
mod sha256;
#[path = "../../src/adb.rs"]
#[allow(dead_code)]
mod adb;
#[path = "../../src/fsx.rs"]
#[allow(dead_code)]
mod fsx;

/// adb.rs uses crate::pc::base64 for the public key.
mod pc {
    pub fn base64(data: &[u8]) -> String {
        const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        out
    }
}

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
    sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

/// The speaker build (scripts/build-installer.sh puts it there before building).
#[cfg(not(feature = "dev"))]
static BUILT_IN: &[u8] = include_bytes!("../../dist/soundya-armv7.tar");
/// The developer build installs the speaker's developer build (scripts/build-mac.sh --dev).
#[cfg(feature = "dev")]
static BUILT_IN: &[u8] = include_bytes!("../../dist/soundya-armv7-dev.tar");

/// The speaker's package; tests give another one (an x86 build) with SOUNDYA_INSTALLER_TAR.
fn tar() -> &'static [u8] {
    static T: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    match std::env::var("SOUNDYA_INSTALLER_TAR") {
        Ok(p) => T.get_or_init(|| std::fs::read(p).unwrap_or_else(|_| BUILT_IN.to_vec())),
        Err(_) => BUILT_IN,
    }
}
static PAGE: &str = include_str!("wizard.html");
static CARGO: &str = include_str!("../../Cargo.toml");
#[cfg(windows)]
static PC_FILES: [(&str, &str); 3] = [
    ("install.ps1", include_str!("../../pc/install.ps1")),
    ("soundya-pc.ps1", include_str!("../../pc/soundya-pc.ps1")),
    ("soundya-pc-tray.ps1", include_str!("../../pc/soundya-pc-tray.ps1")),
];

fn version() -> String {
    let v = CARGO.lines().find_map(|l| l.strip_prefix("version = \"")).and_then(|v| v.strip_suffix('"')).unwrap_or("?");
    if cfg!(feature = "dev") { format!("{v}+dev") } else { v.to_owned() }
}

/// The wizard; in the developer build with the feedback mode.
fn page() -> &'static str {
    #[cfg(feature = "dev")]
    {
        static P: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        P.get_or_init(|| dev::page(PAGE, &version()))
    }
    #[cfg(not(feature = "dev"))]
    PAGE
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Where the installer keeps its ADB key.
fn data_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var("APPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir())
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME").map(|h| PathBuf::from(h).join("Library/Application Support")).unwrap_or_else(|_| std::env::temp_dir())
    } else {
        std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")).unwrap_or_else(|_| std::env::temp_dir())
    };
    let d = base.join("soundya-installer");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn key() -> Result<adb::Key, String> {
    adb::Key::load_or_create(&data_dir()).map_err(|e| format!("не создался ключ ADB: {e}"))
}

// ---------------------------------------------------------------- the speaker

const PROBE: &str = "id -u; [ -d /system/vendor/quasar ] && echo QUASAR; echo HOST=$(cat /proc/sys/kernel/hostname); \
    echo MODEL=$(tr -d '\\0' < /proc/device-tree/model 2>/dev/null); \
    echo SOUNDYA=$(/data/opt/soundya/soundya version 2>/dev/null | head -1); \
    [ -f /data/opt/soundya/config.json ] && echo CONFIG";

/// What a speaker at `ip` is: None if no root ADB there.
fn probe(ip: &str, key: &adb::Key) -> Option<Value> {
    let out = adb::shell(ip, key, PROBE, Duration::from_secs(2)).ok()?;
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    let get = |k: &str| lines.iter().find_map(|l| l.strip_prefix(k)).unwrap_or("").trim().to_owned();
    let root = lines.first() == Some(&"0");
    let host = get("HOST=");
    let quasar = lines.contains(&"QUASAR") || host.to_lowercase().contains("yandex");
    if !quasar {
        return None;
    }
    let soundya = get("SOUNDYA=").trim_start_matches("soundya ").to_owned();
    Some(json!({
        "ip": ip,
        "root": root,
        "host": host,
        "model": model_name(&format!("{host} {}", get("MODEL="))),
        "soundya": (!soundya.is_empty()).then_some(soundya),
        "configured": lines.contains(&"CONFIG"),
    }))
}

fn model_name(raw: &str) -> String {
    let m = raw.to_lowercase().replace(['_', '-', ' '], "");
    for (k, v) in [
        ("yandexmini2", "Станция Мини 2"),
        ("mini2", "Станция Мини 2"),
        ("yandexmini3", "Станция Мини 3"),
        ("bergamot", "Станция Мини 3"),
        ("yandexmicro", "Станция Лайт"),
        ("micro", "Станция Лайт"),
        // What soundya's panel reports (device.model).
        ("stationlite", "Станция Лайт"),
        ("stationmini3", "Станция Мини 3"),
        ("stationmini2", "Станция Мини 2"),
        ("yandexmidi", "Станция Миди"),
        ("yandexstation2", "Станция 2"),
        ("plum", "Станция (Plum)"),
        ("yandexmini", "Станция Мини"),
        ("yandexstation", "Яндекс Станция"),
    ] {
        if m.contains(k) {
            return v.to_owned();
        }
    }
    "Яндекс Станция".to_owned()
}

/// The address this computer would use towards `target` (no packet is sent).
fn route_ip(target: &str) -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(target).ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_loopback() && !a.ip().is_unspecified() => Some(*a.ip()),
        _ => None,
    }
}

/// This computer's IPv4 addresses (address, point-to-point: a VPN tunnel and the
/// like), loopback left out. From the system's interface list (getifaddrs).
#[cfg(unix)]
fn interfaces() -> Vec<(Ipv4Addr, bool)> {
    use std::ffi::c_void;
    #[repr(C)]
    struct IfAddrs {
        next: *mut IfAddrs,
        name: *const std::ffi::c_char,
        flags: std::ffi::c_uint,
        addr: *const u8,
        netmask: *const u8,
        dst: *const u8,
        data: *mut c_void,
    }
    extern "C" {
        fn getifaddrs(out: *mut *mut IfAddrs) -> i32;
        fn freeifaddrs(list: *mut IfAddrs);
    }
    const IFF_UP: u32 = 0x1;
    const IFF_LOOPBACK: u32 = 0x8;
    const IFF_POINTOPOINT: u32 = 0x10;
    let mut out = Vec::new();
    let mut list: *mut IfAddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list that is only read here and freed once.
    unsafe {
        if getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let i = &*cur;
            cur = i.next;
            if i.addr.is_null() || i.flags & IFF_UP == 0 || i.flags & IFF_LOOPBACK != 0 {
                continue;
            }
            // sockaddr_in: the family is byte 1 on BSD/macOS (after sa_len), a u16 on Linux; the address at 4..8.
            let family = if cfg!(any(target_os = "macos", target_os = "freebsd")) {
                u16::from(*i.addr.add(1))
            } else {
                u16::from_ne_bytes([*i.addr, *i.addr.add(1)])
            };
            if family != 2 {
                continue; // not AF_INET
            }
            let ip = Ipv4Addr::new(*i.addr.add(4), *i.addr.add(5), *i.addr.add(6), *i.addr.add(7));
            out.push((ip, i.flags & IFF_POINTOPOINT != 0));
        }
        freeifaddrs(list);
    }
    out
}

#[cfg(not(unix))]
fn interfaces() -> Vec<(Ipv4Addr, bool)> {
    Vec::new()
}

/// The /24 networks to look in, from this computer's addresses (address,
/// point-to-point), in order: home networks only (private addresses), VPN
/// tunnels left out, each once, at most four.
fn subnets(addrs: &[(Ipv4Addr, bool)]) -> Vec<[u8; 3]> {
    let mut out: Vec<[u8; 3]> = Vec::new();
    for (ip, p2p) in addrs {
        if *p2p || !ip.is_private() {
            continue;
        }
        let [a, b, c, _] = ip.octets();
        if !out.contains(&[a, b, c]) {
            out.push([a, b, c]);
        }
    }
    out.truncate(4);
    out
}

/// This computer's addresses to search from: its interfaces, then the ones it
/// would use towards the usual home routers, then towards the internet. (The
/// route to the internet alone pointed into a VPN tunnel: 10.8.1.0/24 was
/// searched while the speaker sat in 192.168.0.0/24.)
fn search_from() -> Vec<(Ipv4Addr, bool)> {
    let mut all = interfaces();
    for t in ["192.168.0.1:9", "192.168.1.1:9", "192.168.31.1:9", "192.168.88.1:9", "192.168.100.1:9", "10.0.0.1:9", "10.0.1.1:9", "172.16.0.1:9", "8.8.8.8:53"] {
        if let Some(ip) = route_ip(t) {
            let p2p = all.iter().any(|(a, p)| *a == ip && *p);
            all.push((ip, p2p));
        }
    }
    all
}

/// The computer's address on the home network, for the page.
fn own_ip() -> Option<Ipv4Addr> {
    subnets(&search_from()).first().and_then(|n| {
        search_from().into_iter().map(|(ip, _)| ip).find(|ip| ip.octets()[..3] == n[..])
    })
}

/// Every speaker with root ADB in this computer's home networks (each /24).
/// Connections at once while scanning (well under macOS's 256 open files).
const SCAN_WORKERS: usize = 128;

fn scan() -> Value {
    let nets = subnets(&search_from());
    if nets.is_empty() {
        return json!({ "error": "компьютер не в домашней сети" });
    }
    let hits = Arc::new(Mutex::new(Vec::new()));
    // A bounded pool, one socket per worker: a Mac app started from Finder may open
    // only 256 files, and 254 connects per subnet at once failed past that (EMFILE),
    // so a second subnet (Wi-Fi plus a VM bridge) or its last addresses were skipped.
    let queue: Arc<Mutex<Vec<(u8, u8, u8, u8)>>> =
        Arc::new(Mutex::new(nets.iter().flat_map(|&[a, b, c]| (1..=254u8).map(move |d| (a, b, c, d))).collect()));
    let threads: Vec<_> = (0..SCAN_WORKERS)
        .map(|_| {
            let (hits, queue) = (hits.clone(), queue.clone());
            thread::spawn(move || loop {
                let Some((a, b, c, d)) = queue.lock().unwrap().pop() else { break };
                // ADB, or soundya's panel (ADB is often closed after the speaker restarts).
                let adb = SocketAddr::from(([a, b, c, d], 5555));
                let panel = SocketAddr::from(([a, b, c, d], 8765));
                let on_adb = TcpStream::connect_timeout(&adb, Duration::from_millis(700)).is_ok();
                if on_adb || TcpStream::connect_timeout(&panel, Duration::from_millis(700)).is_ok() {
                    hits.lock().unwrap().push((format!("{a}.{b}.{c}.{d}"), on_adb));
                }
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    let key = match key() {
        Ok(k) => Arc::new(k),
        Err(e) => return json!({ "error": e }),
    };
    let found = Arc::new(Mutex::new(Vec::new()));
    let probes: Vec<_> = hits
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .map(|(ip, on_adb)| {
            let (key, found) = (key.clone(), found.clone());
            thread::spawn(move || {
                if let Some(v) = on_adb.then(|| probe(&ip, &key)).flatten().or_else(|| panel_probe(&ip)) {
                    found.lock().unwrap().push(v);
                }
            })
        })
        .collect();
    for t in probes {
        let _ = t.join();
    }
    let mut list = found.lock().unwrap().clone();
    list.sort_by_key(|v| v["ip"].as_str().unwrap_or("").to_owned());
    let subnet = nets.iter().map(|[a, b, c]| format!("{a}.{b}.{c}.0/24")).collect::<Vec<_>>().join(", ");
    json!({ "devices": list, "subnet": subnet })
}

/// A speaker found by its soundya panel, not ADB: updated through soundya («adb»: false).
fn panel_probe(ip: &str) -> Option<Value> {
    let (code, body) = speaker_http(ip, "GET", "/api/status", "").ok()?;
    if code != 200 {
        return None;
    }
    panel_device(ip, &serde_json::from_str::<Value>(&body).ok()?)
}

fn panel_device(ip: &str, st: &Value) -> Option<Value> {
    let version = st["device"]["version"].as_str().or_else(|| st["update"]["current"].as_str())?;
    Some(json!({
        "ip": ip,
        "root": true,
        "model": model_name(st["device"]["model"].as_str().unwrap_or("")),
        "soundya": version,
        "configured": true,
        "adb": false,
        // soundya from before network updates answers «слишком большой запрос» to the archive.
        "net_update": st["net_update"].as_bool().unwrap_or(false),
    }))
}

/// soundya's version if its panel answers at ip; else why not, in words.
fn panel_online(ip: &str) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(700));
        }
        match speaker_http(ip, "GET", "/api/status", "") {
            Ok((200, body)) => {
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                return Ok(v["device"]["version"].as_str().or_else(|| v["update"]["current"].as_str()).unwrap_or("").to_owned());
            }
            Ok((code, _)) => last = format!("страница колонки ответила {code}"),
            Err(e) => last = e,
        }
    }
    Err(offline_reason(&last))
}

/// The error in words a person can act on.
fn offline_reason(err: &str) -> String {
    let e = err.to_lowercase();
    if e.contains("no route to host") || e.contains("os error 65") {
        // macOS 15 answers so when the app may not use the local network.
        "macOS не пускает soundya в домашнюю сеть: Системные настройки → Конфиденциальность и безопасность → Локальная сеть → включите soundya".into()
    } else if e.contains("refused") || e.contains("os error 61") {
        "колонка в сети, но soundya на ней не запущена (перезагрузите колонку)".into()
    } else if e.contains("timed out") || e.contains("os error 60") || e.contains("host is down") || e.contains("os error 64") {
        "колонка не отвечает — включена ли она, тот ли Wi-Fi?".into()
    } else {
        format!("колонка не отвечает: {err}")
    }
}

/// A plain HTTP request to the speaker's panel (port 8765).
fn speaker_http(ip: &str, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    let addr: SocketAddr = format!("{ip}:8765").parse().map_err(|_| "неверный адрес".to_owned())?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| format!("панель колонки не отвечает: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(20))).ok();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {ip}:8765\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    let code = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
    Ok((code, body))
}

/// The panel PIN → the session cookie («name=value») for the next requests.
fn speaker_login(ip: &str, pin: &str) -> Result<String, String> {
    let addr: SocketAddr = format!("{ip}:8765").parse().map_err(|_| "неверный адрес".to_owned())?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| format!("панель колонки не отвечает: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(s, "POST /api/login HTTP/1.1\r\nHost: {ip}:8765\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{pin}", pin.len()).map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    cookie_of(&out).ok_or_else(|| "PIN не подошёл".to_owned())
}

/// «Set-Cookie: soundya_session=abc; Max-Age=…» in an answer → «soundya_session=abc».
fn cookie_of(answer: &str) -> Option<String> {
    let head = answer.split("\r\n\r\n").next()?;
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("set-cookie").then(|| v.trim().split(';').next().unwrap_or("").trim().to_owned())
    }).filter(|c| c.contains('='))
}

/// The update archive to soundya itself (when the speaker's ADB is closed); the speaker
/// then asks out loud and installs only after «Алиса, да». (code, answer).
fn speaker_upload(ip: &str, cookie: &str, data: &[u8], progress: impl FnMut(usize)) -> Result<(u16, String), String> {
    let addr: SocketAddr = format!("{ip}:8765").parse().map_err(|_| "неверный адрес".to_owned())?;
    upload_at(addr, cookie, data, progress)
}

fn upload_at(addr: SocketAddr, cookie: &str, data: &[u8], mut progress: impl FnMut(usize)) -> Result<(u16, String), String> {
    let ip = addr.ip();
    let port = addr.port();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| format!("панель колонки не отвечает: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok();
    s.set_write_timeout(Some(Duration::from_secs(60))).ok();
    let cookie = if cookie.is_empty() { String::new() } else { format!("Cookie: {cookie}\r\n") };
    write!(
        s,
        "POST /api/update/upload HTTP/1.1\r\nHost: {ip}:{port}\r\nContent-Type: application/octet-stream\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    )
    .map_err(|e| e.to_string())?;
    let mut sent = 0;
    let mut broken = None;
    for chunk in data.chunks(64 * 1024) {
        // The speaker may answer and close before the end (413 from an older soundya):
        // its answer, not «Broken pipe», says what happened.
        if let Err(e) = s.write_all(chunk) {
            broken = Some(e);
            break;
        }
        sent += chunk.len();
        progress(sent);
    }
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let out = String::from_utf8_lossy(&raw).into_owned();
    if let (Some(e), false) = (&broken, out.starts_with("HTTP/")) {
        return Err(format!("связь с колонкой оборвалась: {e}"));
    }
    let code = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map_or("", |(_, b)| b).trim().to_owned();
    Ok((code, body))
}

// ---------------------------------------------------------------- install job

#[derive(Default)]
struct Job {
    running: bool,
    phase: String,
    percent: u32,
    log: Vec<String>,
    /// "ok" | "rolledback" | "failed"
    result: Option<String>,
    reason: String,
    version: String,
}

fn job_json(j: &Job) -> Value {
    json!({ "running": j.running, "phase": j.phase, "percent": j.percent, "log": j.log,
            "result": j.result, "reason": j.reason, "version": j.version })
}

/// The speaker soundya was installed on from this computer: next time the app opens
/// as a launcher for it (panel, update, this Mac) instead of the install wizard.
fn speaker_file(dir: &std::path::Path) -> PathBuf {
    dir.join("speaker.json")
}

fn remember_speaker(dir: &std::path::Path, ip: &str, model: &str, version: &str) -> Result<(), String> {
    if ip.parse::<Ipv4Addr>().is_err() {
        return Err("неверный адрес".into());
    }
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).take(60).collect::<String>();
    let v = json!({ "ip": ip, "model": clean(model), "version": clean(version), "at": now() });
    let tmp = speaker_file(dir).with_extension("json.tmp");
    std::fs::write(&tmp, v.to_string()).and_then(|_| std::fs::rename(&tmp, speaker_file(dir))).map_err(|e| e.to_string())
}

fn remembered_speaker(dir: &std::path::Path) -> Option<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(speaker_file(dir)).ok()?).ok()?;
    v["ip"].as_str()?.parse::<Ipv4Addr>().ok()?;
    Some(v)
}

/// What install.sh is run as, on the speaker.
const RUN_INSTALL: &str = "sh /tmp/soundya/install.sh > /tmp/soundya-install.log 2>&1; touch /tmp/soundya-install.done";

/// Where install.sh is, from its own log lines: (percent, what is happening).
fn progress_of(lines: &[&str]) -> (u32, &'static str) {
    let has = |p: &str| lines.iter().any(|l| l.contains(p));
    if has("update: ok") || has("Installed soundya") {
        (96, "Готово, проверяю…")
    } else if has("rolling back") || has("update: rolledback") {
        (90, "Новая версия не запустилась — возвращаю прежнюю…")
    } else if has("soundya started") {
        (85, "Жду, пока soundya запустится и скажет, что готова…")
    } else if has("soundya stopped") {
        (78, "Заменяю программу…")
    } else if has("backup:") {
        (74, "Сохранил копию программы и настроек…")
    } else if has("package:") {
        (70, "Архив проверен, делаю копию настроек…")
    } else {
        (66, "Запускаю установку на колонке…")
    }
}

fn install(ip: String, features: Option<Value>, job: Arc<Mutex<Job>>) {
    // A result written before this moment is not this install's.
    let install_began = now();
    let set = |phase: &str, pct: u32| {
        let mut j = job.lock().unwrap();
        j.phase = phase.to_owned();
        j.percent = pct;
        j.log.push(phase.to_owned());
    };
    let fail = |why: String| {
        let mut j = job.lock().unwrap();
        j.running = false;
        j.result = Some("failed".into());
        j.log.push(format!("Ошибка: {why}"));
        j.reason = why;
    };
    let key = match key() {
        Ok(k) => k,
        Err(e) => return fail(e),
    };
    set("Загружаю soundya на колонку…", 2);
    let total = tar().len();
    let mut pushed = false;
    for attempt in 1..=4 {
        let r = adb::push(&ip, &key, tar(), "/tmp/soundya-armv7.tar", |sent| {
            job.lock().unwrap().percent = 2 + (sent as u64 * 60 / total.max(1) as u64) as u32;
        });
        // A push the speaker answered OKAY can still leave no file (seen on a Station Lite
        // after a broken first try): the size on the speaker decides, else one more try.
        let problem = match r {
            Ok(()) => {
                let size = adb::shell(&ip, &key, "wc -c < /tmp/soundya-armv7.tar 2>/dev/null || echo 0", Duration::from_secs(5)).unwrap_or_default();
                let size = size.trim().lines().last().unwrap_or("").trim().to_owned();
                if size == total.to_string() {
                    pushed = true;
                    break;
                }
                format!("файл дошёл не целиком ({} из {total} байт)", if size.is_empty() { "0" } else { size.as_str() })
            }
            Err(e) => format!("связь оборвалась ({e})"),
        };
        if attempt < 4 {
            job.lock().unwrap().log.push(format!("{problem}, попытка {} из 4", attempt + 1));
            thread::sleep(Duration::from_secs(2));
        } else {
            return fail(format!("не удалось загрузить файл на колонку за 4 попытки: {problem}. Перезагрузите колонку (выдерните на 10 секунд) и запустите установку снова"));
        }
    }
    if !pushed {
        return fail("не удалось загрузить файл на колонку".into());
    }
    if let Some(f) = features {
        let on = |k: &str| if f[k].as_bool().unwrap_or(true) { 1 } else { 0 };
        // «Что показывать на главной»: widget names only (letters), in the chosen order.
        let home: Vec<String> = f["home"].as_array().map(|a| {
            a.iter().filter_map(|w| w.as_str()).filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase())).map(str::to_owned).collect()
        }).unwrap_or_default();
        let text = format!(
            "soundcloud={}\\nairplay={}\\npc={}\\ntv={}\\nbypass={}\\n{}",
            on("soundcloud"), on("airplay"), on("pc"), on("tv"), on("bypass"),
            if home.is_empty() { String::new() } else { format!("home={}\\n", home.join(",")) }
        );
        let _ = adb::shell(&ip, &key, &format!("printf '{text}' > /tmp/soundya-features"), Duration::from_secs(5));
    }
    set("Устанавливаю: копия настроек, замена программы, проверка запуска…", 64);
    // install.sh runs on the speaker by itself, so a dropped ADB link does not
    // break it. The ADB «shell:» service runs commands on a terminal: when the
    // session ends, the terminal's process group gets SIGHUP — a background job
    // that has not reached nohup/setsid yet dies with it, silently (0.5.00
    // betas «hung» at 95 % because of this). So SIGHUP is ignored before the
    // job starts (an ignored signal stays ignored across exec), and the log
    // must exist before the session closes.
    let started = adb::shell_for(
        &ip,
        &key,
        &format!(
            "cd /tmp && rm -rf /tmp/soundya /tmp/soundya-install.done /tmp/soundya-install.log && tar -xf soundya-armv7.tar && \
             [ -f /tmp/soundya/install.sh ] && {{ trap '' HUP; (trap '' HUP; nohup setsid sh -c '{RUN_INSTALL}' </dev/null >/dev/null 2>&1 &); \
             i=0; while [ $i -lt 10 ] && [ ! -e /tmp/soundya-install.log ]; do sleep 1; i=$((i+1)); done; \
             if [ -e /tmp/soundya-install.log ]; then echo STARTED; else echo NOTSTARTED; fi; }}"
        ),
        Duration::from_secs(10),
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| format!("error: {e}"));
    let foreground = if started.contains("NOTSTARTED") {
        true
    } else if started.contains("STARTED") {
        false
    } else {
        return fail(format!("архив не распаковался на колонке: {}", started.trim()));
    };
    if foreground {
        // The background start did not take: run it in this session and wait (up to 6 minutes).
        job.lock().unwrap().log.push("Фоновый запуск не удался — устанавливаю в обычном режиме, не закрывайте окно…".into());
        let out = adb::shell_for(
            &ip,
            &key,
            "sh /tmp/soundya/install.sh > /tmp/soundya-install.log 2>&1; echo __RC=$?; touch /tmp/soundya-install.done; cat /tmp/soundya-install.log",
            Duration::from_secs(10),
            Duration::from_secs(360),
        );
        let mut j = job.lock().unwrap();
        match out {
            Ok(o) => {
                for l in o.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("__RC=")) {
                    j.log.push(l.trim().to_owned());
                }
            }
            Err(e) => j.log.push(format!("связь с колонкой оборвалась ({e}); проверяю результат…")),
        }
    } else {
        // Follow its log; the bar moves with install.sh's own steps.
        let t0 = Instant::now();
        let mut seen = 0usize;
        let mut done = false;
        let mut misses = 0;
        let mut last_new = Instant::now();
        while t0.elapsed() < Duration::from_secs(420) {
            thread::sleep(Duration::from_secs(2));
            let out = match adb::shell(
                &ip,
                &key,
                "cat /tmp/soundya-install.log 2>/dev/null; [ -f /tmp/soundya-install.done ] && echo __DONE__; \
                 ps >/dev/null 2>&1 && echo __PS__; ps 2>/dev/null | grep -q '[i]nstall.sh' && echo __ALIVE__; true",
                Duration::from_secs(5),
            ) {
                Ok(o) => {
                    misses = 0;
                    o
                }
                Err(e) => {
                    misses += 1;
                    if misses == 3 {
                        job.lock().unwrap().log.push(format!("колонка не отвечает ({e}), продолжаю ждать…"));
                    }
                    continue;
                }
            };
            let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("__")).collect();
            {
                let mut j = job.lock().unwrap();
                if lines.len() > seen {
                    last_new = Instant::now();
                }
                for l in lines.iter().skip(seen) {
                    j.log.push(l.trim().to_owned());
                }
                seen = lines.len().max(seen);
                let (pct, phase) = progress_of(&lines);
                j.percent = pct;
                let waited = t0.elapsed().as_secs();
                j.phase = format!("{phase} ({waited} с)");
            }
            if out.contains("__DONE__") {
                done = true;
                break;
            }
            // Nothing new for a while and install.sh is gone (and no «done»): it died; say so now.
            if out.contains("__PS__") && !out.contains("__ALIVE__") && last_new.elapsed() > Duration::from_secs(45) {
                let tail: Vec<&str> = lines.iter().rev().take(3).rev().copied().collect();
                return fail(format!(
                    "установщик на колонке остановился без результата{}",
                    if tail.is_empty() { String::new() } else { format!(": {}", tail.join(" / ")) }
                ));
            }
        }
        if !done {
            return fail("установщик на колонке не закончил за 7 минут — журнал: adb shell cat /tmp/soundya-install.log".into());
        }
    }
    // The result file over ADB; the speaker's adbd may stop answering after the
    // install's many sessions (seen on a Station Lite), so a few tries only.
    let mut res = String::new();
    for _ in 0..3 {
        match adb::shell(&ip, &key, "cat /data/opt/soundya/update-result 2>/dev/null", Duration::from_secs(5)) {
            Ok(r) if r.contains("status=") => {
                res = r;
                break;
            }
            _ => thread::sleep(Duration::from_secs(2)),
        }
    }
    set("Жду, пока откроется панель колонки…", 97);
    let mut panel = None;
    for _ in 0..30 {
        if let Ok((200, body)) = speaker_http(&ip, "GET", "/api/status", "") {
            panel = serde_json::from_str::<Value>(&body).ok();
            break;
        }
        thread::sleep(Duration::from_secs(2));
    }
    let log = job.lock().unwrap().log.clone();
    let (status, reason, version) = outcome(&res, panel.as_ref(), &log, install_began);
    let mut j = job.lock().unwrap();
    j.running = false;
    j.percent = 100;
    j.version = version;
    j.reason = if status == "failed" && reason.is_empty() { "колонка не сообщила результат установки".into() } else { reason };
    j.result = Some(status);
}

const OLD_SOUNDYA: &str = "на колонке стоит soundya, которая ещё не умеет обновляться без ADB, а ADB на колонке сейчас выключен. \
    Один последний раз включите ADB (как при получении root; с кабелем — «adb tcpip 5555») и нажмите «Обновить» — дальше обновления пойдут без ADB";

/// «Обновить» when the speaker's ADB is closed but soundya answers: the archive goes to
/// soundya, the speaker asks «установить?» and installs after «Алиса, да» (install.sh
/// with its rollback, as always). `result` "pin" — the panel has a PIN: the page asks it.
fn net_update(ip: String, pin: String, job: Arc<Mutex<Job>>) {
    let began = now();
    let set = |phase: &str, pct: u32| {
        let mut j = job.lock().unwrap();
        j.phase = phase.to_owned();
        j.percent = pct;
        j.log.push(phase.to_owned());
    };
    let finish = |result: &str, why: String| {
        let mut j = job.lock().unwrap();
        j.running = false;
        j.result = Some(result.into());
        if !why.is_empty() {
            j.log.push(format!("Ошибка: {why}"));
        }
        j.reason = why;
    };
    set("ADB на колонке закрыт — передаю обновление через soundya…", 2);
    let cookie = if pin.trim().is_empty() {
        String::new()
    } else {
        match speaker_login(&ip, pin.trim()) {
            Ok(c) => c,
            Err(e) => return finish("pin", e),
        }
    };
    // An older soundya cannot take the archive: say so before sending 7 MB for nothing.
    if let Ok((200, body)) = speaker_http(&ip, "GET", "/api/status", "") {
        if serde_json::from_str::<Value>(&body).ok().is_some_and(|v| v["net_update"].as_bool() != Some(true)) {
            return finish("failed", OLD_SOUNDYA.into());
        }
    }
    let total = tar().len();
    let r = speaker_upload(&ip, &cookie, tar(), |sent| job.lock().unwrap().percent = 2 + (sent as u64 * 55 / total.max(1) as u64) as u32);
    match r {
        Ok((200, _)) => {}
        Ok((401, _)) => return finish("pin", "на панели колонки стоит PIN".into()),
        // A soundya from before this: no such path, and 64 KB is its limit for everything else.
        Ok((404 | 413, _)) => return finish("failed", OLD_SOUNDYA.into()),
        Ok((_, body)) => return finish("failed", body.trim_start_matches("error:").trim().to_owned()),
        Err(e) => return finish("failed", e),
    }
    set("Колонка спрашивает, ставить ли обновление. Скажите ей: «Алиса, да»", 60);
    let t0 = Instant::now();
    let mut asked = true;
    let mut went_down = false;
    while t0.elapsed() < Duration::from_secs(480) {
        thread::sleep(Duration::from_secs(2));
        let panel = match speaker_http(&ip, "GET", "/api/status", "") {
            Ok((200, body)) => serde_json::from_str::<Value>(&body).ok(),
            _ => None,
        };
        let Some(panel) = panel else {
            if !went_down {
                went_down = true;
                set("Колонка ставит обновление: копия настроек, замена программы, проверка запуска…", 75);
            }
            continue;
        };
        let state = panel["update"]["state"].as_str().unwrap_or("").to_owned();
        if asked && state.contains("отменено") {
            return finish("failed", "установку отменили на колонке («Алиса, нет» или время вышло)".into());
        }
        if asked && state.contains("жду подтверждения") {
            if t0.elapsed() > Duration::from_secs(75) {
                return finish("failed", "на колонке не сказали «Алиса, да» за минуту — нажмите «Обновить» ещё раз".into());
            }
            continue;
        }
        if asked && state.starts_with("устанавливаю") {
            asked = false;
            set("Подтверждено, колонка устанавливает…", 70);
        }
        let last = &panel["update"]["last"];
        if last["time"].as_u64().is_some_and(|t| t + 5 >= began) && last["status"].as_str().is_some_and(|s| !s.is_empty()) {
            let (status, reason, version) = outcome("", Some(&panel), &[], began);
            let mut j = job.lock().unwrap();
            j.running = false;
            j.percent = 100;
            j.version = version;
            j.reason = reason;
            j.result = Some(status);
            return;
        }
        if asked && t0.elapsed() > Duration::from_secs(90) && !went_down {
            return finish("failed", "колонка не начала установку".into());
        }
    }
    finish("failed", "колонка не сообщила результат за 8 минут — откройте её страницу, раздел Обновления".into())
}

/// The demo walkthrough (dev builds offer it in the launcher; SOUNDYA_INSTALLER_DEMO=1 starts in it).
static DEMO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const DEMO_IP: &str = "192.168.0.250";

fn demo_speaker() -> Value {
    json!({ "ip": DEMO_IP, "root": true, "host": "yandexmicro-demo", "model": "Станция Лайт (демо)", "soundya": null, "configured": false, "adb": true })
}

/// What the demo answers instead of the real route; None: the real one (the page itself, /api/job, /api/open…).
fn demo_route(method: &str, path: &str, job: &Arc<Mutex<Job>>) -> Option<Value> {
    Some(match (method, path) {
        ("GET", "/api/launcher") => json!({ "speaker": null }),
        ("GET", "/api/scan") => {
            thread::sleep(Duration::from_millis(1500));
            json!({ "devices": [demo_speaker()], "subnet": "192.168.0.0/24 (демо)" })
        }
        ("POST", "/api/probe") => demo_speaker(),
        ("POST", "/api/online") => json!({ "online": true, "version": version() }),
        ("POST", "/api/install") | ("POST", "/api/netupdate") => {
            let mut j = job.lock().unwrap();
            if j.running {
                return Some(json!({ "error": "установка уже идёт" }));
            }
            *j = Job { running: true, ..Default::default() };
            drop(j);
            let job = job.clone();
            thread::spawn(move || demo_install(job));
            json!({ "ok": true })
        }
        ("POST", "/api/settings") | ("POST", "/api/remember") | ("POST", "/api/forget") => json!({ "ok": true }),
        ("POST", "/api/pc") => json!({ "ok": true, "text": "демо: на этом компьютере ничего не установлено" }),
        ("POST", "/api/pc/remove") => json!({ "ok": true, "text": "демо" }),
        _ => return None,
    })
}

/// The install's steps as install.sh reports them, in about fifteen seconds.
fn demo_install(job: Arc<Mutex<Job>>) {
    let steps = [
        (3, "Загружаю soundya на колонку… (демо)"),
        (25, "Загружаю soundya на колонку… (демо)"),
        (55, "Загружаю soundya на колонку… (демо)"),
        (64, "Устанавливаю: копия настроек, замена программы, проверка запуска…"),
        (72, "package: checksums OK"),
        (80, "soundya started"),
        (88, "Жду, пока soundya запустится и скажет, что готова…"),
        (97, "Жду, пока откроется панель колонки…"),
    ];
    for (pct, phase) in steps {
        thread::sleep(Duration::from_millis(1800));
        let mut j = job.lock().unwrap();
        j.percent = pct;
        j.phase = phase.into();
        j.log.push(phase.into());
    }
    thread::sleep(Duration::from_millis(1200));
    let mut j = job.lock().unwrap();
    j.running = false;
    j.percent = 100;
    j.version = version().to_owned();
    j.result = Some("ok".into());
}

/// The install's outcome (status, reason, new version), from the first source that
/// has it: the speaker's update-result file (`file`, over ADB), the update the
/// panel reports (`panel`, its /api/status: the same file, if written after
/// `since`), install.sh's last «update: …» line in `log`. None: «failed».
fn outcome(file: &str, panel: Option<&Value>, log: &[String], since: u64) -> (String, String, String) {
    let field = |k: &str| file.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap_or("").trim().to_owned();
    if !field("status").is_empty() {
        return (field("status"), field("reason"), field("to"));
    }
    let last = panel.map(|v| &v["update"]["last"]);
    if let Some(l) = last.filter(|l| l["time"].as_u64().is_some_and(|t| t + 5 >= since)) {
        if let Some(status) = l["status"].as_str().filter(|s| !s.is_empty()) {
            let text = |k: &str| l[k].as_str().unwrap_or("").to_owned();
            return (status.to_owned(), text("reason"), text("to"));
        }
    }
    if let Some(line) = log.iter().rev().find_map(|l| l.trim().strip_prefix("update: ")) {
        let (status, reason) = line.split_once(": ").unwrap_or((line, ""));
        // «Installed soundya 0.5.0. Try: …»
        let version = log
            .iter()
            .find_map(|l| l.strip_prefix("Installed soundya "))
            .and_then(|r| r.split_whitespace().next())
            .map(|v| v.trim_end_matches('.').to_owned())
            .unwrap_or_default();
        return (status.trim().to_owned(), reason.trim().to_owned(), version);
    }
    ("failed".into(), String::new(), String::new())
}

// ---------------------------------------------------------------- this PC (Windows)

/// Installs soundya-pc (asks for administrator rights) and fills the PC's
/// address, key and MAC into the speaker's settings.
#[cfg(windows)]
fn pc_setup(ip: &str) -> Result<String, String> {
    let dir = std::env::temp_dir().join("soundya-pc-setup");
    let _ = std::fs::create_dir_all(&dir);
    for (name, text) in PC_FILES {
        std::fs::write(dir.join(name), text).map_err(|e| e.to_string())?;
    }
    let local = std::env::var("LOCALAPPDATA").map_err(|_| "нет LOCALAPPDATA".to_owned())?;
    let result = PathBuf::from(&local).join("soundya-pc").join("setup.json");
    let _ = std::fs::remove_file(&result);
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(dir.join("install.ps1"))
        .arg("-Quiet")
        .status()
        .map_err(|e| format!("PowerShell не запустился: {e}"))?;
    // The elevated copy runs on its own: wait for its result file.
    let t0 = Instant::now();
    let text = loop {
        if let Some(t) = fsx::read_string(&result) {
            break t;
        }
        if t0.elapsed() > Duration::from_secs(240) {
            return Err("установка программы для ПК не закончилась (разрешили доступ администратора?)".into());
        }
        thread::sleep(Duration::from_secs(1));
    };
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("ответ установщика ПК: {e}"))?;
    let patch = json!({ "pc_on": true, "pc_host": v["ip"], "pc_token": v["key"], "pc_mac": v["mac"] });
    match speaker_http(ip, "POST", "/api/settings", &patch.to_string()) {
        Ok((200, body)) if body.starts_with("OK") => Ok(format!("Компьютер {} подключён к колонке", v["ip"].as_str().unwrap_or(""))),
        Ok((401, _)) => Err("на панели колонки стоит PIN: впишите данные ПК в «Устройства» вручную".into()),
        Ok((_, body)) => Err(format!("колонка не приняла настройки: {body}")),
        Err(e) => Err(e),
    }
}

/// Sets up the PC agent on this Mac (a launchd job of this app, no administrator
/// rights) and fills the Mac's address, key and MAC into the speaker's settings.
#[cfg(target_os = "macos")]
fn pc_setup(ip: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let home = agent::home();
    if !agent::installed_app(&exe, &home) {
        return Err("сначала перетащите soundya в «Программы» и откройте её оттуда: программа для Mac работает из «Программ»".into());
    }
    let key = agent::write_files(&home, &exe)?;
    launchctl_restart(&home)?;
    // It answers a signed ping on this Mac: started and has the key.
    let t0 = Instant::now();
    loop {
        if let Ok(t) = agent_ping(&key) {
            if t.starts_with("Компьютер") {
                break;
            }
        }
        if t0.elapsed() > Duration::from_secs(15) {
            return Err(format!("программа для Mac не отвечает; журнал: {}", home.join("Library/Logs/soundya-pc.log").display()));
        }
        thread::sleep(Duration::from_millis(500));
    }
    // The address the speaker sees: the route from this Mac to it.
    let me = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect((ip, 8765)).map(|()| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip().to_string())
        .map_err(|e| format!("не понять адрес этого Mac: {e}"))?;
    let ifconfig = std::process::Command::new("/sbin/ifconfig").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let mac = agent::mac_for(&ifconfig, &me).unwrap_or_default();
    let patch = json!({ "pc_on": true, "pc_host": me, "pc_token": key, "pc_mac": mac });
    match speaker_http(ip, "POST", "/api/settings", &patch.to_string()) {
        Ok((200, body)) if body.starts_with("OK") => Ok(format!("Этот Mac ({me}) подключён к колонке")),
        Ok((401, _)) => Err(format!("на панели колонки стоит PIN: впишите в «Устройства» адрес {me} и ключ из {}", agent::dir(&home).join("config.json").display())),
        Ok((_, body)) => Err(format!("колонка не приняла настройки: {body}")),
        Err(e) => Err(e),
    }
}

/// (Re)starts the launchd job from its plist.
#[cfg(target_os = "macos")]
fn launchctl_restart(home: &std::path::Path) -> Result<(), String> {
    let domain = format!("gui/{}", unsafe { getuid() });
    let plist = home.join(format!("Library/LaunchAgents/{}.plist", agent::LABEL));
    let _ = std::process::Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{}", agent::LABEL)]).output();
    let o = std::process::Command::new("/bin/launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist)
        .output()
        .map_err(|e| format!("launchctl не запустился: {e}"))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(format!("launchd не запустил программу для Mac: {}", String::from_utf8_lossy(&o.stderr).trim()))
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn getuid() -> u32;
}

/// A signed ping to the agent on this Mac.
#[cfg(target_os = "macos")]
fn agent_ping(key: &str) -> Result<String, String> {
    let mut s = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], agent::PORT)), Duration::from_secs(2)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut req = "POST /ping HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n".to_owned();
    for (k, v) in agent::sign(key, "/ping", b"") {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    Ok(out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned())
}

/// Stops and removes the agent from this Mac; the speaker stops calling it.
#[cfg(target_os = "macos")]
fn pc_remove(ip: &str) -> Result<String, String> {
    let domain = format!("gui/{}", unsafe { getuid() });
    let _ = std::process::Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{}", agent::LABEL)]).output();
    agent::remove_files(&agent::home());
    if !ip.is_empty() {
        let _ = speaker_http(ip, "POST", "/api/settings", &json!({ "pc_on": false }).to_string());
    }
    Ok("Программа для Mac удалена".into())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn pc_setup(_ip: &str) -> Result<String, String> {
    Err("управление компьютером есть для Windows и macOS".into())
}

#[cfg(not(target_os = "macos"))]
fn pc_remove(_ip: &str) -> Result<String, String> {
    Err("на Windows программа для ПК удаляется файлом pc\\uninstall.cmd".into())
}

// ---------------------------------------------------------------- web server

static LAST_SEEN: AtomicU64 = AtomicU64::new(0);

fn respond(mut s: &TcpStream, code: &str, kind: &str, body: &[u8]) {
    let _ = write!(s, "HTTP/1.1 {code}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
    let _ = s.write_all(body);
}

/// The wizard's requests are small; the developer build's notes carry pictures.
fn body_limit(path: &str) -> usize {
    #[cfg(feature = "dev")]
    if path.starts_with("/api/dev/") {
        return dev::BODY_LIMIT;
    }
    let _ = path;
    64 * 1024
}

fn handle(conn: TcpStream, port: u16, job: Arc<Mutex<Job>>) {
    LAST_SEEN.store(now(), Ordering::Release);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
    let mut r = BufReader::new(&conn);
    let mut line = String::new();
    if r.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("/").to_owned());
    // «/?app=1» (the Mac app's window) is the same page.
    let path = path.split('?').next().unwrap_or("/").to_owned();
    let (mut len, mut host, mut origin) = (0usize, String::new(), String::new());
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().unwrap_or(0).min(body_limit(&path)),
                "host" => host = v.trim().to_owned(),
                "origin" => origin = v.trim().to_owned(),
                _ => {}
            }
        }
    }
    // Only this page: no other site in the browser, no DNS rebinding.
    let me = format!("127.0.0.1:{port}");
    if host != me || (!origin.is_empty() && origin != format!("http://{me}")) {
        return respond(&conn, "403 Forbidden", "text/plain", b"forbidden");
    }
    let mut body = vec![0; len];
    if r.read_exact(&mut body).is_err() {
        return;
    }
    #[cfg(feature = "dev")]
    if let Some((code, kind, out)) = dev::route(&method, &path, &body, &job.lock().unwrap().log.clone(), &data_dir()) {
        return respond(&conn, code, kind, &out);
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let ip = v["ip"].as_str().unwrap_or("").trim().to_owned();
    let valid_ip = ip.parse::<Ipv4Addr>().is_ok();
    let js = |v: Value| respond(&conn, "200 OK", "application/json", v.to_string().as_bytes());
    // The demo: the whole first-run walkthrough with nothing sent to a speaker or set up on this computer.
    if method == "POST" && path == "/api/demo" {
        DEMO.store(v["on"].as_bool().unwrap_or(false), Ordering::Release);
        return js(json!({ "ok": true, "demo": DEMO.load(Ordering::Acquire) }));
    }
    if DEMO.load(Ordering::Acquire) {
        if let Some(answer) = demo_route(&method, &path, &job) {
            return js(answer);
        }
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => respond(&conn, "200 OK", "text/html; charset=utf-8", page().as_bytes()),
        ("GET", "/api/info") => js(json!({
            "demo": DEMO.load(Ordering::Acquire),
            "version": version(),
            "os": if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" },
            "computer": own_ip().map(|i| i.to_string()),
        })),
        ("GET", "/api/ping") => js(json!({ "ok": true })),
        ("GET", "/api/scan") => js(scan()),
        ("POST", "/api/probe") if valid_ip => js(match key() {
            Ok(k) => probe(&ip, &k).or_else(|| panel_probe(&ip)).unwrap_or_else(|| json!({ "error": format!("по адресу {ip} не отвечают ни soundya, ни ADB — включена ли колонка, тот ли Wi-Fi?") })),
            Err(e) => json!({ "error": e }),
        }),
        ("POST", "/api/install") if valid_ip => {
            let mut j = job.lock().unwrap();
            if j.running {
                return js(json!({ "error": "установка уже идёт" }));
            }
            *j = Job { running: true, ..Default::default() };
            drop(j);
            let (job, features) = (job.clone(), (!v["features"].is_null()).then(|| v["features"].clone()));
            thread::spawn(move || install(ip, features, job));
            js(json!({ "ok": true }))
        }
        // «Обновить» without ADB: through soundya, confirmed by voice at the speaker.
        ("POST", "/api/netupdate") if valid_ip => {
            let mut j = job.lock().unwrap();
            if j.running {
                return js(json!({ "error": "установка уже идёт" }));
            }
            *j = Job { running: true, ..Default::default() };
            drop(j);
            let (job, pin) = (job.clone(), v["pin"].as_str().unwrap_or("").to_owned());
            thread::spawn(move || net_update(ip, pin, job));
            js(json!({ "ok": true }))
        }
        ("GET", "/api/job") => js(job_json(&job.lock().unwrap())),
        // The launcher: the remembered speaker, at once; whether it answers — /api/online.
        ("GET", "/api/launcher") => js(match remembered_speaker(&data_dir()) {
            Some(sp) => json!({ "speaker": sp }),
            None => json!({ "speaker": null }),
        }),
        // Does soundya answer at ip: its real status page, a few tries (a Station Lite busy
        // starting a track, or macOS still deciding on «Локальная сеть», misses one quick try).
        ("POST", "/api/online") if valid_ip => js(match panel_online(&ip) {
            Ok(version) => json!({ "online": true, "version": version }),
            Err(why) => json!({ "online": false, "why": why }),
        }),
        ("POST", "/api/remember") if valid_ip => js(match remember_speaker(&data_dir(), &ip, v["model"].as_str().unwrap_or(""), v["version"].as_str().unwrap_or("")) {
            Ok(()) => json!({ "ok": true }),
            Err(e) => json!({ "error": e }),
        }),
        ("POST", "/api/forget") => {
            let _ = std::fs::remove_file(speaker_file(&data_dir()));
            js(json!({ "ok": true }))
        }
        // The speaker's own settings, through the installer (no cross-site requests from the page).
        ("POST", "/api/settings") if valid_ip => js(match speaker_http(&ip, "POST", "/api/settings", &v["patch"].to_string()) {
            Ok((200, b)) => json!({ "ok": b.starts_with("OK"), "text": b }),
            Ok((401, _)) => json!({ "error": "на панели колонки стоит PIN — эту настройку сделайте в панели" }),
            Ok((c, b)) => json!({ "error": format!("{c}: {b}") }),
            Err(e) => json!({ "error": e }),
        }),
        ("POST", "/api/pc") if valid_ip => js(match pc_setup(&ip) {
            Ok(t) => json!({ "ok": true, "text": t }),
            Err(e) => json!({ "error": e }),
        }),
        ("POST", "/api/pc/remove") => js(match pc_remove(if valid_ip { &ip } else { "" }) {
            Ok(t) => json!({ "ok": true, "text": t }),
            Err(e) => json!({ "error": e }),
        }),
        // Links from the app's window open in the user's browser: only the speaker's page.
        ("POST", "/api/open") => {
            let target = v["url"].as_str().unwrap_or("");
            let ok = target.strip_prefix("http://").and_then(|r| r.split_once(':')).is_some_and(|(host, rest)| {
                host.parse::<std::net::Ipv4Addr>().is_ok_and(|a| a.is_private()) && rest.starts_with("8765/")
            });
            if ok {
                open_browser(target);
            }
            js(json!({ "ok": ok }))
        }
        ("POST", "/api/quit") => {
            js(json!({ "ok": true }));
            thread::spawn(|| {
                thread::sleep(Duration::from_millis(300));
                std::process::exit(0);
            });
        }
        _ => respond(&conn, "404 Not Found", "text/plain", b"not found"),
    }
}

fn open_browser(url: &str) {
    let r = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).status()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    if r.is_err() {
        println!("Откройте в браузере: {url}");
    }
}

fn main() {
    // launchd starts the same binary as this Mac's PC agent.
    #[cfg(target_os = "macos")]
    if std::env::args().nth(1).as_deref() == Some("agent") {
        agent::main();
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("cannot listen on 127.0.0.1");
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let url = format!("http://127.0.0.1:{port}/");
    println!("soundya {} — установщик", version());
    println!("Мастер установки: {url}");
    LAST_SEEN.store(now(), Ordering::Release);
    if std::env::var("SOUNDYA_INSTALLER_DEMO").is_ok_and(|v| v == "1") {
        DEMO.store(true, Ordering::Release);
        println!("демо: колонка и этот компьютер не трогаются");
    }
    let serve = move || {
        let job = Arc::new(Mutex::new(Job::default()));
        for conn in listener.incoming().flatten() {
            let job = job.clone();
            thread::spawn(move || handle(conn, port, job));
        }
    };
    // On a Mac: the app's own window (no browser); the server runs beside it.
    #[cfg(target_os = "macos")]
    if std::env::var("SOUNDYA_INSTALLER_NO_BROWSER").is_err() {
        thread::spawn(serve);
        if let Err(e) = macwin::run(&format!("{url}?app=1"), "soundya") {
            eprintln!("своё окно не открылось ({e}), открываю браузер");
            open_browser(&url);
            loop {
                thread::sleep(Duration::from_secs(3600));
            }
        }
        std::process::exit(0);
    }
    if std::env::var("SOUNDYA_INSTALLER_NO_BROWSER").is_err() {
        println!("Мастер установки открыт в браузере. Не закрывайте это окно до конца установки.");
        open_browser(&url);
    }
    // Nobody on the page for 20 minutes: the installer leaves by itself.
    thread::spawn(|| loop {
        thread::sleep(Duration::from_secs(30));
        if now().saturating_sub(LAST_SEEN.load(Ordering::Acquire)) > 20 * 60 {
            std::process::exit(0);
        }
    });
    serve();
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    #[test]
    fn the_installed_speaker_is_remembered_for_the_launcher() {
        let dir = std::env::temp_dir().join(format!("soundya-launcher-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(remembered_speaker(&dir).is_none());
        assert!(remember_speaker(&dir, "not-an-ip", "x", "y").is_err());
        remember_speaker(&dir, "192.168.0.112", "Станция Лайт\n", "0.5.0").unwrap();
        let v = remembered_speaker(&dir).unwrap();
        assert_eq!((v["ip"].as_str(), v["model"].as_str(), v["version"].as_str()), (Some("192.168.0.112"), Some("Станция Лайт"), Some("0.5.0")));
        // A broken file is as good as none.
        std::fs::write(speaker_file(&dir), r#"{"ip":"evil.com"}"#).unwrap();
        assert!(remembered_speaker(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn looks_in_the_home_network_not_in_a_vpn() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        // This Mac: Wi-Fi 192.168.0.82, a VPN tunnel 10.8.1.1 (point-to-point) that
        // also carries the route to 8.8.8.8. Before, only 10.8.1.0/24 was searched.
        assert_eq!(subnets(&[(ip("192.168.0.82"), false), (ip("10.8.1.1"), true), (ip("10.8.1.1"), true)]), vec![[192, 168, 0]]);
        // Windows (no interface list): the home network first, the rest after.
        assert_eq!(subnets(&[(ip("192.168.0.82"), false), (ip("10.8.1.1"), false)]), vec![[192, 168, 0], [10, 8, 1]]);
        // Not a home network: public, carrier NAT, link-local, loopback.
        assert!(subnets(&[(ip("95.1.2.3"), false), (ip("100.64.1.2"), false), (ip("169.254.1.1"), false), (ip("127.0.0.1"), false)]).is_empty());
        // At most four networks, each once.
        let many: Vec<_> = (0..9u8).map(|n| (Ipv4Addr::new(192, 168, n, 5), false)).chain([(ip("192.168.0.9"), false)]).collect();
        assert_eq!(subnets(&many).len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn reads_this_computers_interfaces() {
        // Whatever the machine: loopback is there and marked as such (left out).
        let all = interfaces();
        assert!(all.iter().all(|(ip, _)| !ip.is_loopback()), "{all:?}");
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn a_hung_adb_does_not_turn_a_good_install_into_a_failure() {
        let log: Vec<String> = ["package: checksums OK", "soundya started (log: /tmp/soundya.log)", "update: ok", "Installed soundya 0.5.0. Try: …"]
            .iter().map(|s| s.to_string()).collect();
        let panel = json!({ "update": { "last": { "status": "ok", "reason": "", "to": "0.5.0", "time": 1_000_100 } } });
        // The speaker's ADB stopped answering after the install (seen on a Station Lite):
        // update-result could not be read. Before: «failed».
        assert_eq!(outcome("", Some(&panel), &log, 1_000_000), ("ok".into(), "".into(), "0.5.0".into()));
        // No panel either: install.sh's own last word.
        assert_eq!(outcome("", None, &log, 1_000_000).0, "ok");
        // The file, when it can be read, comes first.
        let file = "status=rolledback\nfrom=0.4.41\nto=0.5.0\ntime=1000100\nreason=новая версия не запустилась\r\n";
        assert_eq!(outcome(file, Some(&panel), &log, 1_000_000), ("rolledback".into(), "новая версия не запустилась".into(), "0.5.0".into()));
        // A result older than this install is not this install's.
        let stale = json!({ "update": { "last": { "status": "ok", "to": "0.4.41", "time": 999_000 } } });
        assert_eq!(outcome("", Some(&stale), &[], 1_000_000).0, "failed");
        // install.sh said why it rolled back.
        let rb: Vec<String> = vec!["update: rolledback: новая версия не запустилась".into()];
        assert_eq!(outcome("", None, &rb, 1_000_000), ("rolledback".into(), "новая версия не запустилась".into(), "".into()));
    }
}

#[cfg(test)]
mod net_update_tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn a_speaker_without_adb_is_found_by_its_panel() {
        let st = json!({"device": {"model": "yandex-station-lite", "version": "0.5.0+dev"}, "net_update": true});
        let d = panel_device("192.168.0.112", &st).unwrap();
        assert_eq!((d["model"].as_str(), d["adb"].as_bool(), d["net_update"].as_bool()), (Some("Станция Лайт"), Some(false), Some(true)));
        let old = json!({"device": {"model": "yandex-station-lite", "version": "0.5.0"}});
        assert_eq!(panel_device("1.2.3.4", &old).unwrap()["net_update"], false, "an older soundya: ADB once more");
        assert!(panel_device("1.2.3.4", &json!({"ok": true})).is_none(), "not a soundya panel");
    }

    #[test]
    fn an_early_answer_beats_a_broken_pipe() {
        // An older soundya: «413» at once and the connection closed while the archive is still going.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = c.read(&mut buf);
            c.write_all("HTTP/1.1 413 Payload Too Large\r\nConnection: close\r\n\r\nerror: слишком большой запрос".as_bytes()).unwrap();
            let _ = c.shutdown(std::net::Shutdown::Both);
        });
        let data = vec![7u8; 8 << 20];
        let r = upload_at(addr, "", &data, |_| {});
        server.join().unwrap();
        assert_eq!(r.map(|(c, _)| c), Ok(413), "the speaker's answer, not «Broken pipe»");
    }

    #[test]
    fn why_the_speaker_is_not_seen_is_said_plainly() {
        assert!(offline_reason("панель колонки не отвечает: No route to host (os error 65)").contains("Локальная сеть"));
        assert!(offline_reason("Connection refused (os error 61)").contains("не запущена"));
        assert!(offline_reason("connection timed out").contains("Wi-Fi"));
    }

    #[test]
    fn the_demo_never_reaches_a_speaker() {
        let job = Arc::new(Mutex::new(Job::default()));
        // Everything that would touch a speaker or this computer is answered by the demo itself.
        for (m, p) in [("GET", "/api/launcher"), ("POST", "/api/probe"), ("POST", "/api/settings"), ("POST", "/api/remember"), ("POST", "/api/pc"), ("POST", "/api/online")] {
            assert!(demo_route(m, p, &job).is_some(), "{m} {p}");
        }
        assert_eq!(demo_route("GET", "/api/launcher", &job).unwrap()["speaker"], Value::Null, "the walkthrough starts from the beginning");
        assert_eq!(demo_route("POST", "/api/probe", &job).unwrap()["configured"], false, "a fresh speaker: the features step shows");
        assert!(demo_route("GET", "/api/job", &job).is_none(), "the job is the real one (the demo fills it)");
        assert!(demo_route("POST", "/api/open", &job).is_none());
    }

    #[test]
    fn the_pin_session_is_read_from_the_answer() {
        let a = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: soundya_session=abc123; Max-Age=31536000; Path=/; HttpOnly\r\n\r\nok";
        assert_eq!(cookie_of(a).as_deref(), Some("soundya_session=abc123"));
        assert_eq!(cookie_of("HTTP/1.1 403 Forbidden\r\n\r\nerror: неверный PIN"), None);
    }

    #[test]
    fn the_archive_reaches_soundya_whole_with_the_session() {
        // A fake panel: reads the request, answers like the speaker.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let want = data.clone();
        let server = thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut r = std::io::BufReader::new(c.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0;
            loop {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut r, &mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.strip_prefix("Content-Length: ") {
                    len = v.trim().parse().unwrap();
                }
                head.push_str(&line);
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            c.write_all("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nOK: Колонка спрашивает".as_bytes()).unwrap();
            (head, body)
        });
        let mut last = 0;
        let (code, answer) = upload_at(addr, "soundya_session=abc", &data, |n| last = n).unwrap();
        let (head, body) = server.join().unwrap();
        assert_eq!((code, answer.as_str()), (200, "OK: Колонка спрашивает"));
        assert!(head.starts_with("POST /api/update/upload ") && head.contains("Cookie: soundya_session=abc"), "{head}");
        assert!(!head.to_lowercase().contains("origin:"), "not a browser: the speaker's site check lets it in");
        assert_eq!(body, want, "every byte");
        assert_eq!(last, data.len(), "progress to the end");
    }
}
