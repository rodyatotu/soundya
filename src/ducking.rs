//! Smart Audio Ducking: soundya's music (SoundCloud, AirPlay) gets quieter
//! while something important happens — a call on the computer, a
//! notification — and comes back when it is over.
//!
//! How it is kept honest:
//! - Events come from system APIs on the devices (the PC agent reports which
//!   app uses the microphone right now — a call — or a notification; any
//!   other device can POST /api/duck). soundya never listens to the room and
//!   records nothing.
//! - Only soundya's own sound is lowered, by the same gain that ducks the music
//!   under Alice's voice. The speaker's volume is never changed, so a volume
//!   the user set by hand is never overwritten, and «restoring» can never go
//!   above what the user chose: it just stops lowering.
//! - Several events at once: the lowest level wins; music comes back when
//!   the last one ends. Notifications end by themselves after a few seconds;
//!   calls have a safety limit of two hours in case «end» never arrives.
//!
//! Modes: "smart" (calls and notifications), "calls", "notify",
//! "manual" (only «приглуши звук» by voice or the page), "off".

use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::Shared;

const CALL_LIMIT: Duration = Duration::from_secs(2 * 3600);

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub mode: String,
    /// Share of the music left during a call / a notification / manual, 0..=1.
    pub call_level: f32,
    pub notify_level: f32,
    pub notify_for: Duration,
    /// Apps whose events are ignored (lower case, part of the name).
    pub exclude: Vec<String>,
}

impl Config {
    pub fn from_settings(s: &crate::settings::Settings) -> Config {
        Config {
            mode: s.duck_mode.clone(),
            call_level: f32::from(s.duck_call_level.min(100)) / 100.0,
            notify_level: f32::from(s.duck_notify_level.min(100)) / 100.0,
            notify_for: Duration::from_secs(u64::from(s.duck_notify_secs.clamp(1, 120))),
            exclude: s.duck_exclude.iter().map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty()).collect(),
        }
    }
}

/// An event from a device.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Request {
    /// Who reports it: "pc", "phone", …
    #[serde(default)]
    pub source: String,
    /// "call" | "notify"
    #[serde(default)]
    pub kind: String,
    /// The same id ends what it started; "" = source+kind.
    #[serde(default)]
    pub id: String,
    /// "start" | "end"
    #[serde(default)]
    pub state: String,
    /// The app (for exclusions and the page): "Telegram", "Zoom"…
    #[serde(default)]
    pub app: String,
}

#[derive(Debug, Clone)]
struct Event {
    id: String,
    kind: String,
    app: String,
    source: String,
    level: f32,
    until: Instant,
}

#[derive(Default)]
struct State {
    config: Option<Config>,
    events: Vec<Event>,
    /// «Приглуши звук» by voice or the page.
    manual: Option<f32>,
}

static STATE: Mutex<State> = Mutex::new(State { config: None, events: Vec::new(), manual: None });

pub fn configure(c: Config) {
    let mut st = STATE.lock().unwrap();
    // Events the new mode no longer covers end now.
    let keep = |k: &str| accepts(&c.mode, k);
    st.events.retain(|e| keep(&e.kind));
    if c.mode == "off" {
        st.manual = None;
    }
    st.config = Some(c);
}

fn accepts(mode: &str, kind: &str) -> bool {
    match mode {
        "smart" => matches!(kind, "call" | "notify"),
        "calls" => kind == "call",
        "notify" => kind == "notify",
        _ => false,
    }
}

