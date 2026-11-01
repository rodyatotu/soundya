//! Alice's volume from the web page: soundya presses the speaker's own + and −
//! buttons. The events are written into the button device, so Alice sees an
//! ordinary press: she changes her volume and shows it on the ring, as always.

//!
//! Alice does not tell her volume, so soundya follows it: every press of + and −
//! (its own and the real buttons, seen in src/buttons.rs) and «громкость 5»,
//! «громче», «тише» said to her. Until one of those, the level is unknown and
//! the slider first goes all the way down, then up to the chosen level.

use std::{
    fs::OpenOptions,
    io::Write,
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Mutex,
    },
};

/// Alice's volume, 0..=10; −1 = not known yet.
static LEVEL: AtomicI32 = AtomicI32::new(-1);
/// soundya is pressing: buttons.rs must not count those presses again.
static PRESSING: AtomicBool = AtomicBool::new(false);
/// One slider move at a time.
static BUSY: Mutex<()> = Mutex::new(());
pub const MAX: i32 = 10;

pub fn level() -> Option<i32> {
    let l = LEVEL.load(Ordering::Acquire);
    (l >= 0).then_some(l)
}

fn nudge(by: i32) {
    let mut l = LEVEL.load(Ordering::Acquire);
    while l >= 0 {
        match LEVEL.compare_exchange(l, (l + by).clamp(0, MAX), Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(now) => l = now,
        }
    }
}

/// A key event seen on the button device (src/buttons.rs).
pub fn seen_key(code: u16, value: i32) {
    if value != 1 || PRESSING.load(Ordering::Acquire) {
        return;
    }
    match code {
        KEY_VOLUMEUP => nudge(1),
        KEY_VOLUMEDOWN => nudge(-1),
        _ => {}
    }
}

/// What was said to Alice: «громкость 5», «поставь громкость на семь», «громче», «тише».
pub fn heard(text: &str) {
    let t = text.to_lowercase().replace('ё', "е");
    if ["телевизор", "проектор", "тв", "компьютер"].iter().any(|w| t.contains(w)) {
        return;
    }
    if t.contains("громкост") {
        const WORDS: [&str; 11] =
            ["ноль", "один", "два", "три", "четыре", "пять", "шесть", "семь", "восемь", "девять", "десять"];
        let n = t.split(|c: char| !c.is_alphanumeric()).find_map(|w| {
            w.parse::<i32>().ok().or_else(|| WORDS.iter().position(|x| *x == w).map(|i| i as i32))
        });
        match n {
            Some(n) if (0..=MAX).contains(&n) => {
                LEVEL.store(n, Ordering::Release);
                learn_later(n);
            }
            // «громкость на максимум», «громкость 50 процентов»: not followed.
            _ => LEVEL.store(-1, Ordering::Release),
        }
    } else if t.contains("громче") {
        nudge(1);
    } else if t.contains("тише") {
        nudge(-1);
    }
}

// ---- straight into the mixer ----
//
// Alice sets her volume with set_volume.sh = `amixer set Master N%`, the
// «Master Playback Volume» control (0–255). The slider writes the same control:
// instantly, with no presses and no tick. Which value Alice uses for each of her
// levels is learnt from her own «громкость N» (kept in volume-levels.txt);
// until then level N is N×10 %.

const CONTROL: &str = "name=Master Playback Volume";

fn state_file() -> PathBuf {
    PathBuf::from(std::env::var("SOUNDYA_DIR").unwrap_or_else(|_| "/data/opt/soundya".into())).join("volume-levels.txt")
}

/// The mixer value for each level that Alice has been seen to use.
fn learnt() -> [Option<u32>; 11] {
    let mut out = [None; 11];
    for line in crate::fsx::read_string(&state_file()).unwrap_or_default().lines() {
        if let Some((l, v)) = line.split_once('=') {
            if let (Ok(l), Ok(v)) = (l.trim().parse::<usize>(), v.trim().parse::<u32>()) {
                if l <= 10 && v <= 255 {
                    out[l] = Some(v);
                }
            }
        }
    }
    out
}

