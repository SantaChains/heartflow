use std::fmt::{Display, Formatter};

/// Errors surfaced by the conversation store. Mirrors the hand-rolled error
/// enums used elsewhere in the workspace (no `thiserror` dependency).
#[derive(Debug)]
pub enum StoreError {
    /// A SQLite-level failure (open, prepare, step, constraint, etc.).
    Sqlite(rusqlite::Error),
    /// JSON (de)serialization of message blocks failed.
    Json(serde_json::Error),
    /// The stored row could not be decoded into a domain type.
    Format(String),
    /// An incremental append found a message count other than the caller's
    /// base assumption (a compaction shortened the session, the DB was rebuilt,
    /// or another process rewrote it). The append was rejected untouched; the
    /// caller should fall back to a full `save_session`. `expected` is the base
    /// the caller believed present, `found` the actual stored count.
    Drift { expected: usize, found: usize },
}

impl Display for StoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "sqlite error: {error}"),
            Self::Json(error) => write!(f, "json error: {error}"),
            Self::Format(error) => write!(f, "store format error: {error}"),
            Self::Drift { expected, found } => {
                write!(
                    f,
                    "append base drift: expected {expected} stored rows, found {found}"
                )
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}
