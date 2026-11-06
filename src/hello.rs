//! Onboarding on the speaker.
//!
//! First install: while soundya starts and looks for the network the ring (and
//! the clock display, if the station has one) shows «loading»; then Alice says
//! where the panel is and how to hear it again («Алиса, повтори адрес») and
//! waits — the ring breathes, the display scrolls the address. When the panel's
//! «Начать установку» is pressed she moves on, the ring fills step by step with
//! the wizard, and at the end she thanks the user.
//!
//! After an update she says the new version once; an ordinary restart is
//! silent. «Алиса, повтори адрес» works at any time. The voice is off with
//! «Голосом сообщать…» in the settings (the light still shows the steps).

use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use crate::{led::Install, Shared};

/// Which version was last announced (absent: never — a fresh install).
const MARKER: &str = "hello";
pub const WIZARD_STEPS: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Idle,
    /// The address was said; waiting for the panel.
    Waiting,
    Wizard,
}

static STAGE: Mutex<Stage> = Mutex::new(Stage::Idle);
static VOICE: AtomicBool = AtomicBool::new(true);
static PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(8765);

pub fn set_voice(on: bool) {
    VOICE.store(on, Ordering::Release);
}

fn say(text: String) {
    if VOICE.load(Ordering::Acquire) && std::env::var("SOUNDYA_NO_VOICE").is_err() {
        eprintln!("hello: {text}");
        crate::alice::say(text);
    }
}

/// «192.168.0.12» as Alice should say it.
pub fn spoken_ip(ip: &str) -> String {
    ip.split('.').collect::<Vec<_>>().join(" точка ")
}

/// Where the panel is, as a sentence.
pub fn address(ip: Option<&str>, port: u16) -> String {
    match ip {
        Some(ip) => format!("Откройте в браузере {}, порт {port}, или саундя точка локал", spoken_ip(ip)),
        None => format!("Откройте в браузере саундя точка локал, порт {port}"),
    }
}

/// What to say at start: None when this version was already announced.
pub fn phrase(last: Option<&str>, version: &str, ip: Option<&str>, port: u16) -> Option<String> {
    let short = version.split('.').take(2).collect::<Vec<_>>().join(".");
    match last {
        None => Some(format!(
            "Привет! Саундя установлена. Чтобы всё настроить, {}. Если нужно повторить адрес, скажите: Алиса, повтори адрес.",
            lower_first(&address(ip, port))
        )),
        Some(v) if v.trim() != version => Some(format!("Саундя обновлена до версии {short}.")),
        Some(_) => None,
    }
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// «Алиса, повтори адрес».
pub fn repeat() -> String {
    let ip = crate::mdns::own_ip().map(|i| i.to_string());
    let text = format!("{}.", address(ip.as_deref(), PORT.load(Ordering::Acquire)));
    // Asked for: said even with the announcements switched off.
    crate::alice::say(text.clone());
    text
}

/// The panel's «Начать установку».
pub fn start() {
    let mut st = STAGE.lock().unwrap();
    if *st == Stage::Waiting {
        say("Отлично, продолжаем на странице. Выберите, что включить.".into());
    }
    *st = Stage::Wizard;
    crate::led::set_install(Some(Install::Step(1, WIZARD_STEPS)));
}

/// Step n of the wizard (1-based).
pub fn step(n: u8) {
    crate::led::set_install(Some(Install::Step(n.clamp(1, WIZARD_STEPS), WIZARD_STEPS)));
}

/// The wizard is finished (or skipped).
pub fn done() {
    *STAGE.lock().unwrap() = Stage::Idle;
    say("Готово! Спасибо, что используете саундя.".into());
    crate::led::set_install(Some(Install::Done));
}

pub fn stage() -> Stage {
    *STAGE.lock().unwrap()
}

/// `existing`: soundya was set up before (config from an older version), so a
/// missing marker means an update, not a fresh install.
pub fn run(dir: &Path, shared: Arc<Shared>, voice: bool, existing: bool, port: u16) {
    set_voice(voice);
    PORT.store(port, Ordering::Release);
    let dir = dir.to_owned();
    let marker = dir.join(MARKER);
    let last = crate::fsx::read_string(&marker).or_else(|| existing.then(|| "0".to_owned()));
    let fresh = last.is_none();
    if fresh {
        crate::led::set_install(Some(Install::Loading));
    }
    thread::spawn(move || {
        let version = env!("CARGO_PKG_VERSION");
        // Wait for the network (the address is the point), at most 3 minutes.
        let mut ip = None;
        for _ in 0..36 {
            ip = crate::mdns::own_ip();
            if ip.is_some() {
                break;
            }
            thread::sleep(Duration::from_secs(5));
        }
        let Some(text) = phrase(last.as_deref(), version, ip.map(|i| i.to_string()).as_deref(), port) else {
            return;
        };
        // Let Alice finish her own start-up sounds; never over music.
        thread::sleep(Duration::from_secs(8));
        for _ in 0..60 {
            if !shared.playing.load(Ordering::Acquire) && !shared.duck.active() {
                break;
            }
            thread::sleep(Duration::from_secs(5));
        }
        say(text);
        if fresh && stage() == Stage::Idle {
            *STAGE.lock().unwrap() = Stage::Waiting;
            crate::led::set_install(Some(Install::Waiting));
        }
        if let Err(e) = crate::settings::write_synced(&marker, version.as_bytes()) {
            eprintln!("hello: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_it_once_per_version() {
        let first = phrase(None, "0.5.0", Some("192.168.0.12"), 8765).unwrap();
        assert!(first.contains("192 точка 168 точка 0 точка 12, порт 8765"), "{first}");
        assert!(first.contains("скажите: Алиса, повтори адрес"), "{first}");
        assert!(phrase(None, "0.5.0", None, 8765).unwrap().contains("саундя точка локал"));
        assert_eq!(phrase(Some("0.4.41"), "0.5.0", None, 8765).unwrap(), "Саундя обновлена до версии 0.5.");
        assert_eq!(phrase(Some("0.5.0\n"), "0.5.0", None, 8765), None);
    }

    #[test]
    fn wizard_stages() {
        set_voice(false);
        *STAGE.lock().unwrap() = Stage::Waiting;
        start();
        assert_eq!(stage(), Stage::Wizard);
        assert_eq!(crate::led::install_phase(), Some(Install::Step(1, WIZARD_STEPS)));
        step(3);
        assert_eq!(crate::led::install_phase(), Some(Install::Step(3, WIZARD_STEPS)));
        done();
        assert_eq!(stage(), Stage::Idle);
        assert_eq!(crate::led::install_phase(), Some(Install::Done));
        crate::led::set_install(None);
    }
}
