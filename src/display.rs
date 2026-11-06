//! The clock display of the stations that have one (Mini 3, Plum, Pickle:
//! «<board>:white:clock<A-G|H><0-3>» in sysfs, plus DH/DL — the colon — and
//! DEGREE/DM), used only during the first install: a spinner while soundya
//! starts, the panel's address while it waits for you, the wizard's step,
//! and «HI» at the end. Afterwards the firmware's own clock is put back (it
//! also redraws it once a minute by itself).
//!
//! Segment names and glyphs as in Luna's station crates (MIT,
//! github.com/nyakokitsu/luna, stations/yandexmini3, yandexplum, pickle).
//! Detected by the sysfs nodes, not by the model name: a station without them
//! simply has no display here. Not checked on a real speaker.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

const ROOT: &str = "/sys/class/leds";
const DIGITS: usize = 4;

pub struct Clock {
    /// Segment name → brightness file.
    paths: HashMap<String, PathBuf>,
    /// The «H» segment (a dot after the digit) exists on this board.
    dots: bool,
}

/// Segments of one character on a seven-segment digit.
pub fn glyph(c: char) -> Option<&'static str> {
    Some(match c.to_ascii_uppercase() {
        '0' | 'O' | 'D' => "ABCDEF",
        '1' | 'I' => "BC",
        '2' => "ABDEG",
        '3' => "ABCDG",
        '4' => "BCFG",
        '5' | 'S' => "ACDFG",
        '6' => "ACDEFG",
        '7' => "ABC",
        '8' => "ABCDEFG",
        '9' => "ABCDFG",
        'A' => "ABCEFG",
        'B' => "CDEFG",
        'C' => "ADEF",
        'E' => "ADEFG",
        'H' => "BCEFG",
        'L' => "DEF",
        'N' => "CEG",
        'P' => "ABEFG",
        'T' => "DEFG",
        'U' => "BCDEF",
        'Y' => "BCDFG",
        'G' => "ACDEF",
        'R' => "EG",
        '-' => "G",
        '_' => "D",
        ' ' => "",
        _ => return None,
    })
}

/// Up to four characters (dots attach to the character before) → lit segments.
pub fn segments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for c in text.chars() {
        if c == '.' {
            if pos > 0 {
                out.push(format!("H{}", pos - 1));
            }
            continue;
        }
        if pos >= DIGITS {
            break;
        }
        if let Some(g) = glyph(c) {
            out.extend(g.chars().map(|p| format!("{p}{pos}")));
        }
        pos += 1;
    }
    out
}

/// «192.168.0.12» as display pages: «192.», «168.», «0.», «12».
pub fn ip_pages(ip: &str) -> Vec<String> {
    let parts: Vec<&str> = ip.split('.').collect();
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| if i + 1 < parts.len() { format!("{p:>3}.") } else { format!("{p:>3}") })
        .collect()
}

/// The spinner: one outer segment lit, going round all four digits.
pub fn spinner(tick: usize) -> Vec<String> {
    // Top edge left→right, right side down, bottom right→left, left side up.
    let mut path: Vec<String> = (0..DIGITS).map(|d| format!("A{d}")).collect();
    path.push(format!("B{}", DIGITS - 1));
    path.push(format!("C{}", DIGITS - 1));
    path.extend((0..DIGITS).rev().map(|d| format!("D{d}")));
    path.push("E0".into());
    path.push("F0".into());
    let i = tick % path.len();
    vec![path[i].clone(), path[(i + path.len() - 1) % path.len()].clone()]
}

impl Clock {
    pub fn detect() -> Option<Clock> {
        // SOUNDYA_LEDS_ROOT: only for testing on a computer with a fake sysfs.
        let root = std::env::var("SOUNDYA_LEDS_ROOT").unwrap_or_else(|_| ROOT.into());
        Self::detect_in(Path::new(&root))
    }

