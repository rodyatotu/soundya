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

/// BlueALSA's arguments as soundya needs them; None if it already runs so.
/// - The sending profile added (old BlueALSA takes «-p a2dp-source», newer also
///   «--profile=a2dp-source»; started with no profile it does both sides already).
/// - The SBC encoder at «medium» instead of the Station's «high» (~345 kbit/s): with a weak
///   signal or a crowded 2.4 GHz band the radio cannot keep up with «high», the chip's queue
///   of 8 packets fills and the sound stutters. «Medium» is a third less to send.
pub fn with_source(args: &[String]) -> Option<Vec<String>> {
    let sends = args.windows(2).any(|w| w[0] == "-p" && w[1] == "a2dp-source")
        || args.iter().any(|a| a == "--profile=a2dp-source" || a == "-pa2dp-source");
    let has_profiles = args.iter().any(|a| a == "-p" || a.starts_with("--profile") || (a.starts_with("-p") && a.len() > 2));
    let mut out = args.to_vec();
    if has_profiles && !sends {
        out.push("-p".into());
        out.push("a2dp-source".into());
    }
    match out.iter_mut().find(|a| a.starts_with("--sbc-quality")) {
        Some(q) if matches!(q.as_str(), "--sbc-quality=medium" | "--sbc-quality=low") => {}
        Some(q) => *q = SBC_QUALITY.into(),
        None => out.push(SBC_QUALITY.into()),
    }
    (out != args).then_some(out)
}

const SBC_QUALITY: &str = "--sbc-quality=medium";

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
        eprintln!("bt: bluealsa already set up ({})", args.join(" "));
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
    let steps: Vec<(&str, u64, &[&str])> = steps.iter().map(|(c, w)| (*c, *w, &[][..])).collect();
    ctl_until(&steps, limit)
}

