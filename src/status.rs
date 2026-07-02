//! The bottom status bar (tmux-style). Renders a single line of 256-color SGR
//! text exactly `cols` columns wide: a `csm` block, one tab per live session
//! (the active one highlighted), and a right-aligned segment.
//!
//! The right side carries a per-session usage readout (model, in/out tokens,
//! cache, estimated cost), each metric in its own color with a Nerd Font glyph.
//! It reflects whichever session is active, so it updates on every switch.
//!
//! Customization via env:
//!   CSM_STATUS=off            disable the bar entirely
//!   CSM_STATUS_ACCENT=<0-255> accent color (default 39)
//!   CSM_STATUS_RIGHT=<text>   right segment text (default: directory basename)

use crate::sessions::SessionUsage;

const BAR_BG: u8 = 236;
const BAR_FG: u8 = 246;
const BLACK: u8 = 16;

// Usage-readout colors (256-color), one per metric for at-a-glance scanning.
const C_MODEL: u8 = 141; // purple
const C_IN: u8 = 114; // green
const C_OUT: u8 = 215; // orange
const C_CACHE: u8 = 75; // blue
const C_COST: u8 = 220; // gold

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
/// terminated with a reset. `usage`, when present, is the active session's token
/// totals, rendered as a colored readout just left of the right segment.
pub fn render(
    cols: u16,
    tabs: &[Tab],
    right: &str,
    accent: u8,
    usage: Option<&SessionUsage>,
) -> String {
    let cols = cols as usize;
    if cols == 0 {
        return String::new();
    }

    let left_txt = " csm ";
    let left_vis = left_txt.chars().count();

    let right_txt = if right.trim().is_empty() {
        String::new()
    } else {
        format!(
            " {} ",
            trunc(right, cols.saturating_sub(left_vis + 4).max(1))
        )
    };
    let right_vis = right_txt.chars().count();

    // Usage readout, dropped whole if it would not fit beside csm + right.
    let (mut usage_txt, mut usage_vis) = match usage {
        Some(u) => build_usage(u),
        None => (String::new(), 0),
    };
    if left_vis + right_vis + usage_vis > cols {
        usage_txt = String::new();
        usage_vis = 0;
    }

    let mut s = String::new();
    let mut vis = 0;

    // Left block.
    s.push_str(&seg(left_txt, accent, BLACK, true));
    vis += left_vis.min(cols);

    // Tabs, within the budget left after the left + usage + right blocks.
    let tab_budget = cols.saturating_sub(left_vis + right_vis + usage_vis);
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

    // Fill the gap with bar background, then the usage readout and right segment.
    let fill = cols.saturating_sub(vis + usage_vis + right_vis);
    if fill > 0 {
        s.push_str(&seg(&" ".repeat(fill), BAR_BG, BAR_FG, false));
    }
    if usage_vis > 0 {
        s.push_str(&usage_txt);
    }
    if right_vis > 0 {
        s.push_str(&seg(&right_txt, accent, BLACK, true));
    }

    s.push_str("\x1b[0m");
    s
}

/// Build the colored usage readout (model · in · out · cache · cost), each with
/// a Nerd Font glyph. Returns the SGR string and its visible column width.
fn build_usage(u: &SessionUsage) -> (String, usize) {
    let cache = u.cache_read + u.cache_creation;
    let pieces: [(u8, String); 5] = [
        (C_MODEL, format!("\u{f2db} {}", u.model)), //  microchip — model
        (C_IN, format!("\u{f063} {}", fmt_count(u.input))), //  arrow-down — input tokens
        (C_OUT, format!("\u{f062} {}", fmt_count(u.output))), //  arrow-up — output tokens
        (C_CACHE, format!("\u{f1c0} {}", fmt_count(cache))), //  database — cache tokens
        // f0d6 (money bill) instead of f155 (dollar): f155 overdraws its cell
        // in Nerd Fonts and gets clipped.
        (C_COST, format!("\u{f0d6} {}", fmt_cost(u.cost_usd))), //  money — estimated cost
    ];
    let mut s = String::new();
    let mut vis = 0;
    for (i, (col, txt)) in pieces.iter().enumerate() {
        let sep = if i == 0 { " " } else { "  " };
        s.push_str(&seg(sep, BAR_BG, BAR_FG, false));
        s.push_str(&seg(txt, BAR_BG, *col, false));
        vis += sep.chars().count() + txt.chars().count();
    }
    s.push_str(&seg(" ", BAR_BG, BAR_FG, false));
    vis += 1;
    (s, vis)
}

/// Compact token count: 1234 -> "1.2k", 352789 -> "353k", 2.1M -> "2.10M".
fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Cost in dollars, with extra precision for cheap sessions.
fn fmt_cost(c: f64) -> String {
    if c >= 1.0 {
        format!("${:.2}", c)
    } else {
        format!("${:.3}", c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionUsage;

    #[test]
    fn count_formatting() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1234), "1.2k");
        assert_eq!(fmt_count(352789), "353k");
        assert_eq!(fmt_count(2_100_000), "2.10M");
    }

    #[test]
    fn cost_formatting() {
        assert_eq!(fmt_cost(0.0), "$0.000");
        assert_eq!(fmt_cost(0.123456), "$0.123");
        assert_eq!(fmt_cost(4.6), "$4.60");
    }

    #[test]
    fn usage_readout_has_glyphs_and_colors() {
        let u = SessionUsage {
            input: 8288,
            output: 6513,
            cache_creation: 82051,
            cache_read: 352789,
            cost_usd: 4.62,
            model: "Opus 4.8".to_string(),
        };
        let bar = render(200, &[], "csm", 39, Some(&u));
        // Glyphs present.
        assert!(bar.contains('\u{f2db}')); // model
        assert!(bar.contains('\u{f0d6}')); // cost
                                           // Values present.
        assert!(bar.contains("Opus 4.8"));
        assert!(bar.contains("$4.62"));
        assert!(bar.contains("435k")); // cache read + creation
                                       // Distinct colors applied.
        assert!(bar.contains(&format!("38;5;{C_MODEL}")));
        assert!(bar.contains(&format!("38;5;{C_COST}")));
    }

    #[test]
    fn usage_dropped_when_too_narrow() {
        let u = SessionUsage {
            model: "Opus 4.8".to_string(),
            ..Default::default()
        };
        let bar = render(8, &[], "x", 39, Some(&u));
        assert!(!bar.contains("Opus 4.8"));
    }
}