    fn detect_in(root: &Path) -> Option<Clock> {
        // «plum:white:clockA0» → prefix «plum:white:clock».
        let prefix = std::fs::read_dir(root).ok()?.flatten().find_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix("A0").filter(|p| p.ends_with(":clock")).map(str::to_owned)
        })?;
        let mut paths = HashMap::new();
        for d in 0..DIGITS {
            for p in ["A", "B", "C", "D", "E", "F", "G", "H"] {
                let path = root.join(format!("{prefix}{p}{d}")).join("brightness");
                if crate::fsx::exists(&path) {
                    paths.insert(format!("{p}{d}"), path);
                }
            }
        }
        for extra in ["DEGREE", "DH", "DL", "DM"] {
            let path = root.join(format!("{prefix}{extra}")).join("brightness");
            if crate::fsx::exists(&path) {
                paths.insert(extra.to_owned(), path);
            }
        }
        // All seven segments of all four digits, or it is not a display we know.
        let complete = (0..DIGITS).all(|d| "ABCDEFG".chars().all(|p| paths.contains_key(&format!("{p}{d}"))));
        let dots = paths.contains_key("H0");
        complete.then_some(Clock { paths, dots })
    }

    fn read(&self) -> HashMap<String, String> {
        self.paths
            .iter()
            .map(|(n, p)| (n.clone(), crate::fsx::read_string(p).unwrap_or_default().trim().to_owned()))
            .collect()
    }

    fn show(&self, lit: &[String], colon: bool) {
        let level = (255 * u32::from(BRIGHT.load(std::sync::atomic::Ordering::Acquire)) / 100).max(10).to_string();
        for (name, path) in &self.paths {
            let on = lit.iter().any(|l| l == name) || (colon && (name == "DH" || name == "DL"));
            let _ = std::fs::write(path, if on { level.as_str() } else { "0" });
        }
    }

    fn restore(&self, saved: &HashMap<String, String>) {
        for (name, path) in &self.paths {
            let v = saved.get(name).filter(|v| !v.is_empty()).map_or("0", String::as_str);
            let _ = std::fs::write(path, v);
        }
    }
}

/// What the display shows at `ms` into an install phase.
pub fn frame(phase: &crate::led::Install, ms: u64, ip: Option<&str>, dots: bool) -> (Vec<String>, bool) {
    use crate::led::Install;
    match phase {
        Install::Loading => (spinner((ms / 90) as usize), false),
        Install::Waiting => match ip {
            Some(ip) => {
                let pages = ip_pages(ip);
                let mut text = pages[(ms / 1400) as usize % pages.len()].clone();
                if !dots {
                    text = text.replace('.', "");
                }
                (segments(&text), false)
            }
            None => (spinner((ms / 160) as usize), false),
        },
        Install::Step(n, _) => (segments(&format!("St {}", n.min(&9))), false),
        Install::Done => (segments(if (ms / 300) % 2 == 0 { " HI " } else { "    " }), false),
    }
}

// ---------------------------------------------------------------- widgets
//
// Smart Display: after the install, the display goes round the chosen widgets
// — «clock» (the firmware's own clock: soundya leaves the display alone),
// «pc» (the PC's temperature, pc.rs metrics) and «weather» (Open-Meteo, no key)
// — each for `secs`. A Telegram message or a reminder (notify) takes over for
// a few seconds. Data that is not there (PC off, no city) is skipped.