/// The same, but a step ends as soon as its output shows one of `done` (a pairing that took a
/// second no longer waits nine).
fn ctl_until(steps: &[(&str, u64, &[&str])], limit: Duration) -> Result<String, String> {
    let mut child = Command::new("bluetoothctl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("bluetoothctl: {e}"))?;
    let mut out = child.stdout.take().ok_or("bluetoothctl: нет вывода")?;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let seen2 = seen.clone();
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(n) = out.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen2.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    {
        let mut stdin = child.stdin.take().ok_or("bluetoothctl: нет ввода")?;
        let t0 = Instant::now();
        for (cmd, wait_ms, done) in steps {
            if t0.elapsed() > limit {
                break;
            }
            let from = seen.lock().unwrap().len();
            let _ = writeln!(stdin, "{cmd}");
            let _ = stdin.flush();
            let step = Instant::now();
            while step.elapsed() < Duration::from_millis(*wait_ms) {
                thread::sleep(Duration::from_millis(100));
                if !done.is_empty() {
                    let new = String::from_utf8_lossy(&seen.lock().unwrap()[from..]).into_owned();
                    if done.iter().any(|d| new.contains(d)) {
                        break;
                    }
                }
            }
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
    reader.join().map_err(|_| "bluetoothctl: вывод потерян".to_owned())?;
    let all = seen.lock().unwrap().clone();
    Ok(String::from_utf8_lossy(&all).into_owned())
}

/// Looks for devices around for `secs` seconds: the ones that can play sound first.
pub fn scan(secs: u64) -> Result<Vec<Device>, String> {
    scan_some(secs, false)
}

/// The voice search: only devices that show a name. Each `info` is a second of its own, and a
/// flat full of nameless Bluetooth gadgets made the silence after «Ищу…» a minute long.
pub fn scan_named(secs: u64) -> Result<Vec<Device>, String> {
    scan_some(secs, true)
}

/// Why the speaker cannot send sound over Bluetooth, if the last try to set it up failed.
pub fn source_error() -> Option<String> {
    SOURCE.lock().unwrap().clone().and_then(Result::err)
}

fn scan_some(secs: u64, named_only: bool) -> Result<Vec<Device>, String> {
    let out = ctl(&[("power on", 500), ("scan on", secs * 1000), ("scan off", 300), ("devices", 700)], Duration::from_secs(secs + 10))?;
    let mut list = parse_devices(&out);
    if named_only {
        list.retain(|d| !d.name.is_empty());
    }
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
    // The answer comes in one piece: no need to sit out the whole wait (it was a second of
    // every connection, twice).
    let out = ctl_until(&[(&format!("info {mac}"), 900, &["Connected:", "not available"])], Duration::from_secs(5)).ok()?;
    parse_info(mac, &out)
}

/// «Audio Sink»: what a speaker or headphones take music with.
const A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";

/// Is `mac` among the devices BlueALSA can play to, in `bluealsa-aplay -l`'s words?
fn in_playback_list(list: &str, mac: &str) -> bool {
    let playback = list.split("List of CAPTURE").next().unwrap_or(list);
    playback.to_ascii_uppercase().contains(&mac.to_ascii_uppercase())
}

/// Can music go to the device right now? «Connected» alone does not say it: a Ditoo is two
/// devices on one address — a Bluetooth LE one («Ditoo-Plus-light») and the speaker
/// («Ditoo-Plus-audio») — and BlueZ's own `connect` reaches the LE one: connected, no sound.
/// None = no `bluealsa-aplay` to ask.
fn audio_link(mac: &str) -> Option<bool> {
    let out = Command::new("bluealsa-aplay").arg("-l").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| in_playback_list(&String::from_utf8_lossy(&out.stdout), mac))
}

/// For the watcher and the panel: the sound link, not just any link.
pub fn audio_up(mac: &str) -> bool {
    valid_mac(mac) && audio_link(mac).unwrap_or_else(|| info(mac).is_some_and(|d| d.connected))
}

/// Calls the device's sound profile itself (Device1.ConnectProfile) and waits for the link.
/// None = no `dbus-send` here: the caller falls back to bluetoothctl's `connect`.
fn connect_audio(mac: &str) -> Option<bool> {
    let path = format!("/org/bluez/hci0/dev_{}", mac.to_ascii_uppercase().replace(':', "_"));
    let status = Command::new("dbus-send")
        .args(["--system", "--print-reply", "--reply-timeout=9000", "--dest=org.bluez", &path, "org.bluez.Device1.ConnectProfile", &format!("string:{A2DP_SINK}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return Some(false);
    }
    // BlueALSA lists the device a moment after the profile is up.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(2500) {
        if audio_link(mac)? {
            return Some(true);
        }
        thread::sleep(Duration::from_millis(150));
    }
    Some(false)
}

/// The device shows up in a scan but refuses the connection.
pub const BUSY: &str = "устройство видно, но не принимает подключение: скорее всего, оно сейчас подключено к другому устройству (компьютеру или телефону). Отключите его там или переведите в режим сопряжения и повторите";

/// Pairs (if needed), trusts and connects; the device must be in pairing mode the first time.
pub fn connect(mac: &str) -> Result<Device, String> {
    if !valid_mac(mac) {
        return Err("неверный адрес устройства".into());
    }
    let started = Instant::now();
    ensure_source()?;
    let known = info(mac);
    if !known.as_ref().is_some_and(|d| d.paired) {
        // A device BlueZ has forgotten (the speaker restarted, or it was let go for a phone)
        // needs up to a quarter of a minute to show up. The scan is stopped before pairing
        // (paging while it runs fails more often), and a pairing that did not connect is tried
        // once more: the device is dropped from BlueZ's list after a failure, so it is looked
        // for again.
        let seen_it = format!("Device {mac}");
        let wait = if known.is_some() { 6000 } else { 15000 };
        let mut refused = false;
        for attempt in 0..3 {
            let out = ctl_until(
                &[
                    ("power on", 500, &[]),
                    // The Station's adapter is «Pairable: no»: BlueZ then pairs with «No
                    // Bonding», the key is not stored and the device is forgotten as soon as it
                    // goes away — every «переключись на блютуз» searched and paired from
                    // scratch (half a minute, and it fails now and then). Pairable on = the
                    // key is kept, the next time is a plain `connect`.
                    ("pairable on", 300, &["succeeded", "Pairable: yes"]),
                    ("agent NoInputNoOutput", 300, &["Agent registered", "already registered"]),
                    ("default-agent", 300, &["successful", "No agent"]),
                    ("scan on", wait, &[seen_it.as_str()]),
                    ("scan off", 1200, &["Discovery stopped"]),
                    (&format!("pair {mac}"), 12000, &["Pairing successful", "Failed to pair", "AlreadyExists", "not available"]),
                ],
                Duration::from_secs(36),
            )?;
            refused = out.contains("ConnectionAttemptFailed") || out.contains("Connect Failed") || out.contains("Page Timeout");
            if !refused {
                break;
            }
            eprintln!("bt: {mac}: seen, but it did not take the connection (try {})", attempt + 1);
        }
        // Seen in the scan but it does not answer the call itself: it is busy with another
        // device (a Ditoo held by the PC's launcher, headphones on a phone) or not pairable.
        if refused {
            return Err(BUSY.into());
        }
    }
    // A remembered device: its sound profile is called directly — well under a second. A Ditoo
    // that has just lost the link does not answer for a few seconds (it is calling its last
    // host itself), so a missed call is made again.
    let mut now = None;
    for attempt in 0..3 {
        let _ = ctl_until(&[(&format!("trust {mac}"), 500, &["trust succeeded", "not available"])], Duration::from_secs(4));
        let up = match connect_audio(mac) {
            Some(up) => up,
            None => ctl_until(
                &[(&format!("connect {mac}"), 7000, &["Connection successful", "Failed to connect", "not available"])],
                Duration::from_secs(12),
            )?
            .contains("Connection successful"),
        };
        // The call's answer is the answer: no second look needed.
        now = match info(mac) {
            Some(d) if up => Some(Device { connected: true, ..d }),
            Some(d) => Some(Device { connected: false, ..d }),
            None => None,
        };
        if up || now.is_none() {
            break;
        }
        eprintln!("bt: {mac}: did not answer the call (try {})", attempt + 1);
        thread::sleep(Duration::from_millis(1500));
    }
    // In the log: what the device was before and is now (the page only says the outcome).
    let say = |d: &Option<Device>| d.as_ref().map_or("not known".to_owned(), |d| format!("«{}» audio={} paired={} connected={}", d.name, d.audio, d.paired, d.connected));
    eprintln!("bt: connect {mac}: before {}, after {}, {:.1} s", say(&known), say(&now), started.elapsed().as_secs_f32());
    match now {
        Some(d) if d.connected => Ok(d),
        Some(d) if !d.paired => Err("не удалось сопрячь: переведите наушники в режим сопряжения (обычно — долго держать кнопку питания) и попробуйте снова".into()),
        Some(_) => Err("устройство сопряжено, но не подключилось: включено ли оно и не подключено ли к телефону?".into()),
        None => Err("устройство не найдено: включите его рядом с колонкой и нажмите «Найти»".into()),
    }
}

/// Lets the device go: not trusted (it does not come back by itself) and disconnected,
/// so a phone can take it. «Где играет → Колонка» used to keep it held (owner's Ditoo).
pub fn release(mac: &str) -> Result<(), String> {
    if !valid_mac(mac) {
        return Err("неверный адрес устройства".into());
    }
    ctl_until(
        &[
            (&format!("untrust {mac}"), 700, &["untrust succeeded", "not available"]),
            (&format!("disconnect {mac}"), 3000, &["Successful disconnected", "not connected", "not available"]),
        ],
        Duration::from_secs(8),
    )
    .map(|_| ())
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
    fn the_sound_link_is_read_from_bluealsa() {
        let list = "**** List of PLAYBACK Bluetooth Devices ****\nhci0: 11:75:58:E8:92:34 [Ditoo-Plus-audio], trusted unknown\n  A2DP (SBC): S16_LE 2 channels 48000 Hz\n**** List of CAPTURE Bluetooth Devices ****\nhci0: AA:BB:CC:DD:EE:FF [Phone], trusted phone\n";
        assert!(in_playback_list(list, "11:75:58:e8:92:34"));
        assert!(!in_playback_list(list, "AA:BB:CC:DD:EE:FF"), "a phone playing to the Station is not an output");
        assert!(!in_playback_list("**** List of PLAYBACK Bluetooth Devices ****\n**** List of CAPTURE Bluetooth Devices ****\n", "11:75:58:E8:92:34"), "connected over LE only");
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
        assert_eq!(with_source(&a("-p a2dp-sink -i hci0")), Some(a("-p a2dp-sink -i hci0 -p a2dp-source --sbc-quality=medium")));
        assert_eq!(with_source(&a("-p a2dp-sink -p a2dp-source --sbc-quality=medium")), None);
        assert_eq!(with_source(&a("--profile=a2dp-sink --profile=a2dp-source --sbc-quality=low")), None);
        // The Station's own start line, after soundya made it send: the encoder is eased.
        assert_eq!(
            with_source(&a("-p a2dp-sink -p a2dp-source -p hfp-ag --sbc-quality=high")),
            Some(a("-p a2dp-sink -p a2dp-source -p hfp-ag --sbc-quality=medium"))
        );
        // No profiles given: BlueALSA already does both.
        assert_eq!(with_source(&a("-i hci0 --sbc-quality=medium")), None);
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
