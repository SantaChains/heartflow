//! Impact / stress tests for the SQLite history store.
//!
//! `io_correctness.rs` proves single-connection *content* fidelity. This file
//! attacks the dimension that matters for a *shared system database*: many
//! concurrent writers, readers interleaved with writers, checkpoint storms, and
//! bulk volume — the conditions that historically surface `SQLITE_BUSY`, WAL
//! recovery races, FTS5 shadow-table drift, and torn transactions. The realistic
//! threat is several `hf` processes sharing `~/.heartflow/heartflow.db`, so each
//! worker thread here opens its *own* connection to one file (a `Mutex`-shared
//! handle would serialize them and hide exactly the races under test).
//!
//! Invariants asserted are timing-independent (no sleeps racing the clock): the
//! last committed transaction fully wins (never a blend of two writers), every
//! committed session survives reopen, FTS stays structurally consistent
//! (`integrity_check` validates FTS5 shadow tables), and a reader only ever
//! observes a complete old-or-new version, never a partial one.

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::ConversationMessage;
use store::{Integrity, SessionMeta, Store};

/// A self-deleting temp dir whose *path* carries spaces + CJK, so the SQLite
/// file (and its `-wal`/`-shm` sidecars) live under a nasty Windows path.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("压力 {tag} 目录 {nanos}"));
        fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }
    fn db_path(&self) -> PathBuf {
        self.0.join("heart flow.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn meta(at: i64) -> SessionMeta {
    SessionMeta {
        created_at: at,
        updated_at: at,
        ..Default::default()
    }
}

fn user(s: &str) -> ConversationMessage {
    ConversationMessage::user_text(s.to_string())
}

/// A session body of `count` messages, every one carrying `tag` and the shared
/// `stress` token so both exact-version checks and FTS counting are possible.
fn body(tag: &str, count: usize) -> Vec<ConversationMessage> {
    (0..count)
        .map(|i| user(&format!("stress session {tag} message {i} 中文内容")))
        .collect()
}

/// N concurrent writers, each on its own connection, saving *distinct* sessions.
/// Every session must land intact and the file must stay structurally sound.
#[test]
fn concurrent_distinct_session_writers_all_persist() {
    const WRITERS: usize = 6;
    const PER_WRITER: usize = 15;
    let scratch = Scratch::new("distinct_writers");
    let path = scratch.db_path();
    // Seed the schema once so the worker threads' concurrent opens skip
    // migration (racing `CREATE TABLE` on a fresh file would surface SQLITE_BUSY).
    drop(Store::open(&path).expect("seed"));

    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let path = path.clone();
            thread::spawn(move || {
                // A dedicated connection per writer: this is what forces real
                // WAL/busy_timeout contention between "processes".
                let store = Store::open(&path).expect("worker open");
                for k in 0..PER_WRITER {
                    let id = format!("w{w}-s{k}");
                    store
                        .save_session(&id, &meta(1), &body(&id, 4))
                        .expect("worker save must not fail under contention");
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("writer thread panicked");
    }

    let store = Store::open(&path).expect("reopen");
    let ids = store.session_ids().expect("ids");
    assert_eq!(
        ids.len(),
        WRITERS * PER_WRITER,
        "every concurrent write persisted exactly once"
    );
    for w in 0..WRITERS {
        for k in 0..PER_WRITER {
            let id = format!("w{w}-s{k}");
            let loaded = store
                .load_session(&id)
                .expect("load")
                .unwrap_or_else(|| panic!("{id} missing after concurrent write"));
            assert_eq!(loaded.messages.len(), 4, "{id} has a torn message count");
        }
    }
    assert_eq!(store.integrity_check().expect("check"), Integrity::Ok);
}

/// Many writers racing on the *same* session id: transactions are isolated, so
/// the final state is exactly one writer's complete payload, never a blend.
#[test]
fn contended_same_session_writer_last_commit_fully_wins() {
    const WRITERS: usize = 5;
    let scratch = Scratch::new("same_session");
    let path = scratch.db_path();
    drop(Store::open(&path).expect("seed"));

    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let path = path.clone();
            thread::spawn(move || {
                let store = Store::open(&path).expect("open");
                // Each writer uses a distinct message count as its fingerprint.
                let fingerprint = 3 + w;
                for _ in 0..10 {
                    store
                        .save_session("hot", &meta(1), &body(&format!("t{w}"), fingerprint))
                        .expect("save");
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("writer panicked");
    }

    let store = Store::open(&path).expect("reopen");
    let messages = store
        .load_session("hot")
        .expect("load")
        .expect("present")
        .messages;
    // All surviving rows must come from a single transaction (one fingerprint),
    // which the message count and a uniform tag reveal.
    let tags: Vec<Option<char>> = messages
        .iter()
        .map(|m| {
            let text = match &m.blocks[0] {
                runtime::ContentBlock::Text { text } => text.as_str(),
                _ => "",
            };
            text.split("session t")
                .nth(1)
                .and_then(|s| s.chars().next())
        })
        .collect();
    let first = tags[0].expect("tag parsed");
    assert!(
        tags.iter().all(|t| *t == Some(first)),
        "final version is a blend of writers (transaction isolation broken): {tags:?}"
    );
    let count = messages.len();
    assert!(
        (3..3 + WRITERS).contains(&count),
        "unexpected row count {count}"
    );
    assert_eq!(store.integrity_check().expect("check"), Integrity::Ok);
}

/// A writer hammering a session while readers poll it: reads must never error
/// and must never observe a partial version (old-full xor new-full).
#[test]
fn wal_readers_never_observe_torn_versions_during_writes() {
    let scratch = Scratch::new("wal_readers");
    let path = scratch.db_path();
    {
        let seed = Store::open(&path).expect("seed open");
        seed.save_session("live", &meta(1), &body("v0", 5))
            .expect("seed");
    }

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let writer = {
        let path = path.clone();
        let stop = std::sync::Arc::clone(&stop);
        thread::spawn(move || {
            let store = Store::open(&path).expect("writer open");
            let mut n = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                // Alternate between 5 and 9 rows: a torn read is caught by a
                // reader seeing a count that is neither.
                let rows = if n.is_multiple_of(2) { 5 } else { 9 };
                store.save_session("live", &meta(1), &body("x", rows)).ok();
                n += 1;
            }
        })
    };

    let readers: Vec<_> = (0..3)
        .map(|_| {
            let path = path.clone();
            thread::spawn(move || {
                let store = Store::open_read_only(&path).expect("reader open");
                for _ in 0..2000 {
                    if let Some(session) = store
                        .load_session("live")
                        .expect("reader load never errors")
                    {
                        assert!(
                            session.messages.len() == 5 || session.messages.len() == 9,
                            "reader saw a torn write: {} rows",
                            session.messages.len()
                        );
                    }
                    store.search("stress session", None, 5).expect("search");
                }
            })
        })
        .collect();

    for reader in readers {
        reader.join().expect("reader panicked");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().expect("writer panicked");

    let store = Store::open(&path).expect("final open");
    assert_eq!(store.integrity_check().expect("check"), Integrity::Ok);
}

/// Bulk volume plus a checkpoint storm, then reopen: durability and FTS
/// consistency survive heavy churn.
#[test]
fn bulk_writes_and_repeated_checkpoints_stay_consistent() {
    const SESSIONS: usize = 120;
    let scratch = Scratch::new("bulk");
    let path = scratch.db_path();
    {
        let store = Store::open(&path).expect("open");
        for i in 0..SESSIONS {
            store
                .save_session(&format!("b{i}"), &meta(1), &body(&format!("b{i}"), 3))
                .expect("bulk save");
            // Interleave checkpoints to force WAL folds mid-stream.
            if i.is_multiple_of(10) {
                store.checkpoint().expect("checkpoint");
            }
        }
        store.checkpoint().expect("final checkpoint");
    }
    let reopened = Store::open(&path).expect("reopen");
    assert_eq!(reopened.session_ids().expect("ids").len(), SESSIONS);
    // FTS reflects the messages, not a drifting shadow table.
    let hits = reopened
        .search("stress session", None, 5_000)
        .expect("search");
    assert_eq!(
        hits.len(),
        SESSIONS * 3,
        "FTS row count must equal total messages after bulk + checkpoint storm"
    );
    assert_eq!(reopened.integrity_check().expect("check"), Integrity::Ok);
}
