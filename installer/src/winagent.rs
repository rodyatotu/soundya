//! soundya-pc for Windows, native: the speaker's commands to this PC («открой телеграм»,
//! «усыпи компьютер», «напиши маме …», «загрузка процессора»). It replaces
//! pc/soundya-pc.ps1 with the same protocol and the same config.json:
//!
//!   POST http://<pc>:8766/<action>, body = the text (UTF-8), answer = the sentence Alice says.
//!   Signed: X-Soundya-Time / -Nonce / -Sign = HMAC-SHA256(key, «time\nnonce\npath\nbody»),
//!   at most 2 minutes old, each once. Speakers before 0.5.00 send X-Soundya-Key: still
//!   taken unless "allow_plain_key": false (as soundya-pc.ps1 did).
//!   A refusal carries X-Soundya-Auth: hmac, so the speaker never retries with the key.
//!
//! %LOCALAPPDATA%\soundya-pc\config.json:
//!   { "port": 8766, "key": "…", "apps": { "моя игра": "C:\\Games\\game.exe" },
//!     "sites": { "работа": "https://jira.example.com" },
//!     "contacts": { "мама|маме|маму": "@username" }, "telegram_send": true,
//!     "vpn": "AmneziaVPN", "duck": true, "speaker": "192.168.0.112", "speaker_port": 8765 }
//!
//! Everything that touches the system goes through the `Sys` trait (winsys.rs on Windows);
//! the rest — signatures, phrases, finding programs, contacts, ducking — is plain Rust and
//! is tested on any OS. New actions (scenarios, the to-do list…) are one line in ACTIONS.

#![cfg_attr(not(windows), allow(dead_code))]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{atomic::{AtomicUsize, Ordering}, Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

use crate::sha256;

pub const PORT: u16 = 8766;
/// How old a signed request may be (the speaker's and the PC's clocks differ a little).
const MAX_AGE: u64 = 120;
const MAX_BODY: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 16;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ------------------------------------------------------------------ config

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub key: String,
    /// Own spoken names (lower case) → program name, shortcut or .exe path.
    pub apps: BTreeMap<String, String>,
    /// Own spoken names (lower case) → address.
    pub sites: BTreeMap<String, String>,
    /// Telegram contacts: «мама|маме|маму» → "@username" / "+7999…".
    pub contacts: Vec<(String, String)>,
    pub telegram_send: bool,
    pub vpn: String,
    pub duck: bool,
    pub speaker: String,
    pub speaker_port: u16,
    pub allow_plain_key: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config::from_json(&json!({}))
    }
}

impl Config {
    pub fn from_json(v: &Value) -> Config {
        let map = |k: &str| -> BTreeMap<String, String> {
            v[k].as_object()
                .map(|m| m.iter().filter_map(|(a, b)| Some((a.trim().to_lowercase(), b.as_str()?.to_owned()))).collect())
                .unwrap_or_default()
        };
        Config {
            port: v["port"].as_u64().and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0).unwrap_or(PORT),
            key: v["key"].as_str().unwrap_or("").to_owned(),
            apps: map("apps"),
            sites: map("sites"),
            contacts: v["contacts"]
                .as_object()
                .map(|m| m.iter().filter_map(|(a, b)| Some((a.clone(), b.as_str()?.to_owned()))).collect())
                .unwrap_or_default(),
            telegram_send: v["telegram_send"].as_bool().unwrap_or(true),
            vpn: v["vpn"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("AmneziaVPN").to_owned(),
            duck: v["duck"].as_bool().unwrap_or(true),
            speaker: v["speaker"].as_str().unwrap_or("").trim().to_owned(),
            speaker_port: v["speaker_port"].as_u64().and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0).unwrap_or(8765),
            allow_plain_key: v["allow_plain_key"].as_bool().unwrap_or(true),
        }
    }

    pub fn load(dir: &Path) -> Option<Config> {
        let text = std::fs::read_to_string(dir.join("config.json")).ok()?;
        let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        Some(Config::from_json(&v))
    }
}

// ------------------------------------------------------------------ auth

/// Checks signatures and remembers the ones already used.
pub struct Auth {
    key: String,
    allow_plain: bool,
    seen: Mutex<VecDeque<String>>,
}

impl Auth {
    pub fn new(key: &str, allow_plain: bool) -> Auth {
        Auth { key: key.to_owned(), allow_plain, seen: Mutex::new(VecDeque::new()) }
    }

    /// True for a correct, fresh, not yet seen signature of `path` + `body` (or, from an old
    /// speaker, the key itself when that is allowed).
    pub fn check(&self, headers: &BTreeMap<String, String>, path: &str, body: &[u8], now: u64) -> bool {
        if self.key.len() < 16 {
            return false;
        }
        let Some(sign) = headers.get("x-soundya-sign") else {
            return self.allow_plain && headers.get("x-soundya-key").is_some_and(|k| same(k.as_bytes(), self.key.as_bytes()));
        };
        let (Some(ts), Some(nonce)) = (headers.get("x-soundya-time"), headers.get("x-soundya-nonce")) else { return false };
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

/// Only the home network (and this PC itself) may talk to the agent.
pub fn home_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| home_network(IpAddr::V4(v4))),
    }
}

/// Signed headers for a request, as the speaker makes them (src/pc.rs::sign).
pub fn sign(key: &str, path: &str, body: &[u8], nonce_bytes: &[u8]) -> Vec<(String, String)> {
    let (ts, nonce) = (now().to_string(), sha256::hex(nonce_bytes));
    let mut msg = format!("{ts}\n{nonce}\n{path}\n").into_bytes();
    msg.extend_from_slice(body);
    let s = sha256::hex(&sha256::hmac_sha256(key.as_bytes(), &msg));
    vec![("X-Soundya-Time".into(), ts), ("X-Soundya-Nonce".into(), nonce), ("X-Soundya-Sign".into(), s)]
}

// ------------------------------------------------------------------ the system

/// A program, shortcut, game or store app that can be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    /// Without «.exe».
    pub name: String,
    pub path: Option<String>,
    pub has_window: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Lock,
    Sleep,
    Shutdown,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Double,
}

/// What the agent needs from Windows. winsys.rs is the real one; tests have a fake.
pub trait Sys: Send + Sync {
    /// Folders walked with their subfolders (Start menu) and ones looked at only on top (desktops).
    fn shortcut_dirs(&self) -> (Vec<PathBuf>, Vec<PathBuf>);
    /// A registry string: hive "HKCU" / "HKLM", key, value ("" = the default value).
    fn reg_str(&self, hive: &str, key: &str, value: &str) -> Option<String>;
    /// Store apps (Windows 10/11): name and «shell:AppsFolder\<AppUserModelID>».
    fn start_apps(&self) -> Vec<Item>;
    fn processes(&self) -> Vec<Proc>;
    /// Brings a window of one of these processes to the front.
    fn focus(&self, pids: &[u32]) -> bool;
    /// Asks the windows of these processes to close, waits, then ends what is left.
    fn close(&self, pids: &[u32]);
    /// Opens a file, shortcut, address or program like a double click (ShellExecute).
    fn open(&self, target: &str) -> Result<(), String>;
    /// Starts a program with arguments.
    fn run(&self, exe: &str, args: &[String]) -> Result<(), String>;
    fn power(&self, what: Power) -> bool;
    fn click(&self, x: i32, y: i32, button: Button) -> bool;
    /// Virtual-key codes: the modifiers held, then the key.
    fn keys(&self, mods: &[u16], key: u16) -> bool;
    /// Types text into the window in front (Unicode, whatever the layout).
    fn type_text(&self, text: &str) -> bool;
    fn cursor(&self) -> Option<(i32, i32)>;
    fn metrics(&self) -> Value;
    /// Folders: "Desktop", "Documents".
    fn folder(&self, which: &str) -> PathBuf;
    fn local_app_data(&self) -> PathBuf;
    fn hostname(&self) -> String;
    /// Local time (year, month, day, hour, minute, second).
    fn local_time(&self) -> (i32, u32, u32, u32, u32, u32);
    fn sleep_ms(&self, ms: u64);
    /// Apps using the microphone right now (for Smart Ducking).
    fn mic_apps(&self) -> BTreeSet<String>;
    /// Blocks until the microphone list may have changed (or `max` passed).
    fn wait_mic_change(&self, max: Duration);
    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }
    /// The MAC of the interface that has this address ("" if none).
    fn mac_for(&self, _ip: &str) -> String {
        String::new()
    }
}

// ------------------------------------------------------------------ text helpers

const TRANSLIT: &[(char, &str)] = &[
    ('а', "a"), ('б', "b"), ('в', "v"), ('г', "g"), ('д', "d"), ('е', "e"), ('ё', "e"), ('ж', "zh"), ('з', "z"),
    ('и', "i"), ('й', "y"), ('к', "k"), ('л', "l"), ('м', "m"), ('н', "n"), ('о', "o"), ('п', "p"), ('р', "r"),
    ('с', "s"), ('т', "t"), ('у', "u"), ('ф', "f"), ('х', "h"), ('ц', "c"), ('ч', "ch"), ('ш', "sh"), ('щ', "sch"),
    ('ъ', ""), ('ы', "y"), ('ь', ""), ('э', "e"), ('ю', "yu"), ('я', "ya"),
];

pub fn to_latin(s: &str) -> String {
    s.chars().map(|c| TRANSLIT.iter().find(|(k, _)| *k == c).map_or_else(|| c.to_string(), |(_, v)| (*v).to_owned())).collect()
}

fn cyrillic(s: &str) -> bool {
    s.chars().any(|c| ('а'..='я').contains(&c) || c == 'ё')
}

/// Levenshtein distance by characters.
pub fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

pub fn capitalize(s: &str) -> String {
    let s = s.trim();
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A name that may be a file name: no \/:*?"<>|, at most 60 characters.
pub fn clean_name(s: &str) -> String {
    let s: String = s.chars().filter(|c| !"\\/:*?\"<>|".contains(*c)).collect();
    s.trim().chars().take(60).collect::<String>().trim().to_owned()
}

/// `t` without the word `w` at its start («открой» in «открой хром»; «открой» alone gives "").
fn strip_word<'a>(t: &'a str, w: &str) -> Option<&'a str> {
    t.strip_prefix(w).filter(|r| r.is_empty() || r.starts_with(' ')).map(str::trim)
}

fn strip_any<'a>(t: &'a str, words: &[&str]) -> Option<&'a str> {
    words.iter().find_map(|w| strip_word(t, w))
}

fn words(t: &str) -> Vec<&str> {
    t.split_whitespace().collect()
}

/// Some word of `t` begins with one of `prefixes` (regex «\b(?:a|b)»).
fn has_word_prefix(t: &str, prefixes: &[&str]) -> bool {
    t.split_whitespace().any(|w| prefixes.iter().any(|p| w.starts_with(p)))
}

fn has_word(t: &str, list: &[&str]) -> bool {
    t.split_whitespace().any(|w| list.contains(&w))
}

