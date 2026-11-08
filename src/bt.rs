//! Bluetooth output: soundya's music to headphones or a Bluetooth speaker (A2DP source).
//!
//! The Station Lite runs BlueZ (bluetoothd, bluetoothctl) and BlueALSA, but BlueALSA
//! only receives (a2dp-sink: a phone plays on the speaker). To send, BlueALSA must also
//! run with the a2dp-source profile: soundya restarts it with the same arguments plus
//! that profile (the receiving side stays), and goes back to the old arguments if the
//! new one does not come up. Music then goes to the ALSA device
//! `bluealsa:DEV=<mac>,PROFILE=a2dp` (BlueALSA's ALSA plugin). Alice's own sound stays
//! on the speaker.
//!
//! Devices are found, paired and connected with bluetoothctl (stdin commands work on
//! old and new BlueZ alike). Nothing here runs unless the owner asks on the page.

use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Device {
    pub mac: String,
    pub name: String,
    /// Can play sound (an «Audio Sink»), when known.
    pub audio: bool,
    pub paired: bool,
    pub connected: bool,
}

/// 00:11:22:AA:BB:CC (upper or lower case).
pub fn valid_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The ALSA device that plays on a connected Bluetooth device.
pub fn pcm_name(mac: &str) -> String {
    format!("bluealsa:DEV={},PROFILE=a2dp", mac.to_ascii_uppercase())
}

/// bluetoothctl's colours and prompts out.
fn plain(out: &str) -> String {
    let mut s = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC [ … letter
            if chars.peek() == Some(&'[') {
                chars.next();
                for x in chars.by_ref() {
                    if x.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\r' {
            s.push('\n');
            continue;
        }
        if c == '\u{1}' || c == '\u{2}' {
            continue;
        }
        s.push(c);
    }
    s
}

/// A name that is only the address again («5C-C1-D7-A7-DC-15»): BlueZ's alias for an unnamed device.
fn is_address(name: &str, mac: &str) -> bool {
    name.replace('-', ":").eq_ignore_ascii_case(mac)
}

/// «Device MAC Name», «[NEW] Device MAC Name» and «[CHG] Device MAC Name: …» / «Alias: …» lines:
/// the devices seen, each once, with the last real name given (the address is no name).
pub fn parse_devices(out: &str) -> Vec<Device> {
    let mut list: Vec<Device> = Vec::new();
    for line in plain(out).lines() {
        let Some(i) = line.find("Device ") else { continue };
        let before = line[..i].trim();
        let changed = before.ends_with("[CHG]");
        if !(before.is_empty() || before.ends_with("[NEW]") || changed || before.ends_with('#') || before.ends_with('>')) {
            continue;
        }
        let rest = &line[i + 7..];
        let (mac, name) = rest.split_once(' ').unwrap_or((rest, ""));
        let mac = mac.trim();
        if !valid_mac(mac) {
            continue;
        }
        let name = name.trim();
        // A change: only a new name or alias counts.
        let name = if changed {
            match name.strip_prefix("Name: ").or_else(|| name.strip_prefix("Alias: ")) {
                Some(n) => n.trim(),
                None => continue,
            }
        } else {
            name
        };
        if name.starts_with("RSSI") || name.starts_with("ManufacturerData") || name.starts_with("TxPower") {
            continue;
        }
        let name = if is_address(name, mac) { "" } else { name };
        match list.iter_mut().find(|d| d.mac.eq_ignore_ascii_case(mac)) {
            Some(d) => {
                if !name.is_empty() {
                    d.name = name.to_owned();
                }
            }
            None => list.push(Device { mac: mac.to_ascii_uppercase(), name: name.to_owned(), ..Default::default() }),
        }
    }
    list
}

/// `bluetoothctl info MAC` → what the device is now.
pub fn parse_info(mac: &str, out: &str) -> Option<Device> {
    let text = plain(out);
    if !text.contains("Device ") || text.contains("not available") {
        return None;
    }
    let field = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(&format!("{k}: ")).map(|v| v.trim().to_owned()));
    let yes = |k: &str| field(k).as_deref() == Some("yes");
    let audio = text.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("uuid:") && (l.contains("audio sink") || l.contains("0000110b-"))
    }) || field("Icon").is_some_and(|i| i.starts_with("audio"));
    Some(Device {
        mac: mac.to_ascii_uppercase(),
        name: [field("Alias"), field("Name")].into_iter().flatten().find(|n| !n.is_empty() && !is_address(n, mac)).unwrap_or_default(),
        audio,
        paired: yes("Paired"),
        connected: yes("Connected"),
    })
}

