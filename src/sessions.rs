//! Reading claude's on-disk session transcripts.
//!
//! claude stores sessions at `<config>/projects/<encoded-path>/<uuid>.jsonl`.
//! We locate the project dir for a given cwd, then cheaply parse each transcript
//! (bounded read of the first lines only) to extract a title, a preview of the
//! first user message, and the cwd it belongs to. Recency comes from file mtime.

use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::projects_root;

/// Metadata about a single session, cheap to construct.
pub struct SessionMeta {
    pub id: String,
    pub slug: String,
    pub preview: String,
    pub cwd: String,
    pub mtime: SystemTime,
    #[allow(dead_code)]
    pub path: PathBuf,
}

/// Encode an absolute path the way claude does: every non-alphanumeric ASCII
/// char becomes `-`.
pub fn encode_path(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Locate the project transcript dir for `target` (an absolute getcwd-form path).
/// Tries the deterministic encoding first; if that dir is absent, scans all
/// project dirs and matches by the `cwd` recorded inside a transcript. Returns
/// `None` if nothing matches.
pub fn locate_project_dir(target: &str) -> Option<PathBuf> {
    let root = projects_root();
    let encoded = root.join(encode_path(target));
    if encoded.is_dir() {
        return Some(encoded);
    }
    // Fallback: encoding edge cases / collisions. Scan and match by cwd.
    let entries = fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if let Some(first) = first_jsonl(&dir) {
            if let Some(meta) = parse_meta(&first) {
                if meta.cwd == target {
                    return Some(dir);
                }
            }
        }
    }
    None
}

fn first_jsonl(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") && p.is_file() {
            return Some(p);
        }
    }
    None
}

/// All real sessions on disk whose `cwd` equals `target`, regardless of binding.
/// Used for bootstrap adoption and the fallback project-dir match.
pub fn list_on_disk(target: &str) -> Vec<SessionMeta> {
    let mut out = Vec::new();
    let Some(dir) = locate_project_dir(target) else {
        return out;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") || !p.is_file() {
            continue;
        }
        if let Some(meta) = parse_meta(&p) {
            // Guard against encoded-path collisions between distinct cwds.
            if meta.cwd == target {
                out.push(meta);
            }
        }
    }
    out
}

/// Like `list_on_disk` but restricted to the given set of bound session ids,
/// then sorted most-recent first.
pub fn list_bound(target: &str, bound: &[String]) -> Vec<SessionMeta> {
    let mut v: Vec<SessionMeta> = list_on_disk(target)
        .into_iter()
        .filter(|s| bound.iter().any(|b| b == &s.id))
        .collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.mtime)); // most recent first
    v
}

/// Cheaply parse a transcript: read only the first lines, stop once we have a
/// slug, a cwd and the first real user message. Returns `None` for transcripts
/// with no user text (aborted sessions).
fn parse_meta(path: &Path) -> Option<SessionMeta> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut slug: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut preview: Option<String> = None;

    for (i, line) in reader.lines().enumerate() {
        if i > 120 {
            break;
        }
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        let Ok(v): Result<Value, _> = serde_json::from_str(&line) else {
            continue;
        };
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                cwd = Some(c.to_string());
            }
        }
        if slug.is_none() {
            if let Some(s) = v.get("slug").and_then(|x| x.as_str()) {
                slug = Some(s.to_string());
            }
        }
        if preview.is_none() && v.get("type").and_then(|x| x.as_str()) == Some("user") {
            if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
                if let Some(text) = extract_text(content) {
                    preview = Some(text);
                }
            }
        }
        if slug.is_some() && cwd.is_some() && preview.is_some() {
            break;
        }
    }

    let preview = preview?; // require a real user message
    let id = path.file_stem()?.to_str()?.to_string();
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    Some(SessionMeta {
        id,
        slug: slug.unwrap_or_default(),
        preview,
        cwd: cwd.unwrap_or_default(),
        mtime,
        path: path.to_path_buf(),
    })
}

/// Aggregated token/cost usage for one session, summed across its transcript.
#[derive(Clone, Default)]
pub struct SessionUsage {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub cost_usd: f64,
    /// Friendly label of the most recent model, e.g. "Opus 4.8".
    pub model: String,
}

/// Incrementally accumulates a session's usage from its growing transcript.
///
/// A session's `.jsonl` is append-only, so after the first read we remember the
/// byte offset we consumed and, on each `poll`, parse only the bytes appended
/// since — no re-reading the whole file. `poll` does nothing (beyond a cheap
/// length check) when the file hasn't grown, so an idle session costs nothing.
pub struct UsageTracker {
    path: PathBuf,
    offset: u64,
    usage: SessionUsage,
    last_model: Option<String>,
    has_data: bool,
}

