//! Orange ring while SoundCloud plays (Station Lite: three RGB zones in sysfs).
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
        let mut lit = false;
        let mut idle_since = 0u32;
        loop {
            thread::sleep(Duration::from_millis(500));
            let alice_busy = shared.duck.active();
            idle_since = if alice_busy { 0 } else { idle_since.saturating_add(1) };
            let want = shared.ring.load(Ordering::Acquire) && shared.playing.load(Ordering::Acquire);
            if want {
                // Give Alice's own animation a second to finish before taking the ring back.
                if idle_since >= 2 && !ring.is(ORANGE) {
                    ring.set(ORANGE);
                }
                lit = true;
            } else if lit {
                // Only switch off our own colour, never one of Alice's animations.
                if !alice_busy && ring.is(ORANGE) {
                    ring.set((0, 0, 0));
                }
                lit = false;
            }
        }
    });
}