/// «: values=140,140» → 140.
fn parse_mixer(out: &str) -> Option<u32> {
    out.lines().find_map(|l| l.trim().strip_prefix(": values=")).and_then(|v| v.split(',').next()?.trim().parse().ok())
}

fn read_mixer() -> Option<u32> {
    let out = std::process::Command::new("amixer").args(["cget", CONTROL]).output().ok()?;
    parse_mixer(&String::from_utf8_lossy(&out.stdout))
}

fn write_mixer(value: u32) -> bool {
    let amixer = |args: &[&str]| std::process::Command::new("amixer").args(args).status().is_ok_and(|s| s.success());
    // The control by its full name; on firmwares that name it differently,
    // the simple control «Master» as set_volume.sh does.
    amixer(&["-q", "cset", CONTROL, &format!("{value},{value}")])
        || amixer(&["-q", "set", "Master", &format!("{}%", (value * 100 + 127) / 255)])
}

/// The value for `level`: learnt, or level×10 % of 255 as set_volume.sh would.
fn mixer_value(level: i32, learnt: &[Option<u32>; 11]) -> u32 {
    let level = level.clamp(0, MAX) as usize;
    learnt[level].unwrap_or(((level as u32 * 10 * 255) + 50) / 100)
}

/// After Alice's own «громкость N»: note which value she set.
fn learn_later(n: i32) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let Some(value) = read_mixer() else { return };
        // Only if nothing else changed it meanwhile.
        if level() != Some(n) {
            return;
        }
        let mut table = learnt();
        if table[n as usize] == Some(value) {
            return;
        }
        table[n as usize] = Some(value);
        let text: String = table
            .iter()
            .enumerate()
            .filter_map(|(l, v)| v.map(|v| format!("{l}={v}\n")))
            .collect();
        let _ = std::fs::write(state_file(), text);
        eprintln!("volume: Alice's level {n} is {value}/255");
    });
}

/// The slider: Alice's volume to `target` (0..=10), straight into the mixer;
/// with no amixer on the speaker, by pressing her buttons.
pub fn set(target: i32) -> Result<i32, String> {
    let _one = BUSY.lock().unwrap();
    let target = target.clamp(0, MAX);
    // Under Luna: its volume API (Alice's volume manager, else the mixer, done
    // the way Luna knows works on this model).
    if crate::luna::active() {
        match crate::luna::volume(target * 10) {
            Ok(()) => {
                LEVEL.store(target, Ordering::Release);
                return Ok(target);
            }
            Err(e) => eprintln!("volume: luna: {e}"),
        }
    }
    // Best: Alice's own volume manager — she knows the new level, her buttons
    // and «громче» go on from it.
    match maind::set(target) {
        Ok(()) => {
            LEVEL.store(target, Ordering::Release);
            return Ok(target);
        }
        Err(e) => eprintln!("volume: Alice's volume manager: {e}; writing the mixer"),
    }
    if write_mixer(mixer_value(target, &learnt())) {
        LEVEL.store(target, Ordering::Release);
        return Ok(target);
    }
    let from = match level() {
        Some(l) => l,
        None => {
            // Unknown: all the way down first, then count from 0.
            press(false, MAX as u32)?;
            0
        }
    };
    if target != from {
        press(target > from, (target - from).unsigned_abs())?;
    }
    LEVEL.store(target, Ordering::Release);
    Ok(target)
}

/// Alice's volume manager (quasar's `volume_manager` IPC service, the port is in
/// quasar.cfg): a SetVolumeSignal sets her level exactly as her buttons do, so
/// she and the page share one volume. Message layout as in Luna
/// (github.com/nyakokitsu/luna, gateway.rs, MIT).
mod maind {
    use std::{
        io::Write,
        net::{SocketAddr, TcpStream},
        time::Duration,
    };

    const CONFIG: &str = "/system/vendor/quasar/quasar.cfg";

