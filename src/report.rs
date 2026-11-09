//! Bug reports and feedback from the web panel («Сообщить об ошибке»).
//!
//! Replaces the 0.4.40 one-shot `/feedback` (lost when the server or Telegram
//! was down, no diagnostics). A report is built on the speaker the moment the
//! user asks, shown to them file by file (preview), and only then queued in
//! <state dir>/reports/ on flash. A sender thread delivers queued reports to the
//! project's server (server/stats.py `POST /report`), which holds the Telegram
//! bot token and forwards the report with its files as documents. Undelivered
//! reports survive a reboot and are retried with growing pauses.
//!
//! What is never sent: the SoundCloud token, the PC key, the proxy password,
//! Telegram contacts, cookies/OAuth values — they are removed from every file
//! before the preview, so the preview is exactly what leaves the speaker.
//! Recognised voice phrases are hidden unless the user ticks «фразы».
//! No audio is ever recorded or attached.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::Shared;

/// Most screenshot bytes taken (the page also checks).
pub const MAX_SCREENSHOT: usize = 3 << 20;
/// Lines of the soundya log in system.log.
const LOG_LINES: usize = 400;
/// Reports kept on flash (sent ones keep only their record).
const KEEP: usize = 20;
/// Undelivered reports are given up after this many days.
const GIVE_UP_DAYS: u64 = 7;

/// Modules a report can be about (the page shows them in a list).
pub const MODULES: [(&str, &str); 19] = [
    ("general", "Общее / не знаю"),
    ("soundcloud", "SoundCloud"),
    ("yandex_music", "Яндекс Музыка и выбор сервиса"),
    ("voice", "Голосовые команды"),
    ("airplay", "AirPlay"),
    ("eq", "Эквалайзер"),
    ("ambilight", "Подсветка / Ambilight"),
    ("pc", "Управление компьютером"),
    ("tv", "Телевизор / проектор"),
    ("telegram", "Telegram"),
    ("network", "Соединение, прокси, обход"),
    ("update", "Обновление прошивки soundya"),
    ("web", "Веб-панель"),
    ("ducking", "Приглушение звука"),
    ("audio", "Звук / Audio Hub"),
    ("onboarding", "Первая настройка"),
    ("planner", "Планировщик и напоминания"),
    ("calendar", "Календарь (Google и др.)"),
    ("hardware", "Проверка возможностей колонки"),
];

/// What the page sends.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Request {
    /// "bug" or "feedback".
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub expected: String,
    #[serde(default)]
    pub actual: String,
    /// Attach system.log, error.log and diagnostics.json.
    #[serde(default)]
    pub logs: bool,
    /// Keep recognised voice phrases in the log.
    #[serde(default)]
    pub phrases: bool,
    /// Send the random installation number (lets the author tie reports together).
    #[serde(default)]
    pub install_id: bool,
    /// Errors the page itself caught (window.onerror, failed requests).
    #[serde(default)]
    pub frontend_errors: Vec<String>,
    /// base64 PNG/JPEG.
    #[serde(default)]
    pub screenshot: String,
    #[serde(default)]
    pub screenshot_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub id: String,
    pub kind: String,
    pub module: String,
    pub description: String,
    pub expected: String,
    pub actual: String,
    pub created: u64,
    /// "queued" | "sent" | "failed" (given up: the reason is in last_error).
    pub status: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub next_try: u64,
    pub sent_at: Option<u64>,
    pub version: String,
    pub model: String,
    pub firmware: Option<String>,
    pub installation: Option<String>,
    /// Attachment names and sizes (the files are in reports/<id>/ until sent).
    pub files: Vec<(String, u64)>,
}

/// An attachment as the preview shows it.
pub struct File {
    pub name: String,
    pub data: Vec<u8>,
}

static BUSY: AtomicBool = AtomicBool::new(false);
static WAKE: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
/// One report written / sent at a time.
static LOCK: Mutex<()> = Mutex::new(());

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn reports_dir(dir: &Path) -> PathBuf {
    dir.join("reports")
}

/// Reports' own server, set at build time (SOUNDYA_REPORT_URL): the statistics may stay on
/// another one (a server that can reach Telegram is not always the one that counts speakers).
const BUILT_REPORT_URL: Option<&str> = option_env!("SOUNDYA_REPORT_URL");

