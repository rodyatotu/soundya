//! Orange ring while SoundCloud plays, dim on pause, pulsing while something loads
//! (Station Lite: three RGB zones in sysfs).
//!
//! Alice's own animations (listening, speaking, volume) write the same LEDs, so
//! the ring is only set while Alice is idle and put back after her animations.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};

use crate::Shared;

/// SoundCloud orange as it looks on the ring (the green LED is much brighter).
const ORANGE: (u8, u8, u8) = (255, 50, 0);
const TICK_MS: u64 = 50;
/// Alice quiet for this long before the ring is ours again.
const IDLE_MS: u64 = 1000;
/// A playing track whose position has not moved for this long is loading.
const STUCK_MS: u64 = 1500;
/// One breath of the loading animation: fades out and back in.
const BREATH_MS: f32 = 1600.0;
/// Lowest brightness of a breath, as a share of full.
const BREATH_MIN: f32 = 0.06;
/// Brightness on pause, as a share of full.
const PAUSED: f32 = 0.15;

/// Orange at `level` (0..1) of full brightness.
fn orange(level: f32) -> (u8, u8, u8) {
    let l = level.clamp(0.0, 1.0);
    ((ORANGE.0 as f32 * l).round() as u8, (ORANGE.1 as f32 * l).round() as u8, 0)
}

/// Smooth fade out and in, 1 → BREATH_MIN → 1 over BREATH_MS.
fn breath(ms: u64) -> f32 {
    let phase = (ms % BREATH_MS as u64) as f32 / BREATH_MS * std::f32::consts::TAU;
    BREATH_MIN + (1.0 - BREATH_MIN) * (0.5 + 0.5 * phase.cos())
}

struct Zone {
    r: PathBuf,
    g: PathBuf,
    b: PathBuf,
}

fn brightness(path: &Path) -> Option<u8> {
    crate::fsx::read_string(path)?.trim().parse().ok()
}

fn write(path: &Path, value: u8) {
    let _ = fs::write(path, value.to_string());
}

struct Ring(Vec<Zone>);

impl Ring {
    fn detect() -> Option<Self> {
        let zones: Vec<Zone> = (1..=3)
            .map(|z| {
                let p = |c: &str| PathBuf::from(format!("/sys/class/leds/micro:{c}:ring{z}/brightness"));
                Zone { r: p("red"), g: p("green"), b: p("blue") }
            })
            .filter(|z| crate::fsx::exists(&z.r) && crate::fsx::exists(&z.g) && crate::fsx::exists(&z.b))
            .collect();
        (!zones.is_empty()).then_some(Ring(zones))
    }

    fn is(&self, (r, g, b): (u8, u8, u8)) -> bool {
        self.0.iter().all(|z| {
            brightness(&z.r) == Some(r) && brightness(&z.g) == Some(g) && brightness(&z.b) == Some(b)
        })
    }

    fn set(&self, (r, g, b): (u8, u8, u8)) {
        for z in &self.0 {
            write(&z.r, r);
            write(&z.g, g);
            write(&z.b, b);
        }
    }
}

pub fn run(shared: Arc<Shared>) {
    let Some(ring) = Ring::detect() else {
        eprintln!("led: no ring found, light disabled");
        return;
    };
    eprintln!("led: ring found ({} zones)", ring.0.len());
    // Left lit by a previous run that was stopped or crashed.
    if ring.is(ORANGE) {
        ring.set((0, 0, 0));
    }
    thread::spawn(move || {
        // The colour we last wrote; if the ring shows anything else, Alice has taken it.
        let mut shown: Option<(u8, u8, u8)> = None;
        let mut idle_ms = 0u64;
        let mut clock = 0u64;
        // Position of the current track and how long it has not moved.
        let mut last_pos = u64::MAX;
        let mut still_ms = 0u64;
        loop {
            thread::sleep(Duration::from_millis(TICK_MS));
            clock += TICK_MS;
            let alice_busy = shared.duck.active();
            idle_ms = if alice_busy { 0 } else { idle_ms.saturating_add(TICK_MS) };

            let track = shared.track.lock().unwrap().clone();
            let stuck = match &track {
                Some(f) if !f.paused.load(Ordering::Acquire) => {
                    let pos = f.position_ms.load(Ordering::Acquire);
                    still_ms = if pos == last_pos { still_ms.saturating_add(TICK_MS) } else { 0 };
                    last_pos = pos;
                    still_ms >= STUCK_MS
                }
                _ => {
                    last_pos = u64::MAX;
                    still_ms = 0;
                    false
                }
            };
            let loading = shared.fetching.load(Ordering::Acquire) || stuck;
            let want = if !shared.ring.load(Ordering::Acquire) {
                None
            } else if loading {
                // Breathing orange: the speaker is working on it.
                Some(orange(breath(clock)))
            } else if shared.playing.load(Ordering::Acquire) || shared.airplay.is_active() {
                Some(ORANGE)
            } else if track.as_ref().is_some_and(|f| f.paused.load(Ordering::Acquire)) {
                // Paused: dim orange, so it is clear SoundCloud is still there.
                Some(orange(PAUSED))
            } else {
                None
            };
            match want {
                Some(colour) if idle_ms >= IDLE_MS => {
                    if shown != Some(colour) || !ring.is(colour) {
                        ring.set(colour);
                    }
                    shown = Some(colour);
                }
                // Alice is animating: leave the ring to her.
                Some(_) => shown = None,
                None => {
                    // Only switch off our own colour, never one of Alice's animations.
                    if let Some(c) = shown {
                        if !alice_busy && ring.is(c) {
                            ring.set((0, 0, 0));
                        }
                    }
                    shown = None;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breath_fades_smoothly() {
        assert_eq!(orange(breath(0)), ORANGE);
        let low = orange(breath(BREATH_MS as u64 / 2));
        assert!(low.0 <= 20 && low.0 > 0 && low.2 == 0, "{low:?}");
        // No jumps between neighbouring frames.
        for t in (0..3200).step_by(TICK_MS as usize) {
            let (a, b) = (orange(breath(t)).0 as i32, orange(breath(t + TICK_MS)).0 as i32);
            assert!((a - b).abs() <= 40, "{t}: {a} -> {b}");
        }
    }
}
