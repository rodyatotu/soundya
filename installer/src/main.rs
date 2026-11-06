//! soundya installer for Windows and macOS: one file with the speaker build
//! inside. It opens a step-by-step wizard in the browser (served from
//! 127.0.0.1 only), finds the speaker on the network, uploads soundya with its
//! own ADB client (src/adb.rs — no adb to install), runs the speaker's
//! install.sh (backup, check, rollback) and walks through the first settings.
//! On Windows it can also set up this PC (pc/install.ps1) and fill the PC's
//! address, key and MAC into the speaker by itself.

#![cfg_attr(windows, windows_subsystem = "console")]

#[cfg(target_os = "macos")]
mod macwin;
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
static BUILT_IN: &[u8] = include_bytes!("../../dist/soundya-armv7.tar");

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
    CARGO.lines().find_map(|l| l.strip_prefix("version = \"")).and_then(|v| v.strip_suffix('"')).unwrap_or("?").to_owned()
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

/// This computer's address on the home network (the route to the internet).
fn own_ip() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_loopback() => Some(*a.ip()),
        _ => None,
    }
}

/// Every speaker with root ADB in this computer's /24.
fn scan() -> Value {
    let Some(me) = own_ip() else {
        return json!({ "error": "компьютер не в сети" });
    };
    let [a, b, c, _] = me.octets();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (1..=254u8)
        .map(|d| {
            let hits = hits.clone();
            thread::spawn(move || {
                let addr = SocketAddr::from(([a, b, c, d], 5555));
                if TcpStream::connect_timeout(&addr, Duration::from_millis(700)).is_ok() {
                    hits.lock().unwrap().push(format!("{a}.{b}.{c}.{d}"));
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
        .map(|ip| {
            let (key, found) = (key.clone(), found.clone());
            thread::spawn(move || {
                if let Some(v) = probe(&ip, &key) {
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
    json!({ "devices": list, "subnet": format!("{a}.{b}.{c}.0/24") })
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
        match r {
            Ok(()) => {
                pushed = true;
                break;
            }
            Err(e) => {
                job.lock().unwrap().log.push(format!("связь оборвалась ({e}), попытка {} из 4", attempt + 1));
                thread::sleep(Duration::from_secs(2));
            }
        }
    }
    if !pushed {
        return fail("не удалось загрузить файл на колонку: связь всё время обрывается".into());
    }
    // The size on the speaker must match.
    let size = adb::shell(&ip, &key, "wc -c < /tmp/soundya-armv7.tar", Duration::from_secs(5)).unwrap_or_default();
    if size.trim() != total.to_string() {
        return fail(format!("файл дошёл не целиком ({} из {total} байт)", size.trim()));
    }
    if let Some(f) = features {
        let on = |k: &str| if f[k].as_bool().unwrap_or(true) { 1 } else { 0 };
        let text = format!(
            "soundcloud={}\\nairplay={}\\npc={}\\ntv={}\\nbypass={}\\n",
            on("soundcloud"), on("airplay"), on("pc"), on("tv"), on("bypass")
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
    let res = adb::shell(&ip, &key, "cat /data/opt/soundya/update-result 2>/dev/null", Duration::from_secs(5)).unwrap_or_default();
    let field = |k: &str| res.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap_or("").trim().to_owned();
    let status = field("status");
    set("Жду, пока откроется панель колонки…", 97);
    for _ in 0..30 {
        if speaker_http(&ip, "GET", "/api/status", "").is_ok_and(|(c, _)| c == 200) {
            break;
        }
        thread::sleep(Duration::from_secs(2));
    }
    let mut j = job.lock().unwrap();
    j.running = false;
    j.percent = 100;
    j.version = field("to");
    j.reason = field("reason");
    j.result = Some(if status.is_empty() { "failed".into() } else { status });
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

#[cfg(not(windows))]
fn pc_setup(_ip: &str) -> Result<String, String> {
    Err("управление компьютером пока есть только для Windows".into())
}

// ---------------------------------------------------------------- web server

static LAST_SEEN: AtomicU64 = AtomicU64::new(0);

fn respond(mut s: &TcpStream, code: &str, kind: &str, body: &[u8]) {
    let _ = write!(s, "HTTP/1.1 {code}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
    let _ = s.write_all(body);
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
                "content-length" => len = v.trim().parse().unwrap_or(0).min(64 * 1024),
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
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let ip = v["ip"].as_str().unwrap_or("").trim().to_owned();
    let valid_ip = ip.parse::<Ipv4Addr>().is_ok();
    let js = |v: Value| respond(&conn, "200 OK", "application/json", v.to_string().as_bytes());
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => respond(&conn, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/api/info") => js(json!({
            "version": version(),
            "os": if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" },
            "computer": own_ip().map(|i| i.to_string()),
        })),
        ("GET", "/api/ping") => js(json!({ "ok": true })),
        ("GET", "/api/scan") => js(scan()),
        ("POST", "/api/probe") if valid_ip => js(match key() {
            Ok(k) => probe(&ip, &k).unwrap_or_else(|| json!({ "error": format!("по адресу {ip} нет Яндекс Станции с root и ADB по сети") })),
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
        ("GET", "/api/job") => js(job_json(&job.lock().unwrap())),
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("cannot listen on 127.0.0.1");
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let url = format!("http://127.0.0.1:{port}/");
    println!("soundya {} — установщик", version());
    println!("Мастер установки: {url}");
    LAST_SEEN.store(now(), Ordering::Release);
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
