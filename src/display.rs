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
    /// The segments' max_brightness.
    max: u32,
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
        let max = paths.values().next().and_then(|p| crate::fsx::read_string(&p.with_file_name("max_brightness"))).and_then(|t| t.trim().parse().ok()).unwrap_or(255);
        complete.then_some(Clock { paths, dots, max })
    }

    fn read(&self) -> HashMap<String, String> {
        self.paths
            .iter()
            .map(|(n, p)| (n.clone(), crate::fsx::read_string(p).unwrap_or_default().trim().to_owned()))
            .collect()
    }

    fn show(&self, lit: &[String], colon: bool) {
        let level = (self.max * u32::from(BRIGHT.load(std::sync::atomic::Ordering::Acquire)) / 100).max(10).to_string();
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
    eprintln!("display: clock found ({} segments)", clock.paths.len());
    thread::spawn(move || {
        let mut saved: Option<HashMap<String, String>> = None;
        let mut started = Instant::now();
        let cycle = Instant::now();
        let mut last = None;
        // What was written last and when: unchanged segments are written again only once a
        // second (the firmware redraws its clock by itself), not every 90 ms.
        let mut drawn: Option<(Vec<String>, bool)> = None;
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
            let widgets = WIDGETS.lock().unwrap().clone().filter(|w| w.on);
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
                    let frame = (lit, colon);
                    if drawn.as_ref() != Some(&frame) || drawn_at.elapsed() >= Duration::from_secs(1) {
                        clock.show(&frame.0, frame.1);
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
        c.show(&segments("1   "), false);
        // «1» on the left digit: B and C of digit 1, at its own brightest.
        assert_eq!(std::fs::read_to_string(d.join("mini2:white:clockB1/brightness")).unwrap(), "170");
        assert_eq!(std::fs::read_to_string(d.join("mini2:white:clockA1/brightness")).unwrap(), "0");
        let _ = std::fs::remove_dir_all(&d);
    }
}