/// Where reports go: SOUNDYA_REPORT_URL (soundya.env), the one built in, or the statistics server.
pub fn url() -> String {
    if let Ok(u) = std::env::var("SOUNDYA_REPORT_URL") {
        if !u.trim().is_empty() {
            return u.trim().to_owned();
        }
    }
    if let Some(u) = BUILT_REPORT_URL.map(str::trim).filter(|u| !u.is_empty()) {
        return u.to_owned();
    }
    let base = crate::stats::URL;
    if base.is_empty() {
        return String::new();
    }
    base.strip_suffix("/ping").map(|b| format!("{b}/report")).unwrap_or_else(|| format!("{}/report", base.trim_end_matches('/')))
}

fn new_id() -> String {
    let mut b = [0u8; 4];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut b);
    } else {
        b = (now() as u32 ^ std::process::id()).to_le_bytes();
    }
    format!("SY-{}", b.iter().map(|x| format!("{x:02X}")).collect::<String>())
}

// ---------------------------------------------------------------- redaction

/// Values that must never leave the speaker, read from the settings.
pub fn secrets(s: &crate::settings::Settings) -> Vec<String> {
    let mut out = vec![s.token.clone(), s.pc_token.clone(), s.panel_pin.clone(), s.cal_url.clone(), s.g_client_secret.clone(), s.g_refresh.clone(), s.meet_bot_token.clone()];
    out.extend(s.pcs_more.iter().map(|p| p.token.clone()));
    // The calendar's secret part alone too (…/private-<hex>/…), in case it is logged in pieces.
    if let Some(p) = s.cal_url.split('/').find(|p| p.starts_with("private-")) {
        out.push(p.to_owned());
    }
    // Proxy password (and the whole proxy with it, as written).
    if let Some((_, rest)) = s.proxy.split_once("://") {
        if let Some((cred, _)) = rest.split_once('@') {
            if let Some((_, pass)) = cred.split_once(':') {
                out.push(pass.to_owned());
            }
            out.push(cred.to_owned());
        }
    }
    for b in &s.tg_bots {
        out.push(b.token.clone());
    }
    for c in &s.pc_contacts {
        out.push(c.target.clone());
        out.extend(c.names.split('|').map(str::to_owned));
    }
    out.retain(|v| v.trim().chars().count() >= 3);
    // Longest first, so a secret is not half-replaced by a shorter one inside it.
    out.sort_by_key(|v| std::cmp::Reverse(v.len()));
    out.dedup();
    out
}

