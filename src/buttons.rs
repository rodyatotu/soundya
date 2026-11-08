//! The speaker's own ▶‖ button controls SoundCloud.
//!
//! The Station Lite reports its touch buttons through /dev/input (device
//! "sar0_pad0"): ▶‖ is key 582, + is 115, − is 114. Alice reads the same events
//! and, with nothing of hers playing, answers the ▶‖ press out loud; that answer
//! is cut off (see `alice::quiet_for`). She also starts her own Yandex Music on
//! it: with SoundCloud as the default music her player is paused again for a few
//! seconds after every press (it starts late, after loading).

use std::{
    fs::File,
    io::Read,
    sync::{atomic::Ordering, mpsc::Sender, Arc},
    thread,
    time::{Duration, Instant},
};

use crate::{
    alice::{self, Intent},
    Cmd, Shared,
};

/// Key code of the ▶‖ button (KEY_VOICECOMMAND on this board).
const PLAY_PAUSE: u16 = 582;
const EV_KEY: u16 = 1;
/// struct input_event for 32-bit programs: time (8 bytes), type, code, value.
const EVENT_SIZE: usize = 16;

fn parse(event: &[u8]) -> Option<(u16, u16, i32)> {
    if event.len() < EVENT_SIZE {
        return None;
    }
    let kind = u16::from_le_bytes([event[8], event[9]]);
    let code = u16::from_le_bytes([event[10], event[11]]);
    let value = i32::from_le_bytes([event[12], event[13], event[14], event[15]]);
    Some((kind, code, value))
}

/// What a ▶‖ press does: (soundya's intent, hold Alice's own player paused).
/// - SoundCloud playing: paused, and Alice's player held paused (her answer to the
///   press would otherwise swap SoundCloud for Yandex Music), whatever the default.
/// - Alice's own music on purpose («включи радио»): the press pauses it — and only
///   that; the next press is SoundCloud's (SoundCloud default) or Alice's (Yandex default).
/// - Nothing playing, SoundCloud default: SoundCloud resumes, Yandex Music never starts
///   from the button (before, once Alice's music had played, the press started hers again).
/// - Nothing playing, Yandex default (or SoundCloud switched off): the button is Alice's.
fn on_press(soundcloud_playing: bool, alice_music: bool, yandex_default: bool) -> (Option<Intent>, bool) {
    match (soundcloud_playing, alice_music, yandex_default) {
        (true, _, _) => (Some(Intent::Pause), true),
        (false, true, false) => (None, true),
        (false, false, false) => (Some(Intent::Resume), true),
        (false, _, true) => (None, false),
    }
}

/// Alice's player paused now and again while it may still be starting — unless the
/// user asks Alice for her music meanwhile («Алиса, включи радио»).
fn keep_alice_paused(shared: Arc<Shared>) {
    thread::spawn(move || {
        for wait in [0, 300, 700, 1000, 1500, 2500] {
            thread::sleep(Duration::from_millis(wait));
            if shared.alice_music() {
                return;
            }
            alice::pause_native();
        }
    });
}

/// A ▶‖ press (not its release or auto-repeat)?
fn is_play_pause_press(event: &[u8]) -> bool {
    matches!(parse(event), Some((EV_KEY, PLAY_PAUSE, 1)))
}

pub fn run(tx: Sender<Cmd>, shared: Arc<Shared>) {
    for n in 0..4 {
        let path = format!("/dev/input/event{n}");
        let Ok(mut device) = File::open(&path) else { continue };
        let tx = tx.clone();
        let shared = shared.clone();
        thread::spawn(move || {
            let mut event = [0u8; EVENT_SIZE];
            let mut last = Instant::now() - Duration::from_secs(10);
            loop {
                if device.read_exact(&mut event).is_err() {
                    eprintln!("buttons: {path} closed");
                    return;
                }
                if let Some((EV_KEY, code, value)) = parse(&event) {
                    crate::volume::seen_key(code, value);
                    // The firmware shows the volume (or its own answer) on the ring now.
                    if value != 0 && matches!(code, 114 | 115 | PLAY_PAUSE) {
                        crate::led::firmware_animates();
                    }
                }
                if !is_play_pause_press(&event) || last.elapsed() < Duration::from_millis(400) {
                    continue;
                }
                last = Instant::now();
                let playing = shared.playing.load(Ordering::Acquire);
                let yandex_default = shared.keyword_only() || !shared.soundcloud();
                let (intent, block) = on_press(playing, shared.alice_music(), yandex_default);
                eprintln!("buttons: ▶‖ pressed, {}", match (&intent, block) { (Some(Intent::Pause), _) => "pause", (Some(_), _) => "play", (None, true) => "Alice's music paused", (None, false) => "left to Alice" });
                if block {
                    // Alice answers the press herself («нет такой функции»): keep her quiet,
                    // and her music off — the button is SoundCloud's now.
                    alice::quiet_for(Duration::from_secs(4));
                    shared.alice_music.store(false, Ordering::Release);
                    keep_alice_paused(shared.clone());
                }
                if let Some(intent) = intent {
                    let _ = tx.send(Cmd::Voice(intent));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_events_recorded_on_the_speaker() {
        // Recorded with `cat /dev/input/event1 | od -tx1` while pressing ▶‖ and +.
        let press = [0x58, 0xce, 0xdf, 0x6a, 0xcc, 0x6c, 0x04, 0, 0x01, 0, 0x46, 0x02, 0x01, 0, 0, 0];
        let release = [0x59, 0xce, 0xdf, 0x6a, 0x0b, 0xe0, 0x03, 0, 0x01, 0, 0x46, 0x02, 0, 0, 0, 0];
        let sync = [0x58, 0xce, 0xdf, 0x6a, 0xcc, 0x6c, 0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let volume_up = [0x5b, 0xce, 0xdf, 0x6a, 0xff, 0x93, 0x08, 0, 0x01, 0, 0x73, 0, 0x01, 0, 0, 0];
        assert!(is_play_pause_press(&press));
        assert!(!is_play_pause_press(&release));
        assert!(!is_play_pause_press(&sync));
        assert!(!is_play_pause_press(&volume_up));
        assert_eq!(parse(&volume_up), Some((EV_KEY, 115, 1)));
    }

    #[test]
    fn yandex_music_never_starts_from_the_button_by_default() {
        // SoundCloud by default: play or pause SoundCloud, and Alice's player is held paused.
        assert_eq!(on_press(false, false, false), (Some(Intent::Resume), true));
        assert_eq!(on_press(true, false, false), (Some(Intent::Pause), true));
        // Her radio on purpose: the press only pauses it (SoundCloud does not start instead).
        assert_eq!(on_press(false, true, false), (None, true));
        // Yandex Music by default: the button is Alice's, except pausing SoundCloud —
        // and then her player does not jump in either.
        assert_eq!(on_press(false, false, true), (None, false));
        assert_eq!(on_press(false, true, true), (None, false));
        assert_eq!(on_press(true, false, true), (Some(Intent::Pause), true));
    }
}
