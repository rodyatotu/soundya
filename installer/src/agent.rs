//! soundya-pc for macOS: the speaker's commands to this Mac («открой телеграм»,
//! «усыпи компьютер», «найди в гугле …», «загрузка процессора»).
//!
//! The same protocol as pc/soundya-pc.ps1 on Windows (POST /<action>, body =
//! the text, port 8766), but only signed requests are taken: HMAC-SHA256 of
//! «time\nnonce\npath\nbody» with the key, at most 2 minutes old, each once.
//! The key itself is never accepted (this agent is newer than any speaker
//! that sent it), and only devices of the home network may connect.
//!
//! It runs as `soundya.app/Contents/MacOS/soundya agent`, started by launchd
//! (~/Library/LaunchAgents, no administrator rights). It does not listen to
//! the microphone, does not record or send anything from this Mac; it only
//! does the command and answers with the sentence Alice says.
//!
//! Every system action goes through `Runner`, so tests (and SOUNDYA_PC_DRYRUN=1)
//! see the commands without doing them.

use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{atomic::{AtomicUsize, Ordering}, Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

use crate::sha256;

pub const PORT: u16 = 8766;
pub const LABEL: &str = "io.github.rodyatotu.soundya-pc";
/// The menu bar icon (`soundya tray`): started at login, not kept alive after «Выйти».
pub const TRAY_LABEL: &str = "io.github.rodyatotu.soundya-tray";
/// How old a signed request may be (clocks of the speaker and the Mac differ a little).
const MAX_AGE: u64 = 120;
const MAX_BODY: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 16;

// ------------------------------------------------------------------ config

/// ~/Library/Application Support/soundya-pc/config.json
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub port: u16,
    pub key: String,
    /// Own spoken names → application name or path («моя игра» → "/Applications/Game.app").
    pub apps: BTreeMap<String, String>,
    /// Own spoken names → address («джира» → "https://jira.example.com").
    pub sites: BTreeMap<String, String>,
}

impl Config {
    pub fn from_json(v: &Value) -> Config {
        let map = |k: &str| -> BTreeMap<String, String> {
            v[k].as_object()
                .map(|m| m.iter().filter_map(|(a, b)| Some((a.to_lowercase(), b.as_str()?.to_owned()))).collect())
                .unwrap_or_default()
        };
        Config {
            port: v["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(PORT),
            key: v["key"].as_str().unwrap_or("").to_owned(),
            apps: map("apps"),
            sites: map("sites"),
        }
    }
}

pub fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub fn dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support/soundya-pc")
}

// ------------------------------------------------------------------ auth

/// Checks signatures and remembers the ones already used.
pub struct Auth {
    key: String,
    seen: Mutex<VecDeque<String>>,
}

impl Auth {
    pub fn new(key: &str) -> Auth {
        Auth { key: key.to_owned(), seen: Mutex::new(VecDeque::new()) }
    }

    /// True for a correct, fresh, not yet seen signature of `path` + `body`.
    pub fn check(&self, headers: &BTreeMap<String, String>, path: &str, body: &[u8], now: u64) -> bool {
        let (Some(ts), Some(nonce), Some(sign)) =
            (headers.get("x-soundya-time"), headers.get("x-soundya-nonce"), headers.get("x-soundya-sign"))
        else {
            return false;
        };
        let Ok(t) = ts.parse::<u64>() else { return false };
        if t.abs_diff(now) > MAX_AGE || nonce.is_empty() || nonce.len() > 64 {
            return false;
        }
        let mut msg = format!("{ts}\n{nonce}\n{path}\n").into_bytes();
        msg.extend_from_slice(body);
        let want = sha256::hex(&sha256::hmac_sha256(self.key.as_bytes(), &msg));
        if !same(want.as_bytes(), sign.to_ascii_lowercase().as_bytes()) {
            return false;
        }
        let id = format!("{ts}/{nonce}");
        let mut seen = self.seen.lock().unwrap();
        if seen.contains(&id) {
            return false;
        }
        seen.push_back(id);
        while seen.len() > 500 {
            seen.pop_front();
        }
        true
    }
}

/// Compares without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Only the home network (and this Mac itself) may talk to the agent.
pub fn home_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| home_network(IpAddr::V4(v4))),
    }
}

// ------------------------------------------------------------------ system actions

/// What a command printed and whether it succeeded.
#[derive(Debug, Clone, Default)]
pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs programs. The real one runs them; tests record them.
pub trait Runner: Send + Sync {
    /// A command that changes something (opens, quits, sleeps…).
    fn act(&self, program: &str, args: &[&str]) -> Out;
    /// A command that only reads (ps, sysctl, vm_stat…).
    fn read(&self, program: &str, args: &[&str]) -> Out;
    /// Folders with applications.
    fn app_dirs(&self) -> Vec<PathBuf>;
}

pub struct System {
    pub dry_run: bool,
}

fn run(program: &str, args: &[&str]) -> Out {
    match std::process::Command::new(program).args(args).output() {
        Ok(o) => Out {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Out { ok: false, stdout: String::new(), stderr: e.to_string() },
    }
}

impl Runner for System {
    fn act(&self, program: &str, args: &[&str]) -> Out {
        if self.dry_run {
            eprintln!("soundya-pc: dry run: {program} {args:?}");
            return Out { ok: true, ..Out::default() };
        }
        run(program, args)
    }
    fn read(&self, program: &str, args: &[&str]) -> Out {
        run(program, args)
    }
    fn app_dirs(&self) -> Vec<PathBuf> {
        let mut d = vec![
            PathBuf::from("/Applications"),
            PathBuf::from("/Applications/Utilities"),
            PathBuf::from("/System/Applications"),
            PathBuf::from("/System/Applications/Utilities"),
        ];
        d.push(home().join("Applications"));
        d
    }
}

/// Spoken names of common programs → the application's name on a Mac.
const ALIASES: &[(&str, &str)] = &[
    ("телеграм", "Telegram"), ("телеграмм", "Telegram"), ("телега", "Telegram"),
    ("хром", "Google Chrome"), ("гугл хром", "Google Chrome"), ("сафари", "Safari"),
    ("яндекс браузер", "Yandex"), ("яндекс", "Yandex"), ("опера", "Opera"),
    ("файрфокс", "Firefox"), ("фаерфокс", "Firefox"), ("эдж", "Microsoft Edge"),
    ("стим", "Steam"), ("дискорд", "Discord"), ("спотифай", "Spotify"), ("зум", "zoom.us"),
    ("ватсап", "WhatsApp"), ("вотсап", "WhatsApp"), ("скайп", "Skype"),
    ("ворд", "Microsoft Word"), ("эксель", "Microsoft Excel"), ("пауэрпоинт", "Microsoft PowerPoint"),
    ("аутлук", "Microsoft Outlook"), ("вс код", "Visual Studio Code"), ("вскод", "Visual Studio Code"),
    ("визуал студио код", "Visual Studio Code"), ("обс", "OBS"), ("вк", "VK Messenger"),
    ("музыка", "Music"), ("эппл мьюзик", "Music"), ("заметки", "Notes"), ("блокнот", "TextEdit"),
    ("календарь", "Calendar"), ("почта", "Mail"), ("фото", "Photos"), ("калькулятор", "Calculator"),
    ("терминал", "Terminal"), ("настройки", "System Settings"), ("системные настройки", "System Settings"),
    ("мониторинг системы", "Activity Monitor"), ("файнд", "Finder"), ("файндер", "Finder"),
    ("впн", "AmneziaVPN"), ("амнезия", "AmneziaVPN"), ("майнкрафт", "Minecraft"),
];

/// An installed application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub name: String,
    pub path: PathBuf,
}

