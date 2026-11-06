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

/// The ring: its LEDs in sysfs (Lite, Mini 2), the LED chip of the Station
/// Mini 1 on I2C, or — when soundya runs as a Luna package — Luna's LED API.
enum Ring {
    Sysfs(Vec<Zone>),
    Mini1(mini1::Chip),
    Mini3(mini3::Fb),
    /// Six logical zones; Luna maps them onto whatever ring the Station has.
    Luna,
}

impl Ring {
    fn detect() -> Option<Self> {
        // Luna drives the LEDs on this speaker: draw through it, not past it.
        if crate::luna::present() && crate::luna::has_led() {
            return Some(Ring::Luna);
        }
        // Station Lite: micro:red:ring1..3; Mini 2: mini2:red:ring1..6.
        let sysfs = ["micro", "mini2"].into_iter().find_map(|board| {
            let zones: Vec<Zone> = (1..=6)
                .map(|z| {
                    let p = |c: &str| PathBuf::from(format!("/sys/class/leds/{board}:{c}:ring{z}/brightness"));
                    Zone { r: p("red"), g: p("green"), b: p("blue") }
                })
                .filter(|z| crate::fsx::exists(&z.r) && crate::fsx::exists(&z.g) && crate::fsx::exists(&z.b))
                .collect();
            (!zones.is_empty()).then_some(Ring::Sysfs(zones))
        });
        sysfs
            .or_else(|| mini3::Fb::open().map(Ring::Mini3))
            .or_else(|| mini1::Chip::open().map(Ring::Mini1))
    }

    fn zones(&self) -> usize {
        match self {
            Ring::Sysfs(z) => z.len(),
            Ring::Mini1(_) => mini1::LEDS,
            Ring::Mini3(_) => mini3::LEDS,
            Ring::Luna => 6,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Ring::Sysfs(_) => "",
            Ring::Mini1(_) => ", Station Mini 1 LED chip",
            Ring::Mini3(_) => ", Station Mini 3 LED framebuffer",
            Ring::Luna => ", through Luna",
        }
    }

    fn is(&self, (r, g, b): (u8, u8, u8)) -> bool {
        match self {
            Ring::Sysfs(zones) => zones.iter().all(|z| {
                brightness(&z.r) == Some(r) && brightness(&z.g) == Some(g) && brightness(&z.b) == Some(b)
            }),
            // Neither can be read back.
            _ => false,
        }
    }

    /// The ring shows exactly `frame` (one colour per zone)?
    fn shows(&self, frame: &[Rgb]) -> bool {
        match self {
            Ring::Sysfs(zones) => zones.iter().zip(frame.iter().cycle()).all(|(z, &(r, g, b))| {
                brightness(&z.r) == Some(r) && brightness(&z.g) == Some(g) && brightness(&z.b) == Some(b)
            }),
            // Write-only: what we drew last is assumed to be shown (after
            // Alice's animations the loop draws again anyway).
            _ => true,
        }
    }

    /// Writes only what changes.
    fn draw(&self, frame: &[Rgb], before: Option<&[Rgb]>) {
        match self {
            Ring::Sysfs(zones) => {
                for (i, (z, &(r, g, b))) in zones.iter().zip(frame.iter().cycle()).enumerate() {
                    if before.and_then(|p| p.get(i)) == Some(&(r, g, b)) {
                        continue;
                    }
                    write(&z.r, r);
                    write(&z.g, g);
                    write(&z.b, b);
                }
            }
            Ring::Mini1(chip) => {
                if before != Some(frame) {
                    if let Err(e) = chip.show(frame) {
                        eprintln!("led: mini 1: {e}");
                    }
                }
            }
            Ring::Mini3(fb) => {
                if before != Some(frame) {
                    if let Err(e) = fb.show(frame) {
                        eprintln!("led: mini 3: {e}");
                    }
                }
            }
            Ring::Luna => {
                if before != Some(frame) {
                    if let Err(e) = crate::luna::ring(frame) {
                        eprintln!("led: luna: {e}");
                    }
                }
            }
        }
    }

