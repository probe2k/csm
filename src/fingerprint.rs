//! Robust directory identity.
//!
//! A directory is identified by `dev:ino:birthtime`, NOT by its path string.
//! This makes the identity survive renames/moves and — crucially — distinguish a
//! freshly recreated folder at the same path from the original one (different
//! inode and/or birth time), so we never resurrect an old folder's sessions.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Compute the composite fingerprint for a directory.
pub fn fingerprint(path: &Path) -> std::io::Result<String> {
    let md = fs::metadata(path)?;
    let dev = md.dev();
    let ino = md.ino();
    // `created()` returns the birth time on macOS (st_birthtime). If unavailable
    // we fall back to 0 — dev+ino still give reasonable identity.
    let birth = md
        .created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(format!("{dev}:{ino}:{birth}"))
}
