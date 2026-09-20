//! System-level SQLite store for conversation history.
//!
//! Sessions persisted as loose JSON files are already retrievable; the value
//! this crate adds is *queryable* history: full-text search across every past
//! conversation (FTS5 `trigram`, which handles CJK without a segmentation
//! crate, plus a `LIKE` fallback for short terms), token-usage analytics, and
//! transactional single-file integrity. Message content is stored as exact
//! UTF-8 (`TEXT`) so round-trips preserve Chinese, spaces, emoji, and Windows
//! paths byte-for-byte.

mod error;
mod model;
mod search;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use runtime::{ConversationMessage, Session};
use rusqlite::{params, Connection, OpenFlags, Transaction};

pub use error::StoreError;
pub use model::{
    flatten_search_text, role_from_str, role_str, SearchHit, SearchMethod, SessionMeta,
    SessionSummary, UsageTotals,
};
pub use search::{choose_method, fts_match_phrase, like_escape, TRIGRAM_MIN};

/// Schema version tracked via `PRAGMA user_version` (no meta table needed).
/// Version 2 adds the `pinned` column so compaction pins survive resume.
const SCHEMA_VERSION: i64 = 2;

/// One row from the aggregate token-usage query (all `SUM`s are nullable).
type UsageRow = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// Outcome of an integrity scan over the database file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    /// Every page, index, and FTS shadow table is structurally consistent.
    Ok,
    /// Inconsistencies were found. `problems` holds the raw diagnostic lines and
    /// `truncated` is true when SQLite stopped early at its error cap, meaning
    /// more issues exist than are listed.
    Corrupt {
        problems: Vec<String>,
        truncated: bool,
    },
}

/// Interpret the rows returned by an integrity pragma. A healthy run yields a
/// single `"ok"` row. Otherwise each row is one problem; SQLite appends a
/// "suppressing further errors" line when it stops at its cap.
fn classify_integrity(rows: Vec<String>) -> Integrity {
    if rows.len() == 1 && rows[0] == "ok" {
        return Integrity::Ok;
    }
    let truncated = rows
        .last()
        .is_some_and(|line| line.contains("suppressing further errors"));
    Integrity::Corrupt {
        problems: rows,
        truncated,
    }
}

