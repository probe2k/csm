//! The terminal multiplexer: hosts one or more live `claude` sessions in PTYs,
//! renders the active one to the real terminal, and intercepts a prefix hotkey
//! (Ctrl-a by default) to drive a tmux-style session chooser.
//!
//! Architecture:
//!   - Each session runs `claude` inside a PTY (portable-pty). A reader thread
//!     drains its output into a per-session terminal emulator (vt100) AND ships
//!     the raw bytes to the main loop.
//!   - The active session is rendered by passing its raw bytes straight through
//!     (perfect fidelity). Background sessions keep updating their vt100 screen,
//!     so switching to one repaints its exact current state.
//!   - A stdin thread ships raw input bytes to the main loop, which forwards them
//!     to the active session unless they form a prefix command.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, Child, MasterPty, PtySize};
use vt100::Parser;

use crate::index::Index;
use crate::sessions::{self, SessionMeta};
use crate::tui::{self, Chooser, ChooserItem, ItemKind};

/// How csm should open the first session.
pub enum Initial {
    Resume { id: String, title: String },
    New,
}

struct Session {
    id: String,
    title: String,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    #[allow(dead_code)]
    child: Box<dyn Child + Send + Sync>,
    parser: Arc<Mutex<Parser>>,
    alive: bool,
}

enum Ev {
    Output(usize, Vec<u8>),
    Exit(usize),
    Input(Vec<u8>),
    Resize(u16, u16),
}

pub struct Mux {
    fp: String,
    target: String,
    index: Index,
    sessions: Vec<Session>,
    active: usize,
    rows: u16,
    cols: u16,
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    prefix: u8,
    prefix_pending: bool,
    chooser: Option<Chooser>,
    running: bool,
    status: bool,
    accent: u8,
    status_right: String,
    /// Last rendered screen of the active session, for diff-based compositing
    /// (only used when the status bar is on).
    last_screen: Option<vt100::Screen>,
}

/// Entry point: set up the terminal, spawn the first session, run the loop.
pub fn run(
    fp: String,
    target: String,
    index: Index,
    initial: Initial,
    open_chooser: bool,
) -> io::Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (tx, rx) = channel();

    crossterm::terminal::enable_raw_mode()?;
    install_panic_hook();
    spawn_stdin_thread(tx.clone());
    spawn_signal_thread(tx.clone());

    let dir_base = basename(&target);
    let mut mux = Mux {
        fp,
        target,
        index,
        sessions: Vec::new(),
        active: 0,
        rows,
        cols,
        tx,
        rx,
        prefix: crate::config::prefix().0,
        prefix_pending: false,
        chooser: None,
        running: true,
        status: crate::config::status_enabled(),
        accent: crate::config::status_accent(),
        status_right: crate::config::status_right(&dir_base),
        last_screen: None,
    };

    match initial {
        Initial::Resume { id, title } => {
            mux.resume_session(id, title);
        }
        Initial::New => {
            mux.new_session();
        }
    }
    mux.active = 0;

    if open_chooser {
        mux.open_chooser();
    } else {
        mux.render_active(); // paint initial content + status bar
    }

    let result = mux.event_loop();
    mux.teardown();
    result
}

impl Mux {
    fn event_loop(&mut self) -> io::Result<()> {
        while self.running {
            let ev = match self.rx.recv() {
                Ok(ev) => ev,
                Err(_) => break,
            };
            match ev {
                Ev::Output(idx, bytes) => {
                    if self.chooser.is_none() && idx == self.active && self.sessions[idx].alive {
                        if self.status {
                            // Composite from the vt100 grid so the bar's row is
                            // never scrolled over.
                            self.paint_content();
                        } else {
                            // Status bar off: original raw passthrough (exact
                            // fidelity, full height).
                            let mut out = io::stdout().lock();
                            out.write_all(&bytes)?;
                            out.flush()?;
                        }
                    }
                }
                Ev::Exit(idx) => self.handle_exit(idx),
                Ev::Input(bytes) => self.handle_input(&bytes),
                Ev::Resize(c, r) => self.handle_resize(c, r),
            }
        }
        Ok(())
    }

    // ---- session lifecycle -------------------------------------------------

    /// Height available to claude — one row less than the terminal when the
    /// status bar is shown (tmux reserves the bottom line the same way).
    fn content_rows(&self) -> u16 {
        if self.status {
            self.rows.saturating_sub(1).max(1)
        } else {
            self.rows
        }
    }

