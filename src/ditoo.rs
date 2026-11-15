//! What the Divoom Ditoo Plus shows, by mode (developer build's settings, «Ditoo Plus»).
//!
//! The Ditoo is drawn by a launcher on the PC; soundya only says what to draw. Modes are set
//! up on the panel: a name («обычный», «игра», «работа»…), what to show (clock, weather, what
//! plays, the timer, tasks, the PC's load, Alice's last phrase, own text) and how many seconds
//! each screen stays. A scenario switches the mode (step «Режим Ditoo», command `ditoo <mode>`):
//! «Алиса, играю» → `ditoo игра`, «Алиса, запусти рабочий режим» → `ditoo работа`.
//!
//! The launcher reads `GET /api/ditoo`: the mode, its screens in order, the rotation time and
//! the live data for them (see `json`). Polling every few seconds is fine: it is built from
//! memory, no network.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The screens a mode may show, in the panel's order: (key, name on the panel).
pub const SCREENS: [(&str, &str); 10] = [
    ("clock", "Часы"),
    ("weather", "Погода"),
    ("track", "Что играет"),
    ("timer", "Таймер и помодоро"),
    ("tasks", "Задачи на сегодня"),
    ("pc", "Загрузка компьютера"),
    ("heard", "Последняя фраза Алисе"),
    ("mentions", "Упоминания в Telegram"),
    ("text", "Свой текст"),
    ("off", "Ничего (экран погашен)"),
];

/// The mode used when nothing switched it (and after `ditoo обычный`).
pub const DEFAULT: &str = "обычный";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Mode {
    #[serde(default)]
    pub name: String,
    /// Keys from SCREENS, in the order they rotate.
    #[serde(default)]
    pub show: Vec<String>,
    /// Seconds each screen stays (0 = the launcher's own default).
    #[serde(default)]
    pub rotate: u16,
    /// For the «text» screen.
    #[serde(default)]
    pub text: String,
    /// 0–100; 0 = leave the Ditoo's own brightness.
    #[serde(default)]
    pub brightness: u8,
}

/// The mode on now and since when (unix s); "" = DEFAULT.
static ACTIVE: Mutex<(String, u64)> = Mutex::new((String::new(), 0));

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn norm(name: &str) -> String {
    name.trim().to_lowercase().replace('ё', "е")
}

/// The modes with the default one always there (first), names unique, screens known.
pub fn modes(saved: &[Mode]) -> Vec<Mode> {
    let mut out: Vec<Mode> = Vec::new();
    for m in saved {
        let name = m.name.trim();
        if name.is_empty() || out.iter().any(|o| norm(&o.name) == norm(name)) {
            continue;
        }
        let show = m.show.iter().filter(|s| SCREENS.iter().any(|(k, _)| k == &s.as_str())).cloned().collect();
        out.push(Mode { name: name.to_owned(), show, rotate: m.rotate.min(3600), text: m.text.chars().take(200).collect(), brightness: m.brightness.min(100) });
    }
    if !out.iter().any(|m| norm(&m.name) == DEFAULT) {
        out.insert(0, Mode { name: DEFAULT.into(), show: vec!["clock".into(), "weather".into(), "track".into()], rotate: 10, ..Mode::default() });
    }
    out
}

/// `ditoo <mode>`: switches to it; Err with the known names if there is none.
pub fn set(saved: &[Mode], name: &str) -> Result<String, String> {
    let all = modes(saved);
    let want = norm(name);
    let want = if want.is_empty() || want == "off" || want == "сброс" { DEFAULT.to_owned() } else { want };
    match all.iter().find(|m| norm(&m.name) == want) {
        Some(m) => {
            *ACTIVE.lock().unwrap() = (m.name.clone(), now());
            eprintln!("ditoo: mode «{}»", m.name);
            Ok(format!("OK: Ditoo — режим «{}»", m.name))
        }
        None => Err(format!("error: нет режима Ditoo «{name}» (есть: {})", all.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

/// The active mode (DEFAULT if the active one was deleted since).
pub fn active(saved: &[Mode]) -> (Mode, u64) {
    let all = modes(saved);
    let (name, since) = ACTIVE.lock().unwrap().clone();
    match all.iter().find(|m| norm(&m.name) == norm(&name)) {
        Some(m) => (m.clone(), since),
        None => (all.into_iter().next().unwrap_or_default(), since),
    }
}

/// For the launcher: `{"mode", "since", "screens": [..], "rotate", "text", "brightness",
/// "modes": [names], "data": {..}}`. `data` holds only what the mode's screens need; `status`
/// is the speaker's /api/status (what plays, the timer, Alice's phrase, the weather it knows).
pub fn json(saved: &[Mode], status: &Value, tasks: Option<Value>) -> Value {
    let (m, since) = active(saved);
    let mut data = serde_json::Map::new();
    for s in &m.show {
        match s.as_str() {
            "track" => {
                // "Artist — Title", as the panel shows it.
                data.insert("track".into(), json!({
                    "title": status["track"], "state": status["state"],
                    "position_ms": status["position_ms"], "duration_ms": status["duration_ms"],
                }));
            }
            "timer" => {
                data.insert("timer".into(), status["timer"].clone());
            }
            // Who wrote to the owner in the team's group and what (mentions.rs).
            "mentions" => {
                data.insert("mentions".into(), status["mentions"].clone());
            }
            "heard" => {
                data.insert("heard".into(), status["heard"].clone());
            }
            "weather" => {
                data.insert("weather".into(), status["display"]["weather"].clone());
            }
            "pc" => {
                data.insert("pc".into(), status["display"]["pc_temp"].clone());
            }
            "tasks" => {
                data.insert("tasks".into(), tasks.clone().unwrap_or(Value::Null));
            }
            _ => {}
        }
    }
    json!({
        "mode": m.name, "since": since, "screens": m.show, "rotate": m.rotate, "text": m.text,
        "brightness": m.brightness, "modes": modes(saved).iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        "data": data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(name: &str, show: &[&str]) -> Mode {
        Mode { name: name.into(), show: show.iter().map(|s| s.to_string()).collect(), rotate: 5, ..Mode::default() }
    }

    #[test]
    fn modes_switch_and_feed_the_launcher() {
        let saved = vec![mode("Игра", &["clock", "pc", "nonsense"]), mode("игра", &["text"]), mode("Работа", &["timer", "tasks"])];
        let all = modes(&saved);
        assert_eq!(all.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["обычный", "Игра", "Работа"], "default first, duplicates dropped");
        assert_eq!(all[1].show, ["clock", "pc"], "unknown screens dropped");
        assert!(set(&saved, "игра").is_ok());
        let st = json!({"display": {"pc_temp": 61}, "timer": {"left": 300}, "track": "a — b"});
        let j = json(&saved, &st, None);
        assert_eq!(j["mode"], "Игра");
        assert_eq!(j["data"]["pc"], 61);
        assert!(j["data"].get("track").is_none(), "only what the mode shows");
        assert!(set(&saved, "кино").unwrap_err().contains("есть: обычный, Игра, Работа"));
        assert!(set(&saved, "сброс").is_ok());
        assert_eq!(active(&saved).0.name, "обычный");
        let j = json(&saved, &st, Some(json!([{"title": "t"}])));
        assert_eq!(j["screens"], json!(["clock", "weather", "track"]));
    }
}