fn installed(runner: &dyn Runner) -> Vec<App> {
    let mut apps = Vec::new();
    for d in runner.app_dirs() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "app") {
                if let Some(n) = p.file_stem().and_then(|s| s.to_str()) {
                    apps.push(App { name: n.to_owned(), path: p.clone() });
                }
            }
        }
    }
    apps
}

/// The application for a spoken name: own names, then known ones, then by its name.
pub fn find_app(runner: &dyn Runner, config: &Config, spoken: &str) -> Option<App> {
    let spoken = spoken.trim().to_lowercase().replace('ё', "е");
    if spoken.is_empty() {
        return None;
    }
    let target = config
        .apps
        .get(&spoken)
        .cloned()
        .or_else(|| ALIASES.iter().find(|(a, _)| *a == spoken).map(|(_, b)| (*b).to_owned()))
        .unwrap_or_else(|| spoken.clone());
    if target.ends_with(".app") && Path::new(&target).is_dir() {
        let name = Path::new(&target).file_stem().and_then(|s| s.to_str()).unwrap_or(&target).to_owned();
        return Some(App { name, path: PathBuf::from(target) });
    }
    let want = target.to_lowercase();
    let apps = installed(runner);
    apps.iter()
        .find(|a| a.name.to_lowercase() == want)
        .or_else(|| apps.iter().find(|a| a.name.to_lowercase().starts_with(&want)))
        .or_else(|| (want.len() >= 3).then(|| apps.iter().find(|a| a.name.to_lowercase().contains(&want))).flatten())
        .cloned()
}

/// Is the application running? By its executable among this user's processes.
fn running(runner: &dyn Runner, app: &App) -> bool {
    let plist = app.path.join("Contents/Info.plist");
    let exe = runner
        .read("/usr/bin/plutil", &["-extract", "CFBundleExecutable", "raw", "-o", "-", &plist.to_string_lossy()])
        .stdout
        .trim()
        .to_owned();
    let exe = if exe.is_empty() { app.name.clone() } else { exe };
    let inside = app.path.join("Contents/MacOS").join(&exe).to_string_lossy().into_owned();
    runner.read("/bin/ps", &["-axo", "comm="]).stdout.lines().any(|l| l.trim() == inside || l.trim().ends_with(&format!("/{exe}")))
}

/// macOS refused an Apple event (the user said no in «Автоматизация»).
fn not_permitted(o: &Out) -> bool {
    o.stderr.contains("-1743") || o.stderr.contains("Not authorized") || o.stderr.contains("не разрешено")
}

const AUTOMATION: &str = "Разрешите это на компьютере: Системные настройки → Конфиденциальность и безопасность → Автоматизация → soundya";

/// System Events: restart / shut down (the user's own apps may ask to save first).
fn power(runner: &dyn Runner, what: &str, done: &str) -> String {
    let o = runner.act("/usr/bin/osascript", &["-e", &format!("tell application \"System Events\" to {what}")]);
    if o.ok {
        done.to_owned()
    } else if not_permitted(&o) {
        format!("macOS не разрешила soundya управлять компьютером. {AUTOMATION} → System Events")
    } else {
        eprintln!("soundya-pc: {what}: {}", o.stderr.trim());
        "Не получилось. Посмотрите на экран компьютера".into()
    }
}

const ACCESSIBILITY: &str = "Разрешите на компьютере: Системные настройки → Конфиденциальность и безопасность → Универсальный доступ → soundya";

/// "x,y" / "x,y,double" (the speaker checked it; checked again here: nothing but numbers reaches AppleScript).
fn parse_click(spec: &str) -> Option<(u32, u32, &str)> {
    let p: Vec<&str> = spec.split(',').collect();
    let x: u32 = p.first()?.parse().ok().filter(|v| *v <= 20_000)?;
    let y: u32 = p.get(1)?.parse().ok().filter(|v| *v <= 20_000)?;
    let mode = p.get(2).copied().unwrap_or("");
    (p.len() <= 3 && ["", "double", "right"].contains(&mode)).then_some((x, y, mode))
}

/// A mouse click at screen points (System Events; needs «Универсальный доступ»).
fn click(runner: &dyn Runner, spec: &str) -> String {
    let Some((x, y, mode)) = parse_click(spec) else { return "Не поняла, куда кликнуть".into() };
    if mode == "right" {
        return "Правый клик на Mac пока не умею".into();
    }
    let times = if mode == "double" { 2 } else { 1 };
    let mut script = String::from("tell application \"System Events\"\n");
    for _ in 0..times {
        script.push_str(&format!("click at {{{x}, {y}}}\n"));
    }
    script.push_str("end tell");
    let o = runner.act("/usr/bin/osascript", &["-e", &script]);
    if o.ok {
        "Готово".into()
    } else if not_permitted(&o) || o.stderr.contains("-25211") || o.stderr.contains("assistive") {
        format!("macOS не разрешила soundya нажимать мышью. {ACCESSIBILITY}")
    } else {
        eprintln!("soundya-pc: click: {}", o.stderr.trim());
        "Не получилось кликнуть".into()
    }
}

