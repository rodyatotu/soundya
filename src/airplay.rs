//! AirPlay receiver: the speaker shows up in the AirPlay list on iPhone, iPad
//! and Mac (classic AirPlay, "AirPlay 1").
//!
//! The protocol is handled by shairport-sync (built for the speaker, shipped
//! next to soundya). It writes the received sound to its stdout as raw
//! 44.1 kHz 16-bit stereo; soundya plays it through the same ALSA output as
//! SoundCloud, pauses SoundCloud when AirPlay starts, keeps the ring orange and
//! turns the sound down while Alice talks.
//!
//! shairport-sync announces itself by running `avahi-publish-service`. That
//! name is a link to soundya (see [`publisher_main`]), which hands the service
//! to soundya's own mDNS responder: the speaker has no Avahi.

use std::{
    io::{BufRead, BufReader, Read},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{alsa, mdns, Cmd, Shared};

const RATE: u32 = 44_100;
const CHANNELS: usize = 2;
const PORT: u16 = 5000;
/// Where the fake avahi-publish-service leaves what shairport-sync wants announced.
const ANNOUNCE: &str = "/tmp/soundya-airplay.mdns";
const PUBLISHER: &str = "avahi-publish-service";
/// No sound for this long: the sender paused or disconnected.
const IDLE: Duration = Duration::from_millis(1500);
/// Sound kept queued in the card. shairport-sync sends in real time and cannot
/// see the card, so soundya keeps the queue level itself: the iPhone's clock and
/// the speaker's never run at exactly the same speed. A full card would block
/// shairport-sync, which then drops late sound (audible stutter).
const TARGET_MS: usize = 250;
/// Within ±this, nothing is corrected.
const BAND_MS: usize = 60;
/// While correcting, one frame in this many is dropped or doubled (0.25 %, inaudible).
const CORRECT_EVERY: u64 = 400;
const FRAME_BYTES: usize = CHANNELS * 2;
pub const DEFAULT_NAME: &str = "Яндекс Станция";

/// State shared with the controller and the web page.
pub struct AirPlay {
    enabled: AtomicBool,
    name: Mutex<String>,
    /// Sound is coming in right now.
    active: AtomicBool,
    /// shairport-sync is running and announced.
    ready: AtomicBool,
    /// Drop the current sender (SoundCloud was started by voice).
    kick: AtomicBool,
    /// Bumped when the name or switch changes: restart shairport-sync.
    config: AtomicU32,
    error: Mutex<Option<String>>,
}

impl AirPlay {
    pub fn new(enabled: bool, name: &str) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            name: Mutex::new(clean_name(name)),
            active: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            kick: AtomicBool::new(false),
            config: AtomicU32::new(0),
            error: Mutex::new(None),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Settings changed on the page.
    pub fn configure(&self, enabled: bool, name: &str) {
        let name = clean_name(name);
        let changed = self.enabled.swap(enabled, Ordering::AcqRel) != enabled
            || std::mem::replace(&mut *self.name.lock().unwrap(), name.clone()) != name;
        if changed {
            self.config.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Ends the current AirPlay session: SoundCloud takes over the speaker.
    pub fn interrupt(&self) {
        if self.is_active() {
            eprintln!("airplay: SoundCloud started; dropping the AirPlay sender");
            self.kick.store(true, Ordering::Release);
        }
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled.load(Ordering::Acquire),
            "name": *self.name.lock().unwrap(),
            "ready": self.ready.load(Ordering::Acquire),
            "active": self.is_active(),
            "error": *self.error.lock().unwrap(),
        })
    }
}

/// The name shown on the iPhone: one line, at most 50 characters (shairport-sync's limit).
pub fn clean_name(name: &str) -> String {
    let n: String = name
        .chars()
        .map(|c| if c.is_control() || c == '"' || c == '\\' || c == '.' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let n: String = n.chars().take(50).collect();
    if n.is_empty() {
        DEFAULT_NAME.into()
    } else {
        n
    }
}

/// shairport-sync's settings (libconfig syntax).
fn config_text(name: &str) -> String {
    format!(
        r#"// Written by soundya on every start; edit the name on the settings page.
general = {{
  name = "{name}";
  output_backend = "stdout";
  mdns_backend = "external-avahi";
  port = {PORT};
  udp_port_base = 6001;
  udp_port_range = 10;
  volume_range_db = 50;
}};
sessioncontrol = {{
  allow_session_interruption = "yes";
  session_timeout = 60;
}};
stdout = {{
  output_rate = {RATE};
  output_format = "S16_LE";
  output_channels = {CHANNELS};
  // soundya keeps 0.25 s of sound in the card: send it that much earlier, so
  // video on the iPhone stays in sync with the sound.
  audio_backend_latency_offset_in_seconds = -0.25;
}};
"#
    )
}

/// Run as `avahi-publish-service <name> <type> <port> <txt>…` by shairport-sync:
/// hands the service over to the soundya daemon, then waits to be stopped.
pub fn publisher_main(args: &[String]) -> i32 {
    let _ = std::fs::write(ANNOUNCE, args.join("\n"));
    let parent = unsafe { getppid() };
    // Ends with shairport-sync (it sends SIGTERM; if it was killed, we notice).
    while unsafe { getppid() } == parent {
        thread::sleep(Duration::from_secs(1));
    }
    0
}

extern "C" {
    fn getppid() -> i32;
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

const PR_SET_PDEATHSIG: i32 = 1;
const SIGKILL: i32 = 9;

/// A receiver left over from a soundya that crashed holds the AirPlay port.
fn kill_stale() {
    let Ok(dir) = std::fs::read_dir("/proc") else { return };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let comm = crate::fsx::read_string(&entry.path().join("comm")).unwrap_or_default();
        if comm.trim() == "shairport-sync" {
            eprintln!("airplay: stopping a leftover receiver (pid {pid})");
            unsafe { kill(pid, SIGKILL) };
        }
    }
}

/// What the fake publisher wrote: name, type, port, TXT entries.
fn parse_announce(text: &str) -> Option<mdns::Service> {
    let mut lines = text.lines();
    let instance = lines.next()?.trim().to_owned();
    let kind = lines.next()?.trim().to_owned();
    let port = lines.next()?.trim().parse().ok()?;
    if instance.is_empty() || !kind.starts_with('_') {
        return None;
    }
    let txt = lines.map(str::to_owned).filter(|l| !l.is_empty()).collect();
    Some(mdns::Service { instance, kind, port, txt })
}

/// Directory with `avahi-publish-service` → soundya, put first on shairport-sync's PATH.
fn publisher_dir(dir: &Path) -> Option<PathBuf> {
    let bin = dir.join("airplay-bin");
    crate::fsx::ensure_dir(&bin);
    let link = bin.join(PUBLISHER);
    let exe = std::env::current_exe().ok()?;
    if std::fs::read_link(&link).ok().as_deref() != Some(exe.as_path()) {
        let _ = std::fs::remove_file(&link);
        if let Err(e) = std::os::unix::fs::symlink(&exe, &link) {
            eprintln!("airplay: cannot link {}: {e}", link.display());
            return None;
        }
    }
    Some(bin)
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Starts the receiver if `shairport-sync` is installed next to soundya.
pub fn run(dir: PathBuf, device: String, tx: Sender<Cmd>, shared: Arc<Shared>) {
    let bin = dir.join("shairport-sync");
    if !crate::fsx::exists(&bin) {
        eprintln!("airplay: {} not installed; AirPlay is off", bin.display());
        *shared.airplay.error.lock().unwrap() = Some("приёмник AirPlay не установлен".into());
        return;
    }
    kill_stale();
    thread::spawn(move || {
        let air = &shared.airplay;
        let mut failures = 0u32;
        loop {
            if !air.enabled.load(Ordering::Acquire) {
                thread::sleep(Duration::from_secs(1));
                continue;
            }
            let seen_config = air.config.load(Ordering::Acquire);
            let started = Instant::now();
            match session(&bin, &dir, &device, &tx, &shared, seen_config) {
                Ok(()) => failures = 0,
                Err(e) => {
                    eprintln!("airplay: {e}");
                    *air.error.lock().unwrap() = Some(e);
                    if started.elapsed() < Duration::from_secs(30) {
                        failures += 1;
                    }
                }
            }
            air.ready.store(false, Ordering::Release);
            mdns::withdraw("_raop._tcp");
            if air.active.swap(false, Ordering::AcqRel) {
                let _ = tx.send(Cmd::AirPlay(false));
            }
            // Crashing right away again and again: wait longer each time (max 5 min).
            thread::sleep(Duration::from_secs((2u64 << failures.min(7)).min(300)));
        }
    });
}

/// Runs shairport-sync until it exits or the settings change.
fn session(
    bin: &Path,
    dir: &Path,
    device: &str,
    tx: &Sender<Cmd>,
    shared: &Arc<Shared>,
    seen_config: u32,
) -> Result<(), String> {
    let air = &shared.airplay;
    let name = air.name.lock().unwrap().clone();
    let conf = dir.join("airplay.conf");
    std::fs::write(&conf, config_text(&name)).map_err(|e| format!("cannot write {}: {e}", conf.display()))?;
    let pubdir = publisher_dir(dir).ok_or("cannot set up the AirPlay announcement")?;
    let _ = std::fs::remove_file(ANNOUNCE);
    let path = format!("{}:{}", pubdir.display(), std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()));
    let mut command = Command::new(bin);
    command.arg("-c").arg(&conf).env("PATH", path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Dies with soundya (this thread lives as long as soundya does).
    unsafe {
        command.pre_exec(|| {
            prctl(PR_SET_PDEATHSIG, SIGKILL as u64, 0, 0, 0);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start shairport-sync: {e}"))?;
    eprintln!("airplay: receiver «{name}» started (pid {})", child.id());

    // Its messages go to soundya's log.
    if let Some(err) = child.stderr.take() {
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("airplay: {line}");
            }
        });
    }
    // Sound, in blocks; a bounded queue so a stuck card holds shairport-sync back.
    let (sound_tx, sound) = mpsc::sync_channel::<Vec<u8>>(16);
    let mut out = child.stdout.take().ok_or("no stdout")?;
    thread::spawn(move || {
        let mut buf = vec![0u8; 4096];
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sound_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut output: Option<alsa::Output> = None;
    // The card refused to open: try again a little later, not for every block.
    let mut open_failed: Option<Instant> = None;
    let mut last_sound: Option<Instant> = None;
    // Bytes of an incomplete frame, kept for the next block.
    let mut carry: Vec<u8> = Vec::new();
    let mut drift = Drift::default();
    let mut stats_at = Instant::now();
    let mut gain = crate::player::Gain::default();
    let mut announced = false;
    let mut samples: Vec<i16> = Vec::new();
    let result = loop {
        if !announced {
            if let Some(service) = crate::fsx::read_string(Path::new(ANNOUNCE)).and_then(|t| parse_announce(&t)) {
                eprintln!("airplay: announcing «{}» on port {}", service.instance, service.port);
                mdns::publish(service);
                air.ready.store(true, Ordering::Release);
                *air.error.lock().unwrap() = None;
                announced = true;
            }
        }
        if air.config.load(Ordering::Acquire) != seen_config || !air.enabled.load(Ordering::Acquire) {
            eprintln!("airplay: settings changed; restarting the receiver");
            break Ok(());
        }
        if air.kick.swap(false, Ordering::AcqRel) {
            // shairport-sync has no "hang up" command: restarting it drops the sender.
            break Ok(());
        }
        match sound.recv_timeout(Duration::from_millis(250)) {
            Ok(block) => {
                // Bytes → little-endian samples, whole frames only (a block may end mid-frame).
                carry.extend_from_slice(&block);
                let whole = carry.len() / FRAME_BYTES * FRAME_BYTES;
                samples.clear();
                samples.extend(carry[..whole].chunks_exact(2).map(|p| i16::from_le_bytes([p[0], p[1]])));
                carry.drain(..whole);
                if samples.iter().all(|&s| s == 0) {
                    if !air.is_active() {
                        continue; // silence before the music starts or after it stopped
                    }
                } else {
                    last_sound = Some(Instant::now());
                }
                if !air.active.swap(true, Ordering::AcqRel) {
                    eprintln!("airplay: playing");
                    let _ = tx.send(Cmd::AirPlay(true));
                }
                if output.is_none() && open_failed.map_or(true, |t| t.elapsed() > Duration::from_secs(2)) {
                    match alsa::Output::open(device, RATE, CHANNELS) {
                        Ok(o) => {
                            output = Some(o);
                            open_failed = None;
                        }
                        Err(e) => {
                            eprintln!("airplay: cannot open {device}: {e}");
                            open_failed = Some(Instant::now());
                        }
                    }
                }
                gain.apply(&mut samples, CHANNELS, RATE, shared.duck.active());
                if let Some(o) = output.as_mut() {
                    let queued = o.queued();
                    let corrected = drift.correct(&samples, queued);
                    if let Err(e) = o.write(corrected) {
                        eprintln!("airplay: sound card: {e}; reopening");
                        output = None;
                    } else if o.queued().is_some_and(|q| q >= ms_to_frames(TARGET_MS)) {
                        o.start();
                    }
                }
                if stats_at.elapsed() > Duration::from_secs(30) {
                    stats_at = Instant::now();
                    let q = output.as_ref().and_then(|o| o.queued()).map(|f| f * 1000 / RATE as usize);
                    eprintln!(
                        "airplay: queue {} ms, frames dropped {}, doubled {}",
                        q.map_or("?".into(), |q| q.to_string()),
                        drift.dropped,
                        drift.doubled
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = child.wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                break Err(format!("shairport-sync stopped ({status})"));
            }
        }
        // Nothing (or only silence) for a while: the sender paused or left. Free the card.
        if air.is_active() && last_sound.is_some_and(|t| t.elapsed() > IDLE) {
            eprintln!("airplay: stopped");
            if let Some(mut o) = output.take() {
                o.finish();
            }
            eprintln!("airplay: frames dropped {}, doubled {}", drift.dropped, drift.doubled);
            drift = Drift::default();
            air.active.store(false, Ordering::Release);
            let _ = tx.send(Cmd::AirPlay(false));
        }
    };
    drop(output);
    stop(&mut child);
    let _ = std::fs::remove_file(ANNOUNCE);
    result
}

fn ms_to_frames(ms: usize) -> usize {
    RATE as usize * ms / 1000
}

/// Keeps the card's queue near [`TARGET_MS`] by dropping or doubling single frames.
#[derive(Default)]
struct Drift {
    frame: u64,
    dropped: u64,
    doubled: u64,
    out: Vec<i16>,
}

impl Drift {
    /// `samples`: whole interleaved frames. `queued`: frames waiting in the card.
    fn correct<'a>(&'a mut self, samples: &'a [i16], queued: Option<usize>) -> &'a [i16] {
        let Some(q) = queued else { return samples };
        let (target, band) = (ms_to_frames(TARGET_MS), ms_to_frames(BAND_MS));
        let drop = q > target + band;
        let double = q + band < target && q > 0; // q == 0: not started yet, just filling
        if !drop && !double {
            self.frame += (samples.len() / CHANNELS) as u64;
            return samples;
        }
        self.out.clear();
        for f in samples.chunks_exact(CHANNELS) {
            self.frame += 1;
            if self.frame % CORRECT_EVERY == 0 {
                if drop {
                    self.dropped += 1;
                    continue;
                }
                self.out.extend_from_slice(f);
                self.doubled += 1;
            }
            self.out.extend_from_slice(f);
        }
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(clean_name("  Кухня  "), "Кухня");
        assert_eq!(clean_name(""), DEFAULT_NAME);
        assert_eq!(clean_name("a\"b\\c.d\ne"), "a b c d e");
        assert_eq!(clean_name(&"я".repeat(80)).chars().count(), 50);
    }

    #[test]
    fn reads_announcement() {
        let s = parse_announce("EDF9EDF2DCD3@Станция\n_raop._tcp\n5000\nsf=0x4\nch=2\ntxtvers=1\npw=false\n").unwrap();
        assert_eq!(s.instance, "EDF9EDF2DCD3@Станция");
        assert_eq!(s.kind, "_raop._tcp");
        assert_eq!(s.port, 5000);
        assert_eq!(s.txt, vec!["sf=0x4", "ch=2", "txtvers=1", "pw=false"]);
        assert!(parse_announce("x\n_raop._tcp\nport\n").is_none());
        assert!(parse_announce("").is_none());
    }

    #[test]
    fn drift_correction() {
        let block: Vec<i16> = (0..800).collect(); // 400 stereo frames
        let mut d = Drift::default();
        // Near the target: untouched.
        assert_eq!(d.correct(&block, Some(ms_to_frames(TARGET_MS))).len(), 800);
        // Too full: one frame of 400 dropped.
        assert_eq!(d.correct(&block, Some(ms_to_frames(400))).len(), 798);
        // Too empty (but playing): one frame doubled.
        assert_eq!(d.correct(&block, Some(ms_to_frames(100))).len(), 802);
        // Not started (empty): just fill.
        assert_eq!(d.correct(&block, Some(0)).len(), 800);
        assert_eq!(d.correct(&block, None).len(), 800);
        assert_eq!((d.dropped, d.doubled), (1, 1));
        // Frames stay whole: L/R pairs are never split.
        let out = d.correct(&block, Some(ms_to_frames(400))).to_vec();
        assert!(out.chunks_exact(2).all(|f| f[1] == f[0] + 1 && f[0] % 2 == 0));
    }

    #[test]
    fn config_has_name_and_stdout() {
        let c = config_text("Кухня");
        assert!(c.contains("name = \"Кухня\";"));
        assert!(c.contains("output_backend = \"stdout\";"));
        assert!(c.contains("port = 5000;"));
    }
}
