//! Alice's volume from the web page: soundya presses the speaker's own + and −
//! buttons. The events are written into the button device, so Alice sees an
//! ordinary press: she changes her volume and shows it on the ring, as always.

use std::{fs::OpenOptions, io::Write, os::fd::AsRawFd, path::PathBuf};

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
    let path = device().ok_or("не нашёл кнопки громкости колонки")?;
    let mut dev = OpenOptions::new().write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let code = if up { KEY_VOLUMEUP } else { KEY_VOLUMEDOWN };
    for _ in 0..steps.clamp(1, 10) {
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
    fn ioctl_number_and_bits() {
        // EVIOCGBIT(EV_KEY, 96) as <linux/input.h> spells it.
        assert_eq!(eviocgbit_key(96), 0x8060_4521);
        let mut bits = [0u8; 96];
        bits[115 / 8] |= 1 << (115 % 8);
        assert!(has(&bits, KEY_VOLUMEUP) && !has(&bits, KEY_VOLUMEDOWN));
        assert_eq!(event(EV_KEY, 115, 1)[8..], [1, 0, 0x73, 0, 1, 0, 0, 0]);
    }
}