#[derive(Clone, Debug, PartialEq)]
pub struct Widgets {
    pub on: bool,
    pub list: Vec<String>,
    pub secs: u64,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

static WIDGETS: Mutex<Option<Widgets>> = Mutex::new(None);
static BRIGHT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(100);
/// Latest data: (value °C, when).
static WEATHER: Mutex<Option<(i64, Instant)>> = Mutex::new(None);
static PC_TEMP: Mutex<Option<(i64, Instant)>> = Mutex::new(None);
/// An interrupt: (text, until).
static NOTE: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// What is on the display now, for the page («" 23C°"», «clock»).
static SHOWN: Mutex<String> = Mutex::new(String::new());
static PC: Mutex<Option<crate::settings::Settings>> = Mutex::new(None);

pub fn configure(w: Widgets, bright: u8, settings: &crate::settings::Settings) {
    *WIDGETS.lock().unwrap() = Some(w);
    BRIGHT.store(bright.clamp(10, 100), std::sync::atomic::Ordering::Release);
    *PC.lock().unwrap() = settings.pc_configured().then(|| settings.clone());
}

/// «tg» — a Telegram message, «rem» — a reminder: shown for 6 seconds.
pub fn notify(kind: &str) {
    let text = match kind {
        "tg" => "tEL ",
        "rem" => "rEn ",
        _ => "----",
    };
    *NOTE.lock().unwrap() = Some((text.to_owned(), Instant::now() + Duration::from_secs(6)));
}

/// «23» → " 23C" with the degree sign; «P» prefix for the PC.
pub fn temp_text(t: i64, pc: bool) -> String {
    let t = t.clamp(-99, 199);
    if pc { format!("P{:>2}C", t.min(99)) } else { format!("{t:>3}C") }
}

/// The widget for `ms` since the cycle began: None = the firmware's clock.
fn widget_now(w: &Widgets, ms: u64) -> Option<String> {
    let fresh = |slot: &Mutex<Option<(i64, Instant)>>, max: u64| {
        slot.lock().unwrap().filter(|(_, at)| at.elapsed() < Duration::from_secs(max)).map(|(v, _)| v)
    };
    let ready: Vec<Option<String>> = w
        .list
        .iter()
        .filter_map(|k| match k.as_str() {
            "clock" => Some(None),
            "pc" => fresh(&PC_TEMP, 300).map(|t| Some(temp_text(t, true))),
            "weather" => fresh(&WEATHER, 3600).map(|t| Some(temp_text(t, false))),
            _ => None,
        })
        .collect();
    if ready.is_empty() {
        return None;
    }
    let slot = (ms / (w.secs.max(3) * 1000)) as usize % ready.len();
    ready[slot].clone()
}

pub fn json() -> serde_json::Value {
    serde_json::json!({
        "shown": *SHOWN.lock().unwrap(),
        "weather": WEATHER.lock().unwrap().map(|(t, _)| t),
        "pc_temp": PC_TEMP.lock().unwrap().map(|(t, _)| t),
    })
}

/// Weather from Open-Meteo (free, no key), through the proxy from the settings.
fn fetch_weather(shared: &crate::Shared, lat: f64, lon: f64) -> Result<i64, String> {
    let proxy = shared.proxy.lock().unwrap().clone();
    let agent = crate::sc::build_agent(&proxy).map_err(|e| e.to_string())?;
    let v: serde_json::Value = agent
        .get("https://api.open-meteo.com/v1/forecast")
        .query("latitude", &format!("{lat:.3}"))
        .query("longitude", &format!("{lon:.3}"))
        .query("current", "temperature_2m")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;
    parse_weather(&v).ok_or_else(|| "в ответе нет температуры".into())
}

pub fn parse_weather(v: &serde_json::Value) -> Option<i64> {
    v["current"]["temperature_2m"].as_f64().map(|t| t.round() as i64)
}

/// A city by name → (name, lat, lon), Open-Meteo's geocoding.
pub fn find_city(shared: &crate::Shared, name: &str) -> Result<(String, f64, f64), String> {
    let proxy = shared.proxy.lock().unwrap().clone();
    let agent = crate::sc::build_agent(&proxy).map_err(|e| e.to_string())?;
    let v: serde_json::Value = agent
        .get("https://geocoding-api.open-meteo.com/v1/search")
        .query("name", name)
        .query("count", "1")
        .query("language", "ru")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| format!("сервис погоды не ответил: {e}"))?
        .into_json()
        .map_err(|e| e.to_string())?;
    let r = &v["results"][0];
    match (r["latitude"].as_f64(), r["longitude"].as_f64()) {
        (Some(la), Some(lo)) => Ok((r["name"].as_str().unwrap_or(name).to_owned(), la, lo)),
        _ => Err(format!("город «{name}» не нашёлся")),
    }
}

