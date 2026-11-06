//! The clock display of the stations that have one (Mini 3, Plum, Pickle:
//! «<board>:white:clock<A-G|H><0-3>» in sysfs, plus DH/DL — the colon — and
//! DEGREE/DM), used only during the first install: a spinner while soundya
//! starts, the panel's address while it waits for you, the wizard's step,
//! and «HI» at the end. Afterwards the firmware's own clock is put back (it
//! also redraws it once a minute by itself).
//!
//! Segment names and glyphs as in Luna's station crates (MIT,
//! github.com/nyakokitsu/luna, stations/yandexmini3, yandexplum, pickle).
//! Detected by the sysfs nodes, not by the model name: a station without them
//! simply has no display here. Not checked on a real speaker.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const ROOT: &str = "/sys/class/leds";
const DIGITS: usize = 4;

pub struct Clock {
    /// Segment name → brightness file.
    paths: HashMap<String, PathBuf>,
    /// The «H» segment (a dot after the digit) exists on this board.
    dots: bool,
}

/// Segments of one character on a seven-segment digit.
pub fn glyph(c: char) -> Option<&'static str> {
    Some(match c.to_ascii_uppercase() {
        '0' | 'O' | 'D' => "ABCDEF",
        '1' | 'I' => "BC",
        '2' => "ABDEG",
        '3' => "ABCDG",
        '4' => "BCFG",
        '5' | 'S' => "ACDFG",
        '6' => "ACDEFG",
        '7' => "ABC",
        '8' => "ABCDEFG",
        '9' => "ABCDFG",
        'A' => "ABCEFG",
        'B' => "CDEFG",
        'C' => "ADEF",
        'E' => "ADEFG",
        'H' => "BCEFG",
        'L' => "DEF",
        'N' => "CEG",
        'P' => "ABEFG",
        'T' => "DEFG",
        'U' => "BCDEF",
        'Y' => "BCDFG",
        '-' => "G",
        '_' => "D",
        ' ' => "",
        _ => return None,
    })
}

/// Up to four characters (dots attach to the character before) → lit segments.
pub fn segments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for c in text.chars() {
        if c == '.' {
            if pos > 0 {
                out.push(format!("H{}", pos - 1));
            }
            continue;
        }
        if pos >= DIGITS {
            break;
        }
        if let Some(g) = glyph(c) {
            out.extend(g.chars().map(|p| format!("{p}{pos}")));
        }
        pos += 1;
    }
    out
}

/// «192.168.0.12» as display pages: «192.», «168.», «0.», «12».
pub fn ip_pages(ip: &str) -> Vec<String> {
    let parts: Vec<&str> = ip.split('.').collect();
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| if i + 1 < parts.len() { format!("{p:>3}.") } else { format!("{p:>3}") })
        .collect()
}

/// The spinner: one outer segment lit, going round all four digits.
pub fn spinner(tick: usize) -> Vec<String> {
    // Top edge left→right, right side down, bottom right→left, left side up.
    let mut path: Vec<String> = (0..DIGITS).map(|d| format!("A{d}")).collect();
    path.push(format!("B{}", DIGITS - 1));
    path.push(format!("C{}", DIGITS - 1));
    path.extend((0..DIGITS).rev().map(|d| format!("D{d}")));
    path.push("E0".into());
    path.push("F0".into());
    let i = tick % path.len();
    vec![path[i].clone(), path[(i + path.len() - 1) % path.len()].clone()]
}

impl Clock {
    pub fn detect() -> Option<Clock> {
        Self::detect_in(Path::new(ROOT))
    }

