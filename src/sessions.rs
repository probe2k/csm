//! Reading claude's on-disk session transcripts.
//!
//! claude stores sessions at `<config>/projects/<encoded-path>/<uuid>.jsonl`.
//! We locate the project dir for a given cwd, then cheaply parse each transcript
//! (bounded read of the first lines only) to extract a title, a preview of the
//! first user message, and the cwd it belongs to. Recency comes from file mtime.

use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
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