/// A site name like «github.com» or «ya.ru/maps».
fn is_domain(s: &str) -> bool {
    let host = s.split('/').next().unwrap_or("");
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

/// Percent-encoding for a search query (as [Uri]::EscapeDataString).
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ------------------------------------------------------------------ shortcuts (.lnk)

/// The target of a Windows shortcut (.lnk, MS-SHLLINK): the local path from LinkInfo, or
/// the environment-variable path (%ProgramFiles%\…) from its extra data. None: the
/// shortcut has neither (store apps, installer «advertised» shortcuts) — trusted then.
pub fn lnk_target(b: &[u8]) -> Option<String> {
    let u16le = |o: usize| -> Option<u16> { Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?])) };
    let u32le = |o: usize| -> Option<u32> { Some(u32::from_le_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?])) };
    if u32le(0)? != 0x4C {
        return None;
    }
    let flags = u32le(0x14)?;
    let mut pos = 0x4C;
    if flags & 0x1 != 0 {
        pos += 2 + u16le(pos)? as usize;
    }
    let ansi_z = |o: usize| -> Option<String> {
        let end = b.get(o..)?.iter().position(|c| *c == 0)?;
        // ANSI in the system's code page: paths of programs are ASCII nearly always.
        Some(b[o..o + end].iter().map(|c| *c as char).collect())
    };
    let wide_z = |o: usize| -> Option<String> {
        let mut v = Vec::new();
        let mut i = o;
        loop {
            let c = u16le(i)?;
            if c == 0 {
                break;
            }
            v.push(c);
            i += 2;
        }
        String::from_utf16(&v).ok()
    };
    let mut target = None;
    if flags & 0x2 != 0 {
        let start = pos;
        let size = u32le(start)? as usize;
        let header = u32le(start + 4)? as usize;
        let li_flags = u32le(start + 8)?;
        if li_flags & 1 != 0 {
            let suffix_ansi = ansi_z(start + u32le(start + 24)? as usize).unwrap_or_default();
            let base = if header >= 0x24 {
                let (bu, su) = (u32le(start + 28)? as usize, u32le(start + 32)? as usize);
                wide_z(start + bu).map(|base| base + &wide_z(start + su).unwrap_or_default())
            } else {
                None
            };
            target = base.or_else(|| ansi_z(start + u32le(start + 16)? as usize).map(|base| base + &suffix_ansi));
        }
        pos = start + size;
    }
    if target.as_deref().is_some_and(|t| !t.is_empty()) {
        return target;
    }
    // StringData (name, relative path, working dir, arguments, icon), then ExtraData.
    for bit in [0x4, 0x8, 0x10, 0x20, 0x40] {
        if flags & bit != 0 {
            let count = u16le(pos)? as usize;
            pos += 2 + if flags & 0x80 != 0 { count * 2 } else { count };
        }
    }
    while let Some(size) = u32le(pos).map(|s| s as usize).filter(|s| *s >= 8) {
        if u32le(pos + 4)? == 0xA000_0001 {
            // EnvironmentVariableDataBlock: TargetAnsi[260], TargetUnicode[520 bytes].
            let t = wide_z(pos + 8 + 260).filter(|t| !t.is_empty()).or_else(|| ansi_z(pos + 8))?;
            return Some(expand_env(&t));
        }
        pos += size;
    }
    None
}

/// «%ProgramFiles%\x» with this process's environment.
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ------------------------------------------------------------------ the agent

/// Spoken (Russian) names of common programs → the name to look for.
const ALIASES: &[(&str, &str)] = &[
    ("телеграм", "Telegram"), ("телеграмм", "Telegram"), ("телега", "Telegram"),
    ("хром", "Google Chrome"), ("гугл хром", "Google Chrome"), ("гугл", "Google Chrome"), ("браузер", "Google Chrome"),
    ("яндекс браузер", "Yandex"), ("яндекс", "Yandex"), ("опера", "Opera"), ("опера джи экс", "Opera GX"),
    ("файрфокс", "Firefox"), ("фаерфокс", "Firefox"), ("эдж", "Microsoft Edge"),
    ("стим", "Steam"), ("дискорд", "Discord"), ("спотифай", "Spotify"), ("зум", "Zoom"),
    ("эпик", "Epic Games Launcher"), ("эпик геймс", "Epic Games Launcher"), ("батл нет", "Battle.net"),
    ("обс", "OBS Studio"), ("ватсап", "WhatsApp"), ("вотсап", "WhatsApp"), ("скайп", "Skype"),
    ("ворд", "Word"), ("эксель", "Excel"), ("пауэрпоинт", "PowerPoint"), ("аутлук", "Outlook"),
    ("вс код", "Visual Studio Code"), ("визуал студио код", "Visual Studio Code"), ("вскод", "Visual Studio Code"),
    ("блокнот", "notepad"), ("калькулятор", "calc"), ("проводник", "explorer"), ("паинт", "mspaint"),
    ("диспетчер задач", "taskmgr"), ("параметры", "ms-settings:"), ("настройки", "ms-settings:"),
    ("командная строка", "cmd"), ("терминал", "wt"), ("вк", "VK Messenger"), ("кс", "Counter-Strike 2"),
    ("контра", "Counter-Strike 2"), ("дота", "Dota 2"), ("майнкрафт", "Minecraft"),
    ("впн", "AmneziaVPN"), ("амнезия", "AmneziaVPN"), ("амнезию", "AmneziaVPN"),
    ("дедлок", "Deadlock"), ("варфрейм", "Warframe"), ("раст", "Rust"), ("валорант", "VALORANT"),
];

/// Spoken site names → addresses (config.json "sites" adds to them).
const SITES: &[(&str, &str)] = &[
    ("ютуб", "https://www.youtube.com"), ("youtube", "https://www.youtube.com"), ("ютьюб", "https://www.youtube.com"),
    ("вк", "https://vk.com"), ("вконтакте", "https://vk.com"), ("гугл", "https://www.google.com"),
    ("почта", "https://mail.google.com"), ("почту", "https://mail.google.com"), ("гмейл", "https://mail.google.com"),
    ("яндекс", "https://ya.ru"), ("яндекс почта", "https://mail.yandex.ru"), ("яндекс почту", "https://mail.yandex.ru"),
    ("твич", "https://www.twitch.tv"), ("гитхаб", "https://github.com"), ("кинопоиск", "https://www.kinopoisk.ru"),
    ("саундклауд", "https://soundcloud.com"), ("авито", "https://www.avito.ru"), ("озон", "https://www.ozon.ru"),
    ("вайлдберриз", "https://www.wildberries.ru"), ("вб", "https://www.wildberries.ru"), ("карты", "https://yandex.ru/maps"),
    ("переводчик", "https://translate.google.com"), ("реддит", "https://www.reddit.com"), ("твиттер", "https://x.com"),
    ("инстаграм", "https://www.instagram.com"), ("чат гпт", "https://chatgpt.com"), ("чатгпт", "https://chatgpt.com"),
    ("клод", "https://claude.ai"), ("телеграм веб", "https://web.telegram.org"), ("вотсап веб", "https://web.whatsapp.com"),
    ("ютуб музыку", "https://music.youtube.com"), ("спотифай веб", "https://open.spotify.com"),
];

/// One request's context: the system, the settings, what the speaker sent along.
pub struct Ctx<'a> {
    pub agent: &'a Agent,
    /// X-Soundya-Contacts: the contacts set on the speaker's page (base64 JSON).
    pub contacts_header: Option<String>,
}

impl Ctx<'_> {
    fn sys(&self) -> &dyn Sys {
        self.agent.sys.as_ref()
    }
    fn cfg(&self) -> &Config {
        &self.agent.config
    }
}

pub type ActionFn = fn(&Ctx, &str) -> String;

/// Every action the agent knows: POST /<name>. New ones (scenarios, the to-do list, …) go here.
pub const ACTIONS: &[(&str, ActionFn)] = &[
    ("ping", |c, _| format!("Компьютер {} на связи", c.sys().hostname())),
    ("metrics", |c, _| c.sys().metrics().to_string()),
    ("process", |c, b| test_running(c, b)),
    ("open", |c, b| do_open(c, b)),
    ("close", |c, b| do_close(c, b)),
    ("do", |c, b| do_command(c, b)),
    ("click", |c, b| do_click(c.sys(), b)),
    ("key", |c, b| do_key(c.sys(), b)),
    ("type", |c, b| do_type(c.sys(), b)),
    ("mouse", |c, _| do_mouse(c.sys())),
    ("lock", |c, _| power(c.sys(), Power::Lock, "Заблокировала компьютер")),
    ("sleep", |c, _| power(c.sys(), Power::Sleep, "Усыпляю компьютер")),
    ("shutdown", |c, _| power(c.sys(), Power::Shutdown, "Выключаю компьютер")),
    ("restart", |c, _| power(c.sys(), Power::Restart, "Перезагружаю компьютер")),
];

fn power(sys: &dyn Sys, what: Power, done: &str) -> String {
    if sys.power(what) {
        done.to_owned()
    } else {
        "Не получилось. Посмотрите на экран компьютера".into()
    }
}

/// The answer to an action; None = unknown action (404).
pub fn handle(ctx: &Ctx, action: &str, body: &str) -> Option<String> {
    ACTIONS.iter().find(|(name, _)| *name == action).map(|(_, f)| f(ctx, body.trim()))
}

pub struct Agent {
    pub config: Config,
    pub auth: Auth,
    pub sys: Box<dyn Sys>,
    pub dir: PathBuf,
    shortcuts: Mutex<Option<(Instant, Arc<Vec<Item>>)>>,
    status: Mutex<Status>,
}

impl Agent {
    pub fn new(config: Config, sys: Box<dyn Sys>, dir: PathBuf) -> Agent {
        let mut status = Status::load(&dir);
        status.pid = std::process::id();
        status.started = now();
        Agent { auth: Auth::new(&config.key, config.allow_plain_key), config, sys, dir, shortcuts: Mutex::new(None), status: Mutex::new(status) }
    }

    pub fn log(&self, text: &str) {
        log_to(&self.dir, self.sys.local_time(), text);
    }

    /// Start-menu and desktop shortcuts, read once and kept for 5 minutes.
    fn shortcuts(&self) -> Arc<Vec<Item>> {
        let mut c = self.shortcuts.lock().unwrap();
        if let Some((t, list)) = c.as_ref() {
            if t.elapsed() < Duration::from_secs(300) {
                return list.clone();
            }
        }
        let (deep, flat) = self.sys.shortcut_dirs();
        let list = Arc::new(scan_shortcuts(&deep, &flat));
        *c = Some((Instant::now(), list.clone()));
        list
    }
}

/// %LOCALAPPDATA%\soundya-pc\log.txt, started over past 256 KB.
pub fn log_to(dir: &Path, (y, mo, d, h, mi, s): (i32, u32, u32, u32, u32, u32), text: &str) {
    let path = dir.join("log.txt");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} {text}");
    }
}

fn scan_shortcuts(deep: &[PathBuf], flat: &[PathBuf]) -> Vec<Item> {
    fn walk(dir: &Path, depth: u32, recurse: bool, out: &mut Vec<Item>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if recurse && depth < 6 {
                    walk(&p, depth + 1, recurse, out);
                }
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
            if !matches!(ext.as_deref(), Some("lnk" | "url")) {
                continue;
            }
            let Some(name) = p.file_stem().and_then(|s| s.to_str()) else { continue };
            let low = name.to_lowercase();
            if ["uninstall", "удал", "readme", "help", "справк"].iter().any(|w| low.contains(w)) {
                continue;
            }
            out.push(Item { name: name.to_owned(), path: p.to_string_lossy().into_owned() });
        }
    }
    let mut out = Vec::new();
    for d in deep {
        walk(d, 0, true, &mut out);
    }
    for d in flat {
        walk(d, 0, false, &mut out);
    }
    out
}

/// Does this shortcut still lead somewhere? (Old ones keep pointing at a moved program.)
fn works(sys: &dyn Sys, item: &Item) -> bool {
    if !item.path.to_ascii_lowercase().ends_with(".lnk") {
        return true;
    }
    match std::fs::read(&item.path).ok().and_then(|b| lnk_target(&b)) {
        // Network paths can take many seconds to answer: trusted.
        Some(t) if !t.starts_with("\\\\") => sys.exists(&t),
        _ => true,
    }
}

fn first_working<'a>(sys: &dyn Sys, items: impl IntoIterator<Item = &'a Item>) -> Option<Item> {
    items.into_iter().find(|i| works(sys, i)).cloned()
}

/// Steam's games: name and steam://rungameid/<id>.
fn steam_games(sys: &dyn Sys) -> Vec<Item> {
    let root = sys
        .reg_str("HKCU", "Software\\Valve\\Steam", "SteamPath")
        .or_else(|| {
            [std::env::var("ProgramFiles(x86)").ok(), std::env::var("ProgramFiles").ok()]
                .into_iter()
                .flatten()
                .map(|p| format!("{p}\\Steam"))
                .find(|p| sys.exists(p))
        });
    let Some(root) = root.map(|r| r.replace('/', "\\")) else { return Vec::new() };
    let mut libs = vec![root.clone()];
    if let Ok(vdf) = std::fs::read_to_string(Path::new(&root).join("steamapps").join("libraryfolders.vdf")) {
        libs.extend(vdf_paths(&vdf));
    }
    libs.dedup();
    let mut out = Vec::new();
    for lib in libs {
        let Ok(rd) = std::fs::read_dir(Path::new(&lib).join("steamapps")) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("appmanifest_") && n.ends_with(".acf") {
                if let Some(item) = std::fs::read_to_string(e.path()).ok().and_then(|t| acf_game(&t)) {
                    out.push(item);
                }
            }
        }
    }
    out
}

/// «"key"  "value"» of a Valve text file.
fn vdf_value<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    let pat = format!("\"{key}\"");
    text.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix(&pat)?.trim();
            rest.strip_prefix('"')?.strip_suffix('"')
        })
        .collect()
}

pub fn vdf_paths(vdf: &str) -> Vec<String> {
    vdf_value(vdf, "path").into_iter().map(|p| p.replace("\\\\", "\\")).collect()
}