/// BlueALSA's arguments with the sending profile added; None if it already sends.
/// Old BlueALSA takes «-p a2dp-source», newer also «--profile=a2dp-source».
pub fn with_source(args: &[String]) -> Option<Vec<String>> {
    let sends = args.windows(2).any(|w| w[0] == "-p" && w[1] == "a2dp-source")
        || args.iter().any(|a| a == "--profile=a2dp-source" || a == "-pa2dp-source");
    if sends {
        return None;
    }
    let mut out = args.to_vec();
    // Started with no profile: BlueALSA's default is both sides; nothing to add.
    let has_profiles = args.iter().any(|a| a == "-p" || a.starts_with("--profile") || (a.starts_with("-p") && a.len() > 2));
    if !has_profiles {
        return None;
    }
    out.push("-p".into());
    out.push("a2dp-source".into());
    Some(out)
}

/// The BlueALSA ALSA plugin, where the speaker's libasound looks for it.
pub fn plugin(root: &Path) -> Option<String> {
    for dir in ["usr/lib/alsa-lib", "usr/lib/arm-linux-gnueabihf/alsa-lib", "usr/lib/alsa", "lib/alsa-lib", "usr/lib32/alsa-lib"] {
        let p = root.join(dir).join("libasound_module_pcm_bluealsa.so");
        if crate::fsx::exists(&p) {
            return Some(format!("/{dir}/libasound_module_pcm_bluealsa.so"));
        }
    }
    None
}

/// The running BlueALSA: (pid, its arguments without the program).
fn bluealsa_proc() -> Option<(u32, String, Vec<String>)> {
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let comm = crate::fsx::read_string(&e.path().join("comm")).unwrap_or_default();
        if comm.trim() != "bluealsa" {
            continue;
        }
        let raw = crate::fsx::read_bytes(&e.path().join("cmdline")).unwrap_or_default();
        let mut parts: Vec<String> = raw.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
        if parts.is_empty() {
            continue;
        }
        let prog = parts.remove(0);
        return Some((pid, prog, parts));
    }
    None
}

static SOURCE: Mutex<Option<Result<(), String>>> = Mutex::new(None);

/// Makes BlueALSA able to send (once per run). Err: why not.
pub fn ensure_source() -> Result<(), String> {
    if let Some(r) = SOURCE.lock().unwrap().clone() {
        if r.is_ok() {
            return r;
        }
    }
    let r = make_source();
    *SOURCE.lock().unwrap() = Some(r.clone());
    r
}

fn make_source() -> Result<(), String> {
    if plugin(Path::new("/")).is_none() {
        return Err("на колонке нет ALSA-модуля BlueALSA (libasound_module_pcm_bluealsa.so) — звук в Bluetooth отправить нечем".into());
    }
    let Some((pid, prog, args)) = bluealsa_proc() else {
        return Err("BlueALSA на колонке не запущена".into());
    };
    let Some(new_args) = with_source(&args) else {
        eprintln!("bt: bluealsa already sends ({})", args.join(" "));
        return Ok(());
    };
    eprintln!("bt: restarting bluealsa: {} → {}", args.join(" "), new_args.join(" "));
    let start = |a: &[String]| Command::new(&prog).args(a).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let _ = Command::new("kill").arg(pid.to_string()).status();
    thread::sleep(Duration::from_millis(800));
    let up = |t: Duration| {
        let t0 = Instant::now();
        while t0.elapsed() < t {
            if bluealsa_proc().is_some_and(|(_, _, a)| with_source(&a).is_none()) {
                return true;
            }
            thread::sleep(Duration::from_millis(300));
        }
        false
    };
    match start(&new_args) {
        Ok(_) if up(Duration::from_secs(4)) => {
            thread::sleep(Duration::from_secs(1));
            if bluealsa_proc().is_some_and(|(_, _, a)| with_source(&a).is_none()) {
                return Ok(());
            }
            Err(back(&prog, &args, "BlueALSA с передачей звука сразу остановилась"))
        }
        Ok(_) => Err(back(&prog, &args, "BlueALSA с передачей звука не запустилась")),
        Err(e) => Err(back(&prog, &args, &format!("не удалось запустить BlueALSA: {e}"))),
    }
}

