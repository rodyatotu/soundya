//! «Громкость голоса Алисы»: her voice quieter than the music.
//!
//! Alice's speech does not pass through soundya, so it cannot be turned down
//! on its own. What can be done: the speaker's mixer (the same «Master» control
//! Alice sets her volume with, written directly — no button presses, no tick)
//! goes down the moment she starts listening — before she answers, so her first
//! word is already quiet — and comes back when she is silent again. soundya's
//! own music is ducked under her voice anyway, so nothing else changes.
//!
//! Respecting the user: if the volume was changed while she spoke («громче»,
//! the buttons, the slider), soundya does not put the old value back. If
//! soundya stops mid-answer, the volume is restored at its next start
//! (voice-restore file). Without amixer on the speaker the feature is off.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Mutex,
    },
    thread,
    time::Duration,
};

/// Her voice as a share of the current volume, % (100 = as it is: off).
static PERCENT: AtomicU8 = AtomicU8::new(100);
/// …and never above this level of hers, 0..=10 (10 = no cap).
static MAX_LEVEL: AtomicU8 = AtomicU8::new(10);
static BUSY: AtomicBool = AtomicBool::new(false);
/// Bumped on every change of BUSY: a late restore for an old answer is dropped.
static GEN: AtomicU64 = AtomicU64::new(0);
/// (value before, value we set) while lowered.
static LOWERED: Mutex<Option<(u32, u32)>> = Mutex::new(None);
/// Mixer writes in order.
static ONE: Mutex<()> = Mutex::new(());

fn restore_file() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("SOUNDYA_DIR").unwrap_or_else(|_| "/data/opt/soundya".into())).join("voice-restore")
}

pub fn configure(percent: u8, max_level: u8) {
    PERCENT.store(percent.clamp(10, 100), Ordering::Release);
    MAX_LEVEL.store(max_level.min(10), Ordering::Release);
}

pub fn enabled() -> bool {
    PERCENT.load(Ordering::Acquire) < 100 || MAX_LEVEL.load(Ordering::Acquire) < 10
}

/// The mixer value for her voice, from the current one: `percent` of it,
/// capped at the value of `max_level` (None: leave it).
pub fn target(current: u32, percent: u8, max_cap: Option<u32>) -> Option<u32> {
    let mut t = current * u32::from(percent.min(100)) / 100;
    if let Some(cap) = max_cap {
        t = t.min(cap);
    }
    // Never silent, never louder.
    let t = t.max(current.min(8));
    (t < current).then_some(t)
}

fn save_restore(v: Option<(u32, u32)>) {
    match v {
        Some((before, set)) => {
            let _ = crate::settings::write_synced(&restore_file(), format!("{before} {set}").as_bytes());
        }
        None => {
            let _ = std::fs::remove_file(restore_file());
        }
    }
}

/// Alice is listening, thinking or speaking now (the clock display steps aside meanwhile).
pub fn alice_is_busy() -> bool {
    BUSY.load(Ordering::Acquire)
}

/// Alice's state from alice.rs: listening/thinking/speaking (true) or idle.
pub fn alice_busy(busy: bool) {
    if BUSY.swap(busy, Ordering::AcqRel) == busy {
        return;
    }
    let gen = GEN.fetch_add(1, Ordering::AcqRel) + 1;
    if busy {
        if !enabled() {
            return;
        }
        thread::spawn(move || lower(gen));
    } else {
        thread::spawn(move || {
            // She pauses between listening and answering: wait for real silence.
            thread::sleep(Duration::from_millis(700));
            if GEN.load(Ordering::Acquire) == gen {
                restore();
            }
        });
    }
}

fn lower(gen: u64) {
    let _one = ONE.lock().unwrap();
    if GEN.load(Ordering::Acquire) != gen || LOWERED.lock().unwrap().is_some() {
        return;
    }
    let Some(current) = crate::volume::read_mixer_value() else { return };
    let max = MAX_LEVEL.load(Ordering::Acquire);
    let cap = (max < 10).then(|| crate::volume::mixer_for_level(i32::from(max)));
    let Some(t) = target(current, PERCENT.load(Ordering::Acquire), cap) else { return };
    if crate::volume::write_mixer_value(t) {
        *LOWERED.lock().unwrap() = Some((current, t));
        save_restore(Some((current, t)));
    }
}

fn restore() {
    let _one = ONE.lock().unwrap();
    let Some((before, set)) = LOWERED.lock().unwrap().take() else { return };
    save_restore(None);
    // Changed meanwhile («громче», buttons, slider): the user's value stays.
    if crate::volume::read_mixer_value() == Some(set) {
        crate::volume::write_mixer_value(before);
    } else {
        eprintln!("voice: volume changed during the answer; not restoring");
    }
}

/// At start: a volume left lowered by a soundya that stopped mid-answer.
pub fn recover() {
    let Some(text) = crate::fsx::read_string(&restore_file()) else { return };
    let mut it = text.split_whitespace().filter_map(|v| v.parse::<u32>().ok());
    if let (Some(before), Some(set)) = (it.next(), it.next()) {
        if crate::volume::read_mixer_value() == Some(set) {
            crate::volume::write_mixer_value(before);
            eprintln!("voice: restored the volume left lowered ({set} → {before})");
        }
    }
    save_restore(None);
}

pub fn json() -> serde_json::Value {
    serde_json::json!({
        "available": crate::volume::read_mixer_value().is_some(),
        "lowered": LOWERED.lock().unwrap().is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_target() {
        assert_eq!(target(200, 50, None), Some(100));
        assert_eq!(target(200, 100, None), None, "100 % = as it is");
        assert_eq!(target(200, 100, Some(120)), Some(120), "the cap alone");
        assert_eq!(target(100, 80, Some(150)), Some(80));
        assert_eq!(target(60, 50, Some(200)), Some(30));
        assert_eq!(target(5, 10, None), None, "already quiet: never down to nothing");
        assert_eq!(target(40, 10, None), Some(8), "never silent");
    }
}
