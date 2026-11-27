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
//!
//! The Station Midi (45 segments, digits counted from 1, the colon on two more, a latch the
//! firmware wants cleared) is drawn through Luna's clock API on the speaker, not through sysfs:
//! Luna knows its layout and gives the display back to the firmware (`/api/clock`, `/api/clock/off`).
//! Found by Luna's answer to `GET /api/clock`, and only when sysfs has no display of the above.

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
    /// The segments' max_brightness.
    max: u32,
    /// The Station Midi: Luna's API at this address draws the display, `paths` is empty.
    luna: Option<String>,
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

/// soundya's segment name as Luna calls it on the Station Midi: the digits are counted from 1
/// («A0» → «A1»), the colon (DH, DL) is PLUS1 and PLUS2. None: the Midi has no such segment
/// (the dot after a digit, DM).
fn luna_segment(name: &str) -> Option<String> {
    match name {
        "DH" => Some("PLUS1".into()),
        "DL" => Some("PLUS2".into()),
        "DEGREE" => Some("DEGREE".into()),
        _ => {
            let mut chars = name.chars();
            let part = chars.next().filter(|p| "ABCDEFG".contains(*p))?;
            let digit: usize = chars.as_str().parse().ok().filter(|d| *d < DIGITS)?;
            Some(format!("{part}{}", digit + 1))
        }
    }
}

/// What `show` sends Luna: the lit segments (and the colon) at `level`, by Luna's names.
fn luna_frame(lit: &[String], colon: bool, level: u32) -> serde_json::Value {
    let level = level.min(255);
    let colon: &[&str] = if colon { &["DH", "DL"] } else { &[] };
    let segments: serde_json::Map<String, serde_json::Value> = lit
        .iter()
        .map(String::as_str)
        .chain(colon.iter().copied())
        .filter_map(luna_segment)
        .map(|name| (name, level.into()))
        .collect();
    segments.into()
}

impl Clock {
    pub fn detect() -> Option<Clock> {
        // SOUNDYA_LEDS_ROOT: only for testing on a computer with a fake sysfs.
        let root = std::env::var("SOUNDYA_LEDS_ROOT").unwrap_or_else(|_| ROOT.into());
        Self::detect_in(Path::new(&root)).or_else(Self::detect_luna)
    }

    /// The Station Midi: no sysfs display of the others, but Luna on this speaker drives one.
    fn detect_luna() -> Option<Clock> {
        let base = crate::luna::main_api()?;
        Self::from_luna_segments(&base, &crate::luna::clock_segments(&base)?)
    }

    /// The Midi's four digits A1..G4 and its colon PLUS1 + PLUS2 among the segments Luna lists.
    fn from_luna_segments(base: &str, segments: &[String]) -> Option<Clock> {
        let has = |name: &str| segments.iter().any(|s| s == name);
        let digits = (1..=DIGITS).all(|d| "ABCDEFG".chars().all(|p| has(&format!("{p}{d}"))));
        // No dots after the digits, the brightest is 255 (Luna's range).
        (digits && has("PLUS1") && has("PLUS2")).then(|| Clock { paths: HashMap::new(), dots: false, max: 255, luna: Some(base.to_owned()) })
    }

