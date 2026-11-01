//! The ring while music plays (SoundCloud or AirPlay): a steady colour, a
//! pulse with the beat, or an equaliser across the ring's zones («Подсветка»);
//! dim on pause, breathing while something loads.
//! (Station Lite: three RGB zones in sysfs, Mini 2: six).
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
#[cfg(test)]
fn orange(level: f32) -> (u8, u8, u8) {
    scale(ORANGE, level)
}

type Rgb = (u8, u8, u8);

fn scale(c: Rgb, level: f32) -> Rgb {
    let l = level.clamp(0.0, 1.0);
    let f = |v: u8| (v as f32 * l).round() as u8;
    (f(c.0), f(c.1), f(c.2))
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// "#ff7a00" as the LEDs should show it: LEDs are linear and much brighter in
/// green, so the colour is gamma-corrected (2.2) the way a screen would.
pub fn led_colour(hex: &str) -> Option<Rgb> {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    let g = |v: u8| ((v as f32 / 255.0).powf(2.2) * 255.0).round() as u8;
    Some((g(ch(0)?), g(ch(2)?), g(ch(4)?)))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Solid,
    Pulse,
    Eq,
}

/// «Подсветка» from the settings.
#[derive(Clone, Copy, Debug)]
pub struct Light {
    pub mode: Mode,
    pub quiet: Rgb,
    pub loud: Rgb,
    /// Equaliser: bass, middle, treble.
    pub bands: [Rgb; 3],
    pub bright: f32,
}

impl Light {
    pub fn from_settings(s: &crate::settings::Settings) -> Light {
        Light {
            mode: match s.light_mode.as_str() {
                "pulse" => Mode::Pulse,
                "eq" => Mode::Eq,
                _ => Mode::Solid,
            },
            quiet: led_colour(&s.light_color).unwrap_or(ORANGE),
            loud: led_colour(&s.light_color2).unwrap_or((255, 160, 0)),
            bands: [
                led_colour(&s.light_bass).unwrap_or((255, 12, 0)),
                led_colour(&s.light_mid).unwrap_or((255, 80, 0)),
                led_colour(&s.light_high).unwrap_or((255, 190, 18)),
            ],
            bright: (s.light_bright.clamp(10, 100) as f32) / 100.0,
        }
    }
}

/// One frame of the music light: a colour per zone. `smooth` holds the levels
/// shown last (fast up, slow down, so the light does not flicker).
fn music_frame(light: &Light, zones: usize, heard: Option<[f32; 3]>, smooth: &mut [f32; 3]) -> Vec<Rgb> {
    let target = heard.unwrap_or([0.0; 3]);
    for i in 0..3 {
        smooth[i] = if target[i] > smooth[i] { target[i] } else { smooth[i] * 0.82 + target[i] * 0.18 };
    }
    let base = |level: f32| scale(mix(light.quiet, light.loud, level), light.bright * (0.12 + 0.88 * level));
    match light.mode {
        Mode::Solid => vec![scale(light.quiet, light.bright); zones],
        Mode::Pulse => {
            let level = (0.6 * smooth[0] + 0.3 * smooth[1] + 0.1 * smooth[2]).powf(1.4);
            vec![base(level); zones]
        }
        Mode::Eq => (0..zones)
            .map(|z| {
                // Bass, middle, treble around the ring; on six zones mirrored.
                let band = match zones {
                    0..=3 => z.min(2),
                    _ => [0, 1, 2, 2, 1, 0][z % 6],
                };
                let level = smooth[band].powf(1.3);
                scale(light.bands[band], light.bright * (0.1 + 0.9 * level))
            })
            .collect(),
    }
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
        // Station Lite: micro:red:ring1..3; Mini 2: mini2:red:ring1..6.
        ["micro", "mini2"].into_iter().find_map(|board| {
            let zones: Vec<Zone> = (1..=6)
                .map(|z| {
                    let p = |c: &str| PathBuf::from(format!("/sys/class/leds/{board}:{c}:ring{z}/brightness"));
                    Zone { r: p("red"), g: p("green"), b: p("blue") }
                })
                .filter(|z| crate::fsx::exists(&z.r) && crate::fsx::exists(&z.g) && crate::fsx::exists(&z.b))
                .collect();
            (!zones.is_empty()).then_some(Ring(zones))
        })
    }

    fn is(&self, (r, g, b): (u8, u8, u8)) -> bool {
        self.0.iter().all(|z| {
            brightness(&z.r) == Some(r) && brightness(&z.g) == Some(g) && brightness(&z.b) == Some(b)
        })
    }

    /// The ring shows exactly `frame` (one colour per zone)?
    fn shows(&self, frame: &[Rgb]) -> bool {
        self.0.iter().zip(frame.iter().cycle()).all(|(z, &(r, g, b))| {
            brightness(&z.r) == Some(r) && brightness(&z.g) == Some(g) && brightness(&z.b) == Some(b)
        })
    }

    /// Writes only the zones that change.
    fn draw(&self, frame: &[Rgb], before: Option<&[Rgb]>) {
        for (i, (z, &(r, g, b))) in self.0.iter().zip(frame.iter().cycle()).enumerate() {
            if before.and_then(|p| p.get(i)) == Some(&(r, g, b)) {
                continue;
            }
            write(&z.r, r);
            write(&z.g, g);
            write(&z.b, b);
        }
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
    let light = *shared.light.lock().unwrap();
    if ring.is(ORANGE) || ring.is(scale(light.quiet, light.bright)) {
        ring.set((0, 0, 0));
    }
    thread::spawn(move || {
        // The frame we last wrote; if the ring shows anything else, Alice has taken it.
        let mut shown: Option<Vec<Rgb>> = None;
        let mut idle_ms = 0u64;
        let mut clock = 0u64;
        let mut smooth = [0f32; 3];
        let zones = ring.0.len();
        // Position of the current track and how long it has not moved.
        let mut last_pos = u64::MAX;
        let mut still_ms = 0u64;
        loop {
            let light = *shared.light.lock().unwrap();
            let on = shared.ring.load(Ordering::Acquire);
            let music = shared.playing.load(Ordering::Acquire) || shared.airplay.is_active();
            // Sound analysis only when the light moves with it.
            crate::vis::ON.store(on && light.mode != Mode::Solid, Ordering::Relaxed);
            // Smoother while it moves with the music.
            let tick = if on && music && light.mode != Mode::Solid { 40 } else { TICK_MS };
            thread::sleep(Duration::from_millis(tick));
            clock += tick;
            let alice_busy = shared.duck.active();
            idle_ms = if alice_busy { 0 } else { idle_ms.saturating_add(tick) };

            let track = shared.track.lock().unwrap().clone();
            let stuck = match &track {
                Some(f) if !f.paused.load(Ordering::Acquire) => {
                    let pos = f.position_ms.load(Ordering::Acquire);
                    still_ms = if pos == last_pos { still_ms.saturating_add(tick) } else { 0 };
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
            let want: Option<Vec<Rgb>> = if !on {
                None
            } else if loading {
                // Breathing: the speaker is working on it.
                Some(vec![scale(light.quiet, light.bright * breath(clock)); zones])
            } else if music {
                Some(music_frame(&light, zones, crate::vis::now(), &mut smooth))
            } else if track.as_ref().is_some_and(|f| f.paused.load(Ordering::Acquire)) {
                // Paused: dim, so it is clear SoundCloud is still there.
                Some(vec![scale(light.quiet, light.bright * PAUSED); zones])
            } else {
                None
            };
            match want {
                Some(frame) if idle_ms >= IDLE_MS => {
                    let ours = shown.as_deref().is_some_and(|p| ring.shows(p));
                    if !ours || shown.as_deref() != Some(&frame[..]) {
                        ring.draw(&frame, if ours { shown.as_deref() } else { None });
                    }
                    shown = Some(frame);
                }
                // Alice is animating: leave the ring to her.
                Some(_) => shown = None,
                None => {
                    // Only switch off our own light, never one of Alice's animations.
                    if let Some(p) = shown.as_deref() {
                        if !alice_busy && ring.shows(p) {
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
    fn colours_and_frames() {
        assert_eq!(led_colour("#ffffff"), Some((255, 255, 255)));
        assert_eq!(led_colour("ff7a00"), Some((255, 50, 0)));
        assert_eq!(led_colour("#xyz"), None);
        let light = Light {
            mode: Mode::Eq,
            quiet: (255, 0, 0),
            loud: (0, 0, 255),
            bands: [(0, 0, 255), (0, 255, 0), (255, 0, 0)],
            bright: 1.0,
        };
        let mut smooth = [0.0; 3];
        let f = music_frame(&light, 3, Some([1.0, 0.0, 0.0]), &mut smooth);
        assert_eq!(f[0], (0, 0, 255), "loud bass: full bass colour");
        assert!(f[2].0 < 40 && f[2].1 == 0 && f[2].2 == 0, "quiet treble: dim treble colour {:?}", f[2]);
        let f6 = music_frame(&light, 6, Some([1.0, 0.0, 0.0]), &mut [0.0; 3]);
        assert_eq!(f6[0], f6[5]);
        // Falls slowly, not at once.
        let after = music_frame(&light, 3, Some([0.0; 3]), &mut smooth);
        assert!(after[0].2 > 150, "{:?}", after[0]);
        let pulse = Light { mode: Mode::Pulse, ..light };
        assert_eq!(music_frame(&pulse, 3, Some([1.0; 3]), &mut [0.0; 3])[0], (0, 0, 255), "pulse: loud colour");
        let solid = Light { mode: Mode::Solid, bright: 0.5, ..light };
        assert_eq!(music_frame(&solid, 3, None, &mut [0.0; 3]), vec![(128, 0, 0); 3]);
    }

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