pub fn acf_game(acf: &str) -> Option<Item> {
    let id = vdf_value(acf, "appid").into_iter().next()?;
    let name = vdf_value(acf, "name").into_iter().next()?;
    if id.is_empty() || name.is_empty() || ["Steamworks", "Redistributable", "Proton", "Runtime"].iter().any(|w| name.contains(w)) {
        return None;
    }
    Some(Item { name: name.to_owned(), path: format!("steam://rungameid/{id}") })
}

/// What to start for a spoken name.
pub fn find_app(ctx: &Ctx, spoken: &str) -> Option<Item> {
    let sys = ctx.sys();
    let spoken = spoken.trim().to_lowercase();
    if spoken.is_empty() {
        return None;
    }
    let target = ctx
        .cfg()
        .apps
        .get(&spoken)
        .cloned()
        .or_else(|| ALIASES.iter().find(|(a, _)| *a == spoken).map(|(_, b)| (*b).to_owned()))
        .unwrap_or_else(|| spoken.clone());
    if (target.contains('\\') || target.contains('/')) && sys.exists(&target) {
        let name = Path::new(&target).file_stem().and_then(|s| s.to_str()).unwrap_or(&target).to_owned();
        return Some(Item { name, path: target });
    }
    let low = target.to_lowercase();
    let mut wanted = vec![low.clone()];
    if cyrillic(&low) {
        wanted.push(to_latin(&low));
    }
    // Steam itself: where it really is now.
    if low == "steam" {
        if let Some(exe) = sys.reg_str("HKCU", "Software\\Valve\\Steam", "SteamExe").map(|e| e.replace('/', "\\")) {
            if sys.exists(&exe) {
                return Some(Item { name: "Steam".into(), path: exe });
            }
        }
    }
    let all = ctx.agent.shortcuts();
    fn by_len(mut v: Vec<&Item>) -> Vec<&Item> {
        v.sort_by_key(|i| i.name.chars().count());
        v
    }
    for w in &wanted {
        let lower = |i: &&Item| i.name.to_lowercase();
        if let Some(i) = first_working(sys, all.iter().filter(|i| lower(i) == *w)) {
            return Some(i);
        }
        if let Some(i) = first_working(sys, by_len(all.iter().filter(|i| lower(i).starts_with(w.as_str())).collect())) {
            return Some(i);
        }
        if let Some(i) = first_working(sys, by_len(all.iter().filter(|i| lower(i).contains(w.as_str())).collect())) {
            return Some(i);
        }
    }
    // Steam games: «открой дедлок», «запусти доту».
    let games = steam_games(sys);
    for w in &wanted {
        let mut hit: Vec<&Item> = games.iter().filter(|g| g.name.to_lowercase().contains(w.as_str())).collect();
        hit.sort_by_key(|i| i.name.chars().count());
        if let Some(g) = hit.first() {
            return Some((*g).clone());
        }
    }
    // Store apps (Windows 10/11).
    let apps = sys.start_apps();
    for w in &wanted {
        let mut hit: Vec<&Item> = apps.iter().filter(|a| a.name.to_lowercase().contains(w.as_str())).collect();
        hit.sort_by_key(|i| i.name.chars().count());
        if let Some(a) = hit.first() {
            return Some((*a).clone());
        }
    }
    // Close spelling: «дискорд» → «diskord» ~ «Discord».
    for w in &wanted {
        let n = w.chars().count();
        if n < 4 {
            continue;
        }
        // Longer names may be further off: «dedlok» is 3 edits from «deadlock».
        let limit = if n >= 6 { 4 } else { 3 };
        let mut close: Vec<(usize, &Item)> = all
            .iter()
            .chain(games.iter())
            .filter_map(|s| {
                let name = s.name.to_lowercase();
                let first = name.split_whitespace().next().unwrap_or("").to_owned();
                let d = distance(w, &name).min(distance(w, &first));
                (d < limit).then_some((d, s))
            })
            .collect();
        close.sort_by_key(|(d, _)| *d);
        if let Some(i) = first_working(sys, close.into_iter().map(|(_, i)| i)) {
            return Some(i);
        }
    }
    // Built-in programs: notepad, calc, explorer, ms-settings:, …
    if !cyrillic(&target) && !target.chars().any(char::is_whitespace) {
        return Some(Item { name: target.clone(), path: target });
    }
    None
}

/// Process names that belong to an app: its shortcut's or own .exe, the first word of its name.
fn process_names(item: &Item) -> Vec<String> {
    let mut names = Vec::new();
    let low = item.path.to_ascii_lowercase();
    if low.ends_with(".lnk") {
        if let Some(t) = std::fs::read(&item.path).ok().and_then(|b| lnk_target(&b)) {
            if let Some(stem) = win_stem(&t) {
                names.push(stem);
            }
        }
    } else if low.ends_with(".exe") {
        if let Some(stem) = win_stem(&item.path) {
            names.push(stem);
        }
    }
    if let Some(first) = item.name.split_whitespace().next() {
        names.push(first.to_owned());
    }
    let mut seen = BTreeSet::new();
    names.retain(|n| !n.is_empty() && seen.insert(n.to_lowercase()));
    names
}

/// «C:\x\Telegram.exe» → «Telegram» (both kinds of slashes, on any OS).
fn win_stem(path: &str) -> Option<String> {
    let file = path.rsplit(['\\', '/']).next()?;
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    (!stem.is_empty()).then(|| stem.to_owned())
}

/// The app is running already: its window to the front (or, hidden in the tray, its own
/// .exe once more — single-instance apps show themselves).
fn focus_running(ctx: &Ctx, item: &Item) -> Option<String> {
    let names: Vec<String> = process_names(item).into_iter().filter(|n| n.chars().count() >= 3).map(|n| n.to_lowercase()).collect();
    if names.is_empty() {
        return None;
    }
    let procs: Vec<Proc> = ctx
        .sys()
        .processes()
        .into_iter()
        .filter(|p| {
            let pn = p.name.to_lowercase();
            names.iter().any(|n| pn == *n || (n.chars().count() >= 4 && pn.starts_with(n.as_str())))
        })
        .collect();
    if procs.is_empty() {
        return None;
    }
    let with_window: Vec<u32> = procs.iter().filter(|p| p.has_window).map(|p| p.pid).collect();
    if !with_window.is_empty() && ctx.sys().focus(&with_window) {
        ctx.agent.log(&format!("open '{}': running, window to the front", item.name));
        return Some(format!("Переключаю на {}", item.name));
    }
    let exe = procs.iter().find_map(|p| p.path.clone())?;
    ctx.sys().run(&exe, &[]).ok()?;
    ctx.agent.log(&format!("open '{}': running in the tray, started its own exe {exe}", item.name));
    Some(format!("Открываю {}", item.name))
}

fn do_open(ctx: &Ctx, name: &str) -> String {
    if name.is_empty() {
        return "Скажите, что открыть".into();
    }
    let t0 = Instant::now();
    let found = find_app(ctx, name);
    ctx.agent.log(&format!("find '{name}': {} ms", t0.elapsed().as_millis()));
    let Some(app) = found else { return format!("Не нашла на компьютере программу {name}") };
    let special = app.path.starts_with("steam:") || app.path.starts_with("shell:");
    if !special && app.path != app.name {
        if let Some(done) = focus_running(ctx, &app) {
            return done;
        }
    }
    match ctx.sys().open(&app.path) {
        Ok(()) => {
            ctx.agent.log(&format!("open '{name}' -> {}", app.path));
            format!("Открываю {}", app.name)
        }
        Err(e) => {
            ctx.agent.log(&format!("open '{name}' failed: {e}"));
            // The last guess (a built-in command such as notepad) did not exist either.
            if app.path == app.name {
                format!("Не нашла на компьютере программу {name}")
            } else {
                format!("Не получилось открыть {}", app.name)
            }
        }
    }
}

fn do_close(ctx: &Ctx, name: &str) -> String {
    if name.is_empty() {
        return "Скажите, что закрыть".into();
    }
    let app = find_app(ctx, name);
    let names: Vec<String> = match &app {
        Some(a) => process_names(a),
        None => vec![to_latin(&name.to_lowercase())],
    }
    .into_iter()
    .map(|n| n.to_lowercase())
    .collect();
    let pids: Vec<u32> = ctx
        .sys()
        .processes()
        .into_iter()
        .filter(|p| {
            let pn = p.name.to_lowercase();
            names.iter().any(|n| pn.starts_with(n.as_str()))
        })
        .map(|p| p.pid)
        .collect();
    if pids.is_empty() {
        return format!("{name} сейчас не запущен");
    }
    ctx.sys().close(&pids);
    ctx.agent.log(&format!("close '{name}' -> {} processes", pids.len()));
    format!("Закрыла {}", app.map_or_else(|| name.to_owned(), |a| a.name))
}

/// «Запущен ли стим на компьютере»: by the same names as «закрой стим».
fn test_running(ctx: &Ctx, name: &str) -> String {
    let names: Vec<String> = match find_app(ctx, name) {
        Some(a) => process_names(&a),
        None => vec![name.to_owned()],
    }
    .into_iter()
    .map(|n| n.to_lowercase())
    .collect();
    if ctx.sys().processes().iter().any(|p| names.contains(&p.name.to_lowercase())) {
        format!("Да, {name} запущен")
    } else {
        format!("Нет, {name} сейчас не запущен")
    }
}

// ------------------------------------------------------------------ free phrases

/// Normalizes a phrase as soundya-pc.ps1 did: lower case, ё → е, no , ! ?, dots only inside
/// site names, no «пожалуйста».
pub fn normalize(text: &str) -> String {
    let t = text.to_lowercase().replace('ё', "е");
    let t: String = t.chars().map(|c| if matches!(c, ',' | '!' | '?') { ' ' } else { c }).collect();
    let mut out = String::new();
    let chars: Vec<char> = t.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '.' && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) {
            out.push(' ');
        } else {
            out.push(*c);
        }
    }
    out.split_whitespace().filter(|w| *w != "пожалуйста" && *w != "плиз").collect::<Vec<_>>().join(" ")
}

/// What a phrase means and does it. The answer is said by Alice.
pub fn do_command(ctx: &Ctx, text: &str) -> String {
    let t = normalize(text);
    let t = t.as_str();
    // Search: «найди в гугле …», «загугли …», «найди на ютубе …», «открой на ютубе …»
    if strip_any(t, &["найди", "поищи", "загугли", "погугли"]).is_some() {
        return do_browser(ctx, t);
    }
    if let Some(rest) = strip_any(t, &["открой", "включи"]) {
        let rest = strip_word(rest, "мне").unwrap_or(rest);
        if let Some(r) = strip_any(rest, &["на", "в"]) {
            if strip_any(r, &["ютубе", "youtube", "ютюбе", "ютьюбе"]).is_some_and(|q| !q.is_empty()) {
                return do_browser(ctx, t);
            }
        }
    }
    // «допиши в блокнот …»
    if let Some(rest) = strip_any(t, &["допиши", "добавь"]).filter(|r| !r.is_empty()) {
        return do_append(ctx, rest);
    }
    // «напиши маме в телеграм …» (or just «напиши лизе …» when Лиза is a contact)
    if strip_any(t, &["напиши", "написать", "отправь", "скинь", "пошли", "передай"]).is_some() {
        if let Some(r) = do_telegram(ctx, t) {
            return r;
        }
    }
    // «подключи впн», «отключи впн»
    if has_word(t, &["впн", "vpn"]) {
        let vpn = ctx.cfg().vpn.clone();
        return if strip_any(t, &["отключи", "выключи", "разорви", "закрой"]).is_some() { do_close(ctx, &vpn) } else { do_open(ctx, &vpn) };
    }
    // «… и запиши X», «запиши X», «создай документ с текстом X»
    let w = words(t);
    let split = w.iter().enumerate().find_map(|(i, x)| match *x {
        "запиши" | "напиши" | "впиши" => Some((i, i + 1)),
        "с" if w.get(i + 1) == Some(&"текстом") => Some((i, i + 2)),
        _ => None,
    });
    let (head, body) = match split {
        Some((i, j)) => {
            let head_end = if i > 0 && w[i - 1] == "и" { i - 1 } else { i };
            (w[..head_end].join(" "), w[j..].join(" "))
        }
        None => (t.to_owned(), String::new()),
    };
    let head = head.as_str();
    if has_word(head, &["документ", "ворд", "word"]) && strip_any(head, &["создай", "сделай", "открой", "новый"]).is_some() {
        let rest = strip_any(head, &["создай", "сделай", "открой"]).unwrap_or(head).replace("с названием", " ").replace("под названием", " ");
        let title = words(&rest).into_iter().filter(|x| !["новый", "документ", "ворд", "word", "в"].contains(x)).collect::<Vec<_>>().join(" ");
        return do_document(ctx, &title, &body);
    }
    if let Some(rest) = strip_any(head, &["создай", "сделай"]) {
        let rest = strip_word(rest, "новую").unwrap_or(rest);
        if let Some(name) = strip_word(rest, "папку") {
            return do_folder(ctx, name);
        }
    }
    if split.is_some() && (head.is_empty() || has_word_prefix(head, &["блокнот", "заметк", "текстов", "файл"])) {
        return do_note(ctx, &body);
    }
    if let Some(rest) = strip_any(head, &["создай", "сделай"]) {
        let rest = strip_word(rest, "новую").unwrap_or(rest);
        if strip_word(rest, "заметку").is_some() || rest.starts_with("текстовый файл") {
            return do_note(ctx, "");
        }
    }
    // Browser: profiles, sites, «открой в браузере …»
    let obj = strip_any(head, &["открой", "запусти", "включи"]).unwrap_or(head);
    if has_word_prefix(head, &["браузер", "профил", "сайт", "вкладк"]) || site(ctx, obj).is_some() || is_domain(obj) {
        return do_browser(ctx, head);
    }
    // Programs and games
    for (verbs, open) in [(&["открой", "запусти", "включи", "открыть", "запустить"][..], true), (&["закрой", "выключи", "выруби", "закрыть"][..], false)] {
        if let Some(name) = strip_any(head, verbs).filter(|n| !n.is_empty()) {
            let name = strip_any(name, &["программу", "приложение", "игру"]).unwrap_or(name);
            return if open { do_open(ctx, name) } else { do_close(ctx, name) };
        }
    }
    format!("Не поняла, что сделать на компьютере: {t}")
}

