//! Is the speaker's microphone switched off (the button with the red light)? Then Alice hears
//! nothing and soundya gets no phrases — a report «не реагирует на голос вообще» was exactly
//! that. Nothing tells programs about the button (no key event, no ring LED, no word from
//! Alice); what does change: while the mic is on, Alice always records, so the card's capture
//! streams are open; switched off, they are all closed (seen on the Station Lite).

use std::{
    path::Path,
    sync::atomic::{AtomicU8, Ordering},
    thread,
    time::{Duration, Instant},
};

/// 0 — not known (no capture streams to look at), 1 — on, 2 — off.
static STATE: AtomicU8 = AtomicU8::new(0);
/// Closed this long in a row is «off», not Alice restarting her listener.
const OFF_AFTER: Duration = Duration::from_secs(8);
/// No capture right now (looked at twice a second), for the ring to fade out at once.
static SILENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Nothing records at this moment: the mic was likely just switched off (the ring fades out).
pub fn silent_now() -> bool {
    SILENT.load(Ordering::Acquire)
}

/// Some(true) — the microphone is switched off; None — cannot tell on this speaker.
pub fn muted() -> Option<bool> {
    match STATE.load(Ordering::Acquire) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

/// The mic button's line on the Station Lite: `gpio-501 … in lo` while switched off, `hi` while
/// on; it changes the moment the button is pressed. None where there is no such line.
fn button_line(debug_gpio: &str) -> Option<bool> {
    let line = debug_gpio.lines().find(|l| l.trim_start().starts_with("gpio-501 "))?;
    let last = line.split_whitespace().last()?;
    match last {
        "lo" => Some(false),
        "hi" => Some(true),
        _ => None,
    }
}

/// The capture streams' states: Some(true) if any records now, None if there are none.
fn recording(asound: &Path) -> Option<bool> {
    let mut seen = false;
    for card in std::fs::read_dir(asound).ok()?.flatten() {
        let Ok(pcms) = std::fs::read_dir(card.path()) else { continue };
        for pcm in pcms.flatten() {
            let name = pcm.file_name().to_string_lossy().into_owned();
            if !(name.starts_with("pcm") && name.ends_with('c')) {
                continue;
            }
            let Ok(subs) = std::fs::read_dir(pcm.path()) else { continue };
            for sub in subs.flatten() {
                if let Some(text) = crate::fsx::read_string(&sub.path().join("status")) {
                    seen = true;
                    if !text.trim().starts_with("closed") {
                        return Some(true);
                    }
                }
            }
        }
    }
    seen.then_some(false)
}

/// Five times a second. «Off» at once when the button line says so and nothing records;
/// without that line, only after OFF_AFTER of silence from all capture streams.
pub fn run() {
    thread::spawn(|| {
        let asound = Path::new("/proc/asound");
        let mut closed_since: Option<Instant> = None;
        let mut said = 0u8;
        let mut ever = false;
        loop {
            let mut now = recording(asound);
            let button = crate::fsx::read_string(Path::new("/sys/kernel/debug/gpio")).and_then(|t| button_line(&t));
            // A speaker where Alice does not keep recording (another model, another firmware):
            // closed streams say nothing there — only after recording was seen once.
            if now == Some(true) {
                ever = true;
            }
            if !ever {
                now = None;
            }
            SILENT.store(now == Some(false), Ordering::Release);
            let state = match now {
                None => 0,
                Some(true) => {
                    closed_since = None;
                    1
                }
                Some(false) => {
                    let since = *closed_since.get_or_insert_with(Instant::now);
                    if button == Some(false) || since.elapsed() >= OFF_AFTER { 2 } else { STATE.load(Ordering::Acquire).max(1) }
                }
            };
            STATE.store(state, Ordering::Release);
            if state != said && state != 0 {
                eprintln!("mic: {}", if state == 2 { "switched off on the speaker (Alice hears nothing)" } else { "on" });
                said = state;
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn capture_streams_tell_the_microphone() {
        let d = std::env::temp_dir().join(format!("soundya-mic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let sub = |pcm: &str, text: &str| {
            let p = d.join("card0").join(pcm).join("sub0");
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("status"), text).unwrap();
        };
        assert_eq!(super::recording(&d), None, "no capture streams: cannot tell");
        sub("pcm0p", "state: RUNNING\n");
        assert_eq!(super::recording(&d), None, "playback does not count");
        sub("pcm0c", "closed\n");
        sub("pcm1c", "closed\n");
        assert_eq!(super::recording(&d), Some(false), "all closed: the mic is off");
        sub("pcm1c", "state: RUNNING\nowner_pid   : 7479\n");
        assert_eq!(super::recording(&d), Some(true), "Alice records: the mic is on");
        let dbg = "gpiochip497: GPIOs 497-511\n gpio-501 (                    |?                   ) in  lo    \n";
        assert_eq!(super::button_line(dbg), Some(false));
        assert_eq!(super::button_line(&dbg.replace(" lo ", " hi ")), Some(true));
        assert_eq!(super::button_line("gpio-50 (x) in lo"), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