    fn spawn(&mut self, args: &[&str], title: String, id: String) -> usize {
        let crows = self.content_rows();
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: crows,
                cols: self.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let cmd = crate::claude::builder(args, &self.target);
        let child = pair.slave.spawn_command(cmd).expect("spawn claude");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        let parser = Arc::new(Mutex::new(Parser::new(crows, self.cols, 4000)));

        let idx = self.sessions.len();
        let parser_c = parser.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(Ev::Exit(idx));
                        break;
                    }
                    Ok(n) => {
                        if let Ok(mut p) = parser_c.lock() {
                            p.process(&buf[..n]);
                        }
                        if tx.send(Ev::Output(idx, buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Ev::Exit(idx));
                        break;
                    }
                }
            }
        });

        self.sessions.push(Session {
            id,
            title,
            master: pair.master,
            writer,
            child,
            parser,
            alive: true,
        });
        idx
    }

    fn new_session(&mut self) -> usize {
        let id = uuid::Uuid::new_v4().to_string();
        let _ = self.index.bind(&self.fp, &self.target, &id);
        let args = ["--session-id".to_string(), id.clone()];
        self.spawn(&[&args[0], &args[1]], "new session".to_string(), id)
    }

    fn resume_session(&mut self, id: String, title: String) -> usize {
        let args = ["--resume".to_string(), id.clone()];
        self.spawn(&[&args[0], &args[1]], title, id)
    }

    fn switch_to(&mut self, idx: usize) {
        if idx < self.sessions.len() && self.sessions[idx].alive {
            self.active = idx;
            self.render_active();
        }
    }

    fn handle_exit(&mut self, idx: usize) {
        if let Some(s) = self.sessions.get_mut(idx) {
            s.alive = false;
        }
        if idx == self.active && self.chooser.is_none() {
            match self.next_alive(idx) {
                Some(next) => self.switch_to(next),
                None => self.running = false, // last session gone -> quit
            }
        }
    }

    fn next_alive(&self, from: usize) -> Option<usize> {
        let n = self.sessions.len();
        for step in 1..=n {
            let i = (from + step) % n;
            if self.sessions[i].alive {
                return Some(i);
            }
        }
        None
    }

    fn prev_alive(&self, from: usize) -> Option<usize> {
        let n = self.sessions.len();
        for step in 1..=n {
            let i = (from + n - step) % n;
            if self.sessions[i].alive {
                return Some(i);
            }
        }
        None
    }

    // ---- rendering ---------------------------------------------------------

    /// Full repaint of the active session (after a switch, chooser close, or
    /// resize). Draws the content area and, if enabled, the status bar.
    fn render_active(&mut self) {
        if self.status {
            self.refresh_title(self.active);
            self.last_screen = None;
            self.paint_content(); // full, because last_screen was reset
            self.paint_status();
        } else {
            let Some(s) = self.sessions.get(self.active) else {
                return;
            };
            let Ok(parser) = s.parser.lock() else { return };
            let screen = parser.screen();
            let mut out = io::stdout().lock();
            let _ = out.write_all(b"\x1b[2J\x1b[H");
            let _ = out.write_all(&screen.contents_formatted());
            let _ = out.write_all(if screen.hide_cursor() {
                b"\x1b[?25l"
            } else {
                b"\x1b[?25h"
            });
            let _ = out.flush();
        }
    }

    /// Paint the active session's content area from its vt100 grid: a minimal
    /// diff against the last rendered screen, or a full repaint if there is none.
    /// Never touches the bottom row, so the status bar is left intact.
    fn paint_content(&mut self) {
        let Some(s) = self.sessions.get(self.active) else {
            return;
        };
        let cur = {
            let Ok(parser) = s.parser.lock() else { return };
            parser.screen().clone()
        };
        let mut out = io::stdout().lock();
        match &self.last_screen {
            Some(prev) => {
                let _ = out.write_all(&cur.contents_diff(prev));
            }
            None => {
                let _ = out.write_all(b"\x1b[2J");
                let _ = out.write_all(&cur.contents_formatted());
            }
        }
        let _ = out.write_all(if cur.hide_cursor() {
            b"\x1b[?25l"
        } else {
            b"\x1b[?25h"
        });
        let _ = out.flush();
        self.last_screen = Some(cur);
    }

    /// Draw the bottom status bar without disturbing the content cursor.
    fn paint_status(&mut self) {
        if !self.status {
            return;
        }
        let tabs: Vec<crate::status::Tab> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.alive)
            .map(|(i, s)| crate::status::Tab {
                num: i,
                title: s.title.clone(),
                active: i == self.active,
            })
            .collect();
        let bar = crate::status::render(self.cols, &tabs, &self.status_right, self.accent);
        let mut out = io::stdout().lock();
        // Save cursor+attrs, disable autowrap, draw on the last row, restore.
        let _ = write!(out, "\x1b7\x1b[{};1H\x1b[?7l{bar}\x1b[?7h\x1b8", self.rows);
        let _ = out.flush();
    }

    /// Refresh a session's title from its on-disk slug, if one exists yet.
    fn refresh_title(&mut self, idx: usize) {
        let id = match self.sessions.get(idx) {
            Some(s) => s.id.clone(),
            None => return,
        };
        if let Some(m) = sessions::list_bound(&self.target, std::slice::from_ref(&id)).first() {
            let t = title_of(m);
            if let Some(s) = self.sessions.get_mut(idx) {
                s.title = t;
            }
        }
    }

    fn handle_resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        let crows = self.content_rows();
        for s in &self.sessions {
            let _ = s.master.resize(PtySize {
                rows: crows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
            if let Ok(mut p) = s.parser.lock() {
                p.set_size(crows, cols);
            }
        }
        self.last_screen = None; // size changed -> force full repaint
        if self.chooser.is_some() {
            self.draw_chooser();
        } else {
            self.render_active();
        }
    }

    // ---- input -------------------------------------------------------------

    fn handle_input(&mut self, bytes: &[u8]) {
        if self.chooser.is_some() {
            self.chooser_input(bytes);
            return;
        }
        let mut forward: Vec<u8> = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if self.prefix_pending {
                self.prefix_pending = false;
                self.prefix_cmd(b, &mut forward);
            } else if b == self.prefix {
                self.prefix_pending = true;
            } else {
                forward.push(b);
            }
        }
        if !forward.is_empty() {
            self.forward(&forward);
        }
    }

    fn forward(&mut self, data: &[u8]) {
        if let Some(s) = self.sessions.get_mut(self.active) {
            let _ = s.writer.write_all(data);
            let _ = s.writer.flush();
        }
    }

    fn prefix_cmd(&mut self, b: u8, forward: &mut Vec<u8>) {
        match b {
            b's' => self.open_chooser(),
            b'c' => {
                let i = self.new_session();
                self.switch_to(i);
            }
            b'd' | b'q' => self.running = false,
            b'n' => {
                if let Some(i) = self.next_alive(self.active) {
                    self.switch_to(i);
                }
            }
            b'p' => {
                if let Some(i) = self.prev_alive(self.active) {
                    self.switch_to(i);
                }
            }
            b'0'..=b'9' => {
                let i = (b - b'0') as usize;
                self.switch_to(i);
            }
            x if x == self.prefix => forward.push(self.prefix), // prefix prefix -> literal
            _ => {}
        }
    }

    // ---- chooser -----------------------------------------------------------

    fn open_chooser(&mut self) {
        let bound = self.index.bound_ids(&self.fp);
        let disk = sessions::list_bound(&self.target, &bound);

        let live_ids: HashSet<String> = self
            .sessions
            .iter()
            .filter(|s| s.alive)
            .map(|s| s.id.clone())
            .collect();

        let mut items: Vec<ChooserItem> = Vec::new();
        let mut sel = 0;

        // Live sessions first (prefer the fresh on-disk slug if available).
        for (i, s) in self.sessions.iter().enumerate() {
            if !s.alive {
                continue;
            }
            if i == self.active {
                sel = items.len();
            }
            let label = disk
                .iter()
                .find(|m| m.id == s.id)
                .map(title_of)
                .unwrap_or_else(|| s.title.clone());
            items.push(ChooserItem {
                label,
                note: "(open)".to_string(),
                kind: ItemKind::Live(i),
            });
        }

        // Past on-disk sessions not currently running.
        for m in &disk {
            if live_ids.contains(&m.id) {
                continue;
            }
            items.push(ChooserItem {
                label: title_of(m),
                note: tui::rel_time(m.mtime),
                kind: ItemKind::Disk(m.id.clone()),
            });
        }

        // Always a "new" option at the bottom.
        items.push(ChooserItem {
            label: "+ New session".to_string(),
            note: String::new(),
            kind: ItemKind::New,
        });

        let mut chooser = Chooser::new(items, self.target.clone());
        chooser.sel = sel;
        self.chooser = Some(chooser);
        self.draw_chooser();
    }

    fn draw_chooser(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        if let Some(ch) = &self.chooser {
            let mut out = io::stdout();
            let _ = ch.draw(&mut out, cols, rows);
        }
    }

    fn chooser_input(&mut self, bytes: &[u8]) {
        enum Nav {
            Up,
            Down,
            Top,
            Bottom,
            Select,
            New,
            Cancel,
            None,
        }
        let nav = if bytes == b"\x1b[A" {
            Nav::Up
        } else if bytes == b"\x1b[B" {
            Nav::Down
        } else if bytes.len() == 1 {
            match bytes[0] {
                b'k' => Nav::Up,
                b'j' => Nav::Down,
                b'g' => Nav::Top,
                b'G' => Nav::Bottom,
                b'\r' | b'\n' => Nav::Select,
                b'n' => Nav::New,
                0x1b | b'q' => Nav::Cancel,
                _ => Nav::None,
            }
        } else {
            Nav::None
        };

        let ch = match &mut self.chooser {
            Some(ch) => ch,
            None => return,
        };
        match nav {
            Nav::Up => {
                ch.up();
                self.draw_chooser();
            }
            Nav::Down => {
                ch.down();
                self.draw_chooser();
            }
            Nav::Top => {
                ch.top();
                self.draw_chooser();
            }
            Nav::Bottom => {
                ch.bottom();
                self.draw_chooser();
            }
            Nav::Cancel => {
                self.chooser = None;
                self.render_active();
            }
            Nav::New => {
                self.chooser = None;
                let i = self.new_session();
                self.switch_to(i);
            }
            Nav::Select => {
                let chosen = ch.selected().map(|it| it.kind.clone());
                self.chooser = None;
                match chosen {
                    Some(ItemKind::Live(i)) => self.switch_to(i),
                    Some(ItemKind::Disk(id)) => {
                        let title = sessions::list_bound(&self.target, std::slice::from_ref(&id))
                            .first()
                            .map(title_of)
                            .unwrap_or_else(|| id.clone());
                        let i = self.resume_session(id, title);
                        self.switch_to(i);
                    }
                    Some(ItemKind::New) => {
                        let i = self.new_session();
                        self.switch_to(i);
                    }
                    None => self.render_active(),
                }
            }
            Nav::None => {}
        }
    }

    // ---- teardown ----------------------------------------------------------

    fn teardown(&mut self) {
        for s in &mut self.sessions {
            let _ = s.child.kill();
        }
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = io::stdout();
        // Leave alt screen, re-enable cursor, disable mouse modes, reset attrs.
        let _ = out.write_all(b"\x1b[?1049l\x1b[?1006l\x1b[?1002l\x1b[?1003l\x1b[?25h\x1b[0m");
        let _ = out.flush();
    }
}

/// Last path component, for the status bar's right segment.
fn basename(path: &str) -> String {
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// A readable title for a session: its slug, else a trimmed first-message.
pub fn title_of(m: &SessionMeta) -> String {
    if m.slug.is_empty() {
        tui::truncate(&m.preview, 50)
    } else {
        m.slug.clone()
    }
}

fn spawn_stdin_thread(tx: Sender<Ev>) {
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Ev::Input(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_signal_thread(tx: Sender<Ev>) {
    use signal_hook::consts::SIGWINCH;
    use signal_hook::iterator::Signals;
    if let Ok(mut signals) = Signals::new([SIGWINCH]) {
        thread::spawn(move || {
            for _ in signals.forever() {
                if let Ok((c, r)) = crossterm::terminal::size() {
                    if tx.send(Ev::Resize(c, r)).is_err() {
                        break;
                    }
                }
            }
        });
    }
}

/// Restore the terminal even if we panic somewhere.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?1049l\x1b[?25h\x1b[0m\r\n");
        let _ = out.flush();
        original(info);
    }));
}