fn site(ctx: &Ctx, name: &str) -> Option<String> {
    ctx.cfg().sites.get(name).cloned().or_else(|| SITES.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_owned()))
}

// ---- the browser

/// Chrome profiles from its «Local State»: (folder, shown name, every name it goes by).
pub fn chrome_profiles(local_state: &str) -> Vec<(String, String, Vec<String>)> {
    let Ok(v) = serde_json::from_str::<Value>(local_state) else { return Vec::new() };
    let Some(cache) = v["profile"]["info_cache"].as_object() else { return Vec::new() };
    cache
        .iter()
        .map(|(dir, p)| {
            let mut names: Vec<String> = Vec::new();
            for k in ["name", "shortcut_name", "gaia_given_name", "gaia_name", "user_name"] {
                if let Some(s) = p[k].as_str().filter(|s| !s.is_empty()) {
                    if !names.iter().any(|n| n == s) {
                        names.push(s.to_owned());
                    }
                }
            }
            if names.is_empty() {
                names.push(dir.clone());
            }
            // «Person 1» / «Пользователь 1» is Chrome's placeholder: a real name if there is one.
            let placeholder = |n: &str| {
                let n = n.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' ');
                n == "Person" || n == "Пользователь"
            };
            let shown = names.iter().find(|n| !placeholder(n)).unwrap_or(&names[0]).clone();
            (dir.clone(), shown, names)
        })
        .collect()
}

/// The profile a spoken name means.
pub fn find_profile(profiles: &[(String, String, Vec<String>)], spoken: &str) -> Option<(String, String)> {
    let want = spoken.trim().to_lowercase();
    if want.is_empty() {
        return None;
    }
    let lat = to_latin(&want);
    let mut best = None;
    let mut best_d = 3;
    for (dir, shown, names) in profiles {
        for name in names {
            let n = name.to_lowercase();
            if n == want || n == lat {
                return Some((dir.clone(), shown.clone()));
            }
            let d = if n.contains(&want) || want.contains(&n) || n.contains(&lat) { 1 } else { distance(&want, &n).min(distance(&lat, &n)) };
            if d < best_d {
                best = Some((dir.clone(), shown.clone()));
                best_d = d;
            }
        }
    }
    best
}

fn chrome_exe(sys: &dyn Sys) -> Option<String> {
    for hive in ["HKCU", "HKLM"] {
        if let Some(p) = sys.reg_str(hive, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\chrome.exe", "") {
            if sys.exists(&p) {
                return Some(p);
            }
        }
    }
    let local = sys.local_app_data();
    [
        std::env::var("ProgramFiles").ok().map(|p| format!("{p}\\Google\\Chrome\\Application\\chrome.exe")),
        std::env::var("ProgramFiles(x86)").ok().map(|p| format!("{p}\\Google\\Chrome\\Application\\chrome.exe")),
        Some(local.join("Google\\Chrome\\Application\\chrome.exe").to_string_lossy().into_owned()),
    ]
    .into_iter()
    .flatten()
    .find(|p| sys.exists(p))
}

/// What the browser phrase asks: (profile folder and name, address, what for the answer).
pub struct BrowserPlan {
    pub profile: Option<(String, String)>,
    pub url: Option<String>,
    pub what: String,
}

pub fn plan_browser(t: &str, profiles: &[(String, String, Vec<String>)], site_of: &dyn Fn(&str) -> Option<String>) -> Result<BrowserPlan, String> {
    let mut w: Vec<String> = words(t).into_iter().map(str::to_owned).collect();
    let mut profile = None;
    // «в профиле работа», «профиль работа»: the longest name that is a profile.
    if let Some(j) = w.iter().position(|x| x.starts_with("профил")).filter(|j| j + 1 < w.len()) {
        let start = if j > 0 && w[j - 1] == "в" { j - 1 } else { j };
        let after: Vec<String> = w[j + 1..].to_vec();
        for k in (1..=after.len()).rev() {
            if let Some(p) = find_profile(profiles, &after[..k].join(" ")) {
                profile = Some(p);
                let mut rest = w[..start].to_vec();
                rest.extend_from_slice(&after[k..]);
                w = rest;
                break;
            }
        }
        if profile.is_none() {
            let known = profiles.iter().map(|p| p.1.clone()).collect::<Vec<_>>().join(", ");
            return Err(format!("Не нашла в хроме профиль {}. Есть: {known}", after.join(" ")));
        }
    }
    let t = w.join(" ");
    let ws = words(&t);
    let verb = |i: usize, list: &[&str]| ws.get(i).is_some_and(|x| list.contains(x));
    // «… найди на ютубе X»
    let youtube = (0..ws.len()).find_map(|i| {
        if !verb(i, &["найди", "поищи", "открой", "включи", "поиск"]) {
            return None;
        }
        let mut k = i + 1;
        if ws.get(k) == Some(&"мне") {
            k += 1;
        }
        if !verb(k, &["на", "в"]) || !verb(k + 1, &["ютубе", "youtube", "ютюбе", "ютьюбе"]) {
            return None;
        }
        let q = ws[k + 2..].join(" ");
        (!q.is_empty()).then_some(q)
    });
    let search = (0..ws.len()).find_map(|i| {
        if !verb(i, &["найди", "поищи", "загугли", "погугли", "поиск"]) {
            return None;
        }
        let mut k = i + 1;
        if ws.get(k) == Some(&"в") && verb(k + 1, &["браузере", "гугле", "интернете", "хроме"]) {
            k += 2;
        }
        let q = ws[k.min(ws.len())..].join(" ");
        (!q.is_empty()).then_some(q)
    });
    let (url, what) = if let Some(q) = youtube {
        (Some(format!("https://www.youtube.com/results?search_query={}", encode(&q))), format!("ютуб: {q}"))
    } else if let Some(q) = search {
        (Some(format!("https://www.google.com/search?q={}", encode(&q))), format!("поиск: {q}"))
    } else {
        let padded = format!(" {t} ").replace(" в браузере ", " ").replace(" в хроме ", " ");
        let filler = ["открой", "запусти", "включи", "мне", "браузер", "хром", "сайт", "страницу", "вкладку", "новую", "новой", "в", "на", "и", "а", "там", "с"];
        let name = words(&padded).into_iter().filter(|x| !filler.contains(x)).collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            (None, "браузер".to_owned())
        } else if let Some(u) = site_of(&name) {
            (Some(u), name)
        } else if is_domain(&name) {
            (Some(format!("https://{name}")), name)
        } else {
            (Some(format!("https://www.google.com/search?q={}", encode(&name))), format!("поиск: {name}"))
        }
    };
    Ok(BrowserPlan { profile, url, what })
}

fn do_browser(ctx: &Ctx, t: &str) -> String {
    let sys = ctx.sys();
    let local_state = std::fs::read_to_string(sys.local_app_data().join("Google\\Chrome\\User Data\\Local State")).unwrap_or_default();
    let profiles = chrome_profiles(&local_state);
    let plan = match plan_browser(t, &profiles, &|n| site(ctx, n)) {
        Ok(p) => p,
        Err(e) => {
            ctx.agent.log(&e);
            return e;
        }
    };
    let mut args = Vec::new();
    if let Some((dir, _)) = &plan.profile {
        args.push(format!("--profile-directory={dir}"));
    }
    if let Some(u) = &plan.url {
        args.push(u.clone());
    }
    let launched = match chrome_exe(sys) {
        Some(chrome) => sys.run(&chrome, &args).is_ok(),
        None => match &plan.url {
            Some(u) => sys.open(u).is_ok(),
            None => return "Не нашла браузер Chrome".into(),
        },
    };
    if !launched {
        return "Не получилось открыть браузер".into();
    }
    ctx.agent.log(&format!("browser: {}", plan.what));
    let mut answer = if let Some(q) = plan.what.strip_prefix("поиск: ") {
        format!("Ищу в гугле {q}")
    } else if let Some(q) = plan.what.strip_prefix("ютуб: ") {
        format!("Ищу на ютубе {q}")
    } else if plan.what == "браузер" {
        "Открываю браузер".into()
    } else {
        format!("Открываю {}", plan.what)
    };
    if let Some((_, name)) = &plan.profile {
        answer.push_str(&format!(" в профиле {name}"));
    }
    answer
}

// ---- notes, documents, folders

fn notes_dir(sys: &dyn Sys) -> PathBuf {
    sys.folder("Documents").join("Заметки")
}

fn strip_target<'a>(body: &'a str, targets: &[&str]) -> &'a str {
    if let Some(rest) = strip_any(body, &["в", "во"]) {
        for t in targets {
            if let Some(r) = strip_word(rest, t).filter(|r| !r.is_empty()) {
                return r;
            }
        }
    }
    body
}

fn stamp(sys: &dyn Sys) -> String {
    let (y, mo, d, h, mi, _) = sys.local_time();
    format!("{y:04}-{mo:02}-{d:02} {h:02}-{mi:02}")
}

/// A note: its own .txt in Documents\Заметки, opened in the text editor.
fn do_note(ctx: &Ctx, body: &str) -> String {
    let sys = ctx.sys();
    let body = strip_target(body.trim(), &["заметки", "заметку", "блокнот", "блокноте", "файл", "текстовый файл"]);
    let dir = notes_dir(sys);
    let _ = std::fs::create_dir_all(&dir);
    let title = words(body).into_iter().take(6).collect::<Vec<_>>().join(" ");
    let title = if title.is_empty() { "заметка".to_owned() } else { title };
    let path = dir.join(format!("{} {}.txt", stamp(sys), clean_name(&title)));
    let mut text = "\u{feff}".to_owned();
    text.push_str(&capitalize(body));
    text.push_str("\r\n");
    if std::fs::write(&path, text).is_err() {
        return "Не получилось создать заметку".into();
    }
    let _ = sys.open(&path.to_string_lossy());
    ctx.agent.log(&format!("note: {}", path.display()));
    if body.is_empty() { "Создала пустую заметку".into() } else { format!("Записала: {body}") }
}

/// «допиши …»: a line more in the latest note (or a new one), opened.
fn do_append(ctx: &Ctx, body: &str) -> String {
    let sys = ctx.sys();
    let body = strip_target(body.trim(), &["блокнот", "блокноте", "заметку", "заметки", "последнюю заметку", "файл"]);
    if body.is_empty() {
        return "Скажите, что дописать".into();
    }
    let last = std::fs::read_dir(notes_dir(sys))
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("txt")))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p);
    let Some(last) = last else { return do_note(ctx, body) };
    let ok = std::fs::OpenOptions::new().append(true).open(&last).and_then(|mut f| write!(f, "{}\r\n", capitalize(body))).is_ok();
    if !ok {
        return "Не получилось дописать заметку".into();
    }
    let _ = sys.open(&last.to_string_lossy());
    ctx.agent.log(&format!("append: {}", last.display()));
    format!("Дописала: {body}")
}

