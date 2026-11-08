//! File helpers that never call `statx`.
//!
//! The speaker runs Linux 4.9, which predates `statx` (syscall 397 on ARM).
//! On 32-bit ARM an unknown syscall number is not answered with ENOSYS: the
//! kernel kills the process with SIGILL. Rust's std uses `statx` for
//! `metadata`, `Path::exists`/`is_dir`, `create_dir_all` and the size hint in
//! `fs::read_to_string`, so those must not be used anywhere in soundya.

use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
};

pub fn read_string(path: &Path) -> Option<String> {
    String::from_utf8(read_bytes(path).ok()?).ok()
}

/// `std::fs::read` without its size hint (that hint is a `statx`).
pub fn read_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(buf)
}

/// `std::fs::copy` without its `metadata` call: the bytes only, no permissions.
pub fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    let data = read_bytes(from)?;
    std::fs::write(to, data)
}

/// `std::fs::remove_dir_all` without `lstat`: a directory is whatever `read_dir`
/// opens. Not for trees with symlinks to directories (soundya makes none).
pub fn remove_tree(path: &Path) {
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            let p = e.path();
            if is_dir(&p) {
                remove_tree(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
        let _ = std::fs::remove_dir(path);
    }
}

/// A file that opens and is not a directory.
pub fn is_file(path: &Path) -> bool {
    exists(path) && !is_dir(path)
}

pub fn exists(path: &Path) -> bool {
    File::open(path).is_ok()
}

pub fn is_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok()
}

/// Creates one directory level; an existing directory is fine.
pub fn ensure_dir(path: &Path) {
    let _ = std::fs::create_dir(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule at the top of this file, checked: no std call that ends in `statx`
    /// outside tests (the hosts that run `cargo test` all have `statx`, so nothing
    /// else would notice).
    #[test]
    fn no_statx_calls_in_speaker_code() {
        // The installer shares this file but runs on Mac/Windows, where std is fine.
        if env!("CARGO_PKG_NAME") != "soundya" {
            return;
        }
        let banned = [
            "fs::copy(", "fs::read(", "fs::read_to_string(", "fs::metadata(", "fs::symlink_metadata(",
            "create_dir_all(", "remove_dir_all(", ".exists()", ".is_file()", ".is_dir()", ".metadata()",
        ];
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut bad = Vec::new();
        for e in std::fs::read_dir(&src).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name == "fsx.rs" {
                continue;
            }
            let text = read_string(&e.path()).unwrap();
            let code = text.split("#[cfg(test)]").next().unwrap();
            for (n, line) in code.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for b in banned {
                    if line.contains(b) {
                        bad.push(format!("{name}:{}: {b}", n + 1));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "std calls that use statx (SIGILL on the speaker's kernel):\n{}", bad.join("\n"));
    }

    #[test]
    fn copy_and_remove_tree() {
        let d = std::env::temp_dir().join(format!("soundya-fsx-{}", std::process::id()));
        remove_tree(&d);
        ensure_dir(&d);
        ensure_dir(&d.join("sub"));
        std::fs::write(d.join("sub/a"), b"hello").unwrap();
        copy(&d.join("sub/a"), &d.join("b")).unwrap();
        assert_eq!(read_bytes(&d.join("b")).unwrap(), b"hello");
        assert!(is_file(&d.join("b")) && !is_file(&d.join("sub")) && !is_file(&d.join("none")));
        remove_tree(&d);
        assert!(!exists(&d));
    }
}
