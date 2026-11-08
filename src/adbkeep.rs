//! Keeps the speaker's network ADB (port 5555) on, at the owner's request: after a
//! restart the station's adbd listens on USB only, and then the Mac/Windows app and
//! `adb connect` cannot reach it. Checked once a minute; when nothing listens on
//! 5555, adbd is told to listen there (the Android properties the station's adbd
//! reads) and restarted. Nothing is done while it already listens, so open ADB
//! sessions are never cut. Switched off in the settings («ADB по сети»).
//!
//! Not checked on a real speaker: which of the commands the firmware honours is
//! seen in the log («adb: …»).

use std::{
    net::{SocketAddr, TcpStream},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

pub const PORT: u16 = 5555;
static ON: AtomicBool = AtomicBool::new(false);

/// The owner's switch. Turning it off (on → off, not at start-up) undoes what keeps
/// network ADB coming back: the locked flag files and the persistent port. The ADB
/// session open now is not cut; after the next restart ADB listens on USB only.
pub fn configure(on: bool) {
    let was = ON.swap(on, Ordering::AcqRel);
    if was && !on && cfg!(not(test)) {
        thread::spawn(|| match Command::new("sh").arg("-c").arg(off_script()).output() {
            Ok(o) => eprintln!("adb: network ADB will stay closed after restart ({})", String::from_utf8_lossy(&o.stdout).trim()),
            Err(e) => eprintln!("adb: switch-off failed: {e}"),
        });
    }
}

/// The commands that stop network ADB from coming back at the next start. Only the
/// network switch is removed: `activate_adb_quasmodrom` (README step 1.6, made by the
/// owner while rooting) stays, so ADB itself is not taken away. Says what really happened.
pub fn off_script() -> String {
    let f = FLAGS[1];
    format!(
        "chattr -i {f} 2>/dev/null; rm -f {f}; setprop persist.adb.tcp.port '' 2>/dev/null; \
         if [ -e {f} ]; then echo \"{f} not removed (chattr?)\"; else echo done; fi"
    )
}

/// The station's own switches for network ADB (README, step 1.6): Yandex's quasar
/// turns network ADB on at start while these files exist. Locked (chattr +i) so
/// nothing removes them.
pub const FLAGS: [&str; 2] = ["/data/quasar/activate_adb_quasmodrom", "/data/quasar/activate_network_adb"];

/// The flag files back, when one is missing (they take effect at the next start).
pub fn flags_script() -> String {
    let mut s = String::from("mkdir -p /data/quasar; ");
    for f in FLAGS {
        s.push_str(&format!("[ -e {f} ] || {{ touch {f}; chattr +i {f} 2>/dev/null; echo restored {f}; }}; "));
    }
    s
}

/// The commands that turn network ADB on now: the port for this boot and for the next
/// ones, then adbd restarted (by init where there is one, else by hand).
pub fn script() -> String {
    format!(
        "setprop service.adb.tcp.port {PORT} 2>/dev/null; setprop persist.adb.tcp.port {PORT} 2>/dev/null; \
         if command -v stop >/dev/null 2>&1 && command -v start >/dev/null 2>&1; then stop adbd; start adbd; \
         else pid=$(pidof adbd 2>/dev/null); [ -n \"$pid\" ] && kill $pid; sleep 1; \
         pidof adbd >/dev/null 2>&1 || (ADBD_PORT={PORT} adbd >/dev/null 2>&1 &); fi; echo done"
    )
}

fn listening() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok()
}

/// What to do now: (switched on, something listens on 5555) → run the script?
pub fn needs_fix(on: bool, listening: bool) -> bool {
    on && !listening
}

pub fn run() {
    thread::spawn(|| {
        // Let the station finish booting (its own adbd starts late).
        thread::sleep(Duration::from_secs(45));
        let mut tries = 0u32;
        loop {
            // The station's switches first: they bring network ADB back after every restart.
            if ON.load(Ordering::Acquire) {
                if let Ok(o) = Command::new("sh").arg("-c").arg(flags_script()).output() {
                    let said = String::from_utf8_lossy(&o.stdout);
                    if !said.trim().is_empty() {
                        eprintln!("adb: {}", said.trim().replace('\n', "; "));
                    }
                }
            }
            if needs_fix(ON.load(Ordering::Acquire), listening()) {
                tries += 1;
                // Not forever every minute if the firmware ignores it: then every 10 minutes.
                if tries <= 3 || tries % 10 == 0 {
                    let out = Command::new("sh").arg("-c").arg(script()).output();
                    thread::sleep(Duration::from_secs(3));
                    match out {
                        Ok(_) if listening() => eprintln!("adb: network ADB switched on (port {PORT})"),
                        Ok(o) => eprintln!(
                            "adb: still not listening on {PORT} after the switch-on ({})",
                            String::from_utf8_lossy(&o.stderr).trim().chars().take(200).collect::<String>()
                        ),
                        Err(e) => eprintln!("adb: switch-on failed: {e}"),
                    }
                }
            } else {
                tries = 0;
            }
            thread::sleep(Duration::from_secs(60));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_when_switched_on_and_closed() {
        assert!(needs_fix(true, false));
        assert!(!needs_fix(true, true), "already listening: open sessions are not cut");
        assert!(!needs_fix(false, false), "switched off in the settings");
        let s = script();
        assert!(s.contains("service.adb.tcp.port 5555") && s.contains("persist.adb.tcp.port 5555"));
        assert!(s.contains("stop adbd; start adbd"));
        let f = flags_script();
        assert!(f.contains("[ -e /data/quasar/activate_network_adb ] || { touch /data/quasar/activate_network_adb; chattr +i"), "{f}");
        assert!(f.contains("activate_adb_quasmodrom"));
        let off = off_script();
        assert!(off.contains("chattr -i /data/quasar/activate_network_adb 2>/dev/null; rm -f /data/quasar/activate_network_adb"), "{off}");
        assert!(off.contains("persist.adb.tcp.port ''"));
        assert!(!off.contains("quasmodrom"), "the owner's own ADB switch stays");
        assert!(!off.contains("stop adbd"), "the open session is not cut");
        assert!(off.contains("not removed"), "a failed removal is said, not «done»");
    }
}