    fn detect_in(root: &Path) -> Option<Clock> {
        // «plum:white:clockA0» → prefix «plum:white:clock»; the Mini 2 counts its digits
        // from 1 («mini2:white:clockA1» … «D4», no A0): `first` is the left digit's number.
        let names: Vec<String> = std::fs::read_dir(root).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        let find = |suffix: &str| names.iter().find_map(|n| n.strip_suffix(suffix).filter(|p| p.ends_with(":clock")).map(str::to_owned));
        let (prefix, first) = match find("A0") {
            Some(p) => (p, 0),
            None => (find("A1")?, 1),
        };
        let mut paths = HashMap::new();
        for d in 0..DIGITS {
            for p in ["A", "B", "C", "D", "E", "F", "G", "H"] {
                let path = root.join(format!("{prefix}{p}{}", d + first)).join("brightness");
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
        // Its brightest (the Mini 2's segments go to 170, not 255).
        // From a digit's segment, not «the first» of the map: its order is random, and the degree
        // sign (no max_brightness of its own) gave 255 now and then.
        let max_of = |p: &PathBuf| crate::fsx::read_string(&p.with_file_name("max_brightness")).and_then(|t| t.trim().parse().ok());
        let max = ["A0", "B0", "G0"].iter().filter_map(|k| paths.get(*k)).find_map(max_of).or_else(|| paths.values().find_map(max_of)).unwrap_or(255);
        complete.then_some(Clock { paths, dots, max, luna: None })
    }

    fn read(&self) -> HashMap<String, String> {
        self.paths
            .iter()
            .map(|(n, p)| (n.clone(), crate::fsx::read_string(p).unwrap_or_default().trim().to_owned()))
            .collect()
    }

    /// `percent` of this board's brightest.
    fn level(&self, percent: u8) -> u32 {
        (self.max * u32::from(percent.clamp(5, 100)) / 100).max(1)
    }

    fn show(&self, lit: &[String], colon: bool, level: u32) {
        if let Some(base) = &self.luna {
            Self::luna_result(crate::luna::clock_frame(base, &luna_frame(lit, colon, level)));
            return;
        }
        let level = level.to_string();
        for (name, path) in &self.paths {
            let on = lit.iter().any(|l| l == name) || (colon && (name == "DH" || name == "DL"));
            let _ = std::fs::write(path, if on { level.as_str() } else { "0" });
        }
    }

    fn restore(&self, saved: &HashMap<String, String>) {
        // Luna kept the firmware's frame itself (nothing was read into `saved`).
        if let Some(base) = &self.luna {
            Self::luna_result(crate::luna::clock_off(base));
            return;
        }
        for (name, path) in &self.paths {
            let v = saved.get(name).filter(|v| !v.is_empty()).map_or("0", String::as_str);
            let _ = std::fs::write(path, v);
        }
    }

    /// A call to Luna that failed is logged once, not at every frame.
    fn luna_result(result: Result<(), String>) {
        static FAILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        match result {
            Ok(()) => FAILING.store(false, std::sync::atomic::Ordering::Relaxed),
            Err(e) => {
                if !FAILING.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("display: luna: {e}");
                }
            }
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
    pub light: Light,
}

/// How bright soundya draws. `own: false` — as bright as the firmware's clock is right now
/// (it dims it in the evening; the temperature stayed at full brightness next to a dimmed
/// clock). `own: true` — day and night levels by the time of day, and soundya draws the
/// clock itself too, so the whole display is one brightness.
#[derive(Clone, Debug, PartialEq)]
pub struct Light {
    pub own: bool,
    pub day: u8,
    pub night: u8,
    /// The night, minutes of the local day: from..to, across midnight when from > to.
    pub night_from: u16,
    pub night_to: u16,
    /// Local time = UTC + this many minutes.
    pub tz: i32,
}

impl Light {
    /// Brightness % at `minute` of the local day.
    pub fn percent(&self, minute: u16) -> u8 {
        let (f, t) = (self.night_from, self.night_to);
        let night = if f == t { false } else if f < t { (f..t).contains(&minute) } else { minute >= f || minute < t };
        if night { self.night } else { self.day }
    }
}

/// Minute of the local day and «HHMM» for the clock, `tz` minutes from UTC.
pub fn local_time(unix: i64, tz: i32) -> (u16, String) {
    let m = (unix.div_euclid(60) + i64::from(tz)).rem_euclid(1440) as u16;
    (m, format!("{:02}{:02}", m / 60, m % 60))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The firmware's clock level: the brightest of its lit segments (None when nothing is lit).
fn firmware_level(saved: &HashMap<String, String>) -> Option<u32> {
    saved.values().filter_map(|v| v.trim().parse::<u32>().ok()).filter(|&v| v > 0).max()
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

fn bright() -> u8 {
    BRIGHT.load(std::sync::atomic::Ordering::Acquire)
}

pub fn configure(w: Widgets, bright: u8, settings: &crate::settings::Settings) {
    *WIDGETS.lock().unwrap() = Some(w);
    BRIGHT.store(bright.clamp(5, 100), std::sync::atomic::Ordering::Release);
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

/// The widget for `ms` since the cycle began: None = the firmware's clock. With its own
/// brightness soundya draws the clock too: «HHMM:» (the colon on), `now` = «HHMM».
fn widget_now(w: &Widgets, ms: u64, now: &str) -> Option<String> {
    let clock = || w.light.own.then(|| format!("{now}:"));
    let only_clock = ["clock".to_owned()];
    let list: &[String] = if w.on { &w.list } else { &only_clock };
    let fresh = |slot: &Mutex<Option<(i64, Instant)>>, max: u64| {
        slot.lock().unwrap().filter(|(_, at)| at.elapsed() < Duration::from_secs(max)).map(|(v, _)| v)
    };
    let ready: Vec<Option<String>> = list
        .iter()
        .filter_map(|k| match k.as_str() {
            "clock" => Some(clock()),
            "pc" => fresh(&PC_TEMP, 300).map(|t| Some(temp_text(t, true))),
            "weather" => fresh(&WEATHER, 3600).map(|t| Some(temp_text(t, false))),
            _ => None,
        })
        .collect();
    if ready.is_empty() {
        return clock();
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
    let v = forecast(shared, lat, lon)?;
    parse_weather(&v).ok_or_else(|| "в ответе нет температуры".into())
}

/// One wait per source: the panel waited 15 s and then showed an error.
const WEATHER_WAIT: Duration = Duration::from_secs(6);

/// The forecast in Open-Meteo's shape: from Open-Meteo, else from MET Norway.
/// Some Russian networks do not reach api.open-meteo.com (its geocoding, on another
/// address, works): the connection just times out.
fn forecast(shared: &crate::Shared, lat: f64, lon: f64) -> Result<serde_json::Value, String> {
    let proxy = shared.proxy.lock().unwrap().clone();
    let agent = crate::sc::build_agent(&proxy).map_err(|e| e.to_string())?;
    let open_meteo = agent
        .get("https://api.open-meteo.com/v1/forecast")
        .query("latitude", &format!("{lat:.3}"))
        .query("longitude", &format!("{lon:.3}"))
        .query("current", "temperature_2m,weather_code,wind_speed_10m,apparent_temperature")
        .query("daily", "temperature_2m_max,temperature_2m_min,precipitation_probability_max")
        .query("forecast_days", "1")
        .query("timezone", "auto")
        .timeout(WEATHER_WAIT)
        .call()
        .map_err(|e| e.to_string())
        .and_then(|r| r.into_json::<serde_json::Value>().map_err(|e| e.to_string()));
    let first = match open_meteo {
        Ok(v) if parse_weather(&v).is_some() => return Ok(v),
        Ok(_) => "в ответе нет температуры".to_owned(),
        Err(e) => e,
    };
    eprintln!("weather: open-meteo: {first}; trying met.no");
    // MET Norway asks for a User-Agent that says who calls.
    let met = agent
        .get("https://api.met.no/weatherapi/locationforecast/2.0/compact")
        .query("lat", &format!("{lat:.3}"))
        .query("lon", &format!("{lon:.3}"))
        .set("User-Agent", "soundya/0.5 github.com/rodyatotu/soundya")
        .timeout(WEATHER_WAIT)
        .call()
        .map_err(|e| e.to_string())
        .and_then(|r| r.into_json::<serde_json::Value>().map_err(|e| e.to_string()));
    match met.map(|v| met_no_as_open_meteo(&v)) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err(format!("сервис погоды не ответил: {first}; met.no: в ответе нет температуры")),
        Err(e) => Err(format!("сервис погоды не ответил: {first}; met.no: {e}")),
    }
}

/// MET Norway's locationforecast → the fields of Open-Meteo's answer we use:
/// now from the first entry, today's high and low from the next 24 hours.
pub fn met_no_as_open_meteo(v: &serde_json::Value) -> Option<serde_json::Value> {
    let series = v["properties"]["timeseries"].as_array()?;
    let first = series.first()?;
    let now = &first["data"]["instant"]["details"];
    let temp = now["air_temperature"].as_f64()?;
    let symbol = first["data"]["next_1_hours"]["summary"]["symbol_code"]
        .as_str()
        .or_else(|| first["data"]["next_6_hours"]["summary"]["symbol_code"].as_str())
        .unwrap_or("");
    let temps: Vec<f64> = series.iter().take(24).filter_map(|e| e["data"]["instant"]["details"]["air_temperature"].as_f64()).collect();
    let max = temps.iter().cloned().fold(f64::MIN, f64::max);
    let min = temps.iter().cloned().fold(f64::MAX, f64::min);
    Some(serde_json::json!({
        "current": {
            "temperature_2m": temp,
            "weather_code": met_symbol_code(symbol),
            // m/s → km/h, as Open-Meteo gives it.
            "wind_speed_10m": now["wind_speed"].as_f64().map(|w| w * 3.6),
            "apparent_temperature": null,
        },
        "daily": {
            "temperature_2m_max": [max],
            "temperature_2m_min": [min],
            "precipitation_probability_max": [null],
        },
    }))
}

/// MET Norway's symbol («lightrainshowers_day») → the WMO code the panel draws.
fn met_symbol_code(symbol: &str) -> i64 {
    let s = symbol.split('_').next().unwrap_or("");
    if s.contains("thunder") {
        95
    } else if s.contains("snowshowers") {
        85
    } else if s.contains("snow") || s.contains("sleet") {
        71
    } else if s.contains("rainshowers") {
        80
    } else if s.contains("rain") {
        61
    } else {
        match s {
            "clearsky" => 0,
            "fair" => 1,
            "partlycloudy" => 2,
            "cloudy" => 3,
            "fog" => 45,
            _ => -1,
        }
    }
}

/// The home screen's weather (Open-Meteo, no key): now, today's high and low, kept 15 minutes.
static HOME_WEATHER: Mutex<Option<(f64, f64, serde_json::Value, Instant)>> = Mutex::new(None);

pub fn home_weather(shared: &crate::Shared, lat: f64, lon: f64) -> Result<serde_json::Value, String> {
    if let Some((la, lo, v, at)) = HOME_WEATHER.lock().unwrap().clone() {
        if la == lat && lo == lon && at.elapsed() < Duration::from_secs(900) {
            return Ok(v);
        }
    }
    let v = forecast(shared, lat, lon)?;
    let out = home_weather_of(&v).ok_or("в ответе нет погоды")?;
    *HOME_WEATHER.lock().unwrap() = Some((lat, lon, out.clone(), Instant::now()));
    Ok(out)
}

/// Open-Meteo's answer → {temp, feels, code, text, wind, max, min, rain}.
pub fn home_weather_of(v: &serde_json::Value) -> Option<serde_json::Value> {
    let c = &v["current"];
    let temp = c["temperature_2m"].as_f64()?.round() as i64;
    let code = c["weather_code"].as_i64().unwrap_or(-1);
    let day = |k: &str| v["daily"][k][0].as_f64().map(|x| x.round() as i64);
    Some(serde_json::json!({
        "temp": temp,
        "feels": c["apparent_temperature"].as_f64().map(|x| x.round() as i64),
        "code": code,
        "text": weather_words(code),
        "wind": c["wind_speed_10m"].as_f64().map(|x| (x / 3.6).round() as i64),
        "max": day("temperature_2m_max"),
        "min": day("temperature_2m_min"),
        "rain": day("precipitation_probability_max"),
    }))
}

/// WMO weather codes in words.
pub fn weather_words(code: i64) -> &'static str {
    match code {
        0 => "ясно",
        1 => "почти ясно",
        2 => "переменная облачность",
        3 => "пасмурно",
        45 | 48 => "туман",
        51..=57 => "морось",
        61..=67 => "дождь",
        71..=77 => "снег",
        80..=82 => "ливень",
        85 | 86 => "снегопад",
        95..=99 => "гроза",
        _ => "",
    }
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
        .timeout(Duration::from_secs(8))
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

/// How long after Alice falls silent the display stays hers.
const ALICE_GRACE: Duration = Duration::from_millis(2500);

/// Draws the install phases, interrupts and widgets, and puts the firmware's
/// clock back when it is the clock's turn.
pub fn run() {
    let Some(clock) = Clock::detect() else { return };
    match &clock.luna {
        Some(base) => eprintln!("display: clock found, drawn through Luna at {base}"),
        None => eprintln!("display: clock found ({} segments)", clock.paths.len()),
    }
    thread::spawn(move || {
        let mut saved: Option<HashMap<String, String>> = None;
        let mut started = Instant::now();
        let cycle = Instant::now();
        let mut last = None;
        // What was written last and when: unchanged segments are written again only once a
        // second (the firmware redraws its clock by itself), not every 90 ms.
        let mut drawn: Option<(Vec<String>, bool, u32)> = None;
        let mut drawn_at = Instant::now();
        // While Alice listens / speaks the firmware draws on the display itself (its own
        // animation and volume): soundya steps aside until she has been quiet for a moment —
        // writing over her made the display blink (report SY-427B7DFE, Plum).
        let mut quiet_from = Instant::now();
        loop {
            let phase = crate::led::install_phase();
            if phase != last {
                started = Instant::now();
                last = phase.clone();
            }
            if crate::voice::alice_is_busy() {
                quiet_from = Instant::now() + ALICE_GRACE;
            }
            if phase.is_none() && Instant::now() < quiet_from {
                if let Some(s) = saved.take() {
                    clock.restore(&s);
                }
                drawn = None;
                *SHOWN.lock().unwrap() = "clock".into();
                thread::sleep(Duration::from_millis(90));
                continue;
            }
            let note = NOTE.lock().unwrap().clone().filter(|(_, until)| Instant::now() < *until);
            let widgets = WIDGETS.lock().unwrap().clone().filter(|w| w.on || w.light.own);
            let own = widgets.as_ref().map(|w| w.light.clone()).filter(|l| l.own);
            let (minute, now) = local_time(unix_now(), widgets.as_ref().map_or(0, |w| w.light.tz));
            // What to draw: install > interrupt > timer > widget; None = the firmware's clock.
            let draw: Option<(Vec<String>, bool, String)> = if let Some(p) = &phase {
                let ip = crate::mdns::own_ip().map(|i| i.to_string());
                let (lit, colon) = frame(p, started.elapsed().as_millis() as u64, ip.as_deref(), clock.dots);
                Some((lit, colon, format!("{p:?}")))
            } else if let Some((text, until)) = note {
                let blink = (until.saturating_duration_since(Instant::now()).as_millis() / 400) % 2 == 0;
                Some((if blink { segments(&text) } else { Vec::new() }, false, text))
            } else if let Some(text) = crate::timer::display_text() {
                // soundya's timer: MM:SS with the colon (blinking while it runs, steady when paused).
                let paused = crate::timer::current().is_some_and(|t| t.paused.is_some());
                let colon = paused || cycle.elapsed().as_millis() % 1000 < 500;
                Some((segments(&text), colon, text))
            } else if let Some(text) = widgets.as_ref().and_then(|w| widget_now(w, cycle.elapsed().as_millis() as u64, &now)) {
                let (text, colon) = match text.strip_suffix(':') {
                    Some(t) => (t.to_owned(), true),
                    None => (text, false),
                };
                let mut lit = segments(&text);
                if text.ends_with('C') {
                    lit.push("DEGREE".into());
                }
                Some((lit, colon, text))
            } else {
                None
            };
            match draw {
                Some((lit, colon, text)) => {
                    if saved.is_none() {
                        saved = Some(clock.read());
                    }
                    // Own brightness by the time of day; otherwise the firmware clock's own level
                    // (read as soundya took over), so the temperature is not brighter than the clock.
                    let level = match &own {
                        Some(l) if phase.is_none() => clock.level(l.percent(minute)),
                        _ if phase.is_none() => saved.as_ref().and_then(firmware_level).unwrap_or_else(|| clock.level(bright())),
                        _ => clock.level(bright()),
                    };
                    let frame = (lit, colon, level);
                    // The firmware redraws its clock once a minute: over soundya's own clock that
                    // is put right within a quarter of a second.
                    let every = if own.is_some() { Duration::from_millis(250) } else { Duration::from_secs(1) };
                    if drawn.as_ref() != Some(&frame) || drawn_at.elapsed() >= every {
                        clock.show(&frame.0, frame.1, frame.2);
                        drawn = Some(frame);
                        drawn_at = Instant::now();
                    }
                    *SHOWN.lock().unwrap() = text;
                }
                None => {
                    if let Some(s) = saved.take() {
                        clock.restore(&s);
                    }
                    drawn = None;
                    *SHOWN.lock().unwrap() = "clock".into();
                }
            }
            thread::sleep(Duration::from_millis(90));
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn met_no_answer_reads_like_open_meteo() {
        // A real answer for Saint Petersburg (2026-11-08), cut to 26 hours.
        let v: serde_json::Value = serde_json::from_str(include_str!("../testdata/metno-spb.json")).unwrap();
        let om = super::met_no_as_open_meteo(&v).expect("converted");
        let w = super::home_weather_of(&om).expect("the panel's weather");
        assert_eq!(w["temp"], 7, "6.6 °C now");
        assert_eq!(w["code"], 3, "cloudy");
        assert_eq!(w["text"], "пасмурно");
        assert_eq!(w["wind"], 5, "5.3 m/s");
        assert!(w["max"].as_i64().unwrap() >= w["min"].as_i64().unwrap());
        assert_eq!(super::parse_weather(&om), Some(7));
        assert_eq!(super::met_symbol_code("lightrainshowers_day"), 80);
        assert_eq!(super::met_symbol_code("heavysnow"), 71);
        assert_eq!(super::met_symbol_code("rainandthunder"), 95);
        assert_eq!(super::met_symbol_code("clearsky_night"), 0);
        assert!(super::met_no_as_open_meteo(&serde_json::json!({})).is_none());
    }

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
    fn the_home_weather_in_words() {
        let v = serde_json::json!({
            "current": {"temperature_2m": -2.6, "weather_code": 71, "wind_speed_10m": 18.0, "apparent_temperature": -7.2},
            "daily": {"temperature_2m_max": [0.4], "temperature_2m_min": [-5.5], "precipitation_probability_max": [80]}
        });
        let w = home_weather_of(&v).unwrap();
        assert_eq!((w["temp"].as_i64(), w["text"].as_str(), w["wind"].as_i64()), (Some(-3), Some("снег"), Some(5)));
        assert_eq!((w["max"].as_i64(), w["min"].as_i64(), w["rain"].as_i64(), w["feels"].as_i64()), (Some(0), Some(-6), Some(80), Some(-7)));
        assert!(home_weather_of(&serde_json::json!({})).is_none());
    }

    #[test]
    fn widgets_take_turns() {
        assert_eq!(temp_text(23, false), " 23C");
        assert_eq!(temp_text(-3, false), " -3C");
        assert_eq!(temp_text(54, true), "P54C");
        assert_eq!(parse_weather(&serde_json::json!({"current": {"temperature_2m": -2.6}})), Some(-3));
        *WEATHER.lock().unwrap() = Some((7, Instant::now()));
        *PC_TEMP.lock().unwrap() = None;
        let mut w = Widgets { on: true, list: vec!["clock".into(), "pc".into(), "weather".into()], secs: 10, lat: None, lon: None, light: light(false) };
        assert_eq!(widget_now(&w, 0, "2341"), None, "the clock first");
        assert_eq!(widget_now(&w, 10_500, "2341"), Some("  7C".into()), "no PC data: weather next");
        assert_eq!(widget_now(&w, 20_500, "2341"), None, "and round again");
        // Own brightness: soundya draws the clock itself, with the colon.
        w.light.own = true;
        assert_eq!(widget_now(&w, 0, "2341"), Some("2341:".into()));
        assert_eq!(widget_now(&w, 10_500, "2341"), Some("  7C".into()));
        // Widgets off but own brightness: just the clock.
        w.on = false;
        assert_eq!(widget_now(&w, 10_500, "0705"), Some("0705:".into()));
        *WEATHER.lock().unwrap() = None;
    }

    fn light(own: bool) -> Light {
        Light { own, day: 100, night: 20, night_from: 22 * 60, night_to: 7 * 60, tz: 180 }
    }

    #[test]
    fn brightness_by_the_time_of_day() {
        let l = light(true);
        assert_eq!(l.percent(12 * 60), 100);
        assert_eq!(l.percent(22 * 60), 20, "night from 22:00");
        assert_eq!(l.percent(3 * 60), 20, "across midnight");
        assert_eq!(l.percent(7 * 60), 100, "day from 07:00");
        let same = Light { night_from: 0, night_to: 0, ..l.clone() };
        assert_eq!(same.percent(3 * 60), 100, "no night when from = to");
        let evening = Light { night_from: 18 * 60, night_to: 23 * 60, ..l };
        assert_eq!((evening.percent(19 * 60), evening.percent(23 * 60 + 30)), (20, 100));
        // 2026-11-09 20:41 UTC = 23:41 in Moscow (UTC+3); the day wraps.
        assert_eq!(local_time(1_794_256_860, 180), (23 * 60 + 41, "2341".into()));
        assert_eq!(local_time(1_794_256_860, 240).1, "0041");
        // The firmware's level: its brightest lit segment.
        let saved: HashMap<String, String> = [("A0", "0"), ("B0", "40"), ("C0", "40"), ("DH", "")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert_eq!(firmware_level(&saved), Some(40));
        assert_eq!(firmware_level(&HashMap::new()), None);
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
        c.show(&segments("8888"), false, c.level(100));
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockA0/brightness")).unwrap(), "255");
        c.restore(&saved);
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockA0/brightness")).unwrap(), "0");
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockDH/brightness")).unwrap(), "255", "colon back as it was");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn finds_the_mini2_clock_counted_from_one() {
        let d = std::env::temp_dir().join(format!("soundya-clock-mini2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for n in 1..=4 {
            for p in ["A", "B", "C", "D", "E", "F", "G"] {
                let dir = d.join(format!("mini2:white:clock{p}{n}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("brightness"), "3").unwrap();
                std::fs::write(dir.join("max_brightness"), "170").unwrap();
            }
        }
        std::fs::create_dir_all(d.join("mini2:white:clockDEGREE")).unwrap();
        std::fs::write(d.join("mini2:white:clockDEGREE/brightness"), "0").unwrap();
        let c = Clock::detect_in(&d).expect("display found");
        assert_eq!(c.max, 170);
        assert_eq!((c.level(50), c.level(1)), (85, 8), "a share of 170, never below 5%");
        c.show(&segments("1   "), false, c.level(100));
        // «1» on the left digit: B and C of digit 1, at its own brightest.
        assert_eq!(std::fs::read_to_string(d.join("mini2:white:clockB1/brightness")).unwrap(), "170");
        assert_eq!(std::fs::read_to_string(d.join("mini2:white:clockA1/brightness")).unwrap(), "0");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// What Luna on a Station Midi answers to `GET /api/clock` (read on a real speaker).
    const MIDI_CLOCK: &str = r#"{"available":true,"text_available":true,"segments":["A1","B1","C1","D1","E1","F1","G1","A2","B2","C2","D2","E2","F2","G2","A3","B3","C3","D3","E3","F3","G3","A4","B4","C4","D4","E4","F4","G4","PLUS1","PLUS2","DEGREE","H1","H2","L1","L2","M1","M2","P1","P2","P4","P5","Q1","Q2","Q4","Q5"]}"#;

    fn names(list: &str) -> Vec<String> {
        serde_json::from_str::<serde_json::Value>(list).unwrap()["segments"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_owned()).collect()
    }

    #[test]
    fn the_midi_names_its_segments_for_luna() {
        // The digits count from 1, the colon is PLUS1 + PLUS2, the dots and DM are not there.
        let luna: Vec<Option<String>> = ["A0", "G3", "B2", "DH", "DL", "DEGREE", "H0", "DM", "A4", "A", "X1", ""].iter().map(|n| luna_segment(n)).collect();
        let want = [Some("A1"), Some("G4"), Some("B3"), Some("PLUS1"), Some("PLUS2"), Some("DEGREE"), None, None, None, None, None, None];
        assert_eq!(luna, want.map(|n| n.map(str::to_owned)));
        // «12:» — B C of the first digit, A B D E G of the second, and the colon, at 255 at most.
        assert_eq!(
            luna_frame(&segments("12"), true, 300),
            serde_json::json!({"B1": 255, "C1": 255, "A2": 255, "B2": 255, "D2": 255, "E2": 255, "G2": 255, "PLUS1": 255, "PLUS2": 255})
        );
        assert_eq!(luna_frame(&[], false, 255), serde_json::json!({}), "nothing lit: Luna darkens the rest");
        assert_eq!(luna_frame(&["DEGREE".into()], false, 100), serde_json::json!({"DEGREE": 100}));
        // Every segment of the install frames has a name on the Midi (it draws no dots).
        use crate::led::Install;
        for phase in [Install::Loading, Install::Waiting, Install::Step(3, 4), Install::Done] {
            for ms in (0..6000u64).step_by(90) {
                for ip in [None, Some("192.168.0.12")] {
                    let (lit, colon) = frame(&phase, ms, ip, false);
                    let sent = luna_frame(&lit, colon, 255);
                    assert_eq!(sent.as_object().unwrap().len(), lit.len(), "{phase:?} at {ms} ms: {lit:?}");
                }
            }
        }
    }

    #[test]
    fn finds_the_midi_clock_by_lunas_list() {
        let c = Clock::from_luna_segments("http://127.0.0.1:8099/api", &names(MIDI_CLOCK)).expect("display found");
        assert!(c.paths.is_empty() && !c.dots && c.luna.as_deref() == Some("http://127.0.0.1:8099/api"));
        assert_eq!((c.level(100), c.level(50)), (255, 127));
        assert!(c.read().is_empty(), "nothing to save: Luna keeps the firmware's frame");
        // Not the Midi's: the digits from 0 (Mini 3, Lite 2 — those have sysfs), no colon, nothing.
        let from0: Vec<String> = (0..4).flat_map(|d| "ABCDEFG".chars().map(move |p| format!("{p}{d}"))).chain(["DH".to_owned(), "DL".to_owned()]).collect();
        assert!(Clock::from_luna_segments("x", &from0).is_none());
        let no_colon: Vec<String> = names(MIDI_CLOCK).into_iter().filter(|s| s != "PLUS2").collect();
        assert!(Clock::from_luna_segments("x", &no_colon).is_none());
        let three_digits: Vec<String> = names(MIDI_CLOCK).into_iter().filter(|s| !s.ends_with('4') || s == "Q4" || s == "P4").collect();
        assert!(Clock::from_luna_segments("x", &three_digits).is_none());
        assert!(Clock::from_luna_segments("x", &[]).is_none());
    }

    /// A Luna stand-in on 127.0.0.1: answers each connection with the next of `answers`
    /// (a status line and a JSON body) and keeps what was asked: (method, path, body).
    fn fake_luna(answers: Vec<(&'static str, &'static str)>) -> (String, std::thread::JoinHandle<Vec<(String, String, String)>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/api", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, json) in answers {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end().to_ascii_lowercase();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let mut parts = first.split_whitespace();
                seen.push((parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned(), String::from_utf8(body).unwrap()));
                let mut out = stream;
                let _ = write!(out, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len());
            }
            seen
        });
        (base, handle)
    }

    #[test]
    fn draws_the_midi_through_luna() {
        let (base, luna) = fake_luna(vec![("200 OK", MIDI_CLOCK), ("200 OK", ""), ("200 OK", ""), ("200 OK", "")]);
        let listed = crate::luna::clock_segments(&base).expect("Luna lists the display");
        let c = Clock::from_luna_segments(&base, &listed).expect("display found");
        // «HI» on the right: lit segments and the colon go to POST /clock at the display's own level.
        let hi = segments("  HI");
        c.show(&hi, true, c.level(40));
        // The next frame with nothing lit, then the display goes back to the firmware.
        c.show(&[], false, c.level(40));
        let saved = c.read();
        c.restore(&saved);
        let seen = luna.join().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0], ("GET".into(), "/api/clock".into(), String::new()));
        assert_eq!((seen[1].0.as_str(), seen[1].1.as_str()), ("POST", "/api/clock"));
        let body: serde_json::Value = serde_json::from_str(&seen[1].2).unwrap();
        let lit = serde_json::json!({"B3": 102, "C3": 102, "E3": 102, "F3": 102, "G3": 102, "B4": 102, "C4": 102, "PLUS1": 102, "PLUS2": 102});
        assert_eq!(body, serde_json::json!({ "segments": lit }), "H is BCEFG, I is BC; 40 % of 255");
        let blank: serde_json::Value = serde_json::from_str(&seen[2].2).unwrap();
        assert_eq!(blank, serde_json::json!({"segments": {}}));
        assert_eq!((seen[3].0.as_str(), seen[3].1.as_str()), ("POST", "/api/clock/off"));
    }

    #[test]
    fn no_midi_display_when_luna_has_none() {
        let (base, luna) = fake_luna(vec![("200 OK", r#"{"available":false,"segments":[]}"#), ("404 Not Found", r#"{"error":"no clock"}"#)]);
        assert!(crate::luna::clock_segments(&base).is_none(), "available: false");
        assert!(crate::luna::clock_segments(&base).is_none(), "a Luna without the clock API");
        assert_eq!(luna.join().unwrap().len(), 2);
        // Luna not answering at all (nothing listens there).
        assert!(crate::luna::clock_segments("http://127.0.0.1:1/api").is_none());
    }
}