/// Owns the SQLite connection and exposes the persistence + retrieval API.
///
/// The connection is opened once and shared for the process lifetime. It is
/// wrapped in a `Mutex` so every method is `&self` (a system store handed out
/// by reference); SQLite's `busy_timeout` plus WAL keep concurrent readers and
/// writers from erroring even across processes.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) the database at `path`, apply pragmas, and run
    /// migrations. Parent directories are NOT created (caller decides layout).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::init(conn, true)
    }

    /// Open an in-memory database (used by tests and ephemeral tooling).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, true)
    }

    /// Open the database without write access. Search and diagnostics use this so
    /// a read path can never mutate (or damage) the history file; it does not run
    /// migrations either, so a missing schema surfaces as a query error rather
    /// than being silently created.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::init(conn, false)
    }

    fn init(conn: Connection, writable: bool) -> Result<Self, StoreError> {
        // Per-connection tuning that is safe even on a read-only handle:
        // foreign-key enforcement, a busy timeout so concurrent processes wait
        // instead of erroring, and generous memory-mapped + page caches so
        // full-text/`LIKE` scans stay fast without repeated disk reads.
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;
             PRAGMA cache_size=-8000;
             PRAGMA mmap_size=268435456;
             PRAGMA temp_store=MEMORY;",
        )?;
        if writable {
            // WAL for crash-safe multi-process concurrency; NORMAL sync is the
            // standard pairing. Only a read-write handle may set journal mode or
            // migrate, so a reader never touches these.
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
            migrate(&conn)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Acquire the connection guard, mapping lock poisoning to a store error.
    /// Poisoning only happens if a panic occurred while holding the lock.
    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Format("store connection lock poisoned".to_string()))
    }

    /// Insert or fully replace a session and its ordered messages.
    ///
    /// Everything happens in one transaction: the old message rows (and their
    /// FTS entries) are dropped and rewritten, so a crash mid-save leaves the
    /// prior version intact. `session_id` is the stable external key.
    pub fn save_session(
        &self,
        session_id: &str,
        meta: &SessionMeta,
        messages: &[ConversationMessage],
    ) -> Result<(), StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let session_row = upsert_session(&tx, session_id, meta)?;

        tx.execute(
            "DELETE FROM messages_fts WHERE rowid IN (SELECT id FROM messages WHERE session_row = ?1);",
            params![session_row],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE session_row = ?1;",
            params![session_row],
        )?;
        write_message_rows(&tx, session_row, 0, messages)?;

        tx.commit()?;
        Ok(())
    }

    /// Append a new message *tail* onto an already-mirrored session without
    /// rewriting its existing rows — the fast path that turns per-turn history
    /// mirroring from O(session length) into O(new messages).
    ///
    /// `first_seq` is the sequence index the first element of `tail` must take
    /// (the caller's belief about how many rows are already stored), and
    /// `expected_existing` restates that base so the write can be validated.
    /// The whole operation is one transaction guarded by a base check: if the
    /// stored row count differs — a compaction shortened the session, the DB was
    /// rebuilt, or another process rewrote it — this returns
    /// [`StoreError::Drift`] *without* touching any row, and the caller falls
    /// back to the proven full [`save_session`](Self::save_session). The JSON
    /// transcript stays authoritative, so a rejected append never loses or
    /// duplicates history; it only defers to the rewrite.
    pub fn append_messages(
        &self,
        session_id: &str,
        meta: &SessionMeta,
        first_seq: usize,
        tail: &[ConversationMessage],
        expected_existing: usize,
    ) -> Result<(), StoreError> {
        if first_seq != expected_existing {
            return Err(StoreError::Drift {
                expected: expected_existing,
                found: first_seq,
            });
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let session_row = upsert_session(&tx, session_id, meta)?;

        let stored: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_row = ?1;",
            params![session_row],
            |row| row.get(0),
        )?;
        let stored = usize::try_from(stored).map_err(|e| StoreError::Format(e.to_string()))?;
        if stored != expected_existing {
            // Base moved under us: drop without commit (rolls back the header
            // upsert too) and let the caller take the full-rewrite path.
            return Err(StoreError::Drift {
                expected: expected_existing,
                found: stored,
            });
        }

        write_message_rows(&tx, session_row, first_seq, tail)?;
        tx.commit()?;
        Ok(())
    }

    /// Reconstruct a stored session, or `None` if no such session exists.
    pub fn load_session(&self, session_id: &str) -> Result<Option<Session>, StoreError> {
        let conn = self.lock()?;
        let exists: Option<i64> = conn
            .query_row(
                "SELECT id FROM sessions WHERE session_id = ?1;",
                params![session_id],
                |row| row.get(0),
            )
            .ok();
        let Some(session_row) = exists else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "SELECT role, blocks_json, has_usage, input_tokens, output_tokens, \
                    cache_creation_input_tokens, cache_read_input_tokens, pinned \
             FROM messages WHERE session_row = ?1 ORDER BY seq;",
        )?;
        let mut rows = stmt.query(params![session_row])?;
        let mut messages = Vec::new();
        while let Some(row) = rows.next()? {
            let role_raw: String = row.get(0)?;
            let blocks_json: String = row.get(1)?;
            let has_usage: i64 = row.get(2)?;
            let blocks = serde_json::from_str(&blocks_json)?;
            let usage = model::usage_from_columns(
                has_usage != 0,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            );
            let pinned: i64 = row.get(7)?;
            messages.push(ConversationMessage {
                role: role_from_str(&role_raw)?,
                blocks,
                usage,
                pinned: pinned != 0,
            });
        }
        Ok(Some(Session {
            version: 1,
            messages,
        }))
    }

    /// List sessions, newest activity first.
    pub fn list_sessions(&self, limit: i64) -> Result<Vec<SessionSummary>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT s.session_id, s.created_at, s.updated_at, s.provider, s.model, \
                    COUNT(m.id), \
                    COALESCE((SELECT substr(m2.search_text, 1, 120) FROM messages m2 \
                              WHERE m2.session_row = s.id AND m2.role = 'user' \
                              ORDER BY m2.seq LIMIT 1), '') \
             FROM sessions s LEFT JOIN messages m ON m.session_row = s.id \
             GROUP BY s.id ORDER BY s.updated_at DESC LIMIT ?1;",
        )?;
        let mut rows = stmt.query(params![limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(SessionSummary {
                session_id: row.get(0)?,
                created_at: row.get(1)?,
                updated_at: row.get(2)?,
                provider: row.get(3)?,
                model: row.get(4)?,
                message_count: row.get(5)?,
                preview: row.get(6)?,
            });
        }
        Ok(out)
    }

    /// Full-text / substring search across all (or one) session's messages.
    pub fn search(
        &self,
        query: &str,
        session_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let conn = self.lock()?;
        search::search(&conn, query, session_id, limit)
    }

    /// Aggregate token usage across all sessions, or one if `session_id` given.
    pub fn usage_totals(&self, session_id: Option<&str>) -> Result<UsageTotals, StoreError> {
        let conn = self.lock()?;
        let (with_usage, input, output, cache_create, cache_read): UsageRow = conn.query_row(
            "SELECT SUM(m.has_usage), SUM(m.input_tokens), SUM(m.output_tokens), \
                    SUM(m.cache_creation_input_tokens), SUM(m.cache_read_input_tokens) \
             FROM messages m JOIN sessions s ON s.id = m.session_row \
             WHERE (?1 IS NULL OR s.session_id = ?1);",
            params![session_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        Ok(UsageTotals {
            messages_with_usage: with_usage.unwrap_or_default(),
            input_tokens: input.unwrap_or_default(),
            output_tokens: output.unwrap_or_default(),
            cache_creation_input_tokens: cache_create.unwrap_or_default(),
            cache_read_input_tokens: cache_read.unwrap_or_default(),
        })
    }

    /// Delete a session; its messages cascade and its FTS rows are removed first.
    /// Returns whether a row was deleted.
    pub fn delete_session(&self, session_id: &str) -> Result<bool, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "DELETE FROM messages_fts WHERE rowid IN \
             (SELECT m.id FROM messages m JOIN sessions s ON s.id = m.session_row \
              WHERE s.session_id = ?1);",
            params![session_id],
        )?;
        let deleted = tx.execute(
            "DELETE FROM sessions WHERE session_id = ?1;",
            params![session_id],
        )?;
        tx.commit()?;
        let _ = changed;
        Ok(deleted > 0)
    }

    /// All stored session ids (unordered).
    pub fn session_ids(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT session_id FROM sessions;")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    /// Fold the WAL back into the main database file (for clean shutdowns),
    /// first refreshing the query planner's statistics.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Run SQLite's full structural integrity scan of the database file. This
    /// checks every page, index, and FTS5 shadow table; `Integrity::Ok` means the
    /// file is sound. Cost is a full O(size) scan, so callers gate it to on-demand
    /// diagnostics (see `quick_check` for the cheaper variant).
    pub fn integrity_check(&self) -> Result<Integrity, StoreError> {
        self.run_integrity("PRAGMA integrity_check;")
    }

    /// A faster scan that skips cross-checking index keys against table rows.
    /// Catches page/logical corruption and FTS drift but can miss a corrupt
    /// index; used when the database is large enough that a full scan is slow.
    pub fn quick_check(&self) -> Result<Integrity, StoreError> {
        self.run_integrity("PRAGMA quick_check;")
    }

    /// Run an integrity pragma and interpret its rows. A healthy run yields a
    /// single `"ok"` row; otherwise SQLite emits one diagnostic line per problem
    /// (bounded by its own error cap) and may stop early.
    fn run_integrity(&self, pragma: &str) -> Result<Integrity, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(pragma)?;
        let mut problems = Vec::new();
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            problems.push(row?);
        }
        Ok(classify_integrity(problems))
    }

    /// Current schema version stored in the database.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        let conn = self.lock()?;
        let version: i64 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
        Ok(version)
    }
}

