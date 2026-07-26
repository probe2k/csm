//! csm's persisted index, backed by a redb embedded ACID key-value store.
//!
//! This is the ONLY state csm owns: a map of directory fingerprint -> { path,
//! session ids }. Session titles/timestamps are NOT stored here — they are read
//! live from the `.jsonl` transcripts so they are always fresh.
//!
//! Concurrency model: csm is multi-process (one per terminal), and redb takes an
//! exclusive file lock for the lifetime of an open `Database`. So we never hold
//! the database open across a session — every operation opens it transiently
//! (open -> transaction -> drop), retrying briefly if another csm instance holds
//! the lock. Each write is a single committed transaction, so it is atomic and
//! crash-safe; concurrent instances serialize instead of clobbering each other.

use std::io;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use redb::{Database, DatabaseError, ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};

use crate::config::{index_path, legacy_index_path};

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("fingerprints");

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Entry {
    /// Logical absolute path (getcwd form) this fingerprint was last seen at.
    pub path: String,
    /// Session UUIDs created/adopted under this fingerprint.
    pub sessions: Vec<String>,
}

/// A lightweight handle to the index database (just the path — the DB is opened
/// transiently per call). Cheap to clone and move.
#[derive(Clone)]
pub struct Index {
    path: PathBuf,
}

impl Index {
    pub fn load() -> Index {
        let path = index_path();
        migrate_legacy(&path);
        Index { path }
    }

    // ---- reads (best-effort: errors degrade to "empty") --------------------

    pub fn bound_ids(&self, fp: &str) -> Vec<String> {
        self.bound_ids_checked(fp).unwrap_or_default()
    }

    /// Same as `bound_ids`, but surfaces read failures instead of masking them
    /// as "no sessions". Callers that decide whether to *write* based on this
    /// (e.g. the on-disk-adoption fallback) must use this form — otherwise a
    /// transient lock/I/O error looks identical to "never managed" and can
    /// trigger an overwrite of real data.
    pub fn bound_ids_checked(&self, fp: &str) -> io::Result<Vec<String>> {
        Ok(self.get(fp)?.map(|e| e.sessions).unwrap_or_default())
    }

    /// True if any fingerprint already maps to this path (i.e. csm has managed
    /// this path before, possibly under a different physical folder).
    pub fn path_seen_before(&self, path: &str) -> bool {
        self.entries().iter().any(|e| e.path == path)
    }

    /// Every entry, for the global `csm ls` overview.
    pub fn entries(&self) -> Vec<Entry> {
        self.try_entries().unwrap_or_default()
    }

    // ---- writes (atomic single transactions) -------------------------------

    /// Bind a session id under a fingerprint, recording/refreshing the path.
    ///
    /// Propagates read failures instead of defaulting to an empty entry: a
    /// transient error here must never look like "no prior sessions", or a
    /// retry would overwrite (and lose) whatever was already bound.
    pub fn bind(&self, fp: &str, path: &str, session_id: &str) -> io::Result<()> {
        let mut entry = self.get(fp)?.unwrap_or_default();
        entry.path = path.to_string();
        if !entry.sessions.iter().any(|s| s == session_id) {
            entry.sessions.push(session_id.to_string());
        }
        self.put(fp, &entry)
    }

    /// Replace the full set of bound sessions for a fingerprint (used on adopt).
    pub fn set_sessions(&self, fp: &str, path: &str, ids: Vec<String>) -> io::Result<()> {
        self.put(
            fp,
            &Entry {
                path: path.to_string(),
                sessions: ids,
            },
        )
    }

    /// Remove a single session id from a fingerprint's bound list.
    #[allow(dead_code)]
    pub fn unbind(&self, fp: &str, session_id: &str) -> io::Result<()> {
        if let Some(mut entry) = self.get(fp)? {
            entry.sessions.retain(|s| s != session_id);
            self.put(fp, &entry)?;
        }
        Ok(())
    }

    // ---- low-level db access ------------------------------------------------

    /// `Ok(None)` means the fingerprint is genuinely unbound. Any other
    /// failure (lock contention, I/O, decode error) is an `Err`, kept
    /// distinct so read-modify-write callers don't mistake "couldn't read"
    /// for "nothing there" and clobber existing data.
    fn get(&self, fp: &str) -> io::Result<Option<Entry>> {
        let db = self.open()?;
        let rtx = db.begin_read().map_err(ioerr)?;
        let table = match rtx.open_table(TABLE) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(ioerr(e)),
        };
        match table.get(fp).map_err(ioerr)? {
            Some(guard) => bincode::deserialize(guard.value())
                .map(Some)
                .map_err(|e| io::Error::other(format!("index decode: {e}"))),
            None => Ok(None),
        }
    }

    fn try_entries(&self) -> io::Result<Vec<Entry>> {
        let db = self.open()?;
        let rtx = db.begin_read().map_err(ioerr)?;
        let table = match rtx.open_table(TABLE) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(ioerr(e)),
        };
        let mut out = Vec::new();
        for row in table.iter().map_err(ioerr)? {
            let (_k, v) = row.map_err(ioerr)?;
            if let Ok(entry) = bincode::deserialize::<Entry>(v.value()) {
                out.push(entry);
            }
        }
        Ok(out)
    }

    fn put(&self, fp: &str, entry: &Entry) -> io::Result<()> {
        let bytes = bincode::serialize(entry)
            .map_err(|e| io::Error::other(format!("index encode: {e}")))?;
        let db = self.open()?;
        let wtx = db.begin_write().map_err(ioerr)?;
        {
            let mut table = wtx.open_table(TABLE).map_err(ioerr)?;
            table.insert(fp, bytes.as_slice()).map_err(ioerr)?;
        }
        wtx.commit().map_err(ioerr)?;
        Ok(())
    }

    /// Open the database transiently, retrying briefly if another csm instance
    /// currently holds the exclusive lock.
    fn open(&self) -> io::Result<Database> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut last: Option<DatabaseError> = None;
        for _ in 0..100 {
            match Database::create(&self.path) {
                Ok(db) => return Ok(db),
                Err(DatabaseError::DatabaseAlreadyOpen) => {
                    sleep(Duration::from_millis(15));
                }
                Err(e) => return Err(ioerr(e)),
            }
            last = Some(DatabaseError::DatabaseAlreadyOpen);
        }
        Err(ioerr(last.unwrap_or(DatabaseError::DatabaseAlreadyOpen)))
    }
}

fn ioerr<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("index db: {e}"))
}

/// One-time move from the old location (inside claude's own `~/.claude/csm/`)
/// to the new one. Best-effort: if it fails, csm just starts a fresh index
/// rather than erroring out.
fn migrate_legacy(new_path: &PathBuf) {
    if new_path.exists() {
        return;
    }
    let legacy = legacy_index_path();
    if !legacy.exists() {
        return;
    }
    if let Some(parent) = new_path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if std::fs::rename(&legacy, new_path).is_err() {
        let _ = std::fs::copy(&legacy, new_path).map(|_| std::fs::remove_file(&legacy));
    }
}
