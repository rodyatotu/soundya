//! The developer build of the installer (`--features dev`; the normal build has
//! none of this): the wizard with the feedback mode (src/devpage.rs), its notes
//! kept next to the ADB key (the page's port, so its localStorage, changes on
//! every start), «Сохранить файлом» into ~/Downloads (the Mac app's window cannot
//! download) and the install log for the notes.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Requests here may carry pictures.
pub const BODY_LIMIT: usize = 24 << 20;

pub fn page(wizard: &str, version: &str) -> String {
    crate::devpage::inject(
        wizard,
        &json!({
            "app": "wizard",
            "title": "мастер установки",
            "version": version,
            "device": std::env::consts::OS,
            "log": "/api/dev/log",
            "save": "/api/dev/save",
            "store": "/api/dev/notes",
            "h2c": "/dev/html2canvas.js",
        }),
    )
}

/// (status, content type, body) for the developer build's own paths.
pub fn route(method: &str, path: &str, body: &[u8], job_log: &[String], data: &Path) -> Option<(&'static str, &'static str, Vec<u8>)> {
    if method == "GET" {
        if let Some(js) = crate::devpage::script(path) {
            return Some(("200 OK", "text/javascript; charset=utf-8", js.as_bytes().to_vec()));
        }
    }
    let notes = data.join("dev-notes.json");
    Some(match (method, path) {
        ("POST", "/api/dev/log") => {
            let n: usize = std::str::from_utf8(body).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(80);
            let from = job_log.len().saturating_sub(n);
            let text = if job_log.is_empty() { "(установка в этом запуске не начиналась)".to_owned() } else { job_log[from..].join("\n") };
            ("200 OK", "text/plain; charset=utf-8", text.into_bytes())
        }
        ("GET", "/api/dev/notes") => ("200 OK", "application/json", std::fs::read(&notes).unwrap_or_else(|_| b"{}".to_vec())),
        ("POST", "/api/dev/notes") => match serde_json::from_slice::<Value>(body) {
            Ok(v) if v.is_object() => match write_atomic(&notes, body) {
                Ok(()) => ("200 OK", "text/plain; charset=utf-8", b"ok".to_vec()),
                Err(e) => ("500 Internal Server Error", "text/plain; charset=utf-8", e.into_bytes()),
            },
            _ => ("400 Bad Request", "text/plain; charset=utf-8", b"not a JSON object".to_vec()),
        },
        ("POST", "/api/dev/save") => match save(body, &downloads()) {
            Ok(p) => ("200 OK", "text/plain; charset=utf-8", p.display().to_string().into_bytes()),
            Err(e) => ("400 Bad Request", "text/plain; charset=utf-8", e.into_bytes()),
        },
        _ => return None,
    })
}

fn downloads() -> PathBuf {
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let d = home.join("Downloads");
    if d.is_dir() { d } else { home }
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| e.to_string())
}

/// Only plain names: nothing can be written outside the new folder.
fn plain(name: &str) -> Option<&str> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    ok.then_some(name)
}

/// `{name, files: [{name, base64}]}` → <dir>/<name>/<file>…; returns the folder.
fn save(body: &[u8], dir: &Path) -> Result<PathBuf, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let name = v["name"].as_str().and_then(plain).ok_or("неверное имя папки")?;
    let files = v["files"].as_array().filter(|f| !f.is_empty() && f.len() <= 200).ok_or("нет файлов")?;
    let out = dir.join(name);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    for f in files {
        let fname = f["name"].as_str().and_then(plain).ok_or("неверное имя файла")?;
        let data = base64(f["base64"].as_str().unwrap_or("")).ok_or("файл не в base64")?;
        std::fs::write(out.join(fname), data).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64() {
        assert_eq!(base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64("0J/RgNC40LLQtdGC").unwrap(), "Привет".as_bytes());
        assert!(base64("a$b").is_none());
    }

    #[test]
    fn saves_only_inside_a_new_folder() {
        let d = std::env::temp_dir().join(format!("sd-dev-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let body = json!({ "name": "soundya-feedback-wizard-1", "files": [{ "name": "prompt.md", "base64": "aGVsbG8=" }] });
        let out = save(body.to_string().as_bytes(), &d).unwrap();
        assert_eq!(std::fs::read(out.join("prompt.md")).unwrap(), b"hello");
        for bad in [json!({ "name": "../x", "files": [{ "name": "a", "base64": "" }] }),
                    json!({ "name": "ok", "files": [{ "name": "../../a", "base64": "" }] }),
                    json!({ "name": "ok", "files": [{ "name": ".bashrc", "base64": "" }] }),
                    json!({ "name": "ok", "files": [] })] {
            assert!(save(bad.to_string().as_bytes(), &d).is_err(), "{bad}");
        }
        assert!(!d.join("x").exists() && !d.parent().unwrap().join("a").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn notes_are_kept_and_the_log_is_the_install_log() {
        let d = std::env::temp_dir().join(format!("sd-dev-notes-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (_, _, empty) = route("GET", "/api/dev/notes", b"", &[], &d).unwrap();
        assert_eq!(empty, b"{}");
        assert_eq!(route("POST", "/api/dev/notes", b"[1]", &[], &d).unwrap().0, "400 Bad Request");
        route("POST", "/api/dev/notes", br#"{"notes":[{"id":1}]}"#, &[], &d).unwrap();
        assert_eq!(route("GET", "/api/dev/notes", b"", &[], &d).unwrap().2, br#"{"notes":[{"id":1}]}"#);
        let log: Vec<String> = (1..=5).map(|i| format!("line {i}")).collect();
        assert_eq!(route("POST", "/api/dev/log", b"2", &log, &d).unwrap().2, b"line 4\nline 5");
        assert!(route("GET", "/dev/feedback.js", b"", &[], &d).unwrap().2.len() > 1000);
        assert!(route("GET", "/api/info", b"", &[], &d).is_none(), "the wizard's own paths are not taken");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_wizard_gets_the_feedback_mode() {
        let p = page(include_str!("wizard.html"), "0.5.0+dev");
        assert!(p.contains("\"app\":\"wizard\"") && p.contains("/dev/feedback.js") && p.contains("\"store\":\"/api/dev/notes\""));
    }
}