/// The old BlueALSA back (the speaker's own Bluetooth must keep working).
fn back(prog: &str, args: &[String], why: &str) -> String {
    if bluealsa_proc().is_none() {
        let _ = Command::new(prog).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
    eprintln!("bt: {why}; bluealsa back as it was");
    format!("{why}; вернула прежний режим (колонка принимает Bluetooth как раньше)")
}

/// One bluetoothctl session: commands with pauses after them, all of its output.
fn ctl(steps: &[(&str, u64)], limit: Duration) -> Result<String, String> {
    let mut child = Command::new("bluetoothctl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("bluetoothctl: {e}"))?;
    let mut out = child.stdout.take().ok_or("bluetoothctl: нет вывода")?;
    let reader = thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        String::from_utf8_lossy(&s).into_owned()
    });
    {
        let mut stdin = child.stdin.take().ok_or("bluetoothctl: нет ввода")?;
        let t0 = Instant::now();
        for (cmd, wait_ms) in steps {
            if t0.elapsed() > limit {
                break;
            }
            let _ = writeln!(stdin, "{cmd}");
            let _ = stdin.flush();
            thread::sleep(Duration::from_millis(*wait_ms));
        }
        let _ = writeln!(stdin, "quit");
    }
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    reader.join().map_err(|_| "bluetoothctl: вывод потерян".to_owned())
}

/// Looks for devices around for `secs` seconds: the ones that can play sound first.
pub fn scan(secs: u64) -> Result<Vec<Device>, String> {
    let out = ctl(&[("power on", 500), ("scan on", secs * 1000), ("scan off", 300), ("devices", 700)], Duration::from_secs(secs + 10))?;
    let mut list = parse_devices(&out);
    for d in list.iter_mut() {
        if let Some(i) = info(&d.mac) {
            d.audio = i.audio;
            d.paired = i.paired;
            d.connected = i.connected;
            if d.name.is_empty() {
                d.name = i.name;
            }
        }
    }
    list.retain(|d| !d.name.is_empty() || d.audio || d.paired);
    list.sort_by_key(|d| (!d.connected, !d.paired, !d.audio, d.name.to_lowercase()));
    list.truncate(30);
    Ok(list)
}

pub fn info(mac: &str) -> Option<Device> {
    if !valid_mac(mac) {
        return None;
    }
    let out = ctl(&[(&format!("info {mac}"), 900)], Duration::from_secs(5)).ok()?;
    parse_info(mac, &out)
}

/// Pairs (if needed), trusts and connects; the device must be in pairing mode the first time.
pub fn connect(mac: &str) -> Result<Device, String> {
    if !valid_mac(mac) {
        return Err("неверный адрес устройства".into());
    }
    ensure_source()?;
    let known = info(mac);
    if !known.as_ref().is_some_and(|d| d.paired) {
        ctl(&[("power on", 500), ("scan on", 4000), (&format!("pair {mac}"), 9000), ("scan off", 300)], Duration::from_secs(20))?;
    }
    ctl(&[(&format!("trust {mac}"), 700), (&format!("connect {mac}"), 7000)], Duration::from_secs(12))?;
    match info(mac) {
        Some(d) if d.connected => Ok(d),
        Some(d) if !d.paired => Err("не удалось сопрячь: переведите наушники в режим сопряжения (обычно — долго держать кнопку питания) и попробуйте снова".into()),
        Some(_) => Err("устройство сопряжено, но не подключилось: включено ли оно и не подключено ли к телефону?".into()),
        None => Err("устройство не найдено: включите его рядом с колонкой и нажмите «Найти»".into()),
    }
}

pub fn disconnect(mac: &str) -> Result<(), String> {
    if !valid_mac(mac) {
        return Err("неверный адрес устройства".into());
    }
    ctl(&[(&format!("disconnect {mac}"), 2500)], Duration::from_secs(6)).map(|_| ())
}

pub fn forget(mac: &str) -> Result<(), String> {
    if !valid_mac(mac) {
        return Err("неверный адрес устройства".into());
    }
    ctl(&[(&format!("remove {mac}"), 2000)], Duration::from_secs(6)).map(|_| ())
}

/// The device music should go to (None: Bluetooth output off), for the watcher.
static OUTPUT: Mutex<Option<String>> = Mutex::new(None);

pub fn set_output(mac: Option<String>) {
    *OUTPUT.lock().unwrap() = mac.filter(|m| valid_mac(m));
}

pub fn output() -> Option<String> {
    OUTPUT.lock().unwrap().clone()
}

/// Is the output device connected now (checked every few seconds by `watch`).
static CONNECTED: Mutex<Option<(String, bool)>> = Mutex::new(None);

pub fn connected(mac: &str) -> bool {
    CONNECTED.lock().unwrap().as_ref().is_some_and(|(m, c)| m.eq_ignore_ascii_case(mac) && *c)
}

pub fn set_connected(mac: &str, on: bool) {
    *CONNECTED.lock().unwrap() = Some((mac.to_ascii_uppercase(), on));
}

