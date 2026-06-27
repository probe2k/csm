//! Shared configuration paths. Honors `CLAUDE_CONFIG_DIR` exactly like the
//! `claude` CLI does, falling back to `~/.claude`.

use std::path::PathBuf;

/// Base config dir: `$CLAUDE_CONFIG_DIR` or `~/.claude`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude")
}

/// Where claude stores per-project session transcripts.
pub fn projects_root() -> PathBuf {
    config_dir().join("projects")
}

/// csm's own index database (the only state csm owns).
pub fn index_path() -> PathBuf {
    config_dir().join("csm").join("index.redb")
}

/// The multiplexer prefix key, as `(control_byte, human_label)`.
///
/// Configurable via `CSM_PREFIX` — accepts `C-o`, `ctrl-o`, `ctrl+o`, `^o`, or
/// just `o`. Defaults to `Ctrl-o`, which is free in tmux, the terminal, the
/// shell (raw mode disables VDISCARD), and claude's TUI.
pub fn prefix() -> (u8, String) {
    let spec = std::env::var("CSM_PREFIX").unwrap_or_default();
    let letter = parse_prefix_letter(&spec).unwrap_or('o');
    let byte = (letter.to_ascii_uppercase() as u8) & 0x1f;
    (byte, format!("Ctrl-{}", letter.to_ascii_lowercase()))
}

/// Whether the bottom status bar is shown (disable with `CSM_STATUS=off`/`0`).
pub fn status_enabled() -> bool {
    std::env::var("CSM_STATUS")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v != "off" && v != "0" && v != "false"
        })
        .unwrap_or(true)
}

/// Status bar accent color (256-color index), via `CSM_STATUS_ACCENT`.
pub fn status_accent() -> u8 {
    std::env::var("CSM_STATUS_ACCENT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(39)
}

/// Right-hand status segment text, via `CSM_STATUS_RIGHT` (default supplied by
/// the caller, usually the directory basename).
pub fn status_right(default: &str) -> String {
    std::env::var("CSM_STATUS_RIGHT").unwrap_or_else(|_| default.to_string())
}

/// Pull the letter out of a prefix spec like `C-o` / `ctrl-o` / `^o` / `o`.
fn parse_prefix_letter(spec: &str) -> Option<char> {
    let lower = spec.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let tail = lower
        .strip_prefix("ctrl-")
        .or_else(|| lower.strip_prefix("ctrl+"))
        .or_else(|| lower.strip_prefix("c-"))
        .or_else(|| lower.strip_prefix('^'))
        .unwrap_or(&lower);
    tail.chars().next().filter(|c| c.is_ascii_alphabetic())
}
