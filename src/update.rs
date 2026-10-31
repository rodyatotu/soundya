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
    size: u64,
}

#[derive(Default)]
pub struct Updates {
    latest: Mutex<Option<Release>>,
    checked: Mutex<Option<Instant>>,
    error: Mutex<Option<String>>,
    installing: AtomicBool,
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
        size: asset.get("size").and_then(Value::as_u64).unwrap_or(0),
    })
}

impl Updates {
    pub fn due(&self) -> bool {
        self.checked.lock().unwrap().map_or(true, |t| t.elapsed() > EVERY)
    }

    /// Asks GitHub for the latest release (through the SoundCloud proxy, if any).
    pub fn check(&self, proxy: &str) -> Result<Option<Release>, String> {
        *self.checked.lock().unwrap() = Some(Instant::now());
        let result = (|| -> Result<Release, String> {
            let agent = crate::sc::build_agent(proxy).map_err(|e| e.to_string())?;
            let v: Value = agent
                .get(RELEASES)
                .set("Accept", "application/vnd.github+json")
                .call()
                .map_err(|e| e.to_string())?
                .into_json()
                .map_err(|e| e.to_string())?;
            release_from(&v).ok_or_else(|| "в последнем релизе нет сборки".to_owned())
        })();
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
            "installing": self.installing.load(Ordering::Acquire),
            "state": *self.state.lock().unwrap(),
            "error": *self.error.lock().unwrap(),
        })
    }

    fn set_state(&self, s: &str) {
        eprintln!("update: {s}");
        *self.state.lock().unwrap() = Some(s.to_owned());
    }

    /// Downloads the latest release and starts its installer. Returns at once
    /// with the installer running; it restarts soundya when done.
    pub fn install(&self, proxy: &str) -> Result<String, String> {
        if self.installing.swap(true, Ordering::AcqRel) {
            return Err("обновление уже устанавливается".into());
        }
        let result = self.install_inner(proxy);
        if result.is_err() {
            self.installing.store(false, Ordering::Release);
        }
        result
    }

    fn install_inner(&self, proxy: &str) -> Result<String, String> {
        let release = match self.check(proxy)? {
            Some(r) => r,
            None => return Err(format!("уже стоит последняя версия {}", env!("CARGO_PKG_VERSION"))),
        };
        self.set_state(&format!("скачиваю {}…", release.version));
        let agent = crate::sc::build_agent(proxy).map_err(|e| e.to_string())?;
        let resp = agent.get(&release.url).call().map_err(|e| format!("не скачалось: {e}"))?;
        let mut body = resp.into_reader();
        let mut file = std::fs::File::create(TAR).map_err(|e| format!("{TAR}: {e}"))?;
        let mut got = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = body.read(&mut buf).map_err(|e| format!("не скачалось: {e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| format!("{TAR}: {e}"))?;
            got += n as u64;
        }
        drop(file);
        if release.size != 0 && got != release.size {
            return Err(format!("скачалось {got} байт из {}", release.size));
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
        Ok(format!("Устанавливаю soundya {}. Через полминуты страница обновится сама", release.version))
    }
}

/// Checks for a new release now and then, so the page can show it.
pub fn run(shared: std::sync::Arc<crate::Shared>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(180));
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
    }
}
