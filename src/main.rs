//! csm — Claude Session Manager.
//!
//! A tmux-like terminal multiplexer specialized for `claude` sessions. It runs
//! claude inside a PTY, keys each directory by a marker stamped into it (so a
//! recreated folder is treated as new), resumes the right session by default,
//! and exposes a prefix-hotkey session chooser.

mod claude;
mod config;
mod fingerprint;
mod index;
mod mux;
mod sessions;
mod status;
mod tui;

use std::env;
use std::path::Path;
use std::process::exit;

use index::Index;
use mux::Initial;
use sessions::SessionMeta;

fn help() -> String {
    let p = config::prefix().1; // e.g. "Ctrl-o"
    format!(
        "\
csm — Claude Session Manager (a tmux-like multiplexer for claude)

USAGE:
    csm [DIR]            Open the multiplexer in DIR (or cwd), resuming the latest
                        session there (or starting one if none exist)
    csm -n [DIR]         Open with a NEW session
    csm -l [DIR]         Open with the session chooser showing immediately
    csm ls               List all directories csm manages and their sessions
    csm -h | --help      Show this help
    csm -V | --version   Show version

INSIDE THE MULTIPLEXER (prefix = {p}):
    {p} s     session chooser (pick an existing session, or New at the bottom)
    {p} c     new session
    {p} n/p   next / previous session
    {p} 0-9   jump to session N
    {p} d     detach (quit csm; conversations stay saved on disk)
    {p} {p}   send a literal {p} to claude

The prefix key is configurable: set CSM_PREFIX (e.g. CSM_PREFIX=C-g) to remap it.

Sessions are tied to a marker stamped into each directory, so a
deleted-and-recreated folder at the same path is treated as brand new.
"
    )
}

enum Cmd {
    Open(Option<String>),
    New(Option<String>),
    List(Option<String>),
    GlobalList,
    Help,
    Version,
}

