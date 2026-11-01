//! Running as a Luna package (github.com/nyakokitsu/luna): Luna starts soundya
//! as a binary plugin and passes LUNA_API_URL and LUNA_API_TOKEN. soundya then
//! - is Luna's media provider «SoundCloud»: Luna's own player controls (and
//!   other packages) pause, resume and skip it;
//! - draws the ring through Luna's LED API instead of writing sysfs, so it
//!   works on every Station Luna knows and does not fight Luna over the LEDs.
//! Without those variables nothing here runs.

use std::{
    sync::{atomic::Ordering, mpsc::Sender, Arc, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::{alice::Intent, Cmd, Shared};

struct Api {
    url: String,
    token: String,
    agent: ureq::Agent,
}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn api() -> Option<&'static Api> {
    API.get_or_init(|| {
        let url = std::env::var("LUNA_API_URL").ok()?.trim_end_matches('/').to_owned();
        let token = std::env::var("LUNA_API_TOKEN").ok()?;
        (!url.is_empty() && !token.is_empty()).then(|| Api {
            url,
            token,
            // Luna is on the speaker itself: no proxy, short timeouts.
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build(),
        })
    })
    .as_ref()
}

/// soundya was started by Luna.
pub fn active() -> bool {
    api().is_some()
}

fn post(path: &str, body: &Value) -> Result<Option<Value>, String> {
    let api = api().ok_or("not under Luna")?;
    let resp = api
        .agent
        .post(&format!("{}{path}", api.url))
        .set("X-Luna-Token", &api.token)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| e.to_string())?;
    if resp.status() == 204 {
        return Ok(None);
    }
    let text = resp.into_string().map_err(|e| e.to_string())?;
    Ok(serde_json::from_str(&text).ok())
}

/// Luna's own API (`…/api`), from the runtime URL `…/api/runtime/<id>`.
fn main_api() -> Option<String> {
    let url = &api()?.url;
    url.find("/api/runtime/").map(|i| url[..i + 4].to_owned())
}

/// Alice's volume, 0..=100 %, through Luna (`POST /api/volume/set`).
pub fn volume(percent: i32) -> Result<(), String> {
    let base = main_api().ok_or("not under Luna")?;
    let api = api().ok_or("not under Luna")?;
    api.agent
        .post(&format!("{base}/volume/set"))
        .set("X-Luna-Token", &api.token)
        .set("Content-Type", "application/json")
        .send_string(&json!({ "percent": percent.clamp(0, 100) }).to_string())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// "#rrggbb" for Luna's LED API.
pub fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// The Station's light as Luna names it: "ring", or "logo" on models whose
/// only light is the top one (Station Mini 1). Found on the first frame.
static ZONE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn zone() -> &'static str {
    if ZONE.load(Ordering::Relaxed) == 2 { "logo" } else { "ring" }
}

/// One ring frame; Luna spreads the colours over the physical LEDs.
pub fn ring(colours: &[(u8, u8, u8)]) -> Result<(), String> {
    let colours: Vec<String> = colours.iter().copied().map(hex).collect();
    let body = json!({ "colors": colours });
    match post(&format!("/led/{}", zone()), &body) {
        Ok(_) => {
            ZONE.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed).ok();
            Ok(())
        }
        // No ring here: the logo light (Mini 1), from now on.
        Err(e) if ZONE.load(Ordering::Relaxed) == 0 => {
            post("/led/logo", &body).map_err(|l| format!("ring: {e}; logo: {l}"))?;
            ZONE.store(2, Ordering::Relaxed);
            eprintln!("led: luna: no ring on this Station, using its logo light");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

pub fn ring_off() -> Result<(), String> {
    post(&format!("/led/off/{}", zone()), &json!({})).map(|_| ())
}

/// What Luna is told: "playing", "paused" or "unknown". Stopped counts as
/// paused: Luna only switches to a provider that is paused.
fn state(shared: &Shared) -> &'static str {
    if shared.playing.load(Ordering::Acquire) {
        "playing"
    } else {
        "paused"
    }
}

fn intent(action: &str) -> Option<Intent> {
    Some(match action {
        "pause" => Intent::Pause,
        "resume" => Intent::Resume,
        "next" => Intent::Next,
        "previous" => Intent::Previous,
        _ => return None,
    })
}

/// The state a finished command must end in (None: any, the track changed).
fn expected(action: &str) -> Option<&'static str> {
    match action {
        "pause" => Some("paused"),
        "resume" => Some("playing"),
        _ => None,
    }
}

/// Media provider: reports the state every half second and takes Luna's
/// commands (polled every 200 ms), each confirmed with the state it led to.
pub fn run(tx: Sender<Cmd>, shared: Arc<Shared>) {
    if !active() {
        return;
    }
    eprintln!("luna: running as a Luna package; media provider «SoundCloud»");
    thread::spawn(move || {
        let mut reported = Instant::now() - Duration::from_secs(10);
        let mut failing = false;
        loop {
            thread::sleep(Duration::from_millis(200));
            if reported.elapsed() >= Duration::from_millis(500) {
                reported = Instant::now();
                match post("/provider/state", &json!({ "state": state(&shared) })) {
                    Ok(_) => failing = false,
                    Err(e) if !failing => {
                        failing = true;
                        eprintln!("luna: state not accepted: {e}");
                    }
                    Err(_) => {}
                }
            }
            let Ok(Some(cmd)) = post("/provider/next", &json!(null)) else { continue };
            let (Some(id), Some(action)) = (cmd.get("id").and_then(Value::as_str), cmd.get("action").and_then(Value::as_str))
            else {
                continue;
            };
            let budget = cmd.get("expires_in_ms").and_then(Value::as_u64).unwrap_or(4000).min(4500);
            eprintln!("luna: {action}");
            let success = match intent(action) {
                Some(i) => {
                    let _ = tx.send(Cmd::Voice(i));
                    match expected(action) {
                        // Pause and resume: wait until the player is there,
                        // within Luna's deadline.
                        Some(want) => {
                            let until = Instant::now() + Duration::from_millis(budget.saturating_sub(500));
                            while state(&shared) != want && Instant::now() < until {
                                thread::sleep(Duration::from_millis(100));
                            }
                            state(&shared) == want
                        }
                        // Next and previous: the controller has taken it.
                        None => {
                            thread::sleep(Duration::from_millis(300));
                            true
                        }
                    }
                }
                None => false,
            };
            let body = json!({ "id": id, "success": success, "state": state(&shared) });
            if let Err(e) = post("/provider/result", &body) {
                eprintln!("luna: result not accepted: {e}");
            }
            reported = Instant::now() - Duration::from_secs(1);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_and_actions() {
        assert_eq!(hex((255, 10, 0)), "#FF0A00");
        assert_eq!(intent("pause"), Some(Intent::Pause));
        assert_eq!(intent("shuffle"), None);
        assert_eq!(expected("resume"), Some("playing"));
        assert_eq!(expected("next"), None);
        assert!(!active(), "no Luna in tests");
    }
}
