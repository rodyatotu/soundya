//! Updates from GitHub: the page shows when a newer release is out, and one
//! button downloads it and installs it on the speaker by itself.
//!
//! The release's soundya-armv7.tar is the same file deploy.sh and the Windows
//! installer use. Installing runs its install.sh in its own session, so it keeps
//! going while it stops and restarts soundya.

use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use serde_json::Value;

const RELEASES: &str = "https://api.github.com/repos/rodyatotu/soundya/releases/latest";
const ASSET: &str = "soundya-armv7.tar";
const TAR: &str = "/tmp/soundya-update.tar";
const DIR: &str = "/tmp/soundya-update";
const LOG: &str = "/tmp/soundya-update.log";
/// Checked at most this often on its own (the page's button checks at once).
const EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    url: String,
    /// The same file through the API (another host, another certificate).
    api_url: String,
    size: u64,
    /// "sha256:<hex>" as GitHub reports it for the asset ("" on older releases).
    digest: String,
}

#[derive(Default)]
pub struct Updates {
    latest: Mutex<Option<Release>>,
    checked: Mutex<Option<Instant>>,
    error: Mutex<Option<String>>,
    state: Mutex<Option<String>>,
}

/// "v0.4.10" → (0, 4, 10); anything unreadable is (0, 0, 0).
fn parse_version(v: &str) -> (u64, u64, u64) {
    let mut n = v.trim().trim_start_matches(['v', 'V']).split(['.', '-', '+']).map(|p| p.parse().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0))
}

pub fn newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn release_from(v: &Value) -> Option<Release> {
    let asset = v.get("assets")?.as_array()?.iter().find(|a| a.get("name").and_then(Value::as_str) == Some(ASSET))?;
    Some(Release {
        version: v.get("tag_name")?.as_str()?.trim_start_matches('v').to_owned(),
        notes: v.get("body").and_then(Value::as_str).unwrap_or("").chars().take(1500).collect(),
        url: asset.get("browser_download_url")?.as_str()?.to_owned(),
        api_url: asset.get("url").and_then(Value::as_str).unwrap_or("").to_owned(),
        size: asset.get("size").and_then(Value::as_u64).unwrap_or(0),
        digest: asset.get("digest").and_then(Value::as_str).unwrap_or("").to_owned(),
    })
}

impl Updates {
    pub fn due(&self) -> bool {
        self.checked.lock().unwrap().map_or(true, |t| t.elapsed() > EVERY)
    }