/// "cmd+shift+t" → AppleScript for System Events; None if anything is not a known key.
fn key_script(spec: &str) -> Option<String> {
    let parts: Vec<&str> = spec.split('+').collect();
    let (key, mods) = parts.split_last()?;
    let mut using = Vec::new();
    for m in mods {
        using.push(match *m {
            "cmd" | "win" => "command down",
            "ctrl" => "control down",
            "alt" | "option" => "option down",
            "shift" => "shift down",
            _ => return None,
        });
    }
    let code = match *key {
        "enter" => Some(36), "esc" => Some(53), "tab" => Some(48), "space" => Some(49), "backspace" => Some(51),
        "delete" => Some(117), "left" => Some(123), "right" => Some(124), "down" => Some(125), "up" => Some(126),
        "home" => Some(115), "end" => Some(119), "pageup" => Some(116), "pagedown" => Some(121), "insert" => Some(114),
        "f1" => Some(122), "f2" => Some(120), "f3" => Some(99), "f4" => Some(118), "f5" => Some(96), "f6" => Some(97),
        "f7" => Some(98), "f8" => Some(100), "f9" => Some(101), "f10" => Some(109), "f11" => Some(103), "f12" => Some(111),
        _ => None,
    };
    let press = match code {
        Some(c) => format!("key code {c}"),
        None if key.chars().count() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric() || "-=[];',./`".contains(c)) => {
            format!("keystroke {}", applescript_string(key))
        }
        None => return None,
    };
    let using = if using.is_empty() { String::new() } else { format!(" using {{{}}}", using.join(", ")) };
    Some(format!("tell application \"System Events\" to {press}{using}"))
}

fn key(runner: &dyn Runner, spec: &str) -> String {
    let Some(script) = key_script(spec) else { return "Не знаю такой клавиши".into() };
    let o = runner.act("/usr/bin/osascript", &["-e", &script]);
    if o.ok {
        "Готово".into()
    } else if not_permitted(&o) || o.stderr.contains("-1719") || o.stderr.contains("assistive") {
        format!("macOS не разрешила soundya нажимать клавиши. {ACCESSIBILITY}")
    } else {
        eprintln!("soundya-pc: key: {}", o.stderr.trim());
        "Не получилось нажать".into()
    }
}

fn open_app(runner: &dyn Runner, config: &Config, name: &str) -> String {
    if name.trim().is_empty() {
        return "Скажите, что открыть".into();
    }
    let Some(app) = find_app(runner, config, name) else {
        return format!("Не нашла на компьютере программу {name}");
    };
    if runner.act("/usr/bin/open", &["-a", &app.path.to_string_lossy()]).ok {
        format!("Открываю {}", app.name)
    } else {
        format!("Не получилось открыть {}", app.name)
    }
}

fn close_app(runner: &dyn Runner, config: &Config, name: &str) -> String {
    if name.trim().is_empty() {
        return "Скажите, что закрыть".into();
    }
    let Some(app) = find_app(runner, config, name) else {
        return format!("{name} сейчас не запущен");
    };
    if !running(runner, &app) {
        return format!("{} сейчас не запущен", app.name);
    }
    // Politely, as ⌘Q: the app may ask to save first. Never killed.
    let o = runner.act("/usr/bin/osascript", &["-e", &format!("tell application {} to quit", applescript_string(&app.name))]);
    if o.ok {
        format!("Закрыла {}", app.name)
    } else if not_permitted(&o) {
        format!("macOS не разрешила закрыть {}. {AUTOMATION} → {}", app.name, app.name)
    } else {
        format!("Не получилось закрыть {}", app.name)
    }
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn process(runner: &dyn Runner, config: &Config, name: &str) -> String {
    match find_app(runner, config, name) {
        Some(app) if running(runner, &app) => format!("Да, {name} запущен"),
        _ => format!("Нет, {name} сейчас не запущен"),
    }
}

/// CPU, memory, uptime as JSON (no temperatures: macOS gives them only to root).
pub fn metrics(runner: &dyn Runner) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("os".into(), json!("macos"));
    let ncpu: f64 = runner.read("/usr/sbin/sysctl", &["-n", "hw.ncpu"]).stdout.trim().parse().unwrap_or(0.0);
    if ncpu > 0.0 {
        let total: f64 = runner.read("/bin/ps", &["-A", "-o", "%cpu="]).stdout.lines().filter_map(|l| l.trim().replace(',', ".").parse::<f64>().ok()).sum();
        m.insert("cpu".into(), json!(((total / ncpu).round() as i64).clamp(0, 100)));
    }
    let memsize: f64 = runner.read("/usr/sbin/sysctl", &["-n", "hw.memsize"]).stdout.trim().parse().unwrap_or(0.0);
    let vm = runner.read("/usr/bin/vm_stat", &[]).stdout;
    let page: f64 = vm.split("page size of ").nth(1).and_then(|s| s.split_whitespace().next()).and_then(|s| s.parse().ok()).unwrap_or(16384.0);
    let pages = |label: &str| -> f64 {
        vm.lines()
            .find(|l| l.starts_with(label))
            .and_then(|l| l.rsplit(':').next())
            .and_then(|s| s.trim().trim_end_matches('.').parse().ok())
            .unwrap_or(0.0)
    };
    let used = (pages("Pages active") + pages("Pages wired down") + pages("Pages occupied by compressor")) * page;
    if memsize > 0.0 && used > 0.0 {
        m.insert("ram_used".into(), json!(((used * 100.0 / memsize).round() as i64).clamp(0, 100)));
        m.insert("ram_total_gb".into(), json!((memsize / 1073741824.0 * 10.0).round() / 10.0));
    }
    let boot = runner.read("/usr/sbin/sysctl", &["-n", "kern.boottime"]).stdout;
    if let Some(sec) = boot.split("sec = ").nth(1).and_then(|s| s.split(',').next()).and_then(|s| s.trim().parse::<u64>().ok()) {
        m.insert("uptime_h".into(), json!(now().saturating_sub(sec) / 3600));
    }
    Value::Object(m)
}

/// Percent-encoding for a search query.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn strip_any<'a>(t: &'a str, words: &[&str]) -> Option<&'a str> {
    words.iter().find_map(|w| t.strip_prefix(w).filter(|r| r.is_empty() || r.starts_with(' ')).map(str::trim))
}

