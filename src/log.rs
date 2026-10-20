//! Tiny logger: stderr in debug, file in release.

use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn log_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(base).join("FastFind").join("fastfind.log")
}

pub fn init() {
    if cfg!(debug_assertions) {
        return;
    }
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = create_dir_all(dir);
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = LOG.set(Mutex::new(f));
    }
}

pub fn line(msg: &str) {
    if cfg!(debug_assertions) {
        eprintln!("{}", msg);
        return;
    }
    if let Some(m) = LOG.get() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{}", msg);
        }
    }
}

#[macro_export]
macro_rules! flog {
    ($($arg:tt)*) => {
        $crate::log::line(&format!($($arg)*))
    };
}
