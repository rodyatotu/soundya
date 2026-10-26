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
    let mut file = File::open(path).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
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
