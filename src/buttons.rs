//! The speaker's own ▶‖ button controls SoundCloud.
//!
//! The Station Lite reports its touch buttons through /dev/input (device
//! "sar0_pad0"): ▶‖ is key 582, + is 115, − is 114. Alice reads the same events
//! and, with nothing of hers playing, answers the ▶‖ press out loud; that answer
//! is cut off (see `alice::quiet_for`).

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
                }
                if !is_play_pause_press(&event) || last.elapsed() < Duration::from_millis(400) {
                    continue;
                }
                last = Instant::now();
                let playing = shared.playing.load(Ordering::Acquire);
                eprintln!("buttons: ▶‖ pressed, {}", if playing { "pause" } else { "play" });
                // Alice answers the press herself («нет такой функции»): keep her quiet.
                alice::quiet_for(Duration::from_secs(4));
                let intent = if playing { Intent::Pause } else { Intent::Resume };
                let _ = tx.send(Cmd::Voice(intent));
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
}
