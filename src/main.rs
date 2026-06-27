//! csm — Claude Session Manager.
//!
//! A tmux-like terminal multiplexer specialized for `claude` sessions. It runs
//! claude inside a PTY, keys each directory by its inode fingerprint (so a
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

Sessions are tied to a directory's identity (device+inode+birthtime), so a
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
fn prepare(dir: Option<String>) -> std::io::Result<(String, String)> {
    if let Some(d) = dir {
        env::set_current_dir(&d)
            .map_err(|e| std::io::Error::new(e.kind(), format!("cannot enter '{d}': {e}")))?;
    }
    let target = env::current_dir()?.to_string_lossy().into_owned();
    let fp = fingerprint::fingerprint(Path::new("."))?;
    Ok((fp, target))
}

/// Launch the multiplexer.
fn open(dir: Option<String>, force_new: bool, open_chooser: bool) -> std::io::Result<()> {
    let (fp, target) = prepare(dir)?;
    let index = Index::load();

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
///   bootstrap for pre-existing projects).
/// - Otherwise (path managed under a different inode = recreated folder, or
///   truly empty) -> nothing, so we start fresh and old sessions stay hidden.
fn sessions_for_current(index: &Index, fp: &str, target: &str) -> Vec<SessionMeta> {
    let bound = index.bound_ids(fp);
    if !bound.is_empty() {
        return sessions::list_bound(target, &bound);
    }
    if !index.path_seen_before(target) {
        let disk = sessions::list_on_disk(target);
        if !disk.is_empty() {
            let ids: Vec<String> = disk.iter().map(|s| s.id.clone()).collect();
            let _ = index.set_sessions(fp, target, ids.clone());
            return sessions::list_bound(target, &ids);
        }
    }
    Vec::new()
}