    fn set(&self, c: (u8, u8, u8)) {
        match self {
            Ring::Sysfs(zones) => {
                for z in zones {
                    write(&z.r, c.0);
                    write(&z.g, c.1);
                    write(&z.b, c.2);
                }
            }
            Ring::Mini1(chip) => {
                let _ = chip.show(&[c]);
            }
            Ring::Mini3(fb) => {
                let _ = fb.show(&[c]);
            }
            Ring::Luna => {
                let _ = if c == (0, 0, 0) { crate::luna::ring_off() } else { crate::luna::ring(&[c]) };
            }
        }
    }
}

/// The Station Mini 3 shows its six-zone ring as a 6×1 framebuffer, 24 bits
/// per pixel in B, G, R order; the zones are stored as alternating opposite
/// LEDs, and the chip shows a frame one write late, so each frame is written
/// twice. As in Luna's station crate for this model (stations/yandexmini3, MIT).
mod mini3 {
    use std::{fs::File, os::unix::fs::FileExt, path::Path};

    pub const LEDS: usize = 6;
    /// Framebuffer slot → ring position.
    const ORDER: [usize; LEDS] = [0, 3, 1, 4, 2, 5];

    pub struct Fb(File);

    impl Fb {
        pub fn open() -> Option<Fb> {
            let sys = Path::new("/sys/class/graphics/fb0");
            let size = crate::fsx::read_string(&sys.join("virtual_size"))?;
            let depth = crate::fsx::read_string(&sys.join("bits_per_pixel"))?;
            if size.trim() != "6,1" || depth.trim() != "24" {
                return None;
            }
            File::options().write(true).open("/dev/fb0").ok().map(Fb)
        }

        pub fn show(&self, frame: &[super::Rgb]) -> std::io::Result<()> {
            let data = pixels(frame);
            self.0.write_all_at(&data, 0)?;
            self.0.write_all_at(&data, 0)
        }
    }

    pub fn pixels(frame: &[super::Rgb]) -> [u8; LEDS * 3] {
        let mut out = [0u8; LEDS * 3];
        if frame.is_empty() {
            return out;
        }
        for (slot, &pos) in ORDER.iter().enumerate() {
            let (r, g, b) = frame[pos * frame.len() / LEDS];
            out[slot * 3..slot * 3 + 3].copy_from_slice(&[b, g, r]);
        }
        out
    }
}

/// The Station Mini 1 (YNDX-0004) has no LEDs in sysfs: its six RGB LEDs sit
/// on a PWM chip at I2C bus 0, address 0x3c, colours in B, G, R order.
/// Register layout as in Luna's station crate for this model
/// (github.com/nyakokitsu/luna, stations/yandexmini, MIT).
mod mini1 {
    use std::{fs::File, os::fd::AsRawFd};

    pub const LEDS: usize = 6;
    const DEVICE: &str = "/dev/i2c-0";
    const ADDRESS: u16 = 0x3c;
    const REG_ENABLE: u8 = 0x00;
    const REG_PWM: u8 = 0x01;
    const REG_UPDATE: u8 = 0x25;
    const REG_MODE: u8 = 0x26;
    const REG_FREQ: u8 = 0x4b;
    const I2C_RDWR: std::os::raw::c_ulong = 0x0707;

    #[repr(C)]
    struct Message {
        address: u16,
        flags: u16,
        length: u16,
        buffer: *mut u8,
    }

    #[repr(C)]
    struct Request {
        messages: *mut Message,
        count: u32,
    }

    extern "C" {
        fn ioctl(fd: i32, request: std::os::raw::c_ulong, ...) -> i32;
    }

    pub struct Chip(File);

