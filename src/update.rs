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

const RELEASES: &str = "https://api.github.com/repos/Gemeur/soundya/releases/latest";
/// The same without the API: GitHub answers the API 60 times an hour per address, and many
/// speakers behind one provider's address got «status code 403» and never saw an update.
const LATEST_PAGE: &str = "https://github.com/Gemeur/soundya/releases/latest";
const DOWNLOAD: &str = "https://github.com/Gemeur/soundya/releases/download";
const CHANGELOG: &str = "https://raw.githubusercontent.com/Gemeur/soundya";
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

/// The developer build is put on by hand; a release from GitHub would replace it.
#[cfg(feature = "dev")]
fn dev_build() -> Result<(), String> {
    Err(format!(
        "это сборка разработчика ({}): обновление из GitHub выключено, чтобы её не затереть; ставьте dev-архив мастером или scripts/deploy.sh",
        crate::VERSION
    ))
}

#[cfg(not(feature = "dev"))]
fn dev_build() -> Result<(), String> {
    Ok(())
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
                Ok(newer(&r.version, crate::VERSION).then_some(r))
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
        let current = crate::VERSION;
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
        dev_build()?;
        // The archive the speaker is asking about lies at the same path: downloading
        // over it would install something else after «Алиса, да».
        if awaiting() {
            return Err("колонка ждёт «Алиса, да» для обновления с компьютера".into());
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
            None => return Err(format!("уже стоит последняя версия {}", crate::VERSION)),
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
        self.start_installer(&release.version)
    }

    /// An archive sent by the Mac/Windows app over the network (its «Обновить»
    /// when ADB is closed), saved at UPLOAD: checked, then the speaker asks out loud
    /// and installs only after «Алиса, да» (confirm_uploaded) within a minute.
    pub fn offer_uploaded(&self) -> Result<String, String> {
        if crate::luna::active() {
            return Err("soundya стоит как пакет Luna: обновите его через YAM".into());
        }
        let listing = Command::new("tar").arg("-tf").arg(TAR).output().map_err(|e| format!("tar: {e}"))?;
        if !package_ok(&String::from_utf8_lossy(&listing.stdout)) {
            let _ = std::fs::remove_file(TAR);
            return Err("это не архив soundya (нет soundya/install.sh и программы)".into());
        }
        let version = Command::new("tar")
            .args(["-xOf", TAR, "soundya/VERSION"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_default();
        *OFFER.lock().unwrap() = Some((version.clone(), Instant::now()));
        self.set_state("жду подтверждения на колонке: «Алиса, да»");
        crate::alice::say(format!("Пришло обновление саундя {version} с компьютера. Установить? Скажите: Алиса, да. Или: Алиса, нет"));
        Ok(format!("Колонка спрашивает, ставить ли soundya {version}: скажите ей «Алиса, да» в течение минуты"))
    }

    /// «Да» / «нет» to the offered archive.
    pub fn confirm_uploaded(&self, yes: bool) -> String {
        let Some((version, at)) = OFFER.lock().unwrap().take() else { return "Нечего устанавливать".into() };
        if !yes || at.elapsed() > OFFER_FOR {
            let _ = std::fs::remove_file(TAR);
            self.set_state("обновление из приложения отменено");
            return if yes { "Время на подтверждение вышло, нажмите Обновить ещё раз".into() } else { "Хорошо, не устанавливаю".into() };
        }
        if INSTALLING.swap(true, Ordering::AcqRel) {
            return "Обновление уже устанавливается".into();
        }
        *LAST_STUCK.lock().unwrap() = None;
        match self.start_installer(&version) {
            Ok(_) => format!("Устанавливаю саундя {version}. Через минуту вернусь"),
            Err(e) => {
                INSTALLING.store(false, Ordering::Release);
                format!("Не получилось: {e}")
            }
        }
    }

    fn start_installer(&self, version: &str) -> Result<String, String> {
        self.set_state(&format!("устанавливаю {version}…"));
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
        let version = version.to_owned();
        let said = version.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(240));
            let tail = crate::fsx::read_string(std::path::Path::new(LOG)).unwrap_or_default();
            let tail: String = tail.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" / ");
            eprintln!("update: installer for {version} did not finish in {} s: {tail}", started.elapsed().as_secs());
            INSTALLING.store(false, Ordering::Release);
            *LAST_STUCK.lock().unwrap() = Some(format!("установка {version} не завершилась: {tail}"));
        });
        Ok(format!("Устанавливаю soundya {said}. Через полминуты страница обновится сама"))
    }
}

/// Where an archive from the app is saved before it is checked and confirmed.
pub const UPLOAD: &str = TAR;
/// The biggest archive accepted over the network (the real one is ~7 MB).
pub const UPLOAD_MAX: usize = 48 << 20;
/// An archive from the app waiting for «Алиса, да»: (version, since).
static OFFER: Mutex<Option<(String, Instant)>> = Mutex::new(None);
const OFFER_FOR: Duration = Duration::from_secs(60);

/// The speaker asked «установить?» a moment ago.
pub fn awaiting() -> bool {
    OFFER.lock().unwrap().as_ref().is_some_and(|(_, at)| at.elapsed() < OFFER_FOR)
}

/// Takes the update slot for an archive arriving from the app: no upload over an
/// archive being installed or one the speaker is asking about (the file is shared).
/// `end_upload` gives the slot back once the archive is offered or refused.
pub fn begin_upload() -> Result<(), &'static str> {
    if awaiting() {
        return Err("колонка уже спрашивает про другое обновление, ответьте ей «Алиса, да» или «нет»");
    }
    if INSTALLING.swap(true, Ordering::AcqRel) {
        return Err("обновление уже устанавливается");
    }
    Ok(())
}