/// A free phrase («найди в гугле …», «открой ютуб», «закрой хром»): what to do with it.
pub fn command(runner: &dyn Runner, config: &Config, text: &str) -> String {
    let mut t = text.to_lowercase().replace('ё', "е");
    t = t.chars().map(|c| if matches!(c, ',' | '!' | '?') { ' ' } else { c }).collect();
    let t = t.trim().trim_end_matches('.').split_whitespace().filter(|w| *w != "пожалуйста").collect::<Vec<_>>().join(" ");
    // «… на компьютере / на маке / на компе» is about this Mac anyway.
    let t = [" на компьютере", " на компе", " на маке", " на макбуке"].iter().fold(t, |t, s| t.replace(s, ""));
    let t = t.trim();

    if let Some(rest) = strip_any(t, &["найди", "поищи", "открой", "включи"]) {
        if let Some(q) = strip_any(rest.trim_start_matches("мне ").trim(), &["на ютубе", "в ютубе", "на youtube", "в youtube", "на ютюбе", "на ютьюбе"]) {
            if !q.is_empty() {
                return browse(runner, &format!("https://www.youtube.com/results?search_query={}", encode(q)), &format!("ютуб: {q}"));
            }
        }
    }
    if let Some(rest) = strip_any(t, &["найди", "поищи", "загугли", "погугли"]) {
        let q = strip_any(rest, &["в гугле", "в интернете", "в браузере"]).unwrap_or(rest);
        if q.is_empty() {
            return "Скажите, что найти".into();
        }
        return browse(runner, &format!("https://www.google.com/search?q={}", encode(q)), &format!("поиск: {q}"));
    }
    if let Some(name) = strip_any(t, &["закрой", "закрыть", "выключи", "выруби"]) {
        return close_app(runner, config, name);
    }
    if let Some(obj) = strip_any(t, &["открой", "открыть", "запусти", "запустить", "включи"]) {
        let obj = strip_any(obj, &["программу", "приложение", "игру", "сайт"]).unwrap_or(obj);
        if let Some(url) = config.sites.get(obj) {
            return browse(runner, url, obj);
        }
        if obj == "ютуб" || obj == "youtube" {
            return browse(runner, "https://www.youtube.com", "ютуб");
        }
        let site = obj.split('/').next().unwrap_or("");
        if site.contains('.') && site.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
            return browse(runner, &format!("https://{obj}"), obj);
        }
        return open_app(runner, config, obj);
    }
    "На Mac я это пока не умею".into()
}

/// Opens an address in the default browser.
fn browse(runner: &dyn Runner, url: &str, what: &str) -> String {
    if runner.act("/usr/bin/open", &[url]).ok {
        format!("Открываю {what}")
    } else {
        "Не получилось открыть браузер".into()
    }
}

