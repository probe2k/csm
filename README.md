# csm — Claude Session Manager

A fast, tmux-like **terminal multiplexer for [`claude`](https://code.claude.com)
sessions**. It opens like a tmux window, runs `claude` live inside it, and gives
you a prefix-hotkey session chooser — so you never have to remember a session hash
again.

```
csm                 # open the multiplexer here; resume the latest session (or start one)
csm ~/code/api      # cd into a dir first, then open there
csm -n              # open with a brand-new session
csm -l              # open with the session chooser showing immediately
csm ls              # list every directory csm manages
```

## Inside the multiplexer (prefix = `Ctrl-o`)

Just like tmux: press the prefix, then a command.

| Keys | Action |
|------|--------|
| `Ctrl-o` `s` | **Session chooser** — pick an existing session, or **+ New session** at the bottom |
| `Ctrl-o` `c` | New session |
| `Ctrl-o` `n` / `p` | Next / previous session |
| `Ctrl-o` `0`–`9` | Jump to session N |
| `Ctrl-o` `d` | Detach (quit csm — conversations stay saved on disk) |
| `Ctrl-o` `Ctrl-o` | Send a literal `Ctrl-o` to claude |

In the chooser: `↑`/`↓` (or `j`/`k`) move, `Enter` selects, `n` makes a new
session, `q`/`Esc` cancels. Sessions already running show `(open)`; past sessions
show how long ago they were used.

**The prefix is `Ctrl-o` by default** — chosen so it doesn't clash with tmux
(whose prefix is usually `Ctrl-b` or `Ctrl-a`), the macOS terminal, the shell, or
claude itself. So you can run csm *inside* tmux without the two fighting over a
key. To remap it, set `CSM_PREFIX` to any `Ctrl-<letter>`:

```sh
CSM_PREFIX=C-g csm      # use Ctrl-g instead   (also accepts ctrl-g, ^g, or just g)
```

> Running csm inside tmux? csm hides the outer `TMUX` env from claude, so claude
> treats csm's pane as a normal terminal (no "tmux detected" message).

Multiple sessions stay **live** at once — switching is instant and each session's
screen is preserved exactly (a per-session terminal emulator keeps its state while
it's in the background).

## Status bar

Like tmux, csm draws a status bar on the bottom row showing a `csm` block, one
tab per live session (`N:title`, the active one highlighted in the accent color),
and a right-hand segment (the directory name by default):

```
 csm  0:fix auth bug  1:refactor parser                                   api
```

The session numbers map directly to the `<prefix> 0`–`9` jump keys. csm reserves
that bottom row and runs claude in the rows above it (exactly how tmux reserves
its status line), compositing claude's screen so it can never scroll over the bar.

Customize it with env vars:

| Variable | Effect |
|----------|--------|
| `CSM_STATUS=off` | Hide the bar (reverts to exact raw passthrough, full height) |
| `CSM_STATUS_ACCENT=<0-255>` | Accent color (256-color index; default `39`) |
| `CSM_STATUS_RIGHT=<text>` | Right-hand segment text (default: directory basename) |

```sh
CSM_STATUS_ACCENT=201 CSM_STATUS_RIGHT="$(whoami)@api" csm
```

## Configuration

Every csm setting is an environment variable — set them per-invocation or export
them from your shell profile.

| Variable | Default | Effect |
|----------|---------|--------|
| `CSM_PREFIX` | `C-o` | Multiplexer prefix key. Accepts `C-g`, `ctrl-g`, `^g`, or `g`. |
| `CSM_STATUS` | `on` | Set to `off`/`0` to hide the status bar (raw passthrough, full height). |
| `CSM_STATUS_ACCENT` | `39` | Status bar accent color (256-color index, `0`–`255`). |
| `CSM_STATUS_RIGHT` | dir basename | Text for the status bar's right segment. |
| `CSM_CLAUDE_BIN` | `claude` | Path to the claude binary to launch. |
| `CSM_BINDIR` | `~/.local/bin` | Install/uninstall target dir (used by the scripts). |
| `CLAUDE_CONFIG_DIR` | `~/.claude` | Base config dir; csm reads `…/projects/` and writes `…/csm/index.redb`. (Shared with claude.) |

```sh
# example: green accent, custom prefix, custom right segment
export CSM_PREFIX=C-g
export CSM_STATUS_ACCENT=35
export CSM_STATUS_RIGHT="$(whoami)@$(hostname -s)"
```

## Why not just `claude --continue`?

`claude` keys sessions by the directory **path string**. If you delete a folder
`xyz` and later create a new, unrelated `xyz` at the same path, `claude` will
happily offer the old folder's conversations.

`csm` identifies a directory by its **inode fingerprint** (`device + inode +
birthtime`), not its name. A recreated folder is a different physical directory,
so its old sessions stay hidden and you start fresh — automatically.

## How it works

- **Hosting**: each session runs `claude` in a pseudo-terminal
  ([`portable-pty`](https://crates.io/crates/portable-pty)). A
  [`vt100`](https://crates.io/crates/vt100) emulator per session keeps its screen
  so background sessions repaint perfectly on switch. With the status bar on, csm
  composites the active session's grid into the rows above the bar (diffing for
  minimal redraws); with `CSM_STATUS=off` it falls back to raw byte passthrough at
  full height. Input is forwarded raw, with the prefix key intercepted — the same
  model tmux uses.
- **Identity & state**: `claude` already stores transcripts at
  `~/.claude/projects/<encoded-path>/<uuid>.jsonl` (honors `CLAUDE_CONFIG_DIR`).
  `csm` keeps one small embedded database, `~/.claude/csm/index.redb`
  ([redb](https://crates.io/crates/redb)), mapping each directory fingerprint to
  its session ids. Titles/timestamps are read live from the transcripts, never
  duplicated. Each update is an atomic, crash-safe transaction, and because csm
  is multi-process (one per terminal), the DB is opened only transiently per
  operation — so several csm instances running at once serialize their writes
  instead of clobbering each other.
- **Launching**: new sessions use `claude --session-id <uuid>` (so csm knows the
  id up front); resumes use `claude --resume <uuid>`.
- **Bootstrap**: the first time you point csm at a directory that already has
  sessions, it adopts them. After that, the fingerprint is the source of truth.

This build is an **in-memory multiplexer**: quitting csm ends the live `claude`
processes, but every conversation is already saved by claude and resumes
instantly next launch.

## Install

Requires a Rust toolchain to build. Runtime dependency: the `claude` CLI on
`PATH` (override with `CSM_CLAUDE_BIN`).

The `install.sh` script builds csm in release mode and symlinks it onto your
`PATH`:

```sh
./install.sh        # cargo build --release + symlink into ~/.local/bin
```

Then just run `csm` in any project. If `~/.local/bin` isn't on your `PATH`, the
script will say so — add it to your shell profile.

Want it somewhere else? Set `CSM_BINDIR`:

```sh
CSM_BINDIR=/usr/local/bin ./install.sh
```

Or do it by hand:

```sh
cargo build --release
ln -sf "$PWD/target/release/csm" ~/.local/bin/csm
```

## Uninstall

The `uninstall.sh` script is the mirror of `install.sh` — it removes the symlink
it created (and only that one; a `csm` on your `PATH` from elsewhere is left
untouched):

```sh
./uninstall.sh            # remove the installed csm symlink
./uninstall.sh --purge    # also delete csm's index (~/.claude/csm/)
./uninstall.sh --help
```

`--purge` removes only csm's own bookkeeping under `~/.claude/csm/`. It **never**
touches claude's session transcripts in `~/.claude/projects/`, so your
conversations are safe either way. Use the same `CSM_BINDIR` override if you
installed to a custom location:

```sh
CSM_BINDIR=/usr/local/bin ./uninstall.sh
```

Build artifacts are left in place; run `cargo clean` to remove them.
