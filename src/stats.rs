//! Anonymous count of running speakers (the «работает на N колонках» badge).
//!
//! Once a day the speaker sends: a random install number (made up on first
//! start, not tied to the speaker or any account), soundya's version, the model
//! and whether AirPlay / the PC are switched on. Nothing else: no IP is stored
//! on the server, no tracks, tokens or commands. Off with «Анонимная статистика»
//! on the settings page. The server is server/stats.py in this repository (also forwards bug reports and feedback to a Telegram bot, see report.rs).

use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};

use crate::Shared;

/// Where pings go; set at build time with SOUNDYA_STATS_URL to use another server.
pub(crate) const URL: &str = match option_env!("SOUNDYA_STATS_URL") {
    Some(u) => u,
    None => "http://195.209.211.30:8787/ping",
};
const DAY: Duration = Duration::from_secs(24 * 3600);

/// The community size for the panel's «100+» banner: the server's
/// /stats.json, asked at most once an hour. Only with «Анонимная статистика»
/// on (otherwise the speaker does not contact the server at all). The last
/// good answer is kept; nothing is ever made up.
static COMMUNITY: std::sync::Mutex<Option<(std::time::Instant, Option<serde_json::Value>)>> = std::sync::Mutex::new(None);

pub fn community(allowed: bool) -> serde_json::Value {
    if !allowed || URL.is_empty() {
        return serde_json::json!({ "ok": false, "why": "статистика выключена" });
    }
    let mut slot = COMMUNITY.lock().unwrap();
    let stale = slot.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(3600));
    if stale {
        let url = std::env::var("SOUNDYA_COMMUNITY_URL")
            .unwrap_or_else(|_| URL.strip_suffix("/ping").map(|b| format!("{b}/stats.json")).unwrap_or_default());
        let fresh = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .get(&url)
            .call()
            .ok()
            .and_then(|r| r.into_json::<serde_json::Value>().ok())
            .filter(|v| v.get("ever").and_then(serde_json::Value::as_u64).is_some());
        let keep = fresh.or_else(|| slot.as_ref().and_then(|(_, v)| v.clone()));
        *slot = Some((std::time::Instant::now(), keep));
    }
    match slot.as_ref().and_then(|(_, v)| v.as_ref()) {
        Some(v) => serde_json::json!({ "ok": true, "ever": v["ever"], "active": v["active"] }),
        None => serde_json::json!({ "ok": false, "why": "сервер статистики не ответил" }),
    }
}

/// Random install number, kept in <state dir>/install_id.
pub(crate) fn install_id(dir: &Path) -> Option<String> {
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
pub(crate) fn model() -> String {
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
        "version": crate::VERSION,
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