/// The answer to a command; None = unknown command (404).
pub fn handle(runner: &dyn Runner, config: &Config, action: &str, body: &str, hostname: &str) -> Option<String> {
    Some(match action {
        "ping" => format!("Компьютер {hostname} на связи"),
        "metrics" => metrics(runner).to_string(),
        "process" => process(runner, config, body),
        "open" => open_app(runner, config, body),
        "close" => close_app(runner, config, body),
        "do" => command(runner, config, body),
        "lock" => {
            // Turning the display off locks the Mac when «Требовать пароль» is set to «Сразу» (the default).
            if runner.act("/usr/bin/pmset", &["displaysleepnow"]).ok { "Блокирую компьютер".into() } else { "Не получилось заблокировать компьютер".into() }
        }
        "sleep" => {
            // A moment later: the answer reaches the speaker first.
            if runner.act("/bin/sh", &["-c", "(sleep 2; /usr/bin/pmset sleepnow) >/dev/null 2>&1 &"]).ok {
                "Усыпляю компьютер".into()
            } else {
                "Не получилось усыпить компьютер".into()
            }
        }
        "click" => click(runner, body),
        "key" => key(runner, body),
        "shutdown" => power(runner, "shut down", "Выключаю компьютер"),
        "restart" => power(runner, "restart", "Перезагружаю компьютер"),
        _ => return None,
    })
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ------------------------------------------------------------------ HTTP

struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_request(conn: &TcpStream) -> Option<Request> {
    let mut r = BufReader::new(conn.take(64 * 1024 + MAX_BODY as u64));
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let mut headers = BTreeMap::new();
    for _ in 0..64 {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return None;
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    Some(Request { method, path, headers, body })
}

fn answer(mut conn: &TcpStream, code: u16, text: &str) {
    let reason = match code {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Bad Request",
    };
    // Like soundya-pc 0.5.00+ on Windows: a refusal says that signatures are checked,
    // so the speaker never retries with the key in the clear.
    let auth = if code == 403 { "X-Soundya-Auth: hmac\r\n" } else { "" };
    let _ = write!(conn, "HTTP/1.1 {code} {reason}\r\n{auth}Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
}

pub struct Agent {
    pub config: Config,
    pub auth: Auth,
    pub runner: Box<dyn Runner>,
    pub hostname: String,
}

impl Agent {
    pub fn serve_one(&self, conn: TcpStream) {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
        let peer = conn.peer_addr().map(|a| a.ip()).ok();
        if !peer.is_some_and(home_network) {
            eprintln!("soundya-pc: refused {peer:?}: not the home network");
            return;
        }
        let Some(req) = read_request(&conn) else {
            return answer(&conn, 400, "Непонятный запрос");
        };
        let path = req.path.split('?').next().unwrap_or("/").to_owned();
        if req.method != "POST" || !self.auth.check(&req.headers, &path, &req.body, now()) {
            eprintln!("soundya-pc: wrong signature from {peer:?} for {path}");
            return answer(&conn, 403, "Неверный ключ");
        }
        let action = path.trim_matches('/').to_ascii_lowercase();
        let body = String::from_utf8_lossy(&req.body).into_owned();
        match handle(self.runner.as_ref(), &self.config, &action, &body, &self.hostname) {
            Some(text) => {
                eprintln!("soundya-pc: {action}: {}", text.lines().next().unwrap_or(""));
                answer(&conn, 200, &text)
            }
            None => answer(&conn, 404, &format!("Неизвестная команда {action}")),
        }
    }

    /// Accepts connections until the process ends.
    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        let busy = Arc::new(AtomicUsize::new(0));
        for conn in listener.incoming().flatten() {
            if busy.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                continue;
            }
            busy.fetch_add(1, Ordering::AcqRel);
            let (me, busy) = (self.clone(), busy.clone());
            std::thread::spawn(move || {
                me.serve_one(conn);
                busy.fetch_sub(1, Ordering::AcqRel);
            });
        }
    }
}

/// `soundya agent`: reads the config and serves until stopped (launchd restarts it).
pub fn main() -> ! {
    let dir = dir(&home());
    let config = std::fs::read_to_string(dir.join("config.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| Config::from_json(&v))
        .unwrap_or_default();
    if config.key.len() < 16 {
        eprintln!("soundya-pc: no key in {}; set it up again from soundya.app", dir.join("config.json").display());
        std::process::exit(1);
    }
    let listener = match TcpListener::bind(("0.0.0.0", config.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("soundya-pc: cannot listen on port {}: {e}", config.port);
            std::process::exit(1);
        }
    };
    let dry_run = std::env::var("SOUNDYA_PC_DRYRUN").is_ok_and(|v| v == "1");
    let hostname = run("/usr/sbin/scutil", &["--get", "ComputerName"]).stdout.trim().to_owned();
    eprintln!("soundya-pc: listening on port {}{}", config.port, if dry_run { " (dry run)" } else { "" });
    let agent = Arc::new(Agent { auth: Auth::new(&config.key), config, runner: Box::new(System { dry_run }), hostname });
    agent.serve(listener);
    std::process::exit(0)
}

// ------------------------------------------------------------------ setup

/// A new key: 24 random bytes, base64 without characters that confuse copying.
pub fn new_key() -> Result<String, String> {
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| format!("нет случайных чисел: {e}"))?;
    Ok(crate::pc::base64(&b).replace('+', "x").replace('/', "y"))
}

/// The launchd job: this very binary (inside soundya.app) with `agent`.
pub fn launch_agent_plist(exe: &Path, log: &Path) -> String {
    let esc = |p: &Path| p.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>agent</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        esc(exe),
        esc(log),
        esc(log)
    )
}

/// The launchd job for the menu bar icon: at login, in the user's session only.
pub fn tray_plist(exe: &Path, log: &Path) -> String {
    let esc = |p: &Path| p.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{TRAY_LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>tray</string></array>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        esc(exe),
        esc(log),
        esc(log)
    )
}

/// This Mac is connected to a speaker (the PC agent's config is there): opening soundya.app
/// shows soundya itself, not the installer.
pub fn set_up(home: &Path) -> bool {
    dir(home).join("config.json").exists()
}

/// The menu bar icon's login job, if a Mac connected before it existed has none.
pub fn ensure_tray_plist(home: &Path, exe: &Path) {
    let plist = home.join(format!("Library/LaunchAgents/{TRAY_LABEL}.plist"));
    if !plist.exists() && installed_app(exe, home) {
        let _ = std::fs::write(&plist, tray_plist(exe, &home.join("Library/Logs/soundya-tray.log")));
    }
}

/// The running menu bar icon's pid file and the «show the window» note it watches.
pub fn tray_pid_file(home: &Path) -> PathBuf {
    dir(home).join("tray.pid")
}
pub fn tray_open_file(home: &Path) -> PathBuf {
    dir(home).join("tray.open")
}

/// The speaker this Mac was connected to (for the menu bar icon); soundya.local if none.
pub fn speaker_of(home: &Path) -> String {
    std::fs::read_to_string(dir(home).join("config.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["speaker"].as_str().map(str::to_owned))
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':'))
        .unwrap_or_else(|| "soundya.local".into())
}

/// Keeps the speaker's address in the agent's config (the key and the rest stay).
pub fn remember_speaker(home: &Path, ip: &str) -> Result<(), String> {
    let path = dir(home).join("config.json");
    let mut v: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    if !v.is_object() {
        v = json!({});
    }
    v["speaker"] = json!(ip);
    write_private(&path, &serde_json::to_string_pretty(&v).unwrap_or_default())
}

/// Where soundya.app must be for the agent to keep working (not a download or a
/// temporary copy that macOS made: the job would point at nothing after it).
pub fn installed_app(exe: &Path, home: &Path) -> bool {
    let s = exe.to_string_lossy();
    (s.starts_with("/Applications/") || exe.starts_with(home.join("Applications"))) && s.contains(".app/Contents/MacOS/")
}

/// Files for the agent under `home`: config (the old key kept), launchd job, log.
/// Returns the key. Does not start anything.
pub fn write_files(home: &Path, exe: &Path) -> Result<String, String> {
    let d = dir(home);
    std::fs::create_dir_all(&d).map_err(|e| format!("не создалась папка {}: {e}", d.display()))?;
    let path = d.join("config.json");
    let mut v: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    if !v.is_object() {
        v = json!({});
    }
    let key = match v["key"].as_str() {
        Some(k) if k.len() >= 16 => k.to_owned(),
        _ => new_key()?,
    };
    v["key"] = json!(key);
    if v["port"].as_u64().is_none() {
        v["port"] = json!(PORT);
    }
    for k in ["apps", "sites"] {
        if !v[k].is_object() {
            v[k] = json!({});
        }
    }
    write_private(&path, &serde_json::to_string_pretty(&v).unwrap_or_default())?;
    let agents = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents).map_err(|e| format!("не создалась папка {}: {e}", agents.display()))?;
    let log = home.join("Library/Logs/soundya-pc.log");
    let _ = std::fs::create_dir_all(home.join("Library/Logs"));
    std::fs::write(agents.join(format!("{LABEL}.plist")), launch_agent_plist(exe, &log)).map_err(|e| format!("не записался {LABEL}.plist: {e}"))?;
    let tray_log = home.join("Library/Logs/soundya-tray.log");
    std::fs::write(agents.join(format!("{TRAY_LABEL}.plist")), tray_plist(exe, &tray_log)).map_err(|e| format!("не записался {TRAY_LABEL}.plist: {e}"))?;
    Ok(key)
}

/// Only the user can read the key.
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("не записался {}: {e}", path.display()))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    f.write_all(text.as_bytes()).map_err(|e| format!("не записался {}: {e}", path.display()))
}