impl UsageTracker {
    /// Start tracking the active session. Reads whatever is already on disk so
    /// the bar is correct immediately after a switch.
    pub fn new(target: &str, id: &str) -> Self {
        let dir =
            locate_project_dir(target).unwrap_or_else(|| projects_root().join(encode_path(target)));
        let mut t = UsageTracker {
            path: dir.join(format!("{id}.jsonl")),
            offset: 0,
            usage: SessionUsage::default(),
            last_model: None,
            has_data: false,
        };
        t.poll();
        t
    }

    /// The accumulated usage, or `None` if no assistant message has landed yet.
    pub fn usage(&self) -> Option<&SessionUsage> {
        self.has_data.then_some(&self.usage)
    }

    /// Fold any newly-appended transcript lines into the running totals. Returns
    /// `true` if the totals changed (so the caller knows to repaint). Cheap when
    /// nothing was appended: just one length check.
    pub fn poll(&mut self) -> bool {
        let Ok(file) = File::open(&self.path) else {
            return false;
        };
        let Ok(meta) = file.metadata() else {
            return false;
        };
        // Nothing new (or the file was unexpectedly truncated/rotated).
        if meta.len() <= self.offset {
            return false;
        }
        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }

        let mut consumed = self.offset;
        let mut changed = false;
        let mut line = String::new();
        loop {
            line.clear();
            let n = match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            // Only commit complete lines; a partial trailing line (the session
            // is mid-write) is left for the next poll to re-read.
            if !line.ends_with('\n') {
                break;
            }
            consumed += n as u64;
            if self.fold_line(line.trim_end()) {
                changed = true;
            }
        }
        self.offset = consumed;
        if changed {
            if let Some(m) = &self.last_model {
                self.usage.model = model_label(m);
            }
        }
        changed
    }

    /// Parse one transcript line and add its usage to the totals. Returns `true`
    /// if it carried usage.
    fn fold_line(&mut self, line: &str) -> bool {
        if line.is_empty() {
            return false;
        }
        let Ok(v): Result<Value, _> = serde_json::from_str(line) else {
            return false;
        };
        let Some(usage) = v.get("message").and_then(|m| m.get("usage")) else {
            return false;
        };

        let model = v
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let field = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let inp = field("input_tokens");
        let out = field("output_tokens");
        let cc = field("cache_creation_input_tokens");
        let cr = field("cache_read_input_tokens");

        // Cache creation is split into 5-minute and 1-hour TTL pools, billed at
        // different multiples of the base input rate. Fall back to treating all
        // of it as 5-minute when no breakdown is present.
        let (c5, c1) = usage
            .get("cache_creation")
            .map(|cv| {
                let g = |k: &str| cv.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                (
                    g("ephemeral_5m_input_tokens"),
                    g("ephemeral_1h_input_tokens"),
                )
            })
            .unwrap_or((cc, 0));

        self.usage.input += inp;
        self.usage.output += out;
        self.usage.cache_creation += cc;
        self.usage.cache_read += cr;
        self.usage.cost_usd += message_cost(model, inp, out, cr, c5, c1);
        if !model.is_empty() {
            self.last_model = Some(model.to_string());
        }
        self.has_data = true;
        true
    }
}

/// Estimated USD cost of one assistant message from its token counts.
fn message_cost(
    model: &str,
    inp: u64,
    out: u64,
    cache_read: u64,
    cache_5m: u64,
    cache_1h: u64,
) -> f64 {
    let (pin, pout, pread, p5m, p1h) = price_per_mtok(model);
    (inp as f64 * pin
        + out as f64 * pout
        + cache_read as f64 * pread
        + cache_5m as f64 * p5m
        + cache_1h as f64 * p1h)
        / 1_000_000.0
}

/// Per-million-token prices in USD: (input, output, cache_read, cache_write_5m,
/// cache_write_1h). Unknown models default to Sonnet-tier pricing.
fn price_per_mtok(model: &str) -> (f64, f64, f64, f64, f64) {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        (15.0, 75.0, 1.50, 18.75, 30.0)
    } else if m.contains("haiku") {
        (1.0, 5.0, 0.10, 1.25, 2.0)
    } else {
        (3.0, 15.0, 0.30, 3.75, 6.0)
    }
}