/// Insert-or-refresh the session header row and return its internal id. Shared
/// by the full replace (`save_session`) and the incremental tail-append
/// (`append_messages`) so header semantics (which fields update on conflict) can
/// never drift between the two write paths.
fn upsert_session(
    tx: &Transaction<'_>,
    session_id: &str,
    meta: &SessionMeta,
) -> Result<i64, StoreError> {
    tx.execute(
        "INSERT INTO sessions (session_id, created_at, updated_at, source_path, provider, model)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id) DO UPDATE SET
             updated_at = excluded.updated_at,
             source_path = excluded.source_path,
             provider = excluded.provider,
             model = excluded.model;",
        params![
            session_id,
            meta.created_at,
            meta.updated_at,
            meta.source_path,
            meta.provider,
            meta.model
        ],
    )?;
    let session_row: i64 = tx.query_row(
        "SELECT id FROM sessions WHERE session_id = ?1;",
        params![session_id],
        |row| row.get(0),
    )?;
    Ok(session_row)
}

/// Write `messages` as ordered rows starting at sequence `start_seq`, plus their
/// FTS shadows, reusing one prepared statement per table across the loop.
///
/// This is the single definition of "how a message becomes stored rows", shared
/// byte-for-byte by both the full rewrite and the incremental append, so the two
/// paths always agree on column order, usage flattening, and pin encoding. The
/// old per-message `tx.execute(sql, ..)` re-ran `sqlite3_prepare` on identical
/// SQL 2*N times; `Statement::insert` binds/steps/resets and returns the rowid
/// in one call. FTS rows are written in a second pass so the two prepared
/// statements don't contend for the `tx` borrow.
fn write_message_rows(
    tx: &Transaction<'_>,
    session_row: i64,
    start_seq: usize,
    messages: &[ConversationMessage],
) -> Result<(), StoreError> {
    let mut inserted: Vec<(i64, String)> = Vec::with_capacity(messages.len());
    {
        let mut stmt = tx.prepare(
            "INSERT INTO messages (session_row, seq, role, blocks_json, search_text, has_usage, \
             input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, \
             pinned) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11);",
        )?;
        for (index, message) in messages.iter().enumerate() {
            let seq =
                i64::try_from(start_seq + index).map_err(|e| StoreError::Format(e.to_string()))?;
            let blocks_json = serde_json::to_string(&message.blocks)?;
            let search_text = flatten_search_text(&message.blocks);
            let (has_usage, input, output, cache_create, cache_read) = match message.usage {
                Some(u) => (
                    1,
                    i64::from(u.input_tokens),
                    i64::from(u.output_tokens),
                    i64::from(u.cache_creation_input_tokens),
                    i64::from(u.cache_read_input_tokens),
                ),
                None => (0, 0, 0, 0, 0),
            };
            let rowid = stmt.insert(params![
                session_row,
                seq,
                role_str(message.role),
                blocks_json,
                search_text,
                has_usage,
                input,
                output,
                cache_create,
                cache_read,
                i64::from(u8::from(message.pinned))
            ])?;
            inserted.push((rowid, search_text));
        }
    }
    {
        let mut stmt =
            tx.prepare("INSERT INTO messages_fts (rowid, search_text) VALUES (?1, ?2);")?;
        for (rowid, search_text) in &inserted {
            stmt.execute(params![rowid, search_text])?;
        }
    }
    Ok(())
}

