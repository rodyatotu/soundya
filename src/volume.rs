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
            Some(n) if (0..=MAX).contains(&n) => LEVEL.store(n, Ordering::Release),
            // «громкость на максимум», «громкость 50 процентов»: not followed.
            _ => LEVEL.store(-1, Ordering::Release),
        }
    } else if t.contains("громче") {
        nudge(1);
    } else if t.contains("тише") {
        nudge(-1);
    }
}

/// The slider: Alice's volume to `target` (0..=10).
pub fn set(target: i32) -> Result<i32, String> {
    let _one = BUSY.lock().unwrap();
    let target = target.clamp(0, MAX);
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

/// One press of + (`up`) or −, `steps` times.
pub fn press(up: bool, steps: u32) -> Result<(), String> {
    PRESSING.store(true, Ordering::Release);
    let r = press_keys(up, steps);
    // The button reader sees them a moment later.
    std::thread::sleep(std::time::Duration::from_millis(100));
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
    fn ioctl_number_and_bits() {
        // EVIOCGBIT(EV_KEY, 96) as <linux/input.h> spells it.
        assert_eq!(eviocgbit_key(96), 0x8060_4521);
        let mut bits = [0u8; 96];
        bits[115 / 8] |= 1 << (115 % 8);
        assert!(has(&bits, KEY_VOLUMEUP) && !has(&bits, KEY_VOLUMEDOWN));
        assert_eq!(event(EV_KEY, 115, 1)[8..], [1, 0, 0x73, 0, 1, 0, 0, 0]);
    }
}