fn main() {
    let cmd = parse_args(env::args().skip(1).collect());
    let result = match cmd {
        Cmd::Help => {
            print!("{}", help());
            Ok(())
        }
        Cmd::Version => {
            println!("csm {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Cmd::Open(dir) => open(dir, false, false),
        Cmd::New(dir) => open(dir, true, false),
        Cmd::List(dir) => open(dir, false, true),
        Cmd::GlobalList => global_list(),
    };
    if let Err(e) = result {
        eprintln!("csm: {e}");
        exit(1);
    }
}

fn parse_args(args: Vec<String>) -> Cmd {
    let mut dir: Option<String> = None;
    let mut want_new = false;
    let mut want_list = false;

    for a in args {
        match a.as_str() {
            "-h" | "--help" => return Cmd::Help,
            "-V" | "--version" => return Cmd::Version,
            "-n" | "--new" | "new" => want_new = true,
            "-l" | "--list" | "list" => want_list = true,
            "ls" => return Cmd::GlobalList,
            _ if dir.is_none() => dir = Some(a),
            _ => {}
        }
    }

    if want_list {
        Cmd::List(dir)
    } else if want_new {
        Cmd::New(dir)
    } else {
        Cmd::Open(dir)
    }
}

/// chdir into the target (if given) and compute (fingerprint, getcwd-form path).
fn prepare(dir: Option<String>, index: &Index) -> std::io::Result<(String, String)> {
    if let Some(d) = dir {
        env::set_current_dir(&d)
            .map_err(|e| std::io::Error::new(e.kind(), format!("cannot enter '{d}': {e}")))?;
    }
    let target = env::current_dir()?.to_string_lossy().into_owned();
    let fp = resolve_fingerprint(index)?;
    Ok((fp, target))
}

/// Resolve the current directory's stable identity.
///
/// Prefers the xattr marker stamped the first time csm saw this exact
/// directory object (see `fingerprint.rs`) — immune to inode reuse and to
/// archive tools restoring a stale creation time, so a deleted-and-recreated
/// folder can never be mistaken for the one that used to live there.
///
/// If the marker is confirmed absent (never stamped, or this filesystem
/// doesn't support xattrs), falls back to the legacy `dev:ino:birthtime`
/// identity to decide what to do:
/// - If that legacy identity already has sessions bound to it, this is a
///   directory indexed before markers existed — stamp its legacy identity in
///   as the permanent marker so its history carries forward untouched.
/// - Otherwise it's genuinely new (or its old legacy identity no longer
///   matches, e.g. after a delete+recreate) — mint and stamp a fresh random
///   marker, so it can never collide with anything again.
/// - If the index can't be read right now (lock contention, I/O hiccup),
///   don't decide/stamp anything yet — just use the legacy value for this
///   one launch and try again next time.
///
/// If the marker read fails for any *other* reason (a transient I/O error —
/// e.g. right after waking from sleep, or a network mount hiccup — rather
/// than a clean "no such attribute"), we must NOT treat that as "absent":
/// doing so would mint and stamp a brand new marker over a real one that
/// simply failed to read, permanently orphaning it. So we just use the
/// legacy identity for this one launch, unstamped, and try reading the real
/// marker again next time.
///
/// If xattrs aren't usable at all here (unsupported filesystem, permissions),
/// the marker never persists, so we keep using the legacy identity every
/// launch — the same fallback behavior as before markers existed.
fn resolve_fingerprint(index: &Index) -> std::io::Result<String> {
    let path = Path::new(".");
    match fingerprint::read_marker(path) {
        fingerprint::Marker::Present(id) => return Ok(id),
        fingerprint::Marker::Unknown => return fingerprint::legacy(path),
        fingerprint::Marker::Absent => {}
    }
    let legacy = fingerprint::legacy(path)?;
    let bound = match index.bound_ids_checked(&legacy) {
        Ok(b) => b,
        Err(_) => return Ok(legacy),
    };
    let id = if bound.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        legacy.clone()
    };
    if fingerprint::stamp(path, &id).is_ok() {
        Ok(id)
    } else {
        Ok(legacy)
    }
}

/// Launch the multiplexer.
fn open(dir: Option<String>, force_new: bool, open_chooser: bool) -> std::io::Result<()> {
    let index = Index::load();
    let (fp, target) = prepare(dir, &index)?;

    let initial = if force_new {
        Initial::New
    } else {
        match sessions_for_current(&index, &fp, &target).into_iter().next() {
            Some(m) => Initial::Resume {
                title: mux::title_of(&m),
                id: m.id,
            },
            None => Initial::New,
        }
    };

    mux::run(fp, target, index, initial, open_chooser)
}

/// `csm ls` — overview of everything csm manages.
fn global_list() -> std::io::Result<()> {
    let index = Index::load();
    let mut entries = index.entries();
    if entries.is_empty() {
        println!("csm: no managed directories yet.");
        return Ok(());
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    for e in &entries {
        let metas = sessions::list_bound(&e.path, &e.sessions);
        let latest = metas
            .first()
            .map(|m| format!("{}  ({})", mux::title_of(m), tui::rel_time(m.mtime)))
            .unwrap_or_else(|| "—".to_string());
        println!("{}", e.path);
        println!("    {} session(s)   latest: {}", metas.len(), latest);
    }
    Ok(())
}

/// Resolve the sessions belonging to the CURRENT physical directory.
///
/// - Known fingerprint -> its bound sessions.
/// - Never-managed path with transcripts on disk -> adopt them (one-time
///   bootstrap for pre-existing projects), but only transcripts at least as
///   new as the directory itself: an older one belongs to a previous folder
///   that lived at this path, even if the index was wiped and can no longer
///   say so via `path_seen_before`.
/// - Otherwise (path managed under a different inode = recreated folder, or
///   truly empty) -> nothing, so we start fresh and old sessions stay hidden.
///
/// Uses the checked read so a transient index-read failure can't masquerade
/// as "never managed" and trigger the adopt fallback below, which would
/// overwrite this fingerprint's real (but unreadable-right-now) entry.
fn sessions_for_current(index: &Index, fp: &str, target: &str) -> Vec<SessionMeta> {
    let bound = match index.bound_ids_checked(fp) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    if !bound.is_empty() {
        return sessions::list_bound(target, &bound);
    }
    if !index.path_seen_before(target) {
        let born = std::fs::metadata(target).and_then(|m| m.created()).ok();
        let disk: Vec<SessionMeta> = sessions::list_on_disk(target)
            .into_iter()
            .filter(|s| born.is_none_or(|b| s.mtime >= b))
            .collect();
        if !disk.is_empty() {
            let ids: Vec<String> = disk.iter().map(|s| s.id.clone()).collect();
            let _ = index.set_sessions(fp, target, ids.clone());
            return sessions::list_bound(target, &ids);
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "csm-main-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The scenario from the bug report: a directory is deleted and a new
    /// one created at the same path (e.g. by re-extracting the same zip).
    /// It must never resolve to the old identity, even though inode reuse
    /// or an archive tool restoring the original creation time could make
    /// the legacy dev:ino:birthtime fingerprint collide. Also checks that a
    /// directory already indexed under the legacy scheme (pre-marker) keeps
    /// its identity and bound sessions instead of being treated as new.
    #[test]
    fn resolve_fingerprint_avoids_collision_and_migrates_legacy() {
        let orig_cwd = env::current_dir().unwrap();
        let home = temp_dir("home");
        env::set_var("CLAUDE_CONFIG_DIR", home.join("claude-cfg"));
        env::set_var("XDG_DATA_HOME", home.join("xdg-data"));
        let index = Index::load();

        // Brand-new directory: mints and stamps a fresh marker.
        let proj = temp_dir("proj");
        env::set_current_dir(&proj).unwrap();
        let fp1 = resolve_fingerprint(&index).unwrap();
        assert_eq!(
            fingerprint::read_marker(Path::new(".")),
            fingerprint::Marker::Present(fp1.clone())
        );

        // Delete + recreate at the same path: must NOT collide with fp1.
        env::set_current_dir(&home).unwrap();
        fs::remove_dir_all(&proj).unwrap();
        fs::create_dir(&proj).unwrap();
        env::set_current_dir(&proj).unwrap();
        let fp2 = resolve_fingerprint(&index).unwrap();
        assert_ne!(fp1, fp2, "recreated directory must get a fresh identity");

        // Pre-existing (pre-marker) directory: legacy identity is adopted
        // as the permanent marker, not discarded in favor of a random one.
        env::set_current_dir(&home).unwrap();
        let legacy_proj = temp_dir("legacy-proj");
        env::set_current_dir(&legacy_proj).unwrap();
        let legacy_fp = fingerprint::legacy(Path::new(".")).unwrap();
        index
            .bind(&legacy_fp, &legacy_proj.to_string_lossy(), "pre-existing-session")
            .unwrap();
        assert_eq!(
            fingerprint::read_marker(Path::new(".")),
            fingerprint::Marker::Absent
        );

        let fp3 = resolve_fingerprint(&index).unwrap();
        assert_eq!(
            fp3, legacy_fp,
            "a directory already indexed under its legacy fp keeps that identity"
        );
        assert_eq!(
            index.bound_ids(&fp3),
            vec!["pre-existing-session".to_string()]
        );

        env::set_current_dir(&orig_cwd).unwrap();
        let _ = fs::remove_dir_all(&home);
    }
}