/// Create/upgrade the schema to `SCHEMA_VERSION`, idempotently.
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let version: i64 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS sessions (
             id INTEGER PRIMARY KEY,
             session_id TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             source_path TEXT,
             provider TEXT,
             model TEXT
         );
         CREATE TABLE IF NOT EXISTS messages (
             id INTEGER PRIMARY KEY,
             session_row INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             seq INTEGER NOT NULL,
             role TEXT NOT NULL,
             blocks_json TEXT NOT NULL,
             search_text TEXT NOT NULL,
             has_usage INTEGER NOT NULL DEFAULT 0,
             input_tokens INTEGER NOT NULL DEFAULT 0,
             output_tokens INTEGER NOT NULL DEFAULT 0,
             cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
             cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
             pinned INTEGER NOT NULL DEFAULT 0,
             UNIQUE(session_row, seq)
         );
         CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_row);
         CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
             search_text,
             tokenize='trigram'
         );",
    )?;
    // Databases created before the pin feature already have `messages` without
    // the `pinned` column, so `CREATE TABLE IF NOT EXISTS` was a no-op there.
    // A fresh (version 0) database already got the column above.
    if version >= 1 {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;")?;
    }
    conn.execute_batch(&format!(
        "PRAGMA user_version = {SCHEMA_VERSION};
         COMMIT;"
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::ContentBlock;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user_text(text.to_string())
    }

    fn meta(now: i64) -> SessionMeta {
        SessionMeta {
            created_at: now,
            updated_at: now,
            source_path: None,
            provider: Some("test".to_string()),
            model: Some("m1".to_string()),
        }
    }

    #[test]
    fn opens_and_reports_schema_version() {
        let store = Store::open_in_memory().expect("open");
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
        assert_eq!(store.integrity_check().expect("check ran"), Integrity::Ok);
        assert_eq!(store.quick_check().expect("check ran"), Integrity::Ok);
    }

    #[test]
    fn classify_integrity_reads_the_pragma_rows() {
        assert_eq!(classify_integrity(vec!["ok".to_string()]), Integrity::Ok);
        assert_eq!(
            classify_integrity(vec!["page 3 bad".to_string()]),
            Integrity::Corrupt {
                problems: vec!["page 3 bad".to_string()],
                truncated: false,
            }
        );
        let capped = classify_integrity(vec![
            "rowid missing".to_string(),
            "*** in database main ***".to_string(),
            "... (suppressing further errors after 100 issues)".to_string(),
        ]);
        match capped {
            Integrity::Corrupt {
                problems,
                truncated,
            } => {
                assert_eq!(problems.len(), 3);
                assert!(truncated, "sentinel line should flag truncation");
            }
            other @ Integrity::Ok => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_a_session() {
        let store = Store::open_in_memory().expect("open");
        let messages = vec![
            user("hello"),
            ConversationMessage::assistant_with_usage(
                vec![
                    ContentBlock::Text {
                        text: "thinking".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_string(),
                        name: "bash".to_string(),
                        input: "echo hi".to_string(),
                    },
                ],
                Some(runtime::TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 2,
                }),
            ),
        ];
        store
            .save_session("s1", &meta(100), &messages)
            .expect("save");
        let loaded = store.load_session("s1").expect("load").expect("present");
        assert_eq!(loaded.messages, messages);
        assert_eq!(loaded.version, 1);
    }

    #[test]
    fn missing_session_loads_as_none() {
        let store = Store::open_in_memory().expect("open");
        assert!(store.load_session("nope").expect("load").is_none());
    }

    #[test]
    fn empty_session_is_distinguishable_from_missing() {
        let store = Store::open_in_memory().expect("open");
        store.save_session("e", &meta(1), &[]).expect("save empty");
        let loaded = store.load_session("e").expect("load");
        assert!(loaded.is_some(), "session exists even with no messages");
        assert_eq!(loaded.expect("present").messages.len(), 0);
    }
}