    /// Asked within the last `secs` seconds (GitHub allows 60 questions an hour).
    pub fn fresh(&self, secs: u64) -> bool {
        self.checked.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(secs))
    }

    /// Asks GitHub for the latest release (through the SoundCloud proxy, if any).
    pub fn check(&self, proxy: &str) -> Result<Option<Release>, String> {
        *self.checked.lock().unwrap() = Some(Instant::now());
        let result = match fetch_latest(proxy) {
            // The speaker's clock was wrong: once it is set, ask again.
            Err(e) if crate::clock::is_clock_error(&e) && crate::clock::fix(proxy).is_ok() => fetch_latest(proxy),
            other => other,
        };
        match result {
            Ok(r) => {
                *self.error.lock().unwrap() = None;
                *self.latest.lock().unwrap() = Some(r.clone());
                Ok(newer(&r.version, env!("CARGO_PKG_VERSION")).then_some(r))
            }
            Err(e) => {
                eprintln!("update: cannot check: {e}");
                *self.error.lock().unwrap() = Some(e.clone());
                Err(e)
            }
        }
    }

    pub fn json(&self) -> Value {
        let latest = self.latest.lock().unwrap().clone();
        let current = env!("CARGO_PKG_VERSION");
        serde_json::json!({
            "current": current,
            "latest": latest.as_ref().map(|r| r.version.clone()),
            "available": latest.as_ref().is_some_and(|r| newer(&r.version, current)),
            "notes": latest.as_ref().map(|r| r.notes.clone()),
            "installing": INSTALLING.load(Ordering::Acquire),
            "state": *self.state.lock().unwrap(),
            "error": *self.error.lock().unwrap(),
            "last": last_result(&state_dir()),
        })
    }

    fn set_state(&self, s: &str) {
        eprintln!("update: {s}");
        *self.state.lock().unwrap() = Some(s.to_owned());
    }

    /// Downloads the latest release and starts its installer. Returns at once
    /// with the installer running; it restarts soundya when done.
    pub fn install(&self, proxy: &str) -> Result<String, String> {
        // A Luna package is YAM's to update: our own installer would put a
        // second, standalone copy next to it.
        if crate::luna::active() {
            return Err("soundya стоит как пакет Luna: обновите его через YAM — yam install rodyatotu/soundya".into());
        }
        if INSTALLING.swap(true, Ordering::AcqRel) {
            return Err("обновление уже устанавливается".into());
        }
        *LAST_STUCK.lock().unwrap() = None;
        let result = self.install_inner(proxy);
        if result.is_err() {
            INSTALLING.store(false, Ordering::Release);
        }
        result
    }

    fn install_inner(&self, proxy: &str) -> Result<String, String> {
        let release = match self.check(proxy)? {
            Some(r) => r,
            None => return Err(format!("уже стоит последняя версия {}", env!("CARGO_PKG_VERSION"))),
        };
        self.set_state(&format!("скачиваю {}…", release.version));
        let mut last = String::new();
        let mut done = false;
        // Through the proxy and directly, the download link and the API link; a wrong
        // clock on the speaker breaks every HTTPS site, so it is fixed and tried again.
        'outer: for attempt in 0..2 {
            for via_proxy in [true, false] {
                if via_proxy && proxy.trim().is_empty() {
                    continue;
                }
                for (url, api) in [(&release.url, false), (&release.api_url, true)] {
                    if url.is_empty() {
                        continue;
                    }
                    match download(url, api, if via_proxy { proxy } else { "" }, release.size) {
                        Ok(()) => {
                            done = true;
                            break 'outer;
                        }
                        Err(e) => {
                            eprintln!("update: {url}: {e}");
                            last = e;
                        }
                    }
                }
            }
            if attempt == 0 && crate::clock::is_clock_error(&last) {
                match crate::clock::fix(proxy) {
                    Ok(skew) => eprintln!("update: clock corrected by {skew} s; trying again"),
                    Err(e) => {
                        last = format!("часы колонки неверны, и поправить их не вышло: {e}");
                        break;
                    }
                }
            } else {
                break;
            }
        }
        if !done {
            return Err(format!("не скачалось: {last}"));
        }
        if let Err(e) = verify_digest(TAR, &release.digest) {
            let _ = std::fs::remove_file(TAR);
            return Err(e);
        }
        self.set_state(&format!("устанавливаю {}…", release.version));
        // Unpack, then run the installer in its own session: it stops soundya
        // (this very process) and starts the new one. The pause lets this
        // answer reach the page first.
        let script = format!(
            "rm -rf {DIR} && mkdir -p {DIR} && tar -xf {TAR} -C {DIR} && [ -f {DIR}/soundya/install.sh ] && \
             (nohup setsid sh -c 'sleep 2; sh {DIR}/soundya/install.sh' > {LOG} 2>&1 < /dev/null &) && echo started"
        );
        let out = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("не запустился установщик: {e}"))?;
        if !String::from_utf8_lossy(&out.stdout).contains("started") {
            return Err(format!("архив не распаковался: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        // If this process is still alive long after, the installer stopped
        // before restarting soundya: say so instead of «устанавливаю…» forever.
        // (When it works, or rolls back, this process is replaced first.)
        let started = Instant::now();
        let version = release.version.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(240));
            let tail = crate::fsx::read_string(std::path::Path::new(LOG)).unwrap_or_default();
            let tail: String = tail.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" / ");
            eprintln!("update: installer for {version} did not finish in {} s: {tail}", started.elapsed().as_secs());
            INSTALLING.store(false, Ordering::Release);
            *LAST_STUCK.lock().unwrap() = Some(format!("установка {version} не завершилась: {tail}"));
        });
        Ok(format!("Устанавливаю soundya {}. Через полминуты страница обновится сама", release.version))
    }
}

/// An update is being installed (there is one updater per process).
static INSTALLING: AtomicBool = AtomicBool::new(false);
static LAST_STUCK: Mutex<Option<String>> = Mutex::new(None);

fn state_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("SOUNDYA_DIR").unwrap_or_else(|_| "/data/opt/soundya".into()))
}

/// What install.sh wrote about the last update (status ok | rolledback | failed).
pub fn last_result(dir: &std::path::Path) -> Value {
    if let Some(stuck) = LAST_STUCK.lock().unwrap().clone() {
        return serde_json::json!({ "status": "failed", "reason": stuck });
    }
    let Some(text) = crate::fsx::read_string(&dir.join("update-result")) else {
        return Value::Null;
    };
    let mut out = serde_json::Map::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if matches!(k, "status" | "from" | "to" | "time" | "reason") {
                let v = v.trim();
                out.insert(k.to_owned(), if k == "time" { v.parse::<u64>().map(Value::from).unwrap_or(Value::Null) } else { Value::from(v) });
            }
        }
    }
    Value::Object(out)
}

