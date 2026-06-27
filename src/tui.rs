//! The session chooser overlay (tmux's prefix+s tree) and small drawing helpers.
//!
//! The chooser does NOT read input itself — the multiplexer owns the single raw
//! stdin stream and drives navigation. This module is pure state + rendering.

use std::io::{Stdout, Write};
use std::time::SystemTime;

use crossterm::{
    cursor, queue, style,
    terminal::{Clear, ClearType},
};

/// What a chooser row points at.
#[derive(Clone)]
pub enum ItemKind {
    /// An already-running session at this index in the mux's session list.
    Live(usize),
    /// A past on-disk session id, not currently running — resume it.
    Disk(String),
    /// Start a brand-new session.
    New,
}

#[derive(Clone)]
pub struct ChooserItem {
    pub label: String,
    pub note: String,
    pub kind: ItemKind,
}

pub struct Chooser {
    pub items: Vec<ChooserItem>,
    pub sel: usize,
    pub dir: String,
}

impl Chooser {
    pub fn new(items: Vec<ChooserItem>, dir: String) -> Chooser {
        Chooser { items, sel: 0, dir }
    }

    pub fn up(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.sel + 1 < self.items.len() {
            self.sel += 1;
        }
    }

    pub fn top(&mut self) {
        self.sel = 0;
    }

    pub fn bottom(&mut self) {
        self.sel = self.items.len().saturating_sub(1);
    }

    pub fn selected(&self) -> Option<&ChooserItem> {
        self.items.get(self.sel)
    }

    pub fn draw(&self, out: &mut Stdout, cols: u16, rows: u16) -> std::io::Result<()> {
        let width = cols as usize;
        queue!(
            out,
            cursor::Hide,
            Clear(ClearType::All),
            cursor::MoveTo(0, 0),
            style::SetAttribute(style::Attribute::Reverse),
            style::Print(pad(&format!(" csm — sessions — {}", self.dir), width)),
            style::SetAttribute(style::Attribute::Reset),
        )?;

        let top: u16 = 2;
        let max_visible = (rows as usize).saturating_sub(4).max(1);
        let start = if self.sel >= max_visible {
            self.sel - max_visible + 1
        } else {
            0
        };

        for (row, item) in self.items.iter().enumerate().skip(start).take(max_visible) {
            let y = top + (row - start) as u16;
            let marker = if row == self.sel { "> " } else { "  " };
            let note_w = 12;
            let label_w = width.saturating_sub(2 + note_w + 1).max(8);
            let line = format!(
                "{marker}{:<label_w$} {:>note_w$}",
                truncate(&item.label, label_w),
                truncate(&item.note, note_w),
            );
            queue!(out, cursor::MoveTo(0, y))?;
            if row == self.sel {
                queue!(
                    out,
                    style::SetAttribute(style::Attribute::Reverse),
                    style::Print(pad(&line, width)),
                    style::SetAttribute(style::Attribute::Reset),
                )?;
            } else {
                queue!(out, style::Print(truncate(&line, width)))?;
            }
        }

        queue!(
            out,
            cursor::MoveTo(0, rows.saturating_sub(1)),
            style::SetAttribute(style::Attribute::Reverse),
            style::Print(pad(
                " ↑↓ move   ⏎ select   n new   q/esc cancel ",
                width
            )),
            style::SetAttribute(style::Attribute::Reset),
        )?;
        out.flush()
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let cut = max.saturating_sub(1);
    let mut t: String = chars[..cut].iter().collect();
    t.push('…');
    t
}

pub fn pad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        truncate(s, width)
    } else {
        let mut t = s.to_string();
        t.extend(std::iter::repeat_n(' ', width - len));
        t
    }
}

/// Human-friendly relative time from a file mtime.
pub fn rel_time(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 2_592_000 => format!("{}d ago", s / 86_400),
        s => format!("{}mo ago", s / 2_592_000),
    }
}