/// Weather every 15 minutes, the PC's temperature every minute — only for
/// widgets that are switched on.
pub fn run_providers(shared: std::sync::Arc<crate::Shared>) {
    thread::spawn(move || {
        let mut last_weather: Option<Instant> = None;
        loop {
            let w = WIDGETS.lock().unwrap().clone();
            if let Some(w) = w.filter(|w| w.on) {
                if let (true, Some(lat), Some(lon)) = (w.list.iter().any(|k| k == "weather"), w.lat, w.lon) {
                    if last_weather.is_none_or(|t| t.elapsed() > Duration::from_secs(900)) {
                        last_weather = Some(Instant::now());
                        match fetch_weather(&shared, lat, lon) {
                            Ok(t) => *WEATHER.lock().unwrap() = Some((t, Instant::now())),
                            Err(e) => eprintln!("display: weather: {e}"),
                        }
                    }
                }
                let pc = PC.lock().unwrap().clone();
                if let (true, Some(settings)) = (w.list.iter().any(|k| k == "pc"), pc) {
                    if let Ok(v) = crate::pc::metrics_raw(&settings) {
                        if let Some(t) = v["cpu_temp"].as_i64().or_else(|| v["gpu_temp"].as_i64()) {
                            *PC_TEMP.lock().unwrap() = Some((t, Instant::now()));
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(60));
        }
    });
}

/// Draws the install phases, interrupts and widgets, and puts the firmware's
/// clock back when it is the clock's turn.
pub fn run() {
    let Some(clock) = Clock::detect() else { return };
    eprintln!("display: clock found ({} segments)", clock.paths.len());
    thread::spawn(move || {
        let mut saved: Option<HashMap<String, String>> = None;
        let mut started = Instant::now();
        let cycle = Instant::now();
        let mut last = None;
        loop {
            let phase = crate::led::install_phase();
            if phase != last {
                started = Instant::now();
                last = phase.clone();
            }
            let note = NOTE.lock().unwrap().clone().filter(|(_, until)| Instant::now() < *until);
            let widgets = WIDGETS.lock().unwrap().clone().filter(|w| w.on);
            // What to draw: install > interrupt > widget; None = the firmware's clock.
            let draw: Option<(Vec<String>, bool, String)> = if let Some(p) = &phase {
                let ip = crate::mdns::own_ip().map(|i| i.to_string());
                let (lit, colon) = frame(p, started.elapsed().as_millis() as u64, ip.as_deref(), clock.dots);
                Some((lit, colon, format!("{p:?}")))
            } else if let Some((text, until)) = note {
                let blink = (until.saturating_duration_since(Instant::now()).as_millis() / 400) % 2 == 0;
                Some((if blink { segments(&text) } else { Vec::new() }, false, text))
            } else if let Some(text) = widgets.as_ref().and_then(|w| widget_now(w, cycle.elapsed().as_millis() as u64)) {
                let mut lit = segments(&text);
                if text.ends_with('C') {
                    lit.push("DEGREE".into());
                }
                Some((lit, false, text))
            } else {
                None
            };
            match draw {
                Some((lit, colon, text)) => {
                    if saved.is_none() {
                        saved = Some(clock.read());
                    }
                    clock.show(&lit, colon);
                    *SHOWN.lock().unwrap() = text;
                }
                None => {
                    if let Some(s) = saved.take() {
                        clock.restore(&s);
                    }
                    *SHOWN.lock().unwrap() = "clock".into();
                }
            }
            thread::sleep(Duration::from_millis(90));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_ip_on_four_digits() {
        assert_eq!(segments("12"), vec!["B0", "C0", "A1", "B1", "D1", "E1", "G1"]);
        assert!(segments("192.").contains(&"H2".to_owned()), "the dot after the third digit");
        assert_eq!(ip_pages("192.168.0.12"), vec!["192.", "168.", "  0.", " 12"]);
        assert_eq!(segments(" HI ").len(), 5 + 2);
        assert_eq!(spinner(0).len(), 2);
        assert_ne!(spinner(0), spinner(1));
    }

    #[test]
    fn widgets_take_turns() {
        assert_eq!(temp_text(23, false), " 23C");
        assert_eq!(temp_text(-3, false), " -3C");
        assert_eq!(temp_text(54, true), "P54C");
        assert_eq!(parse_weather(&serde_json::json!({"current": {"temperature_2m": -2.6}})), Some(-3));
        *WEATHER.lock().unwrap() = Some((7, Instant::now()));
        *PC_TEMP.lock().unwrap() = None;
        let w = Widgets { on: true, list: vec!["clock".into(), "pc".into(), "weather".into()], secs: 10, lat: None, lon: None };
        assert_eq!(widget_now(&w, 0), None, "the clock first");
        assert_eq!(widget_now(&w, 10_500), Some("  7C".into()), "no PC data: weather next");
        assert_eq!(widget_now(&w, 20_500), None, "and round again");
        *WEATHER.lock().unwrap() = None;
    }

    #[test]
    fn finds_a_clock_by_its_nodes() {
        let d = std::env::temp_dir().join(format!("soundya-clock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(Clock::detect_in(&d).is_none());
        for n in 0..4 {
            for p in ["A", "B", "C", "D", "E", "F", "G"] {
                let dir = d.join(format!("plum:white:clock{p}{n}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("brightness"), "0").unwrap();
            }
        }
        std::fs::create_dir_all(d.join("plum:white:clockDH")).unwrap();
        std::fs::write(d.join("plum:white:clockDH/brightness"), "255").unwrap();
        let c = Clock::detect_in(&d).expect("display found");
        assert!(!c.dots && c.paths.contains_key("DH"));
        let saved = c.read();
        c.show(&segments("8888"), false);
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockA0/brightness")).unwrap(), "255");
        c.restore(&saved);
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockA0/brightness")).unwrap(), "0");
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockDH/brightness")).unwrap(), "255", "colon back as it was");
        let _ = std::fs::remove_dir_all(&d);
    }
}
