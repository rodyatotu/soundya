//! Anonymous count of running speakers (the «работает на N колонках» badge).
//!
//! Once a day the speaker sends: a random install number (made up on first
//! start, not tied to the speaker or any account), soundya's version, the model
//! and whether AirPlay / the PC are switched on. Nothing else: no IP is stored
//! on the server, no tracks, tokens or commands. Off with «Анонимная статистика»
//! on the settings page. The server is server/stats.py in this repository (also forwards web-panel feedback to a Telegram bot).

use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};

use crate::Shared;

/// Where pings go; set at build time with SOUNDYA_STATS_URL to use another server.
const URL: &str = match option_env!("SOUNDYA_STATS_URL") {
    Some(u) => u,
    None => "http://195.209.211.30:8787/ping",
};
const DAY: Duration = Duration::from_secs(24 * 3600);

/// Anonymous feedback from the web panel goes to the same server, which passes
/// it to the author's Telegram bot (server/stats.py). Same base as the pings.
pub fn feedback(text: &str) -> Result<(), String> {
    if URL.is_empty() {
        return Err("сервер обратной связи не задан".into());
    }
    let text = text.trim();
    if text.is_empty() {
        return Err("пустое сообщение".into());
    }
    let url = URL.strip_suffix("/ping").map(|b| format!("{b}/feedback")).unwrap_or_else(|| URL.to_owned());
    let body = serde_json::json!({
        "text": text.chars().take(2000).collect::<String>(),
        "version": env!("CARGO_PKG_VERSION"),
        "model": model(),
    });
    let resp = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    match resp {
        Ok(_) => Ok(()),
        // The server says why (feedback off, too many, not delivered).
        Err(ureq::Error::Status(503, _)) => Err("обратная связь сейчас недоступна".into()),
        Err(ureq::Error::Status(429, _)) => Err("слишком часто, попробуйте через минуту".into()),
        Err(ureq::Error::Status(_, _)) => Err("сервер не принял сообщение".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Random install number, kept in <state dir>/install_id.
fn install_id(dir: &Path) -> Option<String> {
    let path = dir.join("install_id");
    if let Some(id) = crate::fsx::read_string(&path).map(|t| t.trim().to_owned()) {
        if id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(id);
        }
    }
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut bytes).ok()?;
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, &id).ok()?;
    Some(id)
}

/// The speaker model: "yandexmicro" (Lite), "yandexmini", "yandexstation_2"…
fn model() -> String {
    let read = |p: &str| crate::fsx::read_string(Path::new(p)).map(|t| t.trim_matches(|c: char| c.is_whitespace() || c == '\0').to_owned());
    let hostname = read("/proc/sys/kernel/hostname").unwrap_or_default();
    if hostname.starts_with("yandex") {
        return clean(&hostname);
    }
    read("/proc/device-tree/model").map(|m| clean(&m)).filter(|m| !m.is_empty()).unwrap_or_else(|| clean(&hostname))
}

/// Only what the server accepts: letters, digits, `.`, `-`, `_`, space; 40 characters.
fn clean(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric() || " ._-".contains(*c)).take(40).collect()
}

fn ping(dir: &Path, shared: &Shared) -> Result<(), String> {
    let id = install_id(dir).ok_or("no install id")?;
    let body = serde_json::json!({
        "id": id,
        "version": env!("CARGO_PKG_VERSION"),
        "model": model(),
        "airplay": shared.airplay.json()["enabled"].as_bool().unwrap_or(false),
        "pc": shared.pc_configured(),
    });
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .post(URL)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn run(dir: PathBuf, shared: Arc<Shared>) {
    if URL.is_empty() {
        return; // built without a statistics server
    }
    thread::spawn(move || {
        // Let the network and Alice come up first.
        thread::sleep(Duration::from_secs(120));
        loop {
            if shared.stats.load(Ordering::Acquire) {
                match ping(&dir, &shared) {
                    Ok(()) => eprintln!("stats: counted"),
                    Err(e) => {
                        eprintln!("stats: {e}");
                        // Try again in an hour rather than a day.
                        thread::sleep(Duration::from_secs(3600));
                        continue;
                    }
                }
            }
            thread::sleep(DAY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_model_names() {
        assert_eq!(clean("yandexmicro\n"), "yandexmicro");
        assert_eq!(clean("Amlogic Meson AXG <S400>"), "Amlogic Meson AXG S400");
        assert_eq!(clean(&"x".repeat(80)).len(), 40);
    }

    #[test]
    fn makes_and_keeps_an_install_id() {
        let dir = std::env::temp_dir().join(format!("soundya-stats-test-{}", std::process::id()));
        let _ = std::fs::create_dir(&dir);
        let a = install_id(&dir).unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(install_id(&dir).unwrap(), a);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
