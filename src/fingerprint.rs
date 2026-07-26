//! Robust directory identity.
//!
//! A directory is identified by a random marker stamped into an extended
//! attribute the first time csm sees it — NOT by inode number or birth time.
//! Both of those can come back stale: inode numbers get reused by the
//! filesystem, and archive tools (e.g. macOS's Archive Utility) can restore a
//! folder's original creation time from a zip's metadata, so a fresh extract
//! can look identical, on both counts, to a folder deleted moments earlier.
//! An xattr marker can't: deleting the directory destroys it along with
//! everything else the inode held, so a recreated folder at the same path is
//! always a blank slate.
//!
//! Falls back to the legacy `dev:ino:birthtime` identity when xattrs aren't
//! usable (FAT volumes, some network mounts, permission issues) — see
//! `resolve` in `main.rs`, which also handles migrating directories that were
//! already indexed under that legacy scheme before markers existed.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// `user.` namespace is required on Linux and accepted as-is on macOS/BSD.
const MARKER: &str = "user.csm.dirid";

/// Outcome of checking a directory for csm's identity marker.
#[derive(Debug, PartialEq, Eq)]
pub enum Marker {
    /// The marker is set to this value.
    Present(String),
    /// The attribute is confirmed unset — this directory has never been
    /// stamped (or doesn't support xattrs at all).
    Absent,
    /// Couldn't tell either way: a real I/O error, not "no such attribute".
    /// Must NOT be treated as `Absent` — doing so would mint and stamp a
    /// fresh marker over a real one that simply failed to read, silently
    /// and permanently orphaning it.
    Unknown,
}

/// Read the marker stamped on `path`, distinguishing "confirmed absent" from
/// "failed to read" (see `Marker`).
pub fn read_marker(path: &Path) -> Marker {
    match xattr::get(path, MARKER) {
        Ok(Some(bytes)) => match String::from_utf8(bytes) {
            Ok(s) => Marker::Present(s),
            Err(_) => Marker::Unknown,
        },
        Ok(None) => Marker::Absent,
        Err(_) => Marker::Unknown,
    }
}

/// Stamp `id` onto `path`. Best-effort — the caller falls back to the legacy
/// fingerprint when the underlying filesystem doesn't support xattrs.
pub fn stamp(path: &Path, id: &str) -> std::io::Result<()> {
    xattr::set(path, MARKER, id.as_bytes())
}

/// The old composite identity: `device + inode + birthtime`. Still computed
/// as a fallback, and to recognize (and migrate) directories indexed before
/// markers existed.
pub fn legacy(path: &Path) -> std::io::Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_and_survives_delete_recreate() {
        let dir = std::env::temp_dir().join(format!("csm-fp-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();

        assert_eq!(read_marker(&dir), Marker::Absent);
        if stamp(&dir, "abc-123").is_err() {
            // xattrs unsupported on whatever filesystem holds the temp dir
            // (e.g. some CI containers) — nothing to assert here.
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!(read_marker(&dir), Marker::Present("abc-123".to_string()));

        // Deleting and recreating at the same path must NOT carry the marker
        // over — that's the entire point.
        fs::remove_dir_all(&dir).unwrap();
        fs::create_dir(&dir).unwrap();
        assert_eq!(read_marker(&dir), Marker::Absent);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_marker_reports_unknown_not_absent_for_nonexistent_path() {
        // A read failure (here: the path doesn't exist) must never be
        // reported as `Absent` — callers mint and stamp a fresh marker on
        // `Absent`, which would silently overwrite a real marker that simply
        // failed to read (e.g. after a transient I/O hiccup).
        let missing = std::env::temp_dir().join(format!(
            "csm-fp-test-missing-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        assert_eq!(read_marker(&missing), Marker::Unknown);
    }
}