/// Turn a raw model id like "claude-opus-4-8" into a friendly "Opus 4.8".
fn model_label(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    let fam = if lower.contains("opus") {
        "Opus"
    } else if lower.contains("sonnet") {
        "Sonnet"
    } else if lower.contains("haiku") {
        "Haiku"
    } else {
        return model.to_string();
    };
    // Collect the numeric version segments that follow the family name.
    let ver: Vec<&str> = lower
        .split(['-', '_', '.'])
        .skip_while(|seg| !starts_digit(seg))
        .take_while(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
        .take(2)
        .collect();
    if ver.is_empty() {
        fam.to_string()
    } else {
        format!("{} {}", fam, ver.join("."))
    }
}

fn starts_digit(seg: &str) -> bool {
    seg.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Pull display text out of a user message `content`, which is either a plain
/// string or an array of content blocks. Returns `None` for tool-result-only
/// messages (no human text). Strips leading `<system-reminder>`/command noise.
fn extract_text(content: &Value) -> Option<String> {
    let raw = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut t = String::new();
            for b in blocks {
                if b.get("type").and_then(|x| x.as_str()) == Some("text") {
                    if let Some(s) = b.get("text").and_then(|x| x.as_str()) {
                        t.push_str(s);
                    }
                }
            }
            t
        }
        _ => return None,
    };
    let cleaned = clean_preview(&raw);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Collapse whitespace and drop obvious non-prose wrappers so previews read well.
fn clean_preview(s: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for line in s.lines() {
        let line = line.trim();
        if line.starts_with("<system-reminder") || line.starts_with("<command-") {
            continue;
        }
        for ch in line.chars() {
            if ch.is_whitespace() {
                if !prev_space && !out.is_empty() {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(ch);
                prev_space = false;
            }
        }
        if !out.is_empty() && !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_labels() {
        assert_eq!(model_label("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_label("claude-sonnet-4-6-20250101"), "Sonnet 4.6");
        assert_eq!(model_label("claude-haiku-4-5"), "Haiku 4.5");
        assert_eq!(model_label("some-future-model"), "some-future-model");
    }

    #[test]
    fn opus_cost_matches_published_rates() {
        // 1M input + 1M output at Opus rates = $15 + $75.
        let c = message_cost("claude-opus-4-8", 1_000_000, 1_000_000, 0, 0, 0);
        assert!((c - 90.0).abs() < 1e-9, "got {c}");
        // 1M cache-read at $1.50.
        let c = message_cost("claude-opus-4-8", 0, 0, 1_000_000, 0, 0);
        assert!((c - 1.5).abs() < 1e-9, "got {c}");
    }

    fn tracker_at(path: std::path::PathBuf) -> UsageTracker {
        UsageTracker {
            path,
            offset: 0,
            usage: SessionUsage::default(),
            last_model: None,
            has_data: false,
        }
    }

    #[test]
    fn tracker_accumulates_incrementally() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("csm-usage-test-{}.jsonl", std::process::id()));
        let _ = fs::remove_file(&path);

        let line = |inp: u64, out: u64| {
            format!(
                r#"{{"message":{{"model":"claude-opus-4-8","usage":{{"input_tokens":{inp},"output_tokens":{out}}}}}}}"#
            )
        };

        // Empty file -> no usage yet.
        File::create(&path).unwrap();
        let mut t = tracker_at(path.clone());
        assert!(t.usage().is_none());

        // First assistant message lands.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", line(100, 50)).unwrap();
        assert!(t.poll());
        let u = t.usage().unwrap();
        assert_eq!((u.input, u.output), (100, 50));
        assert_eq!(u.model, "Opus 4.8");
        let off_after_first = t.offset;

        // No growth -> poll is a no-op and re-reads nothing.
        assert!(!t.poll());
        assert_eq!(t.offset, off_after_first);

        // Append a second message: only the new bytes are folded in.
        writeln!(f, "{}", line(10, 5)).unwrap();
        assert!(t.poll());
        let u = t.usage().unwrap();
        assert_eq!((u.input, u.output), (110, 55));

        // A partial trailing line (no newline yet) is not committed until complete.
        write!(f, "{}", line(1000, 1000)).unwrap();
        f.flush().unwrap();
        assert!(!t.poll());
        assert_eq!(t.usage().unwrap().input, 110);
        writeln!(f).unwrap(); // finish the line
        assert!(t.poll());
        assert_eq!(t.usage().unwrap().input, 1110);

        let _ = fs::remove_file(&path);
    }
}