/// Signed headers for a request, as the speaker makes them (src/pc.rs::sign).
pub fn sign(key: &str, path: &str, body: &[u8]) -> Vec<(String, String)> {
    let mut n = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut n);
    }
    let (ts, nonce) = (now().to_string(), sha256::hex(&n));
    let mut msg = format!("{ts}\n{nonce}\n{path}\n").into_bytes();
    msg.extend_from_slice(body);
    let sign = sha256::hex(&sha256::hmac_sha256(key.as_bytes(), &msg));
    vec![("X-Soundya-Time".into(), ts), ("X-Soundya-Nonce".into(), nonce), ("X-Soundya-Sign".into(), sign)]
}

/// The MAC address of the interface that has `ip` (from `ifconfig`), for «включи компьютер».
pub fn mac_for(ifconfig: &str, ip: &str) -> Option<String> {
    let mut mac = None;
    for line in ifconfig.lines() {
        if !line.starts_with(['\t', ' ']) {
            mac = None; // a new interface
            continue;
        }
        let mut w = line.split_whitespace();
        match (w.next(), w.next()) {
            (Some("ether"), Some(m)) => mac = Some(m.to_owned()),
            (Some("inet"), Some(a)) if a == ip => {
                // ether comes before inet in macOS's output.
                return mac;
            }
            _ => {}
        }
    }
    None
}