/// Checks the downloaded file against the SHA-256 GitHub reports for the asset,
/// with the speaker's own `sha256sum` (busybox). Without a digest (older
/// releases) or without the tool only the size is checked, as before.
fn verify_digest(path: &str, digest: &str) -> Result<(), String> {
    let Some(want) = digest.strip_prefix("sha256:").map(str::to_lowercase) else {
        eprintln!("update: the release gives no sha256; checked by size only");
        return Ok(());
    };
    let out = match Command::new("sha256sum").arg(path).output() {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("update: sha256sum not available; checked by size only");
            return Ok(());
        }
    };
    let got = String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("").to_lowercase();
    if got == want {
        eprintln!("update: sha256 OK");
        Ok(())
    } else {
        Err(format!("скачанный архив повреждён (sha256 {got} вместо {want})"))
    }
}

fn fetch_latest(proxy: &str) -> Result<Release, String> {
    let agent = crate::sc::build_agent(proxy).map_err(|e| e.to_string())?;
    let v: Value = agent
        .get(RELEASES)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;
    release_from(&v).ok_or_else(|| "в последнем релизе нет сборки".to_owned())
}

/// Downloads one URL to TAR and checks its size.
fn download(url: &str, api: bool, proxy: &str, size: u64) -> Result<(), String> {
    let agent = crate::sc::build_agent(proxy).map_err(|e| e.to_string())?;
    let mut req = agent.get(url);
    if api {
        req = req.set("Accept", "application/octet-stream");
    }
    let resp = req.call().map_err(|e| e.to_string())?;
    let mut body = resp.into_reader();
    let mut file = std::fs::File::create(TAR).map_err(|e| format!("{TAR}: {e}"))?;
    let mut got = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| format!("{TAR}: {e}"))?;
        got += n as u64;
    }
    if size != 0 && got != size {
        return Err(format!("скачалось {got} байт из {size}"));
    }
    Ok(())
}

/// Checks for a new release now and then, so the page can show it.
pub fn run(shared: std::sync::Arc<crate::Shared>) {
    std::thread::spawn(move || {
        // Soon after start (the network and the clock come up first).
        std::thread::sleep(Duration::from_secs(30));
        loop {
            if shared.updates.due() {
                let proxy = shared.proxy.lock().unwrap().clone();
                if let Ok(Some(r)) = shared.updates.check(&proxy) {
                    eprintln!("update: soundya {} is available", r.version);
                }
            }
            std::thread::sleep(Duration::from_secs(600));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.4.2", "0.4.1"));
        assert!(newer("v0.10.0", "0.9.9"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.4.1", "0.4.1"));
        assert!(!newer("0.4.0", "0.4.1"));
        assert!(!newer("garbage", "0.4.1"));
    }

    #[test]
    fn reads_a_github_release() {
        let v = serde_json::json!({
            "tag_name": "v0.4.2",
            "body": "- AirPlay",
            "assets": [
                {"name": "other.zip", "browser_download_url": "https://x/other.zip", "size": 1},
                {"name": "soundya-armv7.tar", "browser_download_url": "https://x/soundya-armv7.tar", "size": 2649088}
            ]
        });
        let r = release_from(&v).unwrap();
        assert_eq!(r.version, "0.4.2");
        assert_eq!(r.url, "https://x/soundya-armv7.tar");
        assert_eq!(r.size, 2_649_088);
        assert!(release_from(&serde_json::json!({"tag_name": "v1", "assets": []})).is_none());
        let v = serde_json::json!({"tag_name": "v0.5.0", "assets": [{"name": "soundya-armv7.tar",
            "browser_download_url": "https://x/a", "size": 3, "digest": "sha256:ABC"}]});
        assert_eq!(release_from(&v).unwrap().digest, "sha256:ABC");
    }

    #[test]
    fn checks_the_digest() {
        let p = std::env::temp_dir().join(format!("soundya-digest-{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        let p = p.to_str().unwrap();
        // sha256("abc")
        let good = "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        if Command::new("sha256sum").arg(p).output().is_ok_and(|o| o.status.success()) {
            assert!(verify_digest(p, good).is_ok());
            assert!(verify_digest(p, "sha256:00").unwrap_err().contains("повреждён"));
        }
        assert!(verify_digest(p, "").is_ok(), "no digest: size check only");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn reads_the_installers_result() {
        let d = std::env::temp_dir().join(format!("soundya-upd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        assert!(last_result(&d).is_null());
        std::fs::write(d.join("update-result"), "status=rolledback\nfrom=0.4.40\nto=0.5.0\ntime=1700000000\nreason=не запустилась за 90 с\njunk\n").unwrap();
        let r = last_result(&d);
        assert_eq!(r["status"], "rolledback");
        assert_eq!(r["from"], "0.4.40");
        assert_eq!(r["time"], 1_700_000_000u64);
        assert_eq!(r["reason"], "не запустилась за 90 с");
        let _ = std::fs::remove_dir_all(&d);
    }
}
