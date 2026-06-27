//! Builds the `claude` command to run inside a PTY. (No more exec — the
//! multiplexer hosts claude in a pseudo-terminal so it can keep several sessions
//! alive at once.)

use portable_pty::CommandBuilder;

/// Build a `CommandBuilder` for claude with the given args, running in `cwd` and
/// inheriting the current environment. `CSM_CLAUDE_BIN` overrides the binary
/// (used by tests).
pub fn builder(args: &[&str], cwd: &str) -> CommandBuilder {
    let bin = std::env::var("CSM_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let mut cmd = CommandBuilder::new(bin);
    for a in args {
        cmd.arg(a);
    }
    cmd.cwd(cwd);
    for (k, v) in std::env::vars_os() {
        // csm is itself the multiplexer hosting claude in its own PTY, so hide
        // any outer tmux — otherwise claude prints "tmux detected" and tries
        // tmux integrations that don't apply here.
        if matches!(k.to_str(), Some("TMUX") | Some("TMUX_PANE")) {
            continue;
        }
        cmd.env(k, v);
    }
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }
    cmd
}