/// Handles an event; the answer says whether it was taken.
pub fn event(req: &Request) -> Result<Value, String> {
    let kind = req.kind.trim().to_lowercase();
    if !matches!(kind.as_str(), "call" | "notify") {
        return Err("kind: call или notify".into());
    }
    let id = if req.id.trim().is_empty() { format!("{}:{}", req.source.trim(), kind) } else { req.id.trim().chars().take(80).collect() };
    let mut st = STATE.lock().unwrap();
    let Some(c) = st.config.clone() else { return Err("ещё не запущено".into()) };
    match req.state.trim() {
        "end" | "stop" => {
            let before = st.events.len();
            st.events.retain(|e| e.id != id);
            eprintln!("duck: {id} ended");
            Ok(json!({ "ok": true, "ended": before != st.events.len() }))
        }
        "start" | "" => {
            if !accepts(&c.mode, &kind) {
                return Ok(json!({ "ok": true, "ignored": format!("режим «{}»", c.mode) }));
            }
            let app = req.app.trim().to_lowercase();
            if !app.is_empty() && c.exclude.iter().any(|x| app.contains(x.as_str())) {
                return Ok(json!({ "ok": true, "ignored": "приложение в исключениях" }));
            }
            let (level, for_) = if kind == "call" { (c.call_level, CALL_LIMIT) } else { (c.notify_level, c.notify_for) };
            st.events.retain(|e| e.id != id);
            st.events.push(Event {
                id: id.clone(),
                kind,
                app: req.app.trim().chars().take(40).collect(),
                source: req.source.trim().chars().take(20).collect(),
                level,
                until: Instant::now() + for_,
            });
            eprintln!("duck: {id} started ({})", req.app.trim());
            Ok(json!({ "ok": true }))
        }
        _ => Err("state: start или end".into()),
    }
}

/// «Приглуши звук» (Some) / «верни громкость» (None: also ends every event).
pub fn manual(level: Option<f32>) {
    let mut st = STATE.lock().unwrap();
    st.manual = level;
    if level.is_none() {
        st.events.clear();
    }
}

/// The level now (1.0 = no ducking); expired events are dropped.
fn level_now(st: &mut State, now: Instant) -> f32 {
    st.events.retain(|e| e.until > now);
    let ev = st.events.iter().map(|e| e.level).fold(1.0f32, f32::min);
    st.manual.map_or(ev, |m| m.min(ev))
}

pub fn json() -> Value {
    let mut st = STATE.lock().unwrap();
    let level = level_now(&mut st, Instant::now());
    json!({
        "mode": st.config.as_ref().map(|c| c.mode.clone()),
        "level": level,
        "manual": st.manual.is_some(),
        "events": st.events.iter().map(|e| json!({ "kind": e.kind, "app": e.app, "source": e.source })).collect::<Vec<_>>(),
    })
}

/// Applies the level to the player and AirPlay four times a second.
pub fn run(shared: Arc<Shared>) {
    thread::spawn(move || loop {
        let level = level_now(&mut STATE.lock().unwrap(), Instant::now());
        shared.duck.set_external(level);
        thread::sleep(Duration::from_millis(250));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: &str) -> Config {
        Config { mode: mode.into(), call_level: 0.15, notify_level: 0.4, notify_for: Duration::from_millis(200), exclude: vec!["youtube".into()] }
    }
    fn req(kind: &str, state: &str, id: &str, app: &str) -> Request {
        Request { source: "pc".into(), kind: kind.into(), id: id.into(), state: state.into(), app: app.into() }
    }

    // One test: the state is global.
    #[test]
    fn events_duck_and_restore() {
        configure(cfg("smart"));
        manual(None);
        let lvl = || level_now(&mut STATE.lock().unwrap(), Instant::now());
        assert_eq!(lvl(), 1.0);
        // A call and a notification at once: the lower wins.
        event(&req("call", "start", "c1", "Telegram")).unwrap();
        event(&req("notify", "start", "n1", "Mail")).unwrap();
        assert_eq!(lvl(), 0.15);
        event(&req("call", "end", "c1", "")).unwrap();
        assert_eq!(lvl(), 0.4, "notification still on");
        // The notification ends by itself.
        thread::sleep(Duration::from_millis(250));
        assert_eq!(lvl(), 1.0);
        // Excluded apps and modes.
        assert!(event(&req("notify", "start", "x", "YouTube Music")).unwrap()["ignored"].is_string());
        configure(cfg("calls"));
        assert!(event(&req("notify", "start", "n2", "Mail")).unwrap()["ignored"].is_string());
        event(&req("call", "start", "c2", "Zoom")).unwrap();
        assert_eq!(lvl(), 0.15);
        // Switching off ends what is running.
        configure(cfg("off"));
        assert_eq!(lvl(), 1.0);
        assert!(event(&req("call", "start", "c3", "Zoom")).unwrap()["ignored"].is_string());
        // Manual works in any mode; «верни громкость» ends it all.
        configure(cfg("manual"));
        manual(Some(0.3));
        assert_eq!(lvl(), 0.3);
        manual(None);
        assert_eq!(lvl(), 1.0);
        assert!(event(&req("ring", "start", "", "")).is_err());
    }
}