/// Masks the value after `key` (up to a space, quote, `&` or `;`).
fn mask_after(text: &str, key: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(key) {
        let (head, tail) = rest.split_at(i + key.len());
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || "\"'&;,)}".contains(c)).unwrap_or(tail.len());
        if end > 0 {
            out.push_str("«скрыто»");
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Long digit runs that look like phone numbers.
fn mask_phones(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let plus = chars[i] == '+';
        let mut digits = 0;
        let mut j = i + usize::from(plus);
        let mut end = j;
        while j < chars.len() && (chars[j].is_ascii_digit() || (digits > 0 && matches!(chars[j], ' ' | '-' | '(' | ')'))) {
            if chars[j].is_ascii_digit() {
                digits += 1;
                end = j + 1;
            }
            j += 1;
        }
        let j = end;
        // Not a part of a longer word or number (timestamps are 10 digits without +7/8 shape).
        let before_ok = start == 0 || !chars[start - 1].is_alphanumeric();
        let phone_like = digits >= 10 && digits <= 13 && (plus || chars[start + usize::from(plus)] == '8' || chars[start] == '7');
        if before_ok && phone_like && j > start {
            out.push_str("«телефон»");
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Hides what was said: `voice: "…"` and `say: "…"` lines keep their shape.
fn mask_phrases(line: &str) -> String {
    for tag in ["voice: \"", "say: \"", "voice result: "] {
        if let Some(i) = line.find(tag) {
            let start = i + tag.len();
            let rest = &line[start..];
            let end = if tag.ends_with('"') { rest.find('"').unwrap_or(rest.len()) } else { rest.len() };
            return format!("{}«фраза скрыта»{}", &line[..start], &rest[end..]);
        }
    }
    line.to_owned()
}

/// Everything that may hold a secret goes through here.
pub fn redact(text: &str, secrets: &[String], phrases: bool) -> String {
    let mut t = text.to_owned();
    for s in secrets {
        t = t.replace(s.as_str(), "«скрыто»");
    }
    for key in ["oauth_token=", "oauth_token\":\"", "OAuth ", "client_id=", "access_token=", "Authorization: ", "X-Soundya-Key: ", "token=", "bot"] {
        if key == "bot" {
            // Telegram bot tokens: bot<digits>:<secret>
            t = mask_bot_tokens(&t);
            continue;
        }
        t = mask_after(&t, key);
    }
    // user:password@host in any URL.
    t = mask_url_passwords(&t);
    t = mask_phones(&t);
    if !phrases {
        t = t.lines().map(mask_phrases).collect::<Vec<_>>().join("\n");
    }
    t
}

fn mask_bot_tokens(t: &str) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find("bot") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let digits = tail.chars().take_while(char::is_ascii_digit).count();
        if digits >= 6 && tail[digits..].starts_with(':') {
            let end = tail[digits + 1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).map_or(tail.len(), |e| e + digits + 1);
            out.push_str("«скрыто»");
            rest = &tail[end..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn mask_url_passwords(t: &str) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || "\"'/".contains(c)).unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rsplit_once('@') {
            Some((cred, host)) if cred.contains(':') => {
                let user = cred.split(':').next().unwrap_or("");
                out.push_str(&format!("{user}:«скрыто»@{host}"));
            }
            _ => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------- diagnostics

fn read(p: &str) -> Option<String> {
    crate::fsx::read_string(Path::new(p))
}

/// The end of /tmp/soundya.log without secrets (the developer build's feedback).
#[cfg(feature = "dev")]
pub fn log_tail(dir: &Path, lines: usize) -> String {
    let secrets = secrets(&crate::settings::Settings::load(dir));
    redact(&tail(&read("/tmp/soundya.log").unwrap_or_default(), lines), &secrets, true)
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The firmware version, if this speaker writes it anywhere we know.
pub fn firmware() -> Option<String> {
    if let Some(prop) = read("/system/build.prop") {
        for key in ["ro.build.version.incremental=", "ro.build.display.id="] {
            if let Some(v) = prop.lines().find_map(|l| l.strip_prefix(key)) {
                return Some(v.trim().to_owned());
            }
        }
    }
    let cfg = read("/system/vendor/quasar/quasar.cfg")?;
    let v: Value = serde_json::from_str(&cfg).ok()?;
    fn find(v: &Value) -> Option<String> {
        match v {
            Value::Object(m) => {
                for k in ["softwareVersion", "firmwareVersion", "quasmodromVersion"] {
                    if let Some(s) = m.get(k).and_then(Value::as_str) {
                        return Some(s.to_owned());
                    }
                }
                m.values().find_map(find)
            }
            _ => None,
        }
    }
    find(&v)
}

fn meminfo() -> Value {
    let text = read("/proc/meminfo").unwrap_or_default();
    let get = |k: &str| text.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok());
    let me = read("/proc/self/status").unwrap_or_default();
    let rss = me.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok());
    let threads = me.lines().find(|l| l.starts_with("Threads:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok());
    json!({ "total_kb": get("MemTotal:"), "available_kb": get("MemAvailable:"), "soundya_rss_kb": rss, "soundya_threads": threads })
}

/// The panel's status JSON with everything personal taken out.
fn status_summary(shared: &Shared) -> Value {
    let raw = shared.status_json.lock().unwrap().clone();
    let mut v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        for k in ["queue", "queue_ids", "queue_liked", "cover", "current_url", "zapret_presets"] {
            o.remove(k);
        }
        if let Some(s) = o.get_mut("settings").and_then(Value::as_object_mut) {
            let set = |x: Option<&Value>| x.and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty());
            let facts = json!({
                "token_set": set(s.get("token")),
                "proxy_set": set(s.get("proxy")),
                "pc_host_set": set(s.get("pc_host")),
                "pc_token_set": set(s.get("pc_token")),
                "pc_mac_set": set(s.get("pc_mac")),
                "tv_host_set": set(s.get("tv_host")),
                "contacts": s.get("pc_contacts").and_then(Value::as_array).map_or(0, Vec::len),
                "tvs_more": s.get("tvs_more").and_then(Value::as_array).map_or(0, Vec::len),
                "pcs_more": s.get("pcs_more").and_then(Value::as_array).map_or(0, Vec::len),
            });
            for k in ["token", "proxy", "pc_host", "pc_token", "pc_mac", "pc_contacts", "tv_host", "tvs_more", "pcs_more", "tv_name", "pc_name", "tv_active", "zapret_args", "airplay_name", "cal_url", "g_client_secret", "g_refresh", "g_client_id",
                      // Personal: team emails, Telegram ids and names, the headphones, the city.
                      "teams", "tg_allowed", "tg_bots", "meet_bot_token", "bt_mac", "display_city", "panel_pin"] {
                s.remove(k);
            }
            s.insert("secrets".into(), facts);
        }
        // Signed in or not, without the name.
        let user = o.get("user").is_some_and(|u| !u.is_null());
        o.insert("user".into(), Value::Bool(user));
        o.remove("track");
    }
    v
}

/// Builds the attachments. Same function for the preview and the report.
pub fn build(dir: &Path, shared: &Shared, req: &Request) -> Vec<File> {
    let settings = crate::settings::Settings::load(dir);
    let secrets = secrets(&settings);
    let clean = |t: &str| redact(t, &secrets, req.phrases);
    let mut files = Vec::new();
    if req.logs {
        let log = read("/tmp/soundya.log").unwrap_or_default();
        files.push(File { name: "system.log".into(), data: clean(&tail(&log, LOG_LINES)).into_bytes() });
        // Errors: from this boot's log, the audio errors, the guard, a failed update,
        // and the end of the previous boot's log (what came before a crash or a hang).
        let mut err = String::new();
        let errs: Vec<&str> = log
            .lines()
            .filter(|l| {
                let l = l.to_lowercase();
                ["error", "failed", "cannot", "panic", "timeout", "не ", "warn"].iter().any(|k| l.contains(k))
            })
            .collect();
        err.push_str(&format!("=== /tmp/soundya.log: {} matching lines (last 150)\n", errs.len()));
        err.push_str(&errs[errs.len().saturating_sub(150)..].join("\n"));
        for (name, lines) in [("audio-errors.log", 60), ("guard.log", 50), ("failed-update.log", 40), ("previous.log", 80)] {
            if let Some(t) = crate::fsx::read_string(&dir.join(name)) {
                err.push_str(&format!("\n\n=== {name} (last {lines} lines)\n{}", tail(&t, lines)));
            }
        }
        if !req.frontend_errors.is_empty() {
            err.push_str("\n\n=== web panel errors\n");
            err.push_str(&req.frontend_errors.iter().take(40).map(|e| e.chars().take(500).collect::<String>()).collect::<Vec<_>>().join("\n"));
        }
        files.push(File { name: "error.log".into(), data: clean(&err).into_bytes() });
        let diag = json!({
            "soundya": crate::VERSION,
            "model": crate::stats::model(),
            "firmware": firmware(),
            "kernel": read("/proc/version").map(|v| v.trim().to_owned()),
            "uptime_s": read("/proc/uptime").and_then(|u| u.split('.').next().and_then(|s| s.parse::<u64>().ok())),
            "loadavg": read("/proc/loadavg").map(|v| v.trim().to_owned()),
            "memory": meminfo(),
            "luna": crate::luna::present(),
            "asound_cards": read("/proc/asound/cards").map(|v| v.trim().to_owned()),
            "pcm_state": read("/proc/asound/card0/pcm0p/sub0/status").map(|v| v.trim().to_owned()),
            "config_layout": settings.config_version,
            "status": status_summary(shared),
            // «Проверить возможности колонки», if it was run.
            "capabilities": crate::probe::last(),
        });
        let text = serde_json::to_string_pretty(&diag).unwrap_or_default();
        files.push(File { name: "diagnostics.json".into(), data: clean(&text).into_bytes() });
    }
    if !req.screenshot.is_empty() {
        if let Some(bytes) = crate::pc::unbase64(&req.screenshot).filter(|b| b.len() <= MAX_SCREENSHOT) {
            let ext = if req.screenshot_type.contains("jpeg") || req.screenshot_type.contains("jpg") { "jpg" } else { "png" };
            files.push(File { name: format!("screenshot.{ext}"), data: bytes });
        }
    }
    files
}

fn module_name(id: &str) -> &'static str {
    MODULES.iter().find(|(k, _)| *k == id).map_or("Общее / не знаю", |(_, v)| v)
}

fn clip(s: &str, n: usize) -> String {
    s.trim().chars().take(n).collect()
}

/// The report's header as the author sees it in Telegram.
pub fn message(r: &Record) -> String {
    let date = date(r.created);
    let device = match &r.firmware {
        Some(f) => format!("{} (прошивка {f})", r.model),
        None => r.model.clone(),
    };
    let mut m = format!(
        "{} #{}\nSoundya Version: {}\nDevice: {}\nModule: {}\nDate: {}\nUser/Installation ID: {}\nDescription: {}\n",
        if r.kind == "feedback" { "FEEDBACK" } else { "BUG REPORT" },
        r.id,
        r.version,
        device,
        module_name(&r.module),
        date,
        r.installation.as_deref().unwrap_or("не передан"),
        if r.description.is_empty() { "—" } else { &r.description },
    );
    if r.kind != "feedback" {
        m.push_str(&format!("Expected: {}\nActual: {}\n", or_dash(&r.expected), or_dash(&r.actual)));
    }
    m.push_str("Attachments:");
    if r.files.is_empty() {
        m.push_str(" нет\n");
    } else {
        m.push('\n');
    }
    for (name, size) in &r.files {
        m.push_str(&format!("* {name} ({} КБ)\n", size.div_ceil(1024)));
    }
    m
}

/// "2026-11-06 14:03:05 UTC" from a Unix time (the speaker has no time zone set).
pub fn date(t: u64) -> String {
    let days = (t / 86_400) as i64;
    let secs = t % 86_400;
    // Civil from days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", secs / 3600, secs / 60 % 60, secs % 60)
}

fn or_dash(s: &str) -> &str {
    if s.trim().is_empty() { "—" } else { s }
}

/// What the page shows before sending: the message and every file in full
/// (screenshots by size only).
pub fn preview(dir: &Path, shared: &Shared, req: &Request) -> Value {
    let files = build(dir, shared, req);
    let rec = record(req, "SY-предпросмотр".into(), &files, dir);
    json!({
        "message": message(&rec),
        "files": files.iter().map(|f| json!({
            "name": f.name,
            "size": f.data.len(),
            "text": if f.name.starts_with("screenshot") { Value::Null } else { Value::from(String::from_utf8_lossy(&f.data).into_owned()) },
        })).collect::<Vec<_>>(),
        "url": url(),
    })
}

fn record(req: &Request, id: String, files: &[File], dir: &Path) -> Record {
    let kind = if req.kind == "feedback" { "feedback" } else { "bug" };
    Record {
        id,
        kind: kind.into(),
        module: if MODULES.iter().any(|(k, _)| *k == req.module) { req.module.clone() } else { "general".into() },
        description: clip(&req.description, 3000),
        expected: clip(&req.expected, 1500),
        actual: clip(&req.actual, 1500),
        created: now(),
        status: "queued".into(),
        attempts: 0,
        last_error: None,
        next_try: 0,
        sent_at: None,
        version: crate::VERSION.into(),
        model: crate::stats::model(),
        firmware: firmware(),
        installation: req.install_id.then(|| crate::stats::install_id(dir)).flatten(),
        files: files.iter().map(|f| (f.name.clone(), f.data.len() as u64)).collect(),
    }
}

fn save_record(dir: &Path, r: &Record) -> std::io::Result<()> {
    let data = serde_json::to_vec_pretty(r).map_err(std::io::Error::other)?;
    let p = reports_dir(dir).join(format!("{}.json", r.id));
    let tmp = p.with_extension("json.tmp");
    crate::settings::write_synced(&tmp, &data)?;
    std::fs::rename(tmp, p)
}

/// Saves the report on flash and wakes the sender. Returns the record.
pub fn submit(dir: &Path, shared: &Shared, req: &Request) -> Result<Record, String> {
    if req.description.trim().is_empty() {
        return Err("опишите, что случилось".into());
    }
    let files = build(dir, shared, req);
    let _guard = LOCK.lock().unwrap();
    let rdir = reports_dir(dir);
    crate::fsx::ensure_dir(&rdir);
    let rec = record(req, new_id(), &files, dir);
    let fdir = rdir.join(&rec.id);
    crate::fsx::ensure_dir(&fdir);
    for f in &files {
        crate::settings::write_synced(&fdir.join(&f.name), &f.data).map_err(|e| format!("не сохранился {}: {e}", f.name))?;
    }
    save_record(dir, &rec).map_err(|e| format!("отчёт не сохранился: {e}"))?;
    prune(dir);
    eprintln!("report: {} queued ({}, {} files)", rec.id, rec.module, rec.files.len());
    if let Some(w) = WAKE.lock().unwrap().as_ref() {
        let _ = w.send(());
    }
    Ok(rec)
}

pub fn list(dir: &Path) -> Vec<Record> {
    let mut out: Vec<Record> = std::fs::read_dir(reports_dir(dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .filter_map(|e| crate::fsx::read_string(&e.path()))
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.created));
    out
}

/// Keeps the newest KEEP records; files of sent reports are removed.
fn prune(dir: &Path) {
    let all = list(dir);
    for (i, r) in all.iter().enumerate() {
        let fdir = reports_dir(dir).join(&r.id);
        if r.status != "queued" {
            crate::fsx::remove_tree(&fdir);
        }
        if i >= KEEP {
            crate::fsx::remove_tree(&fdir);
            let _ = std::fs::remove_file(reports_dir(dir).join(format!("{}.json", r.id)));
        }
    }
}

/// Pause before attempt n+1: 1, 5, 15, 30 minutes, then hourly.
fn backoff(attempts: u32) -> u64 {
    match attempts {
        0 | 1 => 60,
        2 => 300,
        3 => 900,
        4 => 1800,
        _ => 3600,
    }
}

enum Sent {
    Ok,
    /// Try again later (network, server or Telegram down, too many reports).
    Later(String),
    /// The server refused it for good (malformed, too big).
    Never(String),
}

fn send_one(dir: &Path, r: &Record, proxy: &str) -> Sent {
    let target = url();
    if target.is_empty() {
        return Sent::Never("сервер для отчётов не задан в этой сборке".into());
    }
    let fdir = reports_dir(dir).join(&r.id);
    let mut files = Vec::new();
    for (name, _) in &r.files {
        match crate::fsx::read_bytes(&fdir.join(name)) {
            Ok(data) => files.push(json!({ "name": name, "data": crate::pc::base64(&data) })),
            Err(e) => return Sent::Never(format!("файл {name} пропал: {e}")),
        }
    }
    let body = json!({
        "id": r.id, "kind": r.kind, "module": r.module, "module_name": module_name(&r.module),
        "version": r.version, "model": r.model, "firmware": r.firmware, "installation": r.installation,
        "created": r.created, "description": r.description, "expected": r.expected, "actual": r.actual,
        "message": message(r), "files": files,
    })
    .to_string();
    let attempt = |agent: ureq::Agent| agent.post(&target).set("Content-Type", "application/json").send_string(&body);
    let direct = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout(Duration::from_secs(60)).build();
    let mut result = attempt(direct);
    // The server may be reachable only the way SoundCloud is (proxy).
    if matches!(result, Err(ureq::Error::Transport(_))) && !proxy.trim().is_empty() {
        let full = if proxy.contains("://") { proxy.to_owned() } else { format!("http://{proxy}") };
        if let Ok(p) = ureq::Proxy::new(&full) {
            result = attempt(ureq::AgentBuilder::new().proxy(p).timeout_connect(Duration::from_secs(10)).timeout(Duration::from_secs(60)).build());
        }
    }
    match result {
        Ok(_) => Sent::Ok,
        Err(ureq::Error::Status(code, resp)) => {
            let why = resp.into_string().unwrap_or_default();
            let why = clip(&why, 200);
            answer(code, &why)
        }
        Err(e) => Sent::Later(format!("нет связи с сервером: {e}")),
    }
}

/// The server's refusal → try later or give up.
fn answer(code: u16, why: &str) -> Sent {
    match code {
        429 => Sent::Later("сервер просит подождать".into()),
        // No /report at all: a server from before 0.5.00 (only /feedback). Kept and sent
        // again once the server is updated (server/README.md), not thrown away.
        404 => Sent::Later("на сервере старая версия без приёма отчётов (404) — повторю, когда его обновят".into()),
        400 | 413 | 422 => Sent::Never(format!("сервер не принял отчёт ({code} {why})")),
        503 => Sent::Later("приём отчётов на сервере выключен".into()),
        _ => Sent::Later(format!("сервер ответил {code} {why}")),
    }
}

/// One pass over the queue: sends what is due.
pub fn deliver(dir: &Path, proxy: &str, force: bool) {
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    for mut r in list(dir).into_iter().filter(|r| r.status == "queued") {
        let t = now();
        if !force && r.next_try > t {
            continue;
        }
        let outcome = send_one(dir, &r, proxy);
        let _guard = LOCK.lock().unwrap();
        r.attempts += 1;
        match outcome {
            Sent::Ok => {
                r.status = "sent".into();
                r.sent_at = Some(now());
                r.last_error = None;
                eprintln!("report: {} delivered", r.id);
            }
            Sent::Later(e) if t.saturating_sub(r.created) < GIVE_UP_DAYS * 86_400 => {
                eprintln!("report: {} not delivered ({e}); attempt {}", r.id, r.attempts);
                r.next_try = now() + backoff(r.attempts);
                r.last_error = Some(e);
            }
            Sent::Later(e) | Sent::Never(e) => {
                eprintln!("report: {} given up: {e}", r.id);
                r.status = "failed".into();
                r.last_error = Some(e);
            }
        }
        if let Err(e) = save_record(dir, &r) {
            eprintln!("report: {}: {e}", r.id);
        }
    }
    prune(dir);
    BUSY.store(false, Ordering::Release);
}

/// Retry a report the page asks for (status failed → queued again).
pub fn retry(dir: &Path, id: &str) -> Result<(), String> {
    let _guard = LOCK.lock().unwrap();
    let mut r = list(dir).into_iter().find(|r| r.id == id).ok_or("нет такого отчёта")?;
    if r.status == "sent" {
        return Err("уже доставлен".into());
    }
    if r.files.iter().any(|(n, _)| !crate::fsx::exists(&reports_dir(dir).join(&r.id).join(n))) {
        return Err("файлы этого отчёта уже удалены — создайте новый".into());
    }
    r.status = "queued".into();
    r.next_try = 0;
    save_record(dir, &r).map_err(|e| e.to_string())?;
    drop(_guard);
    if let Some(w) = WAKE.lock().unwrap().as_ref() {
        let _ = w.send(());
    }
    Ok(())
}

pub fn list_json(dir: &Path) -> Value {
    json!({
        "reports": list(dir).iter().map(|r| json!({
            "id": r.id, "kind": r.kind, "module": module_name(&r.module), "created": r.created,
            "status": r.status, "attempts": r.attempts, "error": r.last_error, "next_try": r.next_try,
            "sent_at": r.sent_at, "description": clip(&r.description, 80),
        })).collect::<Vec<_>>(),
        "modules": MODULES.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "server": !url().is_empty(),
    })
}

/// The sender: at start (after the network is up), when a report is queued,
/// and every minute for what is due.
pub fn run(dir: PathBuf, shared: Arc<Shared>) {
    let (tx, rx) = mpsc::channel();
    *WAKE.lock().unwrap() = Some(tx);
    thread::spawn(move || {
        // Let the network come up first, unless a report is submitted meanwhile.
        let mut forced = rx.recv_timeout(Duration::from_secs(45)).is_ok();
        loop {
            let proxy = shared.proxy.lock().unwrap().clone();
            deliver(&dir, &proxy, forced);
            forced = rx.recv_timeout(Duration::from_secs(60)).is_ok();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_server_keeps_the_report_for_later() {
        // The owner's VPS ran stats.py from 0.4.40: /report → 404, and the reports were thrown away.
        assert!(matches!(answer(404, "not found"), Sent::Later(e) if e.contains("старая версия")));
        assert!(matches!(answer(413, "too big"), Sent::Never(_)), "a report too big stays refused");
        assert!(matches!(answer(503, ""), Sent::Later(_)));
    }

    #[test]
    fn secrets_never_survive() {
        let mut s = crate::settings::Settings {
            token: "2-123456-abcdefSECRET".into(),
            pc_token: "PcKeyXyZ123".into(),
            proxy: "socks5://bob:hunter22@1.2.3.4:1080".into(),
            cal_url: "https://calendar.google.com/calendar/ical/me%40gmail.com/private-0123abcdef/basic.ics".into(),
            ..Default::default()
        };
        s.pc_contacts.push(crate::settings::Contact { names: "мама|маме".into(), target: "@mommy_tg".into() });
        let sec = secrets(&s);
        let log = "login 2-123456-abcdefSECRET ok\nGET /x?client_id=AbCdEf123&q=1 Authorization: OAuth 2-999-zzz\n\
                   proxy socks5://bob:hunter22@1.2.3.4:1080 up\nsent to @mommy_tg\nkey PcKeyXyZ123\n\
                   tel +7 912 345-67-89 and 89123456789, ts 1700000000\nbot123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw x\n\
                   voice: \"напиши маме я дома\" -> Pc\nsay: \"Отправила\": ok\n\
                   cal https://calendar.google.com/calendar/ical/me%40gmail.com/private-0123abcdef/basic.ics x private-0123abcdef";
        let out = redact(log, &sec, false);
        for bad in ["abcdefSECRET", "AbCdEf123", "2-999-zzz", "hunter22", "mommy_tg", "PcKeyXyZ123", "912 345", "89123456789", "AAHdqTcv", "я дома", "маме", "0123abcdef", "me%40gmail"] {
            assert!(!out.contains(bad), "{bad} leaked:\n{out}");
        }
        assert!(out.contains("ts 1700000000"), "timestamps stay: {out}");
        assert!(out.contains("«телефон» and «телефон»,"), "{out}");
        assert!(out.contains("1.2.3.4:1080"), "the host stays: {out}");
        assert!(out.contains("voice: \"«фраза скрыта»\" -> Pc"), "{out}");
        // With phrases allowed, the phrase stays (secrets still go).
        let with = redact("voice: \"включи bonobo\" 2-123456-abcdefSECRET", &sec, true);
        assert_eq!(with, "voice: \"включи bonobo\" «скрыто»");
    }

    #[test]
    fn message_has_the_agreed_layout() {
        let r = Record {
            id: "SY-0A1B2C3D".into(), kind: "bug".into(), module: "airplay".into(), description: "Звук рвётся".into(),
            expected: "Ровно".into(), actual: "Рвётся".into(), created: 1_700_000_000, status: "queued".into(),
            attempts: 0, last_error: None, next_try: 0, sent_at: None, version: "0.5.0".into(), model: "yandexmini_2".into(),
            firmware: None, installation: None, files: vec![("system.log".into(), 2048)],
        };
        let m = message(&r);
        // Line by line, as agreed with the author.
        let heads: Vec<&str> = m.lines().map(|l| l.split(':').next().unwrap_or("")).collect();
        assert_eq!(
            &heads[..10],
            ["BUG REPORT #SY-0A1B2C3D", "Soundya Version", "Device", "Module", "Date", "User/Installation ID", "Description", "Expected", "Actual", "Attachments"],
            "{m}"
        );
        for part in ["Soundya Version: 0.5.0", "Device: yandexmini_2", "Module: AirPlay", "Date: 2023-11-14 22:13:20 UTC",
                     "Description: Звук рвётся", "Expected: Ровно", "Actual: Рвётся", "* system.log (2 КБ)"] {
            assert!(m.contains(part), "missing {part:?} in\n{m}");
        }
    }

    #[test]
    fn queue_survives_and_backs_off() {
        let d = std::env::temp_dir().join(format!("soundya-reports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let rec = Record {
            id: "SY-TEST".into(), kind: "bug".into(), module: "x".into(), description: "d".into(), expected: "".into(),
            actual: "".into(), created: now(), status: "queued".into(), attempts: 0, last_error: None, next_try: 0,
            sent_at: None, version: "v".into(), model: "m".into(), firmware: None, installation: None, files: vec![],
        };
        crate::fsx::ensure_dir(&reports_dir(&d));
        save_record(&d, &rec).unwrap();
        // «Reboot»: read back from flash.
        assert_eq!(list(&d), vec![rec]);
        assert!(backoff(1) < backoff(3) && backoff(9) == 3600);
        assert_eq!(date(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(date(0), "1970-01-01 00:00:00 UTC");
        assert!(retry(&d, "SY-NONE").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