/// A Word document (.docx) with the text, in Documents, opened by whatever opens .docx
/// (a .txt note when nothing does).
fn do_document(ctx: &Ctx, title: &str, body: &str) -> String {
    let sys = ctx.sys();
    let name = if title.trim().is_empty() { format!("Документ {}", stamp(sys)) } else { clean_name(&capitalize(title)) };
    let path = sys.folder("Documents").join(format!("{name}.docx"));
    if std::fs::write(&path, docx(&capitalize(body))).is_err() {
        return do_note(ctx, body);
    }
    if sys.open(&path.to_string_lossy()).is_err() {
        let _ = std::fs::remove_file(&path);
        return do_note(ctx, body);
    }
    ctx.agent.log(&format!("document: {}", path.display()));
    if title.trim().is_empty() { "Создала документ".into() } else { format!("Создала документ {} в Документах", title.trim()) }
}

fn do_folder(ctx: &Ctx, rest: &str) -> String {
    let sys = ctx.sys();
    let docs = rest.contains("в документах");
    let (base, where_) = if docs { (sys.folder("Documents"), "в документах") } else { (sys.folder("Desktop"), "на рабочем столе") };
    let mut name = format!(" {rest} ");
    for p in [" на рабочем столе ", " в документах ", " с названием ", " под названием ", " новую "] {
        name = name.replace(p, " ");
    }
    let name = name.trim();
    let name = if name.is_empty() { "Новая папка" } else { name };
    let path = base.join(clean_name(&capitalize(name)));
    if std::fs::create_dir_all(&path).is_err() {
        return "Не получилось создать папку".into();
    }
    let _ = sys.open(&path.to_string_lossy());
    ctx.agent.log(&format!("folder: {}", path.display()));
    format!("Создала папку {name} {where_}")
}

// ---- a minimal .docx (a zip of three XML files, stored)

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// A zip with the files stored (no compression): (name, data).
pub fn zip_store(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let len = data.len() as u32;
        let head = |sig: u32, central: bool| -> Vec<u8> {
            let mut h = Vec::new();
            h.extend_from_slice(&sig.to_le_bytes());
            if central {
                h.extend_from_slice(&20u16.to_le_bytes()); // made by
            }
            h.extend_from_slice(&20u16.to_le_bytes()); // needed
            h.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
            h.extend_from_slice(&0u16.to_le_bytes()); // stored
            h.extend_from_slice(&0u32.to_le_bytes()); // time, date
            h.extend_from_slice(&crc.to_le_bytes());
            h.extend_from_slice(&len.to_le_bytes());
            h.extend_from_slice(&len.to_le_bytes());
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // extra
            if central {
                h.extend_from_slice(&[0; 8]); // comment, disk, internal attributes
                h.extend_from_slice(&0u32.to_le_bytes()); // external attributes
                h.extend_from_slice(&offset.to_le_bytes());
            }
            h.extend_from_slice(name.as_bytes());
            h
        };
        out.extend(head(0x0403_4b50, false));
        out.extend_from_slice(data);
        central.extend(head(0x0201_4b50, true));
    }
    let (cd_offset, cd_len, n) = (out.len() as u32, central.len() as u32, files.len() as u16);
    out.extend(central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn docx(text: &str) -> Vec<u8> {
    let types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t xml:space="preserve">{}</w:t></w:r></w:p></w:body></w:document>"#,
        xml_escape(text)
    );
    zip_store(&[("[Content_Types].xml", types.as_bytes().to_vec()), ("_rels/.rels", rels.as_bytes().to_vec()), ("word/document.xml", doc.into_bytes())])
}

// ---- Telegram

/// Contacts: config.json's and the speaker's (header), each spoken form → target.
pub fn contact_map(config: &[(String, String)], header: Option<&str>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut add = |names: &str, target: &str| {
        for n in names.to_lowercase().split('|') {
            let n = n.trim();
            if !n.is_empty() {
                out.insert(n.to_owned(), target.to_owned());
            }
        }
    };
    for (k, v) in config {
        add(k, v);
    }
    if let Some(v) = header.and_then(base64_decode).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        if let Some(m) = v.as_object() {
            for (k, t) in m {
                if let Some(t) = t.as_str() {
                    add(k, t);
                }
            }
        }
    }
    out
}

/// tg:// link for a target: a phone number or a username.
pub fn telegram_link(target: &str) -> String {
    let compact: String = target.chars().filter(|c| !matches!(c, ' ' | '-' | '(' | ')')).collect();
    let digits = compact.strip_prefix('+').unwrap_or(&compact);
    if digits.len() >= 6 && digits.chars().all(|c| c.is_ascii_digit()) {
        format!("tg://resolve?phone={digits}")
    } else {
        format!("tg://resolve?domain={}", target.trim().trim_start_matches('@'))
    }
}

/// (who, text) in «напиши маме в телеграм привет»; Err: what to answer instead.
pub fn telegram_parse(t: &str, contacts: &BTreeMap<String, String>) -> Option<Result<(String, String), String>> {
    let explicit = ["телег", "телеграм", "сообщени", "контакт"].iter().any(|w| t.contains(w));
    if contacts.is_empty() {
        return explicit.then(|| Err("Сначала добавьте контакты телеграма: страница soundya, Настройки, Компьютер".into()));
    }
    let rest = strip_any(t, &["напиши", "написать", "отправь", "скинь", "пошли", "передай"]).unwrap_or(t);
    let ws = words(rest);
    let mut kept = Vec::new();
    let mut i = 0;
    while i < ws.len() {
        let w = ws[i];
        if w == "в" && ws.get(i + 1).is_some_and(|n| n.starts_with("телег")) {
            i += 2;
            continue;
        }
        if ["сообщение", "сообщения", "контакту", "пользователю", "мне"].contains(&w) || w.starts_with("телеграм") {
            i += 1;
            continue;
        }
        kept.push(w);
        i += 1;
    }
    let mut rest = format!(" {} ", kept.join(" "));
    let mut names: Vec<&String> = contacts.keys().collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
    let mut who = None;
    for n in names {
        if let Some(i) = rest.find(&format!(" {n} ")) {
            rest.replace_range(i..i + n.len() + 1, "");
            who = Some(n.clone());
            break;
        }
    }
    let Some(who) = who else {
        // «напиши в блокнот …» and the like: not a message at all.
        return explicit.then(|| Err("Не знаю такого контакта. Добавьте его на странице soundya, в настройках компьютера".into()));
    };
    let text = rest.trim();
    let text = strip_any(text, &["что", "текст", "с текстом"]).unwrap_or(text);
    Some(Ok((who, words(text).join(" "))))
}

fn do_telegram(ctx: &Ctx, t: &str) -> Option<String> {
    let contacts = contact_map(&ctx.cfg().contacts, ctx.contacts_header.as_deref());
    let (who, text) = match telegram_parse(t, &contacts)? {
        Ok(x) => x,
        Err(e) => return Some(e),
    };
    let sys = ctx.sys();
    if sys.open(&telegram_link(&contacts[&who])).is_err() {
        return Some("Не получилось открыть телеграм".into());
    }
    if text.is_empty() {
        return Some(format!("Открыла чат: {who}"));
    }
    sys.sleep_ms(2000);
    let text = capitalize(&text);
    if !sys.type_text(&text) {
        ctx.agent.log("telegram: typing failed");
        return Some(format!("Открыла чат: {who}, но не смогла напечатать текст"));
    }
    let send = ctx.cfg().telegram_send;
    if send {
        sys.sleep_ms(400);
        sys.keys(&[], VK_RETURN);
    }
    ctx.agent.log(&format!("telegram: {who} <- {text} (sent: {send})"));
    Some(if send { format!("Отправила {who}: {text}") } else { format!("Написала {who}: {text}. Нажмите Enter, чтобы отправить") })
}

// ------------------------------------------------------------------ scenario steps

const VK_RETURN: u16 = 0x0D;

/// "x,y" / "x,y,double" / "x,y,right": nothing else reaches the mouse.
pub fn parse_click(spec: &str) -> Option<(i32, i32, Button)> {
    let p: Vec<&str> = spec.trim().split(',').collect();
    let num = |s: &str| -> Option<i32> { (!s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten() };
    let (x, y) = (num(p.first()?)?, num(p.get(1)?)?);
    let b = match p.get(2).copied() {
        None => Button::Left,
        Some("double") => Button::Double,
        Some("right") => Button::Right,
        _ => return None,
    };
    (p.len() <= 3).then_some((x, y, b))
}

fn do_click(sys: &dyn Sys, spec: &str) -> String {
    match parse_click(spec) {
        Some((x, y, b)) if sys.click(x, y, b) => "Готово".into(),
        Some(_) => "Не получилось кликнуть".into(),
        None => "Не поняла, куда кликнуть".into(),
    }
}

/// "ctrl+shift+esc" → (modifier virtual keys, the key's); Err: what to answer.
pub fn parse_key(spec: &str) -> Result<(Vec<u16>, u16), &'static str> {
    let low = spec.trim().to_lowercase();
    let parts: Vec<&str> = low.split('+').collect();
    let (key, mods) = parts.split_last().ok_or("Не знаю такой клавиши")?;
    let mut vm = Vec::new();
    for m in mods {
        vm.push(match *m {
            "ctrl" | "cmd" => 0x11,
            "shift" => 0x10,
            "alt" | "option" => 0x12,
            "win" => 0x5B,
            _ => return Err("Не знаю такой клавиши"),
        });
    }
    let named = [
        ("enter", 0x0D), ("esc", 0x1B), ("tab", 0x09), ("space", 0x20), ("backspace", 0x08), ("delete", 0x2E),
        ("up", 0x26), ("down", 0x28), ("left", 0x25), ("right", 0x27), ("home", 0x24), ("end", 0x23),
        ("pageup", 0x21), ("pagedown", 0x22), ("insert", 0x2D),
    ];
    let vk = if let Some((_, v)) = named.iter().find(|(n, _)| n == key) {
        *v
    } else if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=12).contains(n)) {
        0x6F + n
    } else if key.chars().count() == 1 {
        let c = key.chars().next().unwrap_or(' ');
        match c {
            'a'..='z' => c.to_ascii_uppercase() as u16,
            '0'..='9' => c as u16,
            '-' => 0xBD,
            '=' => 0xBB,
            '[' => 0xDB,
            ']' => 0xDD,
            ';' => 0xBA,
            '\'' => 0xDE,
            ',' => 0xBC,
            '.' => 0xBE,
            '/' => 0xBF,
            '`' => 0xC0,
            _ => return Err("Не знаю такой клавиши"),
        }
    } else {
        return Err("Не знаю такой клавиши");
    };
    Ok((vm, vk))
}

fn do_key(sys: &dyn Sys, spec: &str) -> String {
    match parse_key(spec) {
        Ok((mods, vk)) if sys.keys(&mods, vk) => "Готово".into(),
        Ok(_) => "Не получилось нажать".into(),
        Err(e) => e.into(),
    }
}

/// Text to type: one line, at most 300 characters, no control characters (src/pc.rs::type_spec).
pub fn type_spec(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty() && t.chars().count() <= 300 && !t.chars().any(char::is_control)).then_some(t)
}

fn do_type(sys: &dyn Sys, text: &str) -> String {
    match type_spec(text) {
        Some(t) if sys.type_text(t) => "Готово".into(),
        Some(_) => "Не получилось напечатать".into(),
        None => "Не поняла, что напечатать".into(),
    }
}

/// Where the pointer is, "x,y" in the pixels /click takes.
fn do_mouse(sys: &dyn Sys) -> String {
    match sys.cursor() {
        Some((x, y)) if x >= 0 && y >= 0 => format!("{x},{y}"),
        // Left of or above the main screen: /click takes no negative numbers.
        Some((x, y)) => format!("{},{}", x.max(0), y.max(0)),
        None => "Не получилось узнать, где мышь".into(),
    }
}

// ------------------------------------------------------------------ status (for the tray)

/// %LOCALAPPDATA%\soundya-pc\status.json: who called last, when and what.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub pid: u32,
    pub started: u64,
    pub speaker: String,
    pub last_seen: u64,
    pub last_action: String,
    pub last_text: String,
    pub last_answer: String,
    pub last_ms: u64,
    pub requests: u64,
}

