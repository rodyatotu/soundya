//! Optional PIN for the panel (off by default: before 0.5.00 anyone on the
//! Wi-Fi could change everything, and that stays so unless a PIN is set).
//!
//! With a PIN, every request that changes something needs a session cookie;
//! the browser gets one by entering the PIN once (kept for a year, and over
//! reboots in <state dir>/sessions). Reading the status stays open. Wrong PINs:
//! at most 5 a minute. Forgot it: `soundya pin off` over adb.
//! Devices reporting calls (/api/duck) do not need it: the worst they can do is
//! lower the music for a while.

use std::{
    collections::HashSet,
    io::Read,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

static PIN: Mutex<String> = Mutex::new(String::new());
static SESSIONS: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static FAILS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
pub const COOKIE: &str = "soundya_s";

fn sessions_file(dir: &Path) -> std::path::PathBuf {
    dir.join("sessions")
}

pub fn init(dir: &Path, pin: &str) {
    *PIN.lock().unwrap() = pin.trim().to_owned();
    let saved = crate::fsx::read_string(&sessions_file(dir)).unwrap_or_default();
    *SESSIONS.lock().unwrap() = Some(saved.lines().map(str::trim).filter(|l| l.len() == 32).map(str::to_owned).collect());
}

pub fn enabled() -> bool {
    !PIN.lock().unwrap().is_empty()
}

/// New PIN ("" = off): every browser has to enter it again.
pub fn set_pin(dir: &Path, pin: &str) {
    *PIN.lock().unwrap() = pin.trim().to_owned();
    *SESSIONS.lock().unwrap() = Some(HashSet::new());
    save(dir);
}

fn save(dir: &Path) {
    let list = SESSIONS.lock().unwrap().clone().unwrap_or_default().into_iter().collect::<Vec<_>>().join("\n");
    if let Err(e) = crate::settings::write_synced(&sessions_file(dir), list.as_bytes()) {
        eprintln!("auth: {e}");
    }
}

/// The session from a Cookie header, if valid.
pub fn session_ok(cookie_header: &str) -> bool {
    let Some(token) = cookie_header.split(';').find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}="))) else {
        return false;
    };
    SESSIONS.lock().unwrap().as_ref().is_some_and(|s| s.contains(token.trim()))
}

/// Checks the PIN; Ok(new session token) or Err(why).
pub fn login(dir: &Path, pin: &str) -> Result<String, String> {
    {
        let mut f = FAILS.lock().unwrap();
        f.retain(|t| t.elapsed() < Duration::from_secs(60));
        if f.len() >= 5 {
            return Err("слишком много попыток, подождите минуту".into());
        }
        if !enabled() || pin.trim() != *PIN.lock().unwrap() {
            f.push(Instant::now());
            return Err("неверный PIN".into());
        }
    }
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| e.to_string())?;
    let token: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let mut s = SESSIONS.lock().unwrap();
    let set = s.get_or_insert_with(HashSet::new);
    // A handful of browsers per speaker; the oldest are not tracked, so cap it.
    if set.len() > 50 {
        set.clear();
    }
    set.insert(token.clone());
    drop(s);
    save(dir);
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_and_sessions() {
        let d = std::env::temp_dir().join(format!("soundya-auth-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        init(&d, "");
        assert!(!enabled());
        set_pin(&d, "4821");
        assert!(enabled());
        assert!(login(&d, "0000").is_err());
        let t = login(&d, " 4821 ").unwrap();
        assert!(session_ok(&format!("a=b; {COOKIE}={t}")));
        assert!(!session_ok(&format!("{COOKIE}=nope")));
        // Survives a restart.
        init(&d, "4821");
        assert!(session_ok(&format!("{COOKIE}={t}")));
        // A new PIN logs everybody out.
        set_pin(&d, "1111");
        assert!(!session_ok(&format!("{COOKIE}={t}")));
        // Brute force is slowed down.
        for _ in 0..6 {
            let _ = login(&d, "9999");
        }
        assert!(login(&d, "1111").unwrap_err().contains("попыток"));
        set_pin(&d, "");
        let _ = std::fs::remove_dir_all(&d);
    }
}