    fn detect_in(root: &Path) -> Option<Clock> {
        // «plum:white:clockA0» → prefix «plum:white:clock».
        let prefix = std::fs::read_dir(root).ok()?.flatten().find_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix("A0").filter(|p| p.ends_with(":clock")).map(str::to_owned)
        })?;
        let mut paths = HashMap::new();
        for d in 0..DIGITS {
            for p in ["A", "B", "C", "D", "E", "F", "G", "H"] {
                let path = root.join(format!("{prefix}{p}{d}")).join("brightness");
                if crate::fsx::exists(&path) {
                    paths.insert(format!("{p}{d}"), path);
                }
            }
        }
        for extra in ["DEGREE", "DH", "DL", "DM"] {
            let path = root.join(format!("{prefix}{extra}")).join("brightness");
            if crate::fsx::exists(&path) {
                paths.insert(extra.to_owned(), path);
            }
        }
        // All seven segments of all four digits, or it is not a display we know.
        let complete = (0..DIGITS).all(|d| "ABCDEFG".chars().all(|p| paths.contains_key(&format!("{p}{d}"))));
        let dots = paths.contains_key("H0");
        complete.then_some(Clock { paths, dots })
    }

    fn read(&self) -> HashMap<String, String> {
        self.paths
            .iter()
            .map(|(n, p)| (n.clone(), crate::fsx::read_string(p).unwrap_or_default().trim().to_owned()))
            .collect()
    }

    fn show(&self, lit: &[String], colon: bool) {
        for (name, path) in &self.paths {
            let on = lit.iter().any(|l| l == name) || (colon && (name == "DH" || name == "DL"));
            let _ = std::fs::write(path, if on { "255" } else { "0" });
        }
    }

    fn restore(&self, saved: &HashMap<String, String>) {
        for (name, path) in &self.paths {
            let v = saved.get(name).filter(|v| !v.is_empty()).map_or("0", String::as_str);
            let _ = std::fs::write(path, v);
        }
    }
}

/// What the display shows at `ms` into an install phase.
pub fn frame(phase: &crate::led::Install, ms: u64, ip: Option<&str>, dots: bool) -> (Vec<String>, bool) {
    use crate::led::Install;
    match phase {
        Install::Loading => (spinner((ms / 90) as usize), false),
        Install::Waiting => match ip {
            Some(ip) => {
                let pages = ip_pages(ip);
                let mut text = pages[(ms / 1400) as usize % pages.len()].clone();
                if !dots {
                    text = text.replace('.', "");
                }
                (segments(&text), false)
            }
            None => (spinner((ms / 160) as usize), false),
        },
        Install::Step(n, _) => (segments(&format!("St {}", n.min(&9))), false),
        Install::Done => (segments(if (ms / 300) % 2 == 0 { " HI " } else { "    " }), false),
    }
}

/// Draws the install phases on the display while they last, then puts the
/// firmware's clock back.
pub fn run() {
    let Some(clock) = Clock::detect() else { return };
    eprintln!("display: clock found ({} segments)", clock.paths.len());
    thread::spawn(move || {
        let mut saved: Option<HashMap<String, String>> = None;
        let mut started = Instant::now();
        let mut last = None;
        loop {
            let phase = crate::led::install_phase();
            if phase != last {
                started = Instant::now();
                last = phase.clone();
            }
            match &phase {
                Some(p) => {
                    if saved.is_none() {
                        saved = Some(clock.read());
                    }
                    let ip = crate::mdns::own_ip().map(|i| i.to_string());
                    let (lit, colon) = frame(p, started.elapsed().as_millis() as u64, ip.as_deref(), clock.dots);
                    clock.show(&lit, colon);
                }
                None => {
                    if let Some(s) = saved.take() {
                        clock.restore(&s);
                    }
                }
            }
            thread::sleep(Duration::from_millis(90));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_ip_on_four_digits() {
        assert_eq!(segments("12"), vec!["B0", "C0", "A1", "B1", "D1", "E1", "G1"]);
        assert!(segments("192.").contains(&"H2".to_owned()), "the dot after the third digit");
        assert_eq!(ip_pages("192.168.0.12"), vec!["192.", "168.", "  0.", " 12"]);
        assert_eq!(segments(" HI ").len(), 5 + 2);
        assert_eq!(spinner(0).len(), 2);
        assert_ne!(spinner(0), spinner(1));
    }

    #[test]
    fn finds_a_clock_by_its_nodes() {
        let d = std::env::temp_dir().join(format!("soundya-clock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(Clock::detect_in(&d).is_none());
        for n in 0..4 {
            for p in ["A", "B", "C", "D", "E", "F", "G"] {
                let dir = d.join(format!("plum:white:clock{p}{n}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("brightness"), "0").unwrap();
            }
        }
        std::fs::create_dir_all(d.join("plum:white:clockDH")).unwrap();
        std::fs::write(d.join("plum:white:clockDH/brightness"), "255").unwrap();
        let c = Clock::detect_in(&d).expect("display found");
        assert!(!c.dots && c.paths.contains_key("DH"));
        let saved = c.read();
        c.show(&segments("8888"), false);
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockA0/brightness")).unwrap(), "255");
        c.restore(&saved);
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockA0/brightness")).unwrap(), "0");
        assert_eq!(std::fs::read_to_string(d.join("plum:white:clockDH/brightness")).unwrap(), "255", "colon back as it was");
        let _ = std::fs::remove_dir_all(&d);
    }
}