impl Status {
    pub fn load(dir: &Path) -> Status {
        let v: Value = std::fs::read_to_string(dir.join("status.json")).ok().and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok()).unwrap_or_default();
        Status {
            speaker: v["speaker"].as_str().unwrap_or("").to_owned(),
            last_seen: v["last_seen"].as_u64().unwrap_or(0),
            last_action: v["last_action"].as_str().unwrap_or("").to_owned(),
            last_text: v["last_text"].as_str().unwrap_or("").to_owned(),
            last_answer: v["last_answer"].as_str().unwrap_or("").to_owned(),
            last_ms: v["last_ms"].as_u64().unwrap_or(0),
            requests: v["requests"].as_u64().unwrap_or(0),
            pid: v["pid"].as_u64().unwrap_or(0) as u32,
            started: v["started"].as_u64().unwrap_or(0),
        }
    }
    fn save(&self, dir: &Path) {
        let v = json!({ "pid": self.pid, "started": self.started, "speaker": self.speaker, "last_seen": self.last_seen,
            "last_action": self.last_action, "last_text": self.last_text, "last_answer": self.last_answer,
            "last_ms": self.last_ms, "requests": self.requests });
        let tmp = dir.join("status.json.tmp");
        if std::fs::write(&tmp, v.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("status.json"));
        }
    }
}

