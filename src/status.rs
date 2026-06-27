//! The bottom status bar (tmux-style). Renders a single line of 256-color SGR
//! text exactly `cols` columns wide: a `csm` block, one tab per live session
//! (the active one highlighted), and a right-aligned segment.
//!
//! Customization via env:
//!   CSM_STATUS=off            disable the bar entirely
//!   CSM_STATUS_ACCENT=<0-255> accent color (default 39)
//!   CSM_STATUS_RIGHT=<text>   right segment text (default: directory basename)

const BAR_BG: u8 = 236;
const BAR_FG: u8 = 246;
const BLACK: u8 = 16;

pub struct Tab {
    pub num: usize,
    pub title: String,
    pub active: bool,
}

fn seg(text: &str, bg: u8, fg: u8, bold: bool) -> String {
    let b = if bold { "\x1b[1m" } else { "" };
    format!("\x1b[48;5;{bg}m\x1b[38;5;{fg}m{b}{text}")
}

fn trunc(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut t: String = chars[..max.saturating_sub(1)].iter().collect();
    t.push('…');
    t
}

/// Build the full status line: an SGR string with exactly `cols` visible columns,
/// terminated with a reset.
pub fn render(cols: u16, tabs: &[Tab], right: &str, accent: u8) -> String {
    let cols = cols as usize;
    if cols == 0 {
        return String::new();
    }

    let left_txt = " csm ";
    let left_vis = left_txt.chars().count();

    let right_txt = if right.trim().is_empty() {
        String::new()
    } else {
        format!(" {} ", trunc(right, cols.saturating_sub(left_vis + 4).max(1)))
    };
    let right_vis = right_txt.chars().count();

    let mut s = String::new();
    let mut vis = 0;

    // Left block.
    s.push_str(&seg(left_txt, accent, BLACK, true));
    vis += left_vis.min(cols);

    // Tabs, within the budget left after the left + right blocks.
    let tab_budget = cols.saturating_sub(left_vis + right_vis);
    let mut used = 0;
    for t in tabs {
        let label = format!(" {}:{} ", t.num, trunc(&t.title, 16));
        let lv = label.chars().count();
        if used + lv > tab_budget {
            break;
        }
        if t.active {
            s.push_str(&seg(&label, accent, BLACK, true));
        } else {
            s.push_str(&seg(&label, BAR_BG, BAR_FG, false));
        }
        used += lv;
    }
    vis += used;

    // Fill the gap with bar background, then the right segment.
    let fill = cols.saturating_sub(vis + right_vis);
    if fill > 0 {
        s.push_str(&seg(&" ".repeat(fill), BAR_BG, BAR_FG, false));
    }
    if right_vis > 0 {
        s.push_str(&seg(&right_txt, accent, BLACK, true));
    }

    s.push_str("\x1b[0m");
    s
}
