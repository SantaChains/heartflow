/// Errors surfaced by the conversation store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A SQLite-level failure (open, prepare, step, constraint, etc.).
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON (de)serialization of message blocks failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The stored row could not be decoded into a domain type.
    #[error("store format error: {0}")]
    Format(String),
    /// An incremental append found a message count other than the caller's
    /// base assumption (a compaction shortened the session, the DB was rebuilt,
    /// or another process rewrote it). The append was rejected untouched; the
    /// caller should fall back to a full `save_session`. `expected` is the base
    /// the caller believed present, `found` the actual stored count.
    #[error("append base drift: expected {expected} stored rows, found {found}")]
    Drift { expected: usize, found: usize },
}
