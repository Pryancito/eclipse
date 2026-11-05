//! When a client's sources last changed.
//!
//! Each Lunar client is rebuilt when anything under its own `src/` is newer
//! than its binary. That is not the whole truth for a client that links a
//! crate by path: `tools/lunartext` (the vector text) and
//! `tools/eclipse-i18n` (the UI strings) are source files of every client
//! that links them, and a change there leaves the binaries "up to date"
//! against a tree that no longer says what it says. The image then ships the
//! old text, which reads as the change never having happened, with nothing in
//! any log to say so.
//!
//! So each shared crate gets a function here, and every staleness check that
//! links it folds that function in beside its own `src/`.

use std::{fs, path::Path};

use super::PROJECT_DIR;

/// `tools/lunartext`, the vector text every Lunar client links by path.
pub(super) fn lunartext_mtime() -> Option<std::time::SystemTime> {
    newest_mtime_under(&PROJECT_DIR.join("tools/lunartext/src"))
}

/// `tools/eclipse-i18n`, the UI strings in Spanish and English.
pub(super) fn eclipse_i18n_mtime() -> Option<std::time::SystemTime> {
    newest_mtime_under(&PROJECT_DIR.join("tools/eclipse-i18n/src"))
}

/// The newest modification time of any file under `dir`, recursively.
pub(super) fn newest_mtime_under(dir: &Path) -> Option<std::time::SystemTime> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let ty = e.file_type().ok()?;
            if ty.is_dir() {
                newest_mtime_under(&e.path())
            } else {
                e.metadata().ok()?.modified().ok()
            }
        })
        .max()
}

#[cfg(test)]
#[path = "newest_mtime_tests.rs"]
mod newest_mtime_tests;