/// Removes the launchd job and the agent's files (the log stays for a bug report).
pub fn remove_files(home: &Path) {
    let _ = std::fs::remove_file(home.join(format!("Library/LaunchAgents/{LABEL}.plist")));
    let _ = std::fs::remove_file(home.join(format!("Library/LaunchAgents/{TRAY_LABEL}.plist")));
    let _ = std::fs::remove_dir_all(dir(home));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_bar_icon_finds_its_speaker_and_starts_at_login() {
        let home = std::env::temp_dir().join(format!("soundya-tray-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(speaker_of(&home), "soundya.local", "not connected yet: found by its name");
        std::fs::create_dir_all(dir(&home)).unwrap();
        std::fs::write(dir(&home).join("config.json"), r#"{"key":"k1234567890123456","port":8766}"#).unwrap();
        remember_speaker(&home, "192.168.0.112").unwrap();
        assert_eq!(speaker_of(&home), "192.168.0.112");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(dir(&home).join("config.json")).unwrap()).unwrap();
        assert_eq!(v["key"], "k1234567890123456", "the key stays");
        let p = tray_plist(Path::new("/Applications/soundya.app/Contents/MacOS/soundya"), Path::new("/tmp/t.log"));
        assert!(p.contains("<string>tray</string>") && p.contains(TRAY_LABEL) && !p.contains("KeepAlive"), "{p}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[derive(Default)]
    struct Fake {
        acts: Mutex<Vec<String>>,
        /// (program, stderr) for a failing act.
        fail: Option<(&'static str, &'static str)>,
        apps: Vec<PathBuf>,
        ps: String,
    }

    impl Runner for Fake {
        fn act(&self, program: &str, args: &[&str]) -> Out {
            self.acts.lock().unwrap().push(format!("{program} {}", args.join(" ")));
            match self.fail {
                Some((p, err)) if p == program => Out { ok: false, stderr: err.into(), ..Out::default() },
                _ => Out { ok: true, ..Out::default() },
            }
        }
        fn read(&self, program: &str, args: &[&str]) -> Out {
            let stdout = match (program, args.first().copied()) {
                ("/bin/ps", Some("-axo")) => self.ps.clone(),
                ("/bin/ps", _) => "12.0\n30.0\n 6,0\n".into(),
                ("/usr/sbin/sysctl", Some("-n")) if args[1] == "hw.ncpu" => "8\n".into(),
                ("/usr/sbin/sysctl", Some("-n")) if args[1] == "hw.memsize" => "17179869184\n".into(),
                ("/usr/sbin/sysctl", Some("-n")) => format!("{{ sec = {}, usec = 0 }} Thu Jan  1\n", now() - 5 * 3600),
                ("/usr/bin/vm_stat", _) => "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 100.\nPages active: 300000.\nPages wired down: 100000.\nPages occupied by compressor: 124288.\n".into(),
                // The executable is named like the app (…/Name.app/Contents/Info.plist).
                ("/usr/bin/plutil", _) => args.last().and_then(|p| p.rsplit(".app/").nth(1)).and_then(|p| p.rsplit('/').next()).unwrap_or("").to_owned(),
                _ => String::new(),
            };
            Out { ok: true, stdout, stderr: String::new() }
        }
        fn app_dirs(&self) -> Vec<PathBuf> {
            self.apps.clone()
        }
    }

    fn apps_dir(names: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundya-agent-test-{}-{}", std::process::id(), names.join("_").replace(' ', "")));
        let _ = std::fs::remove_dir_all(&d);
        for n in names {
            std::fs::create_dir_all(d.join(format!("{n}.app/Contents/MacOS"))).unwrap();
        }
        d
    }

    fn fake(names: &[&str]) -> Fake {
        Fake { apps: vec![apps_dir(names)], ..Fake::default() }
    }

    fn signed(key: &str, ts: u64, nonce: &str, path: &str, body: &str) -> BTreeMap<String, String> {
        let mut msg = format!("{ts}\n{nonce}\n{path}\n").into_bytes();
        msg.extend_from_slice(body.as_bytes());
        BTreeMap::from([
            ("x-soundya-time".to_owned(), ts.to_string()),
            ("x-soundya-nonce".to_owned(), nonce.to_owned()),
            ("x-soundya-sign".to_owned(), sha256::hex(&sha256::hmac_sha256(key.as_bytes(), &msg))),
        ])
    }

    #[test]
    fn signature_is_the_speakers() {
        // The vector of src/pc.rs::signature_matches_the_agent (checked with python's hmac).
        let auth = Auth::new("k3y");
        let mut h = signed("k3y", 1700000000, "abcd", "/open", "телеграм");
        assert_eq!(h["x-soundya-sign"], "6b3843f5bdb061ace24a63cbc6497128a7bcba596b9d458fa15ee1c488345c48");
        assert!(auth.check(&h, "/open", "телеграм".as_bytes(), 1700000030));
        // Replay: the same request again is refused.
        assert!(!auth.check(&h, "/open", "телеграм".as_bytes(), 1700000031));
        // Another body, path, key or an old request: refused.
        h = signed("k3y", 1700000000, "abce", "/open", "телеграм");
        assert!(!auth.check(&h, "/open", "хром".as_bytes(), 1700000000));
        assert!(!auth.check(&h, "/close", "телеграм".as_bytes(), 1700000000));
        assert!(!Auth::new("other").check(&h, "/open", "телеграм".as_bytes(), 1700000000));
        assert!(!auth.check(&h, "/open", "телеграм".as_bytes(), 1700000000 + MAX_AGE + 1));
    }

    #[test]
    fn the_key_itself_is_never_taken() {
        let auth = Auth::new("secret-key-123456");
        let h = BTreeMap::from([("x-soundya-key".to_owned(), "secret-key-123456".to_owned())]);
        assert!(!auth.check(&h, "/lock", b"", now()));
    }

    #[test]
    fn only_the_home_network() {
        for ok in ["192.168.1.5", "10.0.0.2", "172.20.1.1", "127.0.0.1", "169.254.3.4", "::1", "::ffff:192.168.0.2"] {
            assert!(home_network(ok.parse().unwrap()), "{ok}");
        }
        for no in ["8.8.8.8", "100.64.0.1", "2a00:1450::1"] {
            assert!(!home_network(no.parse().unwrap()), "{no}");
        }
    }

    #[test]
    fn opens_and_closes_by_spoken_names() {
        let f = Fake { ps: "/Applications/x/Telegram.app/Contents/MacOS/Telegram\n".into(), ..fake(&["Telegram", "Google Chrome"]) };
        let c = Config::default();
        assert_eq!(command(&f, &c, "Открой телеграм на компьютере"), "Открываю Telegram");
        assert!(f.acts.lock().unwrap()[0].starts_with("/usr/bin/open -a ") && f.acts.lock().unwrap()[0].ends_with("Telegram.app"));
        assert_eq!(command(&f, &c, "открой стим"), "Не нашла на компьютере программу стим");
        assert_eq!(command(&f, &c, "закрой телеграм"), "Закрыла Telegram");
        assert_eq!(f.acts.lock().unwrap().last().unwrap(), "/usr/bin/osascript -e tell application \"Telegram\" to quit");
        // Not running: nothing is quit.
        let n = f.acts.lock().unwrap().len();
        assert_eq!(command(&f, &c, "закрой хром"), "Google Chrome сейчас не запущен");
        assert_eq!(f.acts.lock().unwrap().len(), n);
        assert_eq!(handle(&f, &c, "process", "телеграм", "Mac"), Some("Да, телеграм запущен".into()));
    }

    #[test]
    fn own_names_and_sites_from_the_config() {
        let f = fake(&["My Game"]);
        let c = Config::from_json(&json!({ "apps": { "Моя игра": "My Game" }, "sites": { "джира": "https://jira.example.com" } }));
        assert_eq!(command(&f, &c, "запусти мою игру"), "Не нашла на компьютере программу мою игру");
        assert_eq!(command(&f, &c, "запусти моя игра"), "Открываю My Game");
        assert_eq!(command(&f, &c, "открой джира"), "Открываю джира");
        assert_eq!(f.acts.lock().unwrap().last().unwrap(), "/usr/bin/open https://jira.example.com");
        assert_eq!(command(&f, &c, "открой github.com"), "Открываю github.com");
        assert_eq!(f.acts.lock().unwrap().last().unwrap(), "/usr/bin/open https://github.com");
    }

    #[test]
    fn searches() {
        let f = fake(&[]);
        let c = Config::default();
        assert_eq!(command(&f, &c, "найди в гугле как приготовить плов"), "Открываю поиск: как приготовить плов");
        assert_eq!(f.acts.lock().unwrap().last().unwrap(), "/usr/bin/open https://www.google.com/search?q=%D0%BA%D0%B0%D0%BA%20%D0%BF%D1%80%D0%B8%D0%B3%D0%BE%D1%82%D0%BE%D0%B2%D0%B8%D1%82%D1%8C%20%D0%BF%D0%BB%D0%BE%D0%B2");
        assert_eq!(command(&f, &c, "Загугли курс доллара?"), "Открываю поиск: курс доллара");
        assert_eq!(command(&f, &c, "найди на ютубе lofi"), "Открываю ютуб: lofi");
        assert_eq!(f.acts.lock().unwrap().last().unwrap(), "/usr/bin/open https://www.youtube.com/results?search_query=lofi");
        assert_eq!(command(&f, &c, "найди"), "Скажите, что найти");
        // Windows-only things are said honestly, nothing is run.
        let n = f.acts.lock().unwrap().len();
        assert_eq!(command(&f, &c, "запиши купить молоко"), "На Mac я это пока не умею");
        assert_eq!(f.acts.lock().unwrap().len(), n);
    }

    #[test]
    fn clicks_and_keys() {
        let f = fake(&[]);
        let c = Config::default();
        assert_eq!(handle(&f, &c, "click", "500,300", "Mac").as_deref(), Some("Готово"));
        assert_eq!(handle(&f, &c, "click", "10,20,double", "Mac").as_deref(), Some("Готово"));
        assert_eq!(handle(&f, &c, "key", "cmd+shift+t", "Mac").as_deref(), Some("Готово"));
        assert_eq!(handle(&f, &c, "key", "enter", "Mac").as_deref(), Some("Готово"));
        let acts = f.acts.lock().unwrap().clone();
        assert!(acts[0].contains("click at {500, 300}"), "{acts:?}");
        assert_eq!(acts[1].matches("click at {10, 20}").count(), 2);
        assert!(acts[2].ends_with("keystroke \"t\" using {command down, shift down}"), "{acts:?}");
        assert!(acts[3].ends_with("key code 36"), "{acts:?}");
        // Nothing but numbers and known keys reaches AppleScript.
        for (a, b) in [("click", "1,2\"; do shell script \"x"), ("click", "a,b"), ("key", "cmd+\""), ("key", "hello"), ("key", "rm+t")] {
            handle(&f, &c, a, b, "Mac");
        }
        assert_eq!(f.acts.lock().unwrap().len(), 4);
        // «Универсальный доступ» not given: the answer says where.
        let f = Fake { fail: Some(("/usr/bin/osascript", "System Events got an error: osascript is not allowed assistive access. (-25211)")), ..fake(&[]) };
        let r = handle(&f, &c, "click", "1,1", "Mac").unwrap();
        assert!(r.contains("Универсальный доступ"), "{r}");
    }

    #[test]
    fn power_and_a_refused_permission() {
        let f = fake(&[]);
        let c = Config::default();
        assert_eq!(handle(&f, &c, "sleep", "", "Mac").as_deref(), Some("Усыпляю компьютер"));
        assert_eq!(handle(&f, &c, "lock", "", "Mac").as_deref(), Some("Блокирую компьютер"));
        assert_eq!(handle(&f, &c, "restart", "", "Mac").as_deref(), Some("Перезагружаю компьютер"));
        assert_eq!(handle(&f, &c, "ping", "", "Mac").as_deref(), Some("Компьютер Mac на связи"));
        assert_eq!(handle(&f, &c, "format_disk", "", "Mac"), None);
        // «Автоматизация» refused: the answer says where to allow it; nothing else is tried.
        let f = Fake { fail: Some(("/usr/bin/osascript", "execution error: Not authorized to send Apple events to System Events. (-1743)")), ..fake(&[]) };
        let r = handle(&f, &c, "shutdown", "", "Mac").unwrap();
        assert!(r.starts_with("macOS не разрешила") && r.contains("Автоматизация"), "{r}");
        assert_eq!(f.acts.lock().unwrap().len(), 1);
    }

    #[test]
    fn metrics_without_temperatures() {
        let m = metrics(&fake(&[]));
        assert_eq!(m["os"], "macos");
        assert_eq!(m["cpu"], 6); // (12 + 30 + 6) / 8
        assert_eq!(m["ram_used"], 50); // 524288 pages × 16 KiB of 16 GiB
        assert_eq!(m["ram_total_gb"], 16.0);
        assert_eq!(m["uptime_h"], 5);
        assert!(m.get("cpu_temp").is_none());
    }

    fn start(key: &str, runner: Fake) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let agent = Arc::new(Agent {
            config: Config { key: key.into(), ..Config::default() },
            auth: Auth::new(key),
            runner: Box::new(runner),
            hostname: "Mac".into(),
        });
        std::thread::spawn(move || agent.serve(listener));
        addr
    }

    fn post(addr: &str, path: &str, headers: &BTreeMap<String, String>, body: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        let mut req = format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n", body.len());
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(body);
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn over_http_like_the_speaker() {
        let key = "test-key-0123456789";
        let addr = start(key, fake(&["Telegram"]));
        let h = signed(key, now(), "n1", "/do", "открой телеграм");
        let r = post(&addr, "/do", &h, "открой телеграм");
        assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("Открываю Telegram"), "{r}");
        // The same request again (a replay) and a wrong key: 403 marked as «signatures checked».
        let r = post(&addr, "/do", &h, "открой телеграм");
        assert!(r.starts_with("HTTP/1.1 403") && r.contains("X-Soundya-Auth: hmac"), "{r}");
        let r = post(&addr, "/lock", &signed("wrong-key-000000000", now(), "n2", "/lock", ""), "");
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        let r = post(&addr, "/lock", &BTreeMap::from([("x-soundya-key".to_owned(), key.to_owned())]), "");
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        // Unknown command: 404, as on Windows.
        let r = post(&addr, "/format", &signed(key, now(), "n3", "/format", ""), "");
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        // Too big a body is not read at all (only the length is sent here).
        let mut s = TcpStream::connect(&addr).unwrap();
        s.write_all(format!("POST /do HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1).as_bytes()).unwrap();
        let mut r = String::new();
        let _ = s.read_to_string(&mut r);
        assert!(r.starts_with("HTTP/1.1 400"), "{r}");
    }

    #[test]
    fn setup_files_keep_the_key_and_go_away() {
        let home = std::env::temp_dir().join(format!("soundya-agent-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let exe = Path::new("/Applications/soundya.app/Contents/MacOS/soundya");
        let key = write_files(&home, exe).unwrap();
        assert!(key.len() >= 32 && !key.contains('+') && !key.contains('/'));
        let cfg = dir(&home).join("config.json");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o600);
        // Own names survive a reinstall, and so does the key (the speaker keeps working).
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        v["apps"] = json!({ "игра": "Game" });
        std::fs::write(&cfg, v.to_string()).unwrap();
        assert_eq!(write_files(&home, exe).unwrap(), key);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["apps"]["игра"], "Game");
        let plist = std::fs::read_to_string(home.join(format!("Library/LaunchAgents/{LABEL}.plist"))).unwrap();
        assert!(plist.contains("<string>/Applications/soundya.app/Contents/MacOS/soundya</string><string>agent</string>"));
        remove_files(&home);
        assert!(!home.join(format!("Library/LaunchAgents/{LABEL}.plist")).exists() && !dir(&home).exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn mac_address_of_the_right_interface() {
        let ifc = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\tinet 127.0.0.1 netmask 0xff000000\nen0: flags=8863<UP,BROADCAST> mtu 1500\n\tether a4:83:e7:01:02:03\n\tinet6 fe80::1%en0 prefixlen 64\n\tinet 192.168.1.20 netmask 0xffffff00 broadcast 192.168.1.255\nen1: flags=8863<UP> mtu 1500\n\tether 00:11:22:33:44:55\n\tinet 10.0.0.5 netmask 0xff000000\n";
        assert_eq!(mac_for(ifc, "192.168.1.20").as_deref(), Some("a4:83:e7:01:02:03"));
        assert_eq!(mac_for(ifc, "10.0.0.5").as_deref(), Some("00:11:22:33:44:55"));
        assert_eq!(mac_for(ifc, "127.0.0.1"), None);
        assert_eq!(mac_for(ifc, "192.168.1.2"), None);
    }

    #[test]
    fn own_signature_is_accepted() {
        let headers: BTreeMap<String, String> = sign("k3y-0123456789abcdef", "/ping", b"").into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
        assert!(Auth::new("k3y-0123456789abcdef").check(&headers, "/ping", b"", now()));
    }

    #[test]
    fn only_from_applications() {
        let home = Path::new("/Users/me");
        assert!(installed_app(Path::new("/Applications/soundya.app/Contents/MacOS/soundya"), home));
        assert!(installed_app(Path::new("/Users/me/Applications/soundya.app/Contents/MacOS/soundya"), home));
        assert!(!installed_app(Path::new("/private/var/folders/x/T/AppTranslocation/1/d/soundya.app/Contents/MacOS/soundya"), home));
        assert!(!installed_app(Path::new("/Volumes/soundya/soundya.app/Contents/MacOS/soundya"), home));
    }
}