    fn port() -> Option<u16> {
        let text = crate::fsx::read_string(std::path::Path::new(CONFIG))?;
        let cfg: serde_json::Value = serde_json::from_str(&text).ok()?;
        cfg.get("volume_manager")?.get("port")?.as_u64().and_then(|p| u16::try_from(p).ok())
    }

    fn var(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
        var(out, field << 3 | 2);
        var(out, data.len() as u64);
        out.extend_from_slice(data);
    }

    /// QuasarMessage { volume_manager_message (200): VolumeManager {
    /// set_volume_signal (2): { platform_volume (1): 0..10000, muted (3), source (4) } } }
    pub fn message(level: i32) -> Vec<u8> {
        let mut signal = Vec::new();
        var(&mut signal, 1 << 3);
        var(&mut signal, (level.clamp(0, 10) as u64) * 1000);
        var(&mut signal, 3 << 3);
        var(&mut signal, 0);
        bytes(&mut signal, 4, b"soundya");
        let mut manager = Vec::new();
        bytes(&mut manager, 2, &signal);
        let mut message = Vec::new();
        bytes(&mut message, 200, &manager);
        message
    }

    pub fn set(level: i32) -> Result<(), String> {
        let port = port().ok_or("no volume_manager port in quasar.cfg")?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(700)).map_err(|e| e.to_string())?;
        s.set_write_timeout(Some(Duration::from_millis(700))).ok();
        let msg = message(level);
        s.write_all(&(msg.len() as i32).to_le_bytes()).map_err(|e| e.to_string())?;
        s.write_all(&msg).map_err(|e| e.to_string())?;
        s.flush().map_err(|e| e.to_string())
    }
}

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_VOLUMEUP: u16 = 115;

extern "C" {
    fn ioctl(fd: i32, request: u32, ...) -> i32;
}

/// EVIOCGBIT(EV_KEY, len): which keys a device has, one bit per key code.
fn eviocgbit_key(len: u32) -> u32 {
    (2 << 30) | (len << 16) | (u32::from(b'E') << 8) | (0x20 + u32::from(EV_KEY))
}

fn has(bits: &[u8], code: u16) -> bool {
    bits.get(usize::from(code) / 8).is_some_and(|b| b & (1 << (code % 8)) != 0)
}

/// The input device with the volume buttons.
fn device() -> Option<PathBuf> {
    (0..10).map(|n| PathBuf::from(format!("/dev/input/event{n}"))).find(|path| {
        let Ok(f) = std::fs::File::open(path) else { return false };
        let mut bits = [0u8; 96];
        let got = unsafe { ioctl(f.as_raw_fd(), eviocgbit_key(bits.len() as u32), bits.as_mut_ptr()) };
        got > 0 && has(&bits, KEY_VOLUMEUP) && has(&bits, KEY_VOLUMEDOWN)
    })
}

/// struct input_event as a 32-bit program writes it: time (zero), type, code, value.
fn event(kind: u16, code: u16, value: i32) -> [u8; 16] {
    let mut e = [0u8; 16];
    e[8..10].copy_from_slice(&kind.to_le_bytes());
    e[10..12].copy_from_slice(&code.to_le_bytes());
    e[12..16].copy_from_slice(&value.to_le_bytes());
    e
}

/// The «tick» Alice plays on every press of + and −.
const CLICK: &str = "/system/vendor/quasar/sounds/common/volume_sound.opus";
/// 0.2 s of silence in the same format (packaging/silent.opus).
const SILENT: &[u8] = include_bytes!("../packaging/silent.opus");

fn click_covered() -> bool {
    crate::fsx::read_string(std::path::Path::new("/proc/mounts")).is_some_and(|m| m.contains(CLICK))
}

/// Puts silence over Alice's tick while soundya presses (a bind mount: nothing
/// on the read-only system partition changes, and it is undone right after).
fn hush_click() -> bool {
    if !crate::fsx::exists(std::path::Path::new(CLICK)) {
        return false;
    }
    if click_covered() {
        return true;
    }
    let dir = std::env::var("SOUNDYA_DIR").unwrap_or_else(|_| "/data/opt/soundya".into());
    let silent = std::path::Path::new(&dir).join("silent.opus");
    if std::fs::write(&silent, SILENT).is_err() {
        return false;
    }
    let ok = std::process::Command::new("mount")
        .arg("--bind")
        .arg(&silent)
        .arg(CLICK)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("volume: could not silence the tick");
    }
    ok
}