    /// Only on a Station Mini 1 (with the bus): «yandexmini» as the device type
    /// in Alice's config, or YNDX-0004 / yandexmini in the device tree.
    fn is_mini1() -> bool {
        if !crate::fsx::exists(std::path::Path::new(DEVICE)) {
            return false;
        }
        let cfg = crate::fsx::read_string(std::path::Path::new("/system/vendor/quasar/quasar.cfg")).unwrap_or_default();
        let in_cfg = serde_json::from_str::<serde_json::Value>(&cfg).ok().is_some_and(|v| {
            let t = v.get("common").and_then(|c| c.get("deviceType")).or_else(|| v.get("deviceType"));
            t.and_then(|t| t.as_str()) == Some("yandexmini")
        }) || cfg.contains("\"yandexmini\"");
        let tree = ["/proc/device-tree/model", "/proc/device-tree/compatible"]
            .iter()
            .filter_map(|p| crate::fsx::read_string(std::path::Path::new(p)))
            .collect::<String>()
            .to_lowercase()
            .replace('\0', " ");
        let in_tree = tree.contains("yndx-0004")
            || tree.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == "yandexmini" || w == "yandexstationmini");
        in_cfg || in_tree
    }

    impl Chip {
        pub fn open() -> Option<Chip> {
            if !is_mini1() {
                return None;
            }
            let file = File::options().read(true).write(true).open(DEVICE).ok()?;
            let chip = Chip(file);
            let ok = chip.write(REG_ENABLE, &[0x01]).is_ok()
                && chip.write(REG_MODE, &[0x03; LEDS * 3]).is_ok()
                && chip.write(REG_FREQ, &[0x01]).is_ok();
            if !ok {
                eprintln!("led: mini 1: the LED chip does not answer");
                return None;
            }
            Some(chip)
        }

        fn write(&self, register: u8, data: &[u8]) -> std::io::Result<()> {
            let mut bytes = Vec::with_capacity(data.len() + 1);
            bytes.push(register);
            bytes.extend_from_slice(data);
            let mut message = Message { address: ADDRESS, flags: 0, length: bytes.len() as u16, buffer: bytes.as_mut_ptr() };
            let mut request = Request { messages: &mut message, count: 1 };
            if unsafe { ioctl(self.0.as_raw_fd(), I2C_RDWR, &mut request) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        /// One colour per LED (repeated if fewer are given).
        pub fn show(&self, frame: &[super::Rgb]) -> std::io::Result<()> {
            self.write(REG_PWM, &pwm(frame))?;
            self.write(REG_UPDATE, &[0xaa])
        }
    }

    /// The chip's 18 PWM bytes: B, G, R for each of the six LEDs.
    pub fn pwm(frame: &[super::Rgb]) -> [u8; LEDS * 3] {
        let mut out = [0u8; LEDS * 3];
        if frame.is_empty() {
            return out;
        }
        for i in 0..LEDS {
            let (r, g, b) = frame[i * frame.len() / LEDS];
            out[i * 3..i * 3 + 3].copy_from_slice(&[b, g, r]);
        }
        out
    }
}

pub fn run(shared: Arc<Shared>) {
    let Some(ring) = Ring::detect() else {
        eprintln!("led: no ring found, light disabled");
        return;
    };
    eprintln!("led: ring found ({} zones{})", ring.zones(), ring.name());
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
        let zones = ring.zones();
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
    fn mini3_frame_order() {
        let f: Vec<Rgb> = (0..6).map(|i| (i as u8, 0, 0)).collect();
        let p = mini3::pixels(&f);
        let reds: Vec<u8> = p.chunks(3).map(|c| c[2]).collect();
        assert_eq!(reds, vec![0, 3, 1, 4, 2, 5]);
    }

    #[test]
    fn mini1_frame_is_bgr() {
        let p = mini1::pwm(&[(10, 20, 30)]);
        assert_eq!(&p[..6], &[30, 20, 10, 30, 20, 10]);
        let p = mini1::pwm(&[(1, 0, 0), (0, 1, 0), (0, 0, 1)]);
        assert_eq!(&p[..], &[0, 0, 1, 0, 0, 1, 0, 1, 0, 0, 1, 0, 1, 0, 0, 1, 0, 0]);
    }

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
