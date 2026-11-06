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
    /// A wave of the two colours going round the ring; brighter with the music.
    Wave,
    /// A turning rainbow; brighter with the music.
    Rainbow,
    /// Slow breathing in the «quiet» colour.
    Breathe,
    /// A comet running round the ring.
    Running,
}

pub const MODES: [&str; 7] = ["solid", "pulse", "eq", "wave", "rainbow", "breathe", "running"];

/// «Подсветка» from the settings.
#[derive(Clone, Copy, Debug)]
pub struct Light {
    pub mode: Mode,
    pub quiet: Rgb,
    pub loud: Rgb,
    /// Equaliser: bass, middle, treble.
    pub bands: [Rgb; 3],
    pub bright: f32,
    /// Animation speed, 1.0 = normal.
    pub speed: f32,
    /// How strongly the light follows the music, 1.0 = normal.
    pub intensity: f32,
}

impl Light {
    pub fn from_settings(s: &crate::settings::Settings) -> Light {
        Light {
            mode: match s.light_mode.as_str() {
                "pulse" => Mode::Pulse,
                "eq" => Mode::Eq,
                "wave" => Mode::Wave,
                "rainbow" => Mode::Rainbow,
                "breathe" => Mode::Breathe,
                "running" => Mode::Running,
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
            speed: f32::from(s.light_speed.clamp(25, 300)) / 100.0,
            intensity: f32::from(s.light_intensity.clamp(30, 200)) / 100.0,
        }
    }
}

/// One frame of the music light: a colour per zone. `smooth` holds the levels
/// shown last (fast up, slow down, so the light does not flicker).
fn music_frame(light: &Light, zones: usize, heard: Option<[f32; 3]>, smooth: &mut [f32; 3], ms: u64) -> Vec<Rgb> {
    let k = light.intensity;
    let target = heard.map(|h| h.map(|v| (v * k).min(1.0))).unwrap_or([0.0; 3]);
    let z = zones.max(1);
    // Time at the chosen speed, in seconds.
    let t = ms as f32 / 1000.0 * light.speed;
    let loudness = |s: &[f32; 3]| (0.6 * s[0] + 0.3 * s[1] + 0.1 * s[2]).min(1.0);
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
        Mode::Wave => {
            let lift = 0.55 + 0.45 * loudness(smooth);
            (0..z)
                .map(|i| {
                    let phase = (i as f32 / z as f32 - t * 0.35) * std::f32::consts::TAU;
                    let m = 0.5 + 0.5 * phase.sin();
                    scale(mix(light.quiet, light.loud, m), light.bright * lift * (0.35 + 0.65 * m))
                })
                .collect()
        }
        Mode::Rainbow => {
            let lift = 0.5 + 0.5 * loudness(smooth);
            (0..z).map(|i| scale(hue(i as f32 / z as f32 + t * 0.12), light.bright * lift)).collect()
        }
        Mode::Breathe => {
            let b = 0.5 + 0.5 * (t * std::f32::consts::TAU / 4.0).cos();
            vec![scale(light.quiet, light.bright * (0.12 + 0.88 * b)); z]
        }
        Mode::Running => {
            let head = (t * 1.6 * z as f32) % z as f32;
            (0..z)
                .map(|i| {
                    let behind = (head - i as f32).rem_euclid(z as f32);
                    let level = if behind < 1.0 { 1.0 } else { (1.0 - (behind - 1.0) / 2.5).max(0.05) };
                    scale(mix(light.quiet, light.loud, level), light.bright * level)
                })
                .collect()
        }
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
    /// Each LED's `max_brightness` (0.4.40 assumed 255 everywhere and wrote
    /// 0..255: on a driver with a smaller range the kernel clamps, so orange
    /// (255, 50, 0) and red (255, 12, 0) both come out wrong).
    max: [u32; 3],
}

/// Channel orders: which colour the files red, green, blue actually light.
const ORDERS: [(&str, [usize; 3]); 6] =
    [("rgb", [0, 1, 2]), ("grb", [1, 0, 2]), ("brg", [2, 0, 1]), ("bgr", [2, 1, 0]), ("rbg", [0, 2, 1]), ("gbr", [1, 2, 0])];
static ORDER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Sets the order from the settings ("rgb" … "gbr"); false if unknown.
pub fn set_order(name: &str) -> bool {
    match ORDERS.iter().position(|(n, _)| *n == name) {
        Some(i) => {
            ORDER.store(i, Ordering::Release);
            true
        }
        None => false,
    }
}

/// The ring test on the page: light one channel file (0 red, 1 green, 2 blue)
/// of every zone as it is, without the order, for a few seconds.
static TEST: std::sync::Mutex<Option<(usize, std::time::Instant)>> = std::sync::Mutex::new(None);
const TEST_FOR: Duration = Duration::from_secs(6);

pub fn test(channel: usize) -> Result<String, String> {
    if channel > 2 {
        return Err("канал 0, 1 или 2".into());
    }
    *TEST.lock().unwrap() = Some((channel, std::time::Instant::now()));
    Ok(match RING.lock().unwrap().as_str() {
        "" => "error: кольцо не найдено".into(),
        _ => "OK: горит 6 секунд".into(),
    })
}

/// First install (hello.rs): what the ring — and the clock display, if there
/// is one (display.rs) — shows. Each profile draws it its own way.
#[derive(Clone, Debug, PartialEq)]
pub enum Install {
    /// soundya is starting, looking for the network.
    Loading,
    /// The address was said; waiting for «Начать установку» in the panel.
    Waiting,
    /// Wizard step n of total.
    Step(u8, u8),
    /// Finished: a short farewell, then the ring is free again.
    Done,
}

static INSTALL: std::sync::Mutex<Option<(Install, std::time::Instant)>> = std::sync::Mutex::new(None);
const DONE_MS: u64 = 2600;
/// Nobody came to the panel: stop waiting on the ring (the voice command stays).
const WAIT_MAX: Duration = Duration::from_secs(30 * 60);

pub fn set_install(phase: Option<Install>) {
    *INSTALL.lock().unwrap() = phase.map(|p| (p, std::time::Instant::now()));
}

/// The phase now; Done and Waiting end by themselves.
pub fn install_phase() -> Option<Install> {
    let mut g = INSTALL.lock().unwrap();
    let expired = match g.as_ref() {
        Some((Install::Done, at)) => at.elapsed() > Duration::from_millis(DONE_MS),
        Some((Install::Waiting, at)) => at.elapsed() > WAIT_MAX,
        _ => false,
    };
    if expired {
        *g = None;
    }
    g.as_ref().map(|(p, _)| p.clone())
}

fn install_ms() -> u64 {
    INSTALL.lock().unwrap().as_ref().map_or(0, |(_, at)| at.elapsed().as_millis() as u64)
}

/// Hue 0..1 → colour (for the farewell on six zones).
fn hue(h: f32) -> Rgb {
    let h = (h.rem_euclid(1.0)) * 6.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// One frame of an install phase, `ms` into it, for a ring of `zones` in colour `c`.
/// Three zones (Station Lite): zones light in turn, breathe, fill, flash.
/// Six (Mini 1/2/3, Luna): a comet goes round, fills up, a rainbow sweeps.
pub fn install_frame(phase: &Install, ms: u64, zones: usize, c: Rgb) -> Vec<Rgb> {
    let z = zones.max(1);
    let small = z <= 3;
    match phase {
        Install::Loading if small => (0..z)
            .map(|i| {
                let t = ((ms as f32 / 300.0) - i as f32).rem_euclid(z as f32);
                scale(c, (1.0 - t).max(0.08))
            })
            .collect(),
        Install::Loading => {
            let head = (ms / 110) as usize % z;
            (0..z).map(|i| scale(c, (1.0 - ((head + z - i) % z) as f32 / 3.0).max(0.04))).collect()
        }
        Install::Waiting if small => vec![scale(c, 0.6 * breath(ms)); z],
        Install::Waiting => {
            let head = (ms / 400) as usize % z;
            (0..z).map(|i| scale(c, if i == head { 0.5 } else { 0.06 })).collect()
        }
        Install::Step(n, total) => {
            let lit = ((*n as f32 / (*total).max(1) as f32) * z as f32).ceil() as usize;
            (0..z).map(|i| scale(c, if i < lit { 1.0 } else { 0.06 })).collect()
        }
        Install::Done => {
            let fade = 1.0 - (ms.saturating_sub(DONE_MS - 900) as f32 / 900.0).min(1.0);
            if small {
                let flash = ms < 600 && (ms / 150) % 2 == 0;
                vec![scale(if flash { (255, 255, 255) } else { c }, fade); z]
            } else {
                (0..z).map(|i| scale(hue(i as f32 / z as f32 + ms as f32 / 1500.0), fade)).collect()
            }
        }
    }
}

/// What was found, for the page and bug reports.
static RING: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn info() -> serde_json::Value {
    serde_json::json!({
        "ring": *RING.lock().unwrap(),
        "order": ORDERS[ORDER.load(Ordering::Acquire)].0,
    })
}

impl Zone {
    fn open(r: PathBuf, g: PathBuf, b: PathBuf) -> Zone {
        let max = |p: &Path| {
            crate::fsx::read_string(&p.with_file_name("max_brightness"))
                .and_then(|t| t.trim().parse::<u32>().ok())
                .filter(|m| *m > 0)
                .unwrap_or(255)
        };
        let m = [max(&r), max(&g), max(&b)];
        Zone { r, g, b, max: m }
    }

    /// What the three files get for colour `c`: in the channel order, scaled to their range.
    fn values(&self, c: Rgb) -> [u32; 3] {
        let comp = [c.0, c.1, c.2];
        let order = ORDERS[ORDER.load(Ordering::Acquire)].1;
        raw(self.max, [comp[order[0]], comp[order[1]], comp[order[2]]])
    }

    fn paths(&self) -> [&Path; 3] {
        [&self.r, &self.g, &self.b]
    }

    fn put(&self, c: Rgb) {
        for (p, v) in self.paths().into_iter().zip(self.values(c)) {
            write(p, v);
        }
    }

    fn shows(&self, c: Rgb) -> bool {
        self.paths().into_iter().zip(self.values(c)).all(|(p, v)| brightness(p) == Some(v))
    }
}

/// 0..255 per file → 0..max_brightness.
fn raw(max: [u32; 3], v: [u8; 3]) -> [u32; 3] {
    let f = |i: usize| (u32::from(v[i]) * max[i] + 127) / 255;
    [f(0), f(1), f(2)]
}

fn brightness(path: &Path) -> Option<u32> {
    crate::fsx::read_string(path)?.trim().parse().ok()
}

fn write(path: &Path, value: u32) {
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
            // The Mini 2 wires its red and blue sysfs nodes the opposite way
            // round, so orange came out blue; route the colours to match.
            let (rc, bc) = if board == "mini2" { ("blue", "red") } else { ("red", "blue") };
            let zones: Vec<Zone> = (1..=6)
                .map(|z| {
                    let p = |c: &str| PathBuf::from(format!("/sys/class/leds/{board}:{c}:ring{z}/brightness"));
                    Zone::open(p(rc), p("green"), p(bc))
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
            Ring::Sysfs(zones) => zones.iter().all(|z| z.shows((r, g, b))),
            // Neither can be read back.
            _ => false,
        }
    }

    /// The ring shows exactly `frame` (one colour per zone)?
    fn shows(&self, frame: &[Rgb]) -> bool {
        match self {
            Ring::Sysfs(zones) => zones.iter().zip(frame.iter().cycle()).all(|(z, &c)| z.shows(c)),
            // Write-only: what we drew last is assumed to be shown (after
            // Alice's animations the loop draws again anyway).
            _ => true,
        }
    }

    /// Writes only what changes.
    fn draw(&self, frame: &[Rgb], before: Option<&[Rgb]>) {
        match self {
            Ring::Sysfs(zones) => {
                for (i, (z, &c)) in zones.iter().zip(frame.iter().cycle()).enumerate() {
                    if before.and_then(|p| p.get(i)) == Some(&c) {
                        continue;
                    }
                    z.put(c);
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
                    z.put(c);
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
    let maxes = match &ring {
        Ring::Sysfs(z) => z.first().map(|z| format!(", max_brightness {:?}", z.max)).unwrap_or_default(),
        _ => String::new(),
    };
    eprintln!("led: ring found ({} zones{}{maxes})", ring.zones(), ring.name());
    *RING.lock().unwrap() = format!("{} zones{}{maxes}", ring.zones(), ring.name());
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
            // The first install (hello.rs): over the music light, never over Alice.
            if let Some(phase) = install_phase() {
                if !shared.duck.active() {
                    ring.draw(&install_frame(&phase, install_ms(), zones, scale(light.quiet, light.bright)), None);
                    shown = None;
                    continue;
                }
            }
            // The ring test from the page: one raw channel file, ignoring the order.
            let test = TEST.lock().unwrap().filter(|(_, at)| at.elapsed() < TEST_FOR).map(|(c, _)| c);
            if let (Some(ch), Ring::Sysfs(zones)) = (test, &ring) {
                for z in zones {
                    let mut v = [0u8; 3];
                    v[ch] = 255;
                    for (p, x) in z.paths().into_iter().zip(raw(z.max, v)) {
                        write(p, x);
                    }
                }
                shown = None;
                continue;
            }
            if let (Some(ch), false) = (test, matches!(ring, Ring::Sysfs(_))) {
                // Other rings take colours, not files: show the pure colour.
                let mut c = (0, 0, 0);
                match ch { 0 => c.0 = 255, 1 => c.1 = 255, _ => c.2 = 255 }
                ring.set(c);
                shown = None;
                continue;
            }
            if TEST.lock().unwrap().take_if(|(_, at)| at.elapsed() >= TEST_FOR).is_some() {
                ring.set((0, 0, 0));
            }
            let loading = shared.fetching.load(Ordering::Acquire) || stuck;
            let want: Option<Vec<Rgb>> = if !on {
                None
            } else if loading {
                // Breathing: the speaker is working on it.
                Some(vec![scale(light.quiet, light.bright * breath(clock)); zones])
            } else if music {
                Some(music_frame(&light, zones, crate::vis::now(), &mut smooth, clock))
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
    fn scales_to_the_drivers_range_and_order() {
        // A driver with 0..31 (not 0..255): orange keeps its proportions.
        assert_eq!(raw([31, 31, 31], [255, 50, 0]), [31, 6, 0]);
        assert_eq!(raw([255, 255, 255], [255, 50, 0]), [255, 50, 0], "0.4.40 behaviour on 255");
        let z = Zone { r: "r".into(), g: "g".into(), b: "b".into(), max: [255; 3] };
        assert!(set_order("grb"));
        assert_eq!(z.values((255, 50, 0)), [50, 255, 0], "the red file lights green");
        assert!(set_order("bgr"));
        assert_eq!(z.values((1, 2, 3)), [3, 2, 1]);
        assert!(!set_order("xyz"));
        assert!(set_order("rgb"));
        assert_eq!(z.values((1, 2, 3)), [1, 2, 3]);
    }

    #[test]
    fn new_effects_move_and_follow_the_music() {
        let base = Light { mode: Mode::Wave, quiet: (255, 0, 0), loud: (0, 0, 255), bands: [(0, 0, 0); 3], bright: 1.0, speed: 1.0, intensity: 1.0 };
        for mode in [Mode::Wave, Mode::Rainbow, Mode::Breathe, Mode::Running] {
            let l = Light { mode, ..base };
            let a = music_frame(&l, 6, None, &mut [0.0; 3], 0);
            let b = music_frame(&l, 6, None, &mut [0.0; 3], 700);
            assert_eq!(a.len(), 6);
            assert_ne!(a, b, "{mode:?} moves");
            // Faster speed: the same picture sooner.
            let fast = Light { speed: 2.0, ..l };
            assert_eq!(music_frame(&fast, 6, None, &mut [0.0; 3], 350), b, "{mode:?} speed");
        }
        // Wave and rainbow get brighter with the music.
        let sum = |f: Vec<Rgb>| f.iter().map(|c| c.0 as u32 + c.1 as u32 + c.2 as u32).sum::<u32>();
        let r = Light { mode: Mode::Rainbow, ..base };
        assert!(sum(music_frame(&r, 6, Some([1.0; 3]), &mut [0.0; 3], 0)) > sum(music_frame(&r, 6, None, &mut [0.0; 3], 0)));
        // Intensity: a weak signal counts more.
        let weak = Light { mode: Mode::Pulse, intensity: 2.0, ..base };
        let normal = Light { mode: Mode::Pulse, ..base };
        assert!(sum(music_frame(&weak, 3, Some([0.3; 3]), &mut [0.0; 3], 0)) > sum(music_frame(&normal, 3, Some([0.3; 3]), &mut [0.0; 3], 0)));
    }

    #[test]
    fn install_animations_per_profile() {
        let c = (200, 100, 0);
        // Lite (3 zones): one zone leads, the others are dim.
        let f = install_frame(&Install::Loading, 0, 3, c);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0], c);
        assert!(f[1].0 < 100 && f[2].0 < 100, "{f:?}");
        // Six zones: the comet moves.
        assert_ne!(install_frame(&Install::Loading, 0, 6, c), install_frame(&Install::Loading, 120, 6, c));
        // Steps fill the ring: 2 of 4 → half lit.
        let f = install_frame(&Install::Step(2, 4), 0, 6, c);
        assert_eq!(f.iter().filter(|x| x.0 == 200).count(), 3);
        // The farewell fades to dark; six zones get colours, not only the accent.
        assert!(install_frame(&Install::Done, DONE_MS, 6, c).iter().all(|x| *x == (0, 0, 0)));
        let rainbow = install_frame(&Install::Done, 0, 6, c);
        assert!(rainbow.iter().any(|x| x.2 > 100), "{rainbow:?}");
        assert_eq!(install_frame(&Install::Done, 100, 3, c)[0], (255, 255, 255), "Lite flashes");
    }

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
            speed: 1.0,
            intensity: 1.0,
        };
        let mut smooth = [0.0; 3];
        let f = music_frame(&light, 3, Some([1.0, 0.0, 0.0]), &mut smooth, 0);
        assert_eq!(f[0], (0, 0, 255), "loud bass: full bass colour");
        assert!(f[2].0 < 40 && f[2].1 == 0 && f[2].2 == 0, "quiet treble: dim treble colour {:?}", f[2]);
        let f6 = music_frame(&light, 6, Some([1.0, 0.0, 0.0]), &mut [0.0; 3], 0);
        assert_eq!(f6[0], f6[5]);
        // Falls slowly, not at once.
        let after = music_frame(&light, 3, Some([0.0; 3]), &mut smooth, 0);
        assert!(after[0].2 > 150, "{:?}", after[0]);
        let pulse = Light { mode: Mode::Pulse, ..light };
        assert_eq!(music_frame(&pulse, 3, Some([1.0; 3]), &mut [0.0; 3], 0)[0], (0, 0, 255), "pulse: loud colour");
        let solid = Light { mode: Mode::Solid, bright: 0.5, ..light };
        assert_eq!(music_frame(&solid, 3, None, &mut [0.0; 3], 0), vec![(128, 0, 0); 3]);
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