// ------------------------------------------------------------------ HTTP

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub fn read_request(conn: &TcpStream) -> Option<Request> {
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

pub fn answer(mut conn: &TcpStream, code: u16, text: &str) {
    let reason = match code {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Bad Request",
    };
    // A refusal says that signatures are checked, so the speaker never retries with the key.
    let auth = if code == 403 { "X-Soundya-Auth: hmac\r\n" } else { "" };
    let _ = write!(conn, "HTTP/1.1 {code} {reason}\r\n{auth}Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
}

impl Agent {
    pub fn serve_one(&self, conn: TcpStream) {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
        let peer = conn.peer_addr().map(|a| a.ip()).ok();
        if !peer.is_some_and(home_network) {
            self.log(&format!("refused {peer:?}: not the home network"));
            return;
        }
        let Some(req) = read_request(&conn) else {
            return answer(&conn, 400, "Непонятный запрос");
        };
        let path = req.path.split('?').next().unwrap_or("/").to_owned();
        if req.method != "POST" || !self.auth.check(&req.headers, &path, &req.body, now()) {
            self.log(&format!("wrong key from {peer:?} for {path}"));
            return answer(&conn, 403, "Неверный ключ");
        }
        let action = path.trim_matches('/').to_ascii_lowercase();
        let body = String::from_utf8_lossy(&req.body).into_owned();
        let t0 = Instant::now();
        let ctx = Ctx { agent: self, contacts_header: req.headers.get("x-soundya-contacts").cloned() };
        let result = handle(&ctx, &action, &body);
        match &result {
            Some(text) => answer(&conn, 200, text),
            None => answer(&conn, 404, &format!("Неизвестная команда {action}")),
        }
        self.log(&format!("{action}: {} ({} ms)", result.as_deref().unwrap_or("unknown").lines().next().unwrap_or(""), t0.elapsed().as_millis()));
        let mut st = self.status.lock().unwrap();
        // The local tray's ping is not «the speaker».
        if let Some(ip) = peer.filter(|ip| !ip.is_loopback()) {
            st.speaker = ip.to_string();
        }
        st.last_seen = now();
        st.last_action = action;
        st.last_text = body.trim().chars().take(200).collect();
        st.last_answer = result.unwrap_or_default().chars().take(200).collect();
        st.last_ms = t0.elapsed().as_millis() as u64;
        st.requests += 1;
        st.save(&self.dir);
    }

    /// Accepts connections until the process ends.
    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        self.status.lock().unwrap().save(&self.dir);
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

    /// The speaker to tell about calls: config.json's, else the one that called last.
    fn speaker(&self) -> Option<String> {
        Some(self.config.speaker.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| Some(self.status.lock().unwrap().speaker.clone()).filter(|s| !s.is_empty()))
    }

    /// Smart Ducking: tells the speaker when an app starts or stops using the microphone
    /// (a call in Telegram, Zoom, Discord…), so it turns its music down. Windows keeps this in
    /// the registry (the microphone icon in the taskbar); nothing is listened to or recorded.
    pub fn duck_loop(self: Arc<Self>) {
        let mut was: BTreeSet<String> = BTreeSet::new();
        self.log("ducking: watching the microphone for calls");
        loop {
            let now_apps = self.sys.mic_apps();
            let (mut next, events) = duck_diff(&was, &now_apps);
            for (state, app) in events {
                if !self.send_duck(state, &app) {
                    // Not delivered: tried again next time.
                    if state == "start" {
                        next.remove(&app);
                    } else {
                        next.insert(app);
                    }
                }
            }
            was = next;
            // Event-driven (the registry tells when it changes); at most every 30 s anyway,
            // every 3 s while something failed to reach the speaker.
            self.sys.wait_mic_change(if was == now_apps { Duration::from_secs(30) } else { Duration::from_secs(3) });
            self.sys.sleep_ms(300);
        }
    }

    /// Says «here I am» to the speaker at start and every 5 minutes: the panel lists this PC
    /// under «Найдены компьютеры» with «Добавить» (the key never reaches a browser). The speaker
    /// takes the address from the connection itself.
    pub fn hello_loop(self: Arc<Self>) {
        loop {
            match self.speaker() {
                Some(to) => match self.say_hello(&to) {
                    Ok(()) => {}
                    Err(e) => self.log(&format!("hello: speaker {to}: {e}")),
                },
                None => self.log("hello: no speaker known yet"),
            }
            std::thread::sleep(Duration::from_secs(300));
        }
    }

    fn say_hello(&self, to: &str) -> Result<(), String> {
        use std::net::ToSocketAddrs;
        let addr = (to, self.config.speaker_port).to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("no address")?;
        let me = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect(addr).map(|()| s))
            .and_then(|s| s.local_addr())
            .map(|a| a.ip().to_string())
            .map_err(|e| e.to_string())?;
        let body = hello_body(&self.sys.hostname(), self.config.port, &self.sys.mac_for(&me), &self.config.key);
        match http_post(to, self.config.speaker_port, "/api/pc/hello", &body)? {
            (200, _) => Ok(()),
            (code, text) => Err(format!("{code} {}", text.lines().next().unwrap_or(""))),
        }
    }

    fn send_duck(&self, state: &str, app: &str) -> bool {
        let Some(to) = self.speaker() else { return false };
        let body = json!({ "source": "pc", "kind": "call", "id": format!("pc:{app}"), "state": state, "app": app }).to_string();
        let ok = http_post(&to, self.config.speaker_port, "/api/duck", &body).is_ok_and(|(code, _)| code == 200);
        self.log(&format!("duck: {state} {app}{}", if ok { "" } else { " (speaker did not answer)" }));
        ok
    }
}

/// POST /api/pc/hello's body (src/pc.rs::hello): no address (the speaker sees it), MAC as AA:BB:….
pub fn hello_body(name: &str, port: u16, mac: &str, key: &str) -> String {
    json!({ "name": name, "os": "windows", "port": port, "mac": mac.to_ascii_uppercase().replace('-', ":"), "key": key }).to_string()
}

/// (the new set, the events to send): «start» for new apps, «end» for gone ones.
pub fn duck_diff(was: &BTreeSet<String>, now: &BTreeSet<String>) -> (BTreeSet<String>, Vec<(&'static str, String)>) {
    let mut events: Vec<(&'static str, String)> = now.difference(was).map(|a| ("start", a.clone())).collect();
    events.extend(was.difference(now).map(|a| ("end", a.clone())));
    (now.clone(), events)
}

/// The app's name from a ConsentStore key: «C:#Program Files#Telegram#Telegram.exe» → «Telegram»,
/// «Microsoft.SkypeApp_kzf8qxf38zg5c» → «Microsoft.SkypeApp».
pub fn mic_app_name(key: &str) -> String {
    let last = key.rsplit('#').next().unwrap_or(key);
    let last = if last.len() > 4 && last[last.len() - 4..].eq_ignore_ascii_case(".exe") { &last[..last.len() - 4] } else { last };
    match last.rsplit_once('_') {
        Some((head, tail)) if tail.len() == 13 && tail.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) => head.to_owned(),
        _ => last.to_owned(),
    }
}

/// A short JSON POST to the speaker's panel.
pub fn http_post(host: &str, port: u16, path: &str, body: &str) -> Result<(u16, String), String> {
    use std::net::ToSocketAddrs;
    let addr = (host, port).to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("no address")?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    write!(s, "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        .map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    let code = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((code, out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned()))
}

/// The speaker's settings for this PC: the first computer when there is none, or it is this PC
/// (by address or by MAC: the router may have given it another address); else one of «Ещё
/// компьютеры» (a Mac there stays first), updated by address, not doubled.
pub fn pc_patch(current: &Value, me: &str, key: &str, mac: &str) -> Value {
    let first = current["pc_host"].as_str().unwrap_or("").trim();
    let first_mac = current["pc_mac"].as_str().unwrap_or("").trim();
    if first.is_empty() || first == me || (!mac.is_empty() && first_mac.eq_ignore_ascii_case(mac)) {
        return json!({ "pc_on": true, "pc_host": me, "pc_token": key, "pc_mac": mac });
    }
    let mut more: Vec<Value> = current["pcs_more"].as_array().cloned().unwrap_or_default();
    let mine = |p: &Value| p["host"].as_str() == Some(me) || (!mac.is_empty() && p["mac"].as_str().is_some_and(|m| m.eq_ignore_ascii_case(mac)));
    match more.iter_mut().find(|p| mine(p)) {
        Some(p) => {
            p["host"] = json!(me);
            p["token"] = json!(key);
            p["mac"] = json!(mac);
        }
        None => {
            let taken = |n: &str| more.iter().any(|p| p["name"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(n))) || current["pc_name"].as_str() == Some(n);
            let name = ["компьютер", "пк", "комп", "компьютер 2"].into_iter().find(|n| !taken(n)).unwrap_or("компьютер 3");
            more.push(json!({ "name": name, "host": me, "token": key, "mac": mac }));
        }
    }
    json!({ "pc_on": true, "pcs_more": more })
}

/// Is this PC (its address) among the speaker's computers?
pub fn pc_listed(settings: &Value, me: &str) -> bool {
    settings["pc_host"].as_str() == Some(me) || settings["pcs_more"].as_array().is_some_and(|m| m.iter().any(|p| p["host"].as_str() == Some(me)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_pc_joins_without_pushing_out_a_mac() {
        // Nothing yet, or this PC: the first computer.
        assert_eq!(pc_patch(&json!({}), "192.168.0.5", "k", "AA")["pc_host"], "192.168.0.5");
        // The router gave this PC another address: the same first computer (by its MAC).
        assert_eq!(pc_patch(&json!({ "pc_host": "192.168.0.9", "pc_mac": "aa" }), "192.168.0.5", "k", "AA")["pc_host"], "192.168.0.5");
        // A Mac is the first: it stays, this PC is «компьютер» among the others.
        let cur = json!({ "pc_host": "192.168.0.20", "pc_mac": "BB", "pcs_more": [{ "name": "ноут", "host": "192.168.0.7", "token": "••••x", "mac": "" }] });
        let p = pc_patch(&cur, "192.168.0.5", "k", "AA");
        assert!(p.get("pc_host").is_none());
        let more = p["pcs_more"].as_array().unwrap();
        assert_eq!((more.len(), more[1]["name"].as_str(), more[1]["host"].as_str()), (2, Some("компьютер"), Some("192.168.0.5")));
        assert_eq!(more[0]["token"], "••••x", "the others as they were");
        // Again: updated, not doubled.
        let again = pc_patch(&json!({ "pc_host": "192.168.0.20", "pcs_more": more }), "192.168.0.5", "k2", "AA");
        assert_eq!(again["pcs_more"].as_array().unwrap().len(), 2);
        assert!(pc_listed(&json!({ "pc_host": "192.168.0.20", "pcs_more": again["pcs_more"] }), "192.168.0.5"));
        assert!(!pc_listed(&json!({ "pc_host": "192.168.0.20" }), "192.168.0.5"));
    }

    #[derive(Default)]
    struct Fake {
        acts: Mutex<Vec<String>>,
        procs: Vec<Proc>,
        dirs: Vec<PathBuf>,
        home: PathBuf,
        existing: Vec<String>,
        cursor: Option<(i32, i32)>,
        start_apps: Vec<Item>,
        no_open: bool,
    }

    impl Fake {
        fn acts(&self) -> Vec<String> {
            self.acts.lock().unwrap().clone()
        }
        fn act(&self, s: String) {
            self.acts.lock().unwrap().push(s);
        }
    }

    impl Sys for Fake {
        fn shortcut_dirs(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
            (self.dirs.clone(), vec![])
        }
        fn reg_str(&self, _: &str, _: &str, _: &str) -> Option<String> {
            None
        }
        fn start_apps(&self) -> Vec<Item> {
            self.start_apps.clone()
        }
        fn processes(&self) -> Vec<Proc> {
            self.procs.clone()
        }
        fn focus(&self, pids: &[u32]) -> bool {
            self.act(format!("focus {pids:?}"));
            true
        }
        fn close(&self, pids: &[u32]) {
            self.act(format!("close {pids:?}"));
        }
        fn open(&self, target: &str) -> Result<(), String> {
            self.act(format!("open {target}"));
            if self.no_open { Err("no".into()) } else { Ok(()) }
        }
        fn run(&self, exe: &str, args: &[String]) -> Result<(), String> {
            self.act(format!("run {exe} {}", args.join(" ")));
            Ok(())
        }
        fn power(&self, what: Power) -> bool {
            self.act(format!("power {what:?}"));
            true
        }
        fn click(&self, x: i32, y: i32, b: Button) -> bool {
            self.act(format!("click {x},{y} {b:?}"));
            true
        }
        fn keys(&self, mods: &[u16], key: u16) -> bool {
            self.act(format!("keys {mods:?} {key:#x}"));
            true
        }
        fn type_text(&self, text: &str) -> bool {
            self.act(format!("type {text}"));
            true
        }
        fn cursor(&self) -> Option<(i32, i32)> {
            self.cursor
        }
        fn metrics(&self) -> Value {
            json!({ "cpu": 7 })
        }
        fn folder(&self, which: &str) -> PathBuf {
            self.home.join(which)
        }
        fn local_app_data(&self) -> PathBuf {
            self.home.join("Local")
        }
        fn hostname(&self) -> String {
            "DESKTOP-1".into()
        }
        fn local_time(&self) -> (i32, u32, u32, u32, u32, u32) {
            (2026, 11, 12, 9, 5, 0)
        }
        fn sleep_ms(&self, ms: u64) {
            self.act(format!("sleep {ms}"));
        }
        fn mic_apps(&self) -> BTreeSet<String> {
            BTreeSet::new()
        }
        fn wait_mic_change(&self, _: Duration) {}
        fn exists(&self, path: &str) -> bool {
            self.existing.iter().any(|e| e == path) || Path::new(path).exists()
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundya-winagent-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn agent(fake: Fake, cfg: Value, name: &str) -> Agent {
        let dir = temp(name);
        Agent::new(Config::from_json(&cfg), Box::new(fake), dir)
    }

    fn fake_of(a: &Agent) -> &Fake {
        // SAFETY-free downcast for tests: the agent was made from a Fake.
        unsafe { &*(a.sys.as_ref() as *const dyn Sys as *const Fake) }
    }

    fn run(a: &Agent, action: &str, body: &str) -> Option<String> {
        handle(&Ctx { agent: a, contacts_header: None }, action, body)
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
        let key = "k3y-0123456789abcdef";
        let h = signed("k3y", 1700000000, "abcd", "/open", "телеграм");
        assert_eq!(h["x-soundya-sign"], "6b3843f5bdb061ace24a63cbc6497128a7bcba596b9d458fa15ee1c488345c48");
        let auth = Auth::new(key, true);
        let h = signed(key, 1700000000, "abcd", "/open", "телеграм");
        assert!(auth.check(&h, "/open", "телеграм".as_bytes(), 1700000030));
        assert!(!auth.check(&h, "/open", "телеграм".as_bytes(), 1700000031), "replay");
        let h = signed(key, 1700000000, "abce", "/open", "телеграм");
        assert!(!auth.check(&h, "/open", "хром".as_bytes(), 1700000000), "another body");
        assert!(!auth.check(&h, "/close", "телеграм".as_bytes(), 1700000000), "another path");
        assert!(!Auth::new("other-key-0123456789", true).check(&h, "/open", "телеграм".as_bytes(), 1700000000));
        assert!(!auth.check(&h, "/open", "телеграм".as_bytes(), 1700000000 + MAX_AGE + 1), "too old");
        // An empty body (ping, lock, metrics) signs «…\n» with nothing after it.
        let h = signed(key, 1700000000, "n1", "/ping", "");
        assert!(auth.check(&h, "/ping", b"", 1700000000));
    }

    #[test]
    fn the_plain_key_only_from_old_speakers_and_only_if_allowed() {
        let key = "secret-key-123456";
        let h = BTreeMap::from([("x-soundya-key".to_owned(), key.to_owned())]);
        assert!(Auth::new(key, true).check(&h, "/lock", b"", now()), "as soundya-pc.ps1: an old speaker still works");
        assert!(!Auth::new(key, false).check(&h, "/lock", b"", now()), "\"allow_plain_key\": false");
        let bad = BTreeMap::from([("x-soundya-key".to_owned(), "secret-key-123457".to_owned())]);
        assert!(!Auth::new(key, true).check(&bad, "/lock", b"", now()));
        // A wrong signature is never rescued by a key next to it.
        let mut h2 = signed("wrong-key-1234567890", now(), "x", "/lock", "");
        h2.insert("x-soundya-key".into(), key.into());
        assert!(!Auth::new(key, true).check(&h2, "/lock", b"", now()));
        // No key in config.json: nothing is taken.
        assert!(!Auth::new("", true).check(&BTreeMap::from([("x-soundya-key".to_owned(), String::new())]), "/lock", b"", now()));
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
    fn config_as_soundya_pc_ps1_wrote_it() {
        // install.ps1 wrote it with a BOM-less UTF-8; an editor may add one.
        let d = temp("config");
        std::fs::write(
            d.join("config.json"),
            "\u{feff}{\"port\": 8766, \"key\": \"abcdefghijklmnopqr\", \"apps\": {\"Моя Игра\": \"C:\\\\Games\\\\g.exe\"}, \"contacts\": {\"мама|маме\": \"@mom\"}, \"duck\": false, \"telegram_send\": false}",
        )
        .unwrap();
        let c = Config::load(&d).unwrap();
        assert_eq!((c.port, c.key.as_str(), c.duck, c.telegram_send, c.speaker_port), (8766, "abcdefghijklmnopqr", false, false, 8765));
        assert_eq!(c.apps["моя игра"], "C:\\Games\\g.exe");
        assert_eq!(c.contacts, vec![("мама|маме".to_owned(), "@mom".to_owned())]);
        assert_eq!(c.vpn, "AmneziaVPN");
        assert!(c.allow_plain_key);
    }

    #[test]
    fn simple_actions() {
        let a = agent(Fake { cursor: Some((805, 677)), ..Fake::default() }, json!({}), "simple");
        assert_eq!(run(&a, "ping", "").unwrap(), "Компьютер DESKTOP-1 на связи");
        assert_eq!(run(&a, "metrics", "").unwrap(), "{\"cpu\":7}");
        assert_eq!(run(&a, "lock", "").unwrap(), "Заблокировала компьютер");
        assert_eq!(run(&a, "sleep", "").unwrap(), "Усыпляю компьютер");
        assert_eq!(run(&a, "shutdown", "").unwrap(), "Выключаю компьютер");
        assert_eq!(run(&a, "restart", "").unwrap(), "Перезагружаю компьютер");
        assert!(run(&a, "format", "c:").is_none(), "unknown → 404");
        assert_eq!(fake_of(&a).acts(), ["power Lock", "power Sleep", "power Shutdown", "power Restart"]);
    }

    #[test]
    fn type_and_mouse() {
        let a = agent(Fake { cursor: Some((805, 677)), ..Fake::default() }, json!({}), "type");
        assert_eq!(run(&a, "mouse", "").unwrap(), "805,677");
        assert_eq!(run(&a, "type", "  Привет, мир! {}+^%~()[]  ").unwrap(), "Готово");
        assert_eq!(fake_of(&a).acts(), ["type Привет, мир! {}+^%~()[]"], "as is: SendInput takes Unicode, nothing to escape");
        for bad in ["", "   ", "a\nb", "tab\there", &"я".repeat(301)] {
            assert_eq!(run(&a, "type", bad).unwrap(), "Не поняла, что напечатать", "{bad:?}");
        }
        assert_eq!(run(&a, "type", &"я".repeat(300)).unwrap(), "Готово");
        let none = agent(Fake::default(), json!({}), "type2");
        assert_eq!(run(&none, "mouse", "").unwrap(), "Не получилось узнать, где мышь");
        let left = agent(Fake { cursor: Some((-1200, 40)), ..Fake::default() }, json!({}), "type3");
        assert_eq!(run(&left, "mouse", "").unwrap(), "0,40");
        // What /mouse answers, /click takes.
        assert!(parse_click("805,677").is_some());
    }

    #[test]
    fn clicks_and_keys() {
        assert_eq!(parse_click("500,300"), Some((500, 300, Button::Left)));
        assert_eq!(parse_click("500,300,double"), Some((500, 300, Button::Double)));
        assert_eq!(parse_click("500,300,right"), Some((500, 300, Button::Right)));
        for bad in ["500", "500,300,triple", "-1,5", "1,2,3,4", "123456,1", "a,b", ""] {
            assert_eq!(parse_click(bad), None, "{bad}");
        }
        assert_eq!(parse_key("ctrl+shift+esc"), Ok((vec![0x11, 0x10], 0x1B)));
        assert_eq!(parse_key("cmd+t"), Ok((vec![0x11], b'T' as u16)));
        assert_eq!(parse_key("alt+f4"), Ok((vec![0x12], 0x73)));
        assert_eq!(parse_key("win+d"), Ok((vec![0x5B], b'D' as u16)));
        assert_eq!(parse_key("enter"), Ok((vec![], 0x0D)));
        assert_eq!(parse_key("ctrl+/"), Ok((vec![0x11], 0xBF)));
        assert!(parse_key("hyper+x").is_err() && parse_key("ctrl+f13").is_err() && parse_key("ctrl+ф").is_err());
        let a = agent(Fake::default(), json!({}), "keys");
        assert_eq!(run(&a, "click", "10,20,double").unwrap(), "Готово");
        assert_eq!(run(&a, "click", "x").unwrap(), "Не поняла, куда кликнуть");
        assert_eq!(run(&a, "key", "ctrl+c").unwrap(), "Готово");
        assert_eq!(run(&a, "key", "meta+c").unwrap(), "Не знаю такой клавиши");
        assert_eq!(fake_of(&a).acts(), ["click 10,20 Double", "keys [17] 0x43"]);
    }

    #[test]
    fn phrases_are_normalized_as_before() {
        assert_eq!(normalize("Открой, пожалуйста, GitHub.com!"), "открой github.com");
        assert_eq!(normalize("Найди ёлку. Быстро?"), "найди елку быстро");
    }

    #[test]
    fn browser_plans() {
        let profiles = chrome_profiles(r#"{"profile":{"info_cache":{
            "Default":{"name":"Person 1","gaia_given_name":"Родион"},
            "Profile 1":{"name":"Работа"}}}}"#);
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles.iter().find(|p| p.0 == "Default").unwrap().1, "Родион", "a real name over Chrome's placeholder");
        let none = |_: &str| None;
        let p = plan_browser("найди в гугле погоду в москве", &profiles, &none).unwrap();
        assert_eq!((p.url.as_deref(), p.what.as_str()), (Some("https://www.google.com/search?q=%D0%BF%D0%BE%D0%B3%D0%BE%D0%B4%D1%83%20%D0%B2%20%D0%BC%D0%BE%D1%81%D0%BA%D0%B2%D0%B5"), "поиск: погоду в москве"));
        let p = plan_browser("открой на ютубе лофи", &profiles, &none).unwrap();
        assert_eq!(p.what, "ютуб: лофи");
        assert!(p.url.unwrap().starts_with("https://www.youtube.com/results?search_query="));
        let p = plan_browser("открой ютуб в профиле работа", &profiles, &|n| (n == "ютуб").then(|| "https://www.youtube.com".into())).unwrap();
        assert_eq!((p.profile.map(|x| x.0), p.url.as_deref(), p.what.as_str()), (Some("Profile 1".into()), Some("https://www.youtube.com"), "ютуб"));
        let p = plan_browser("открой github.com", &profiles, &none).unwrap();
        assert_eq!(p.url.as_deref(), Some("https://github.com"));
        let p = plan_browser("открой браузер", &profiles, &none).unwrap();
        assert_eq!((p.url, p.what.as_str()), (None, "браузер"));
        let e = plan_browser("открой хром в профиле дача", &profiles, &none).err().unwrap();
        assert!(e.starts_with("Не нашла в хроме профиль дача. Есть:"), "{e}");
    }

    #[test]
    fn telegram_phrases() {
        let contacts = contact_map(&[("мама|маме|маму".into(), "@mom".into())], Some(&crate::pc::base64("{\"саша|саше\":\"+7 (999) 123-45-67\"}".as_bytes())));
        assert_eq!(contacts["маме"], "@mom");
        assert_eq!(telegram_link(&contacts["саше"]), "tg://resolve?phone=79991234567");
        assert_eq!(telegram_link("@mom"), "tg://resolve?domain=mom");
        assert_eq!(telegram_parse("напиши маме в телеграм привет как дела", &contacts), Some(Ok(("маме".into(), "привет как дела".into()))));
        assert_eq!(telegram_parse("отправь саше сообщение что я еду", &contacts), Some(Ok(("саше".into(), "я еду".into()))));
        assert_eq!(telegram_parse("напиши в блокнот купить хлеб", &contacts), None, "not a message: a note");
        assert!(matches!(telegram_parse("напиши пете в телеграм привет", &contacts), Some(Err(_))));
        // No contacts at all: an explicit «в телеграм» says how to add them (soundya-pc.ps1 made a note of it).
        assert!(matches!(telegram_parse("напиши маме в телеграм привет", &BTreeMap::new()), Some(Err(e)) if e.contains("контакты")));
        assert_eq!(telegram_parse("напиши купить хлеб", &BTreeMap::new()), None);
    }

    #[test]
    fn telegram_sends_through_the_fake() {
        let a = agent(Fake::default(), json!({"contacts": {"мама|маме": "@mom"}}), "tg");
        assert_eq!(run(&a, "do", "Напиши маме в телеграм привет").unwrap(), "Отправила маме: Привет");
        assert_eq!(fake_of(&a).acts(), ["open tg://resolve?domain=mom", "sleep 2000", "type Привет", "sleep 400", "keys [] 0xd"]);
    }

    #[test]
    fn notes_folders_and_documents() {
        let home = temp("home");
        let a = agent(Fake { home: home.clone(), ..Fake::default() }, json!({}), "notes");
        assert_eq!(run(&a, "do", "запиши купить молоко").unwrap(), "Записала: купить молоко");
        let notes: Vec<_> = std::fs::read_dir(home.join("Documents/Заметки")).unwrap().flatten().collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].file_name().to_string_lossy(), "2026-11-12 09-05 купить молоко.txt");
        assert_eq!(std::fs::read_to_string(notes[0].path()).unwrap(), "\u{feff}Купить молоко\r\n");
        assert_eq!(run(&a, "do", "допиши в блокнот и хлеб").unwrap(), "Дописала: и хлеб");
        assert_eq!(std::fs::read_to_string(notes[0].path()).unwrap(), "\u{feff}Купить молоко\r\nИ хлеб\r\n");
        assert_eq!(run(&a, "do", "создай папку проекты").unwrap(), "Создала папку проекты на рабочем столе");
        assert!(home.join("Desktop/Проекты").is_dir());
        assert_eq!(run(&a, "do", "создай документ отчёт и напиши всё готово").unwrap(), "Создала документ отчет в Документах");
        let doc = std::fs::read(home.join("Documents/Отчет.docx")).unwrap();
        assert!(doc.starts_with(b"PK\x03\x04") && String::from_utf8_lossy(&doc).contains("Все готово"));
    }

    #[test]
    fn a_docx_is_a_valid_zip() {
        let z = docx("a < b & «ё»");
        assert!(z.starts_with(b"PK\x03\x04"));
        assert!(String::from_utf8_lossy(&z).contains("a &lt; b &amp; «ё»"));
        assert_eq!(&z[z.len() - 22..z.len() - 18], &0x0605_4b50u32.to_le_bytes());
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926, "the standard check value");
    }

    #[test]
    fn programs_are_found_and_switched_to() {
        let menu = temp("menu");
        let sub = menu.join("Telegram Desktop");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("Telegram.lnk"), b"not a real link").unwrap();
        std::fs::write(menu.join("Discord.lnk"), b"x").unwrap();
        std::fs::write(menu.join("Uninstall Discord.lnk"), b"x").unwrap();
        let fake = Fake {
            dirs: vec![menu.clone()],
            procs: vec![Proc { pid: 7, name: "Telegram".into(), path: Some("C:\\T\\Telegram.exe".into()), has_window: true }],
            start_apps: vec![Item { name: "Калькулятор".into(), path: "shell:AppsFolder\\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into() }],
            ..Fake::default()
        };
        let a = agent(fake, json!({ "apps": { "игра": "C:\\Games\\game.exe" } }), "apps");
        assert_eq!(run(&a, "open", "телеграм").unwrap(), "Переключаю на Telegram", "running: its window to the front");
        assert_eq!(run(&a, "open", "дискорд").unwrap(), "Открываю Discord");
        assert_eq!(run(&a, "open", "диськорд").unwrap(), "Открываю Discord", "close spelling");
        assert_eq!(run(&a, "do", "открой калькулятор").unwrap(), "Открываю calc", "the alias is the built-in program");
        assert_eq!(run(&a, "open", "notepad").unwrap(), "Открываю notepad");
        assert_eq!(run(&a, "open", "моя программа").unwrap(), "Не нашла на компьютере программу моя программа");
        assert_eq!(run(&a, "process", "телеграм").unwrap(), "Да, телеграм запущен");
        assert_eq!(run(&a, "process", "дискорд").unwrap(), "Нет, дискорд сейчас не запущен");
        assert_eq!(run(&a, "close", "телеграм").unwrap(), "Закрыла Telegram");
        assert_eq!(run(&a, "close", "дискорд").unwrap(), "дискорд сейчас не запущен");
        let acts = fake_of(&a).acts();
        assert_eq!(acts[0], "focus [7]");
        assert!(acts[1].starts_with("open ") && acts[1].ends_with("Discord.lnk"), "{acts:?}");
        assert!(acts.contains(&"close [7]".to_owned()));
        assert!(acts.contains(&"open notepad".to_owned()));
    }

    #[test]
    fn shortcuts_are_read() {
        // A minimal .lnk: header, LinkInfo with a local base path.
        let mut b = vec![0u8; 0x4C];
        b[0] = 0x4C;
        b[0x14] = 0x02; // HasLinkInfo
        let base = b"C:\\Program Files\\App\\app.exe\0";
        let header = 0x1C;
        let li_size = header + base.len() + 1;
        let mut li = Vec::new();
        li.extend_from_slice(&(li_size as u32).to_le_bytes());
        li.extend_from_slice(&(header as u32).to_le_bytes());
        li.extend_from_slice(&1u32.to_le_bytes()); // VolumeIDAndLocalBasePath
        li.extend_from_slice(&0u32.to_le_bytes()); // volume id offset
        li.extend_from_slice(&(header as u32).to_le_bytes()); // local base path
        li.extend_from_slice(&0u32.to_le_bytes());
        li.extend_from_slice(&((header + base.len()) as u32).to_le_bytes()); // suffix: ""
        li.extend_from_slice(base);
        li.push(0);
        b.extend(li);
        assert_eq!(lnk_target(&b).as_deref(), Some("C:\\Program Files\\App\\app.exe"));
        assert_eq!(lnk_target(b"garbage"), None);
        assert_eq!(win_stem("C:\\x y\\Telegram.exe").as_deref(), Some("Telegram"));
        std::env::set_var("SOUNDYA_TEST_PF", "C:\\PF");
        assert_eq!(expand_env("%SOUNDYA_TEST_PF%\\a\\%NOPE_X%"), "C:\\PF\\a\\%NOPE_X%");
    }

    #[test]
    fn steam_files() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}";
        assert_eq!(vdf_paths(vdf), vec!["D:\\SteamLibrary"]);
        assert_eq!(acf_game("\"AppState\"\n{\n\t\"appid\"\t\t\"1422450\"\n\t\"name\"\t\t\"Deadlock\"\n}"), Some(Item { name: "Deadlock".into(), path: "steam://rungameid/1422450".into() }));
        assert_eq!(acf_game("\"appid\" \"228980\"\n\"name\" \"Steamworks Common Redistributables\""), None);
    }

    #[test]
    fn microphone_apps_and_ducking() {
        assert_eq!(mic_app_name("C:#Users#me#AppData#Roaming#Telegram Desktop#Telegram.exe"), "Telegram");
        assert_eq!(mic_app_name("Microsoft.SkypeApp_kzf8qxf38zg5c"), "Microsoft.SkypeApp");
        assert_eq!(mic_app_name("Discord.EXE"), "Discord");
        let was: BTreeSet<String> = ["Zoom".to_owned()].into();
        let now_: BTreeSet<String> = ["Telegram".to_owned()].into();
        let (next, ev) = duck_diff(&was, &now_);
        assert_eq!(next, now_);
        assert_eq!(ev, vec![("start", "Telegram".to_owned()), ("end", "Zoom".to_owned())]);
    }

    #[test]
    fn the_whole_request_over_tcp() {
        let key = "0123456789abcdef-key";
        let a = Arc::new(agent(Fake { cursor: Some((1, 2)), ..Fake::default() }, json!({ "key": key }), "tcp"));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = a.clone();
        std::thread::spawn(move || srv.serve(l));
        let ask = |path: &str, body: &str, headers: Vec<(String, String)>| -> String {
            let mut s = TcpStream::connect(addr).unwrap();
            let mut req = format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n", body.len());
            for (k, v) in headers {
                req.push_str(&format!("{k}: {v}\r\n"));
            }
            req.push_str("\r\n");
            req.push_str(body);
            s.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        let ok = ask("/mouse", "", sign(key, "/mouse", b"", b"n-1"));
        assert!(ok.starts_with("HTTP/1.1 200 ") && ok.ends_with("\r\n\r\n1,2"), "{ok}");
        let typed = ask("/type", "Привет", sign(key, "/type", "Привет".as_bytes(), b"n-2"));
        assert!(typed.ends_with("Готово"), "{typed}");
        let bad = ask("/lock", "", sign("another-key-0123456", "/lock", b"", b"n-3"));
        assert!(bad.starts_with("HTTP/1.1 403 ") && bad.contains("X-Soundya-Auth: hmac"), "{bad}");
        let unknown = ask("/nope", "", sign(key, "/nope", b"", b"n-4"));
        assert!(unknown.starts_with("HTTP/1.1 404 "), "{unknown}");
        let st = Status::load(&a.dir);
        assert_eq!((st.last_action.as_str(), st.requests), ("nope", 3));
        assert!(std::fs::read_to_string(a.dir.join("log.txt")).unwrap().contains("wrong key"));
        assert!(!std::fs::read_to_string(a.dir.join("log.txt")).unwrap().contains(key), "the key never goes to the log");
    }

    #[test]
    fn hello_reaches_the_speaker() {
        let v: Value = serde_json::from_str(&hello_body("DESKTOP-1", 8766, "aa-bb-cc-dd-ee-ff", "k")).unwrap();
        assert_eq!(v, json!({ "name": "DESKTOP-1", "os": "windows", "port": 8766, "mac": "AA:BB:CC:DD:EE:FF", "key": "k" }));
        assert!(v.get("ip").is_none(), "the speaker takes the address from the connection");
        // A fake speaker: the request as it arrives.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (c, _) = l.accept().unwrap();
            let r = read_request(&c).unwrap();
            answer(&c, 200, "OK");
            r
        });
        let a = agent(Fake::default(), json!({ "key": "0123456789abcdef-key", "speaker": "127.0.0.1", "speaker_port": port }), "hello");
        a.say_hello("127.0.0.1").unwrap();
        let r = srv.join().unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/api/pc/hello"));
        assert!(r.headers["content-type"].starts_with("application/json"));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!((v["os"].as_str(), v["port"].as_u64(), v["key"].as_str()), (Some("windows"), Some(8766), Some("0123456789abcdef-key")));
    }

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "мама|маме", "{\"x\":1}"] {
            assert_eq!(base64_decode(&crate::pc::base64(s.as_bytes())).unwrap(), s.as_bytes());
        }
    }
}