pub fn end_upload() {
    INSTALLING.store(false, Ordering::Release);
}

/// A soundya package: its installer and the program, nothing outside «soundya/».
pub fn package_ok(listing: &str) -> bool {
    let names: Vec<&str> = listing.lines().map(|l| l.trim().trim_start_matches("./")).filter(|l| !l.is_empty()).collect();
    names.contains(&"soundya/install.sh")
        && names.contains(&"soundya/soundya")
        && names.iter().all(|n| n.starts_with("soundya/") || *n == "soundya")
        && !names.iter().any(|n| n.split('/').any(|p| p == ".."))
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
    let api = agent.get(RELEASES).set("Accept", "application/vnd.github+json").call();
    match api {
        Ok(resp) => {
            let v: Value = resp.into_json().map_err(|e| e.to_string())?;
            release_from(&v).ok_or_else(|| "в последнем релизе нет сборки".to_owned())
        }
        // Over the API's limit (403 / 429): the release page, as a browser sees it.
        Err(ureq::Error::Status(code @ (403 | 429), _)) => {
            eprintln!("update: GitHub API said {code}; reading the release page instead");
            fetch_latest_page(&agent)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The latest release without the API: …/releases/latest redirects to …/tag/vX.Y.Z; the file
/// is at …/download/vX.Y.Z/soundya-armv7.tar, the notes in that version's CHANGELOG.md.
fn fetch_latest_page(agent: &ureq::Agent) -> Result<Release, String> {
    let resp = agent.head(LATEST_PAGE).call().map_err(|e| e.to_string())?;
    let tag = tag_from_url(resp.get_url()).ok_or("GitHub не назвал последнюю версию")?;
    let version = tag.trim_start_matches('v').to_owned();
    let notes = agent
        .get(&format!("{CHANGELOG}/{tag}/CHANGELOG.md"))
        .call()
        .ok()
        .and_then(|r| r.into_string().ok())
        .map(|t| notes_of(&t, &version))
        .unwrap_or_default();
    Ok(Release { url: format!("{DOWNLOAD}/{tag}/{ASSET}"), version, notes, api_url: String::new(), size: 0, digest: String::new() })
}

/// "https://github.com/o/r/releases/tag/v0.5.6" → "v0.5.6".
fn tag_from_url(url: &str) -> Option<String> {
    let tag = url.split("/releases/tag/").nth(1)?.split(['?', '#', '/']).next()?.trim();
    (tag.starts_with('v') && tag.len() > 1).then(|| tag.to_owned())
}

/// The «## 0.5.6» section of CHANGELOG.md (as the release notes are).
fn notes_of(changelog: &str, version: &str) -> String {
    let head = format!("## {version}");
    let mut out = String::new();
    let mut inside = false;
    for line in changelog.lines() {
        if line.starts_with("## ") {
            if inside {
                break;
            }
            inside = line.trim() == head;
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().chars().take(1500).collect()
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
    fn only_a_soundya_package_is_offered() {
        let ok = "soundya/\nsoundya/install.sh\nsoundya/soundya\nsoundya/VERSION\nsoundya/lua/x.lua\n";
        assert!(package_ok(ok));
        assert!(!package_ok("soundya/\nsoundya/install.sh\n"), "no program");
        assert!(!package_ok(&format!("{ok}etc/passwd\n")), "a file outside soundya/");
        assert!(!package_ok(&format!("{ok}soundya/../../etc/x\n")), "climbing out");
        assert!(!package_ok(""));
    }

    #[test]
    fn nothing_is_installed_without_yes() {
        let u = Updates::default();
        assert!(!awaiting());
        assert_eq!(u.confirm_uploaded(true), "Нечего устанавливать", "«да» with no offer does nothing");
        *OFFER.lock().unwrap() = Some(("0.5.1".into(), Instant::now()));
        assert!(awaiting());
        assert_eq!(u.confirm_uploaded(false), "Хорошо, не устанавливаю");
        assert!(!awaiting(), "a «нет» ends the offer");
        *OFFER.lock().unwrap() = Some(("0.5.1".into(), Instant::now() - OFFER_FOR - Duration::from_secs(1)));
        assert!(!awaiting());
        assert!(u.confirm_uploaded(true).contains("Время"), "a late «да» installs nothing");
        assert!(!INSTALLING.load(Ordering::Acquire));
        // The archive file is shared: no second upload while one is offered or being received.
        *OFFER.lock().unwrap() = Some(("0.5.1".into(), Instant::now()));
        assert!(begin_upload().is_err(), "not over the archive the speaker is asking about");
        *OFFER.lock().unwrap() = None;
        assert!(begin_upload().is_ok());
        assert!(begin_upload().is_err(), "not two uploads at once");
        end_upload();
        assert!(!INSTALLING.load(Ordering::Acquire));
    }

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
    fn the_release_page_when_the_api_says_403() {
        assert_eq!(tag_from_url("https://github.com/rodyatotu/soundya/releases/tag/v0.5.6").as_deref(), Some("v0.5.6"));
        assert_eq!(tag_from_url("https://github.com/rodyatotu/soundya/releases"), None);
        let log = "# Изменения\n\n## 0.5.6\n- 🔧 one\n- two\n\n## 0.5.5\n- old\n";
        assert_eq!(notes_of(log, "0.5.6"), "- 🔧 one\n- two");
        assert_eq!(notes_of(log, "0.4.0"), "");
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