fn unhush_click() {
    while click_covered() {
        if !std::process::Command::new("umount").arg(CLICK).status().is_ok_and(|s| s.success()) {
            eprintln!("volume: could not bring the tick back");
            return;
        }
    }
}

/// One press of + (`up`) or −, `steps` times, without Alice's tick.
pub fn press(up: bool, steps: u32) -> Result<(), String> {
    PRESSING.store(true, Ordering::Release);
    let hushed = hush_click();
    let r = press_keys(up, steps);
    // The button reader sees them a moment later, and the last tick plays then.
    std::thread::sleep(std::time::Duration::from_millis(700));
    if hushed {
        unhush_click();
    }
    PRESSING.store(false, Ordering::Release);
    if r.is_ok() {
        nudge(if up { steps as i32 } else { -(steps as i32) });
    }
    r
}

fn press_keys(up: bool, steps: u32) -> Result<(), String> {
    let path = device().ok_or("не нашёл кнопки громкости колонки")?;
    let mut dev = OpenOptions::new().write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let code = if up { KEY_VOLUMEUP } else { KEY_VOLUMEDOWN };
    for _ in 0..steps.clamp(1, MAX as u32) {
        for value in [1, 0] {
            dev.write_all(&event(EV_KEY, code, value)).map_err(|e| e.to_string())?;
            dev.write_all(&event(EV_SYN, 0, 0)).map_err(|e| e.to_string())?;
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_what_is_said() {
        LEVEL.store(-1, Ordering::Release);
        heard("громче");
        assert_eq!(level(), None);
        heard("Алиса, громкость пять");
        assert_eq!(level(), Some(5));
        heard("сделай громче");
        seen_key(KEY_VOLUMEUP, 1);
        seen_key(KEY_VOLUMEUP, 0);
        assert_eq!(level(), Some(7));
        heard("тише на телевизоре");
        assert_eq!(level(), Some(7));
        heard("поставь громкость на 10");
        heard("громче");
        assert_eq!(level(), Some(10));
        heard("громкость на максимум");
        assert_eq!(level(), None);
    }

    #[test]
    fn volume_manager_message() {
        // Same bytes Luna sends for 60 % (platform_volume 6000), source differs.
        let m = maind::message(6);
        assert_eq!(&m[..3], &[0xc2, 0x0c, (m.len() - 3) as u8]);
        assert!(m.windows(3).any(|w| w == [0x08, 0xf0, 0x2e]), "{m:x?}");
        assert!(m.ends_with(b"soundya"));
    }

    #[test]
    fn mixer_values() {
        let out = "numid=178,iface=MIXER,name='Master Playback Volume'\n  ; type=INTEGER,access=rw---RW-,values=2,min=0,max=255,step=0\n  : values=140,140\n";
        assert_eq!(parse_mixer(out), Some(140));
        let mut t = [None; 11];
        assert_eq!(mixer_value(5, &t), 128);
        assert_eq!(mixer_value(10, &t), 255);
        assert_eq!(mixer_value(0, &t), 0);
        t[5] = Some(140);
        assert_eq!(mixer_value(5, &t), 140);
    }

    #[test]
    fn ioctl_number_and_bits() {
        // EVIOCGBIT(EV_KEY, 96) as <linux/input.h> spells it.
        assert_eq!(eviocgbit_key(96), 0x8060_4521);
        let mut bits = [0u8; 96];
        bits[115 / 8] |= 1 << (115 % 8);
        assert!(has(&bits, KEY_VOLUMEUP) && !has(&bits, KEY_VOLUMEDOWN));
        assert_eq!(event(EV_KEY, 115, 1)[8..], [1, 0, 0x73, 0, 1, 0, 0, 0]);
    }
}
