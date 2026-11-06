//! Onboarding on the speaker: after the first install Alice says where the
//! panel is (its IP address and soundya.local) while the ring plays a short
//! greeting; after an update she says the new version once. Nothing is said
//! again on an ordinary restart. Off with «Голосовое приветствие» in settings.

use std::{path::Path, sync::Arc, thread, time::Duration};

use crate::Shared;

/// Which version was last announced (absent: never — a fresh install).
const MARKER: &str = "hello";

/// «192.168.0.12» as Alice should say it.
pub fn spoken_ip(ip: &str) -> String {
    ip.split('.').collect::<Vec<_>>().join(" точка ")
}

/// What to say: None when this version was already announced.
pub fn phrase(last: Option<&str>, version: &str, ip: Option<&str>, port: u16) -> Option<String> {
    let short = version.split('.').take(2).collect::<Vec<_>>().join(".");
    let addr = match ip {
        Some(ip) => format!("откройте в браузере {}, порт {port}, или саундя точка локал", spoken_ip(ip)),
        None => format!("откройте в браузере саундя точка локал, порт {port}"),
    };
    match last {
        None => Some(format!(
            "Привет! Саундя установлена. Чтобы всё настроить, {addr}. Там же — SoundCloud, компьютер, телевизор и подсветка."
        )),
        Some(v) if v.trim() != version => Some(format!("Саундя обновлена до версии {short}.")),
        Some(_) => None,
    }
}

/// `existing`: soundya was set up before (config from an older version), so a
/// missing marker means an update, not a fresh install.
pub fn run(dir: &Path, shared: Arc<Shared>, enabled: bool, existing: bool, port: u16) {
    if !enabled || std::env::var("SOUNDYA_NO_VOICE").is_ok() {
        return;
    }
    let dir = dir.to_owned();
    thread::spawn(move || {
        let marker = dir.join(MARKER);
        let last = crate::fsx::read_string(&marker).or_else(|| existing.then(|| "0".to_owned()));
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
            if !shared.playing.load(std::sync::atomic::Ordering::Acquire) && !shared.duck.active() {
                break;
            }
            thread::sleep(Duration::from_secs(5));
        }
        eprintln!("hello: {text}");
        if last.is_none() {
            crate::led::greet();
        }
        crate::alice::say(text);
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
        assert!(phrase(None, "0.5.0", None, 8765).unwrap().contains("саундя точка локал"));
        assert_eq!(phrase(Some("0.4.41"), "0.5.0", None, 8765).unwrap(), "Саундя обновлена до версии 0.5.");
        assert_eq!(phrase(Some("0.5.0\n"), "0.5.0", None, 8765), None);
    }
}