/// What the panel shows about Bluetooth output.
pub fn status_json(mac: &str, on: bool) -> serde_json::Value {
    serde_json::json!({
        "available": crate::fsx::exists(Path::new("/usr/bin/bluetoothctl")) || crate::fsx::exists(Path::new("/bin/bluetoothctl")),
        "plugin": plugin(Path::new("/")).is_some(),
        "source": SOURCE.lock().unwrap().clone().map(|r| r.err().unwrap_or_default()),
        "mac": mac,
        "on": on,
        "connected": !mac.is_empty() && connected(mac),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs() {
        assert!(valid_mac("00:1A:7d:da:71:13"));
        for bad in ["", "00:1A:7D:DA:71", "00:1A:7D:DA:71:1G", "00-1A-7D-DA-71-13", "00:1A:7D:DA:71:13; reboot"] {
            assert!(!valid_mac(bad), "{bad}");
        }
        assert_eq!(pcm_name("00:1a:7d:da:71:13"), "bluealsa:DEV=00:1A:7D:DA:71:13,PROFILE=a2dp");
    }

    #[test]
    fn devices_from_a_scan() {
        let out = "\u{1b}[0;94m[bluetooth]\u{1b}[0m# scan on\r\nDiscovery started\n\
            [\u{1b}[0;92mNEW\u{1b}[0m] Device 11:22:33:44:55:66 11-22-33-44-55-66\n\
            [\u{1b}[0;92mNEW\u{1b}[0m] Device AA:BB:CC:DD:EE:FF WH-1000XM4\n\
            [\u{1b}[0;93mCHG\u{1b}[0m] Device AA:BB:CC:DD:EE:FF RSSI: -61\n\
            [CHG] Device 11:22:33:44:55:66 Name: JBL Flip 5\n\
            [bluetooth]# devices\nDevice AA:BB:CC:DD:EE:FF WH-1000XM4\nDevice 11:22:33:44:55:66 JBL Flip 5\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].mac.as_str(), d[0].name.as_str()), ("11:22:33:44:55:66", "JBL Flip 5"));
        assert_eq!((d[1].mac.as_str(), d[1].name.as_str()), ("AA:BB:CC:DD:EE:FF", "WH-1000XM4"));
    }

    #[test]
    fn device_info() {
        let out = "Device AA:BB:CC:DD:EE:FF (public)\n\tName: WH-1000XM4\n\tAlias: Мои наушники\n\tIcon: audio-headset\n\
            \tPaired: yes\n\tTrusted: yes\n\tConnected: no\n\tUUID: Audio Sink                (0000110b-0000-1000-8000-00805f9b34fb)\n";
        let d = parse_info("aa:bb:cc:dd:ee:ff", out).unwrap();
        assert_eq!((d.name.as_str(), d.audio, d.paired, d.connected), ("Мои наушники", true, true, false));
        assert!(parse_info("AA:BB:CC:DD:EE:FF", "Device AA:BB:CC:DD:EE:FF not available\n").is_none());
        // An unnamed device: BlueZ's alias is the address; that is no name.
        let unnamed = parse_info("5C:C1:D7:A7:DC:15", "Device 5C:C1:D7:A7:DC:15 (random)\n\tAlias: 5C-C1-D7-A7-DC-15\n\tPaired: no\n").unwrap();
        assert_eq!(unnamed.name, "");
        let named_later = parse_info("5C:C1:D7:A7:DC:15", "Device 5C:C1:D7:A7:DC:15\n\tName: JBL Go\n\tAlias: 5C-C1-D7-A7-DC-15\n").unwrap();
        assert_eq!(named_later.name, "JBL Go");
        let phone = parse_info("11:22:33:44:55:66", "Device 11:22:33:44:55:66\n\tName: iPhone\n\tIcon: phone\n\tPaired: no\n\tConnected: no\n").unwrap();
        assert!(!phone.audio);
    }

    #[test]
    fn bluealsa_gets_the_sending_profile_and_keeps_receiving() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(with_source(&a("-p a2dp-sink -i hci0")), Some(a("-p a2dp-sink -i hci0 -p a2dp-source")));
        assert_eq!(with_source(&a("-p a2dp-sink -p a2dp-source")), None);
        assert_eq!(with_source(&a("--profile=a2dp-sink --profile=a2dp-source")), None);
        // No profiles given: BlueALSA already does both.
        assert_eq!(with_source(&a("-i hci0")), None);
    }

    #[test]
    fn the_alsa_plugin_is_looked_for() {
        let root = std::env::temp_dir().join(format!("soundya-bt-{}", std::process::id()));
        assert!(plugin(&root).is_none());
        std::fs::create_dir_all(root.join("usr/lib/alsa-lib")).unwrap();
        std::fs::write(root.join("usr/lib/alsa-lib/libasound_module_pcm_bluealsa.so"), b"").unwrap();
        assert_eq!(plugin(&root).as_deref(), Some("/usr/lib/alsa-lib/libasound_module_pcm_bluealsa.so"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
