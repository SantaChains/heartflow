//! I/O-correctness integration tests for the conversation store.
//!
//! Proves content survives round-trips *exactly* under the inputs that
//! historically break storage: Chinese, emoji, spaces, `\r\n`, tabs, wildcards,
//! quotes, a Windows path containing non-ASCII and spaces (the temp dir here
//! already lives under a profile path with a space), drive letters, durability
//! across reopen, and both search paths (FTS trigram vs. `LIKE` fallback).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::{ContentBlock, ConversationMessage, MessageRole, TokenUsage};
use store::{Integrity, SearchMethod, SessionMeta, Store, StoreError};

/// A temp dir that removes itself (and any `-wal`/`-shm` sidecars) on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        // Deliberately include Chinese and spaces so the *file path* exercises
        // non-ASCII + space handling on Windows, not just the stored content.
        let dir = std::env::temp_dir().join(format!("测试 {tag} dir {nanos}"));
        fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }
    fn db_path(&self) -> PathBuf {
        self.0.join("heart flow.db") // spaces in the file name too
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

fn usage(input: u32, output: u32) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    }
}

fn user(s: &str) -> ConversationMessage {
    ConversationMessage::user_text(s.to_string())
}

fn load(store: &Store, id: &str) -> Vec<ConversationMessage> {
    store
        .load_session(id)
        .expect("load")
        .expect("session present")
        .messages
}

#[test]
fn round_trips_every_nasty_character() {
    let store = Store::open_in_memory().expect("open");
    let nasty = "中文 \u{1f680} emoji line1\r\nline2\ttabbed \\ backslash \
                 100% percent _underscore_ \"quotes\" '<tag>' 
                zero-space 
 end";
    store
        .save_session("nasty", &meta(1), &[user(nasty)])
        .expect("save");
    let restored = load(&store, "nasty");
    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored[0].blocks[0],
        ContentBlock::Text {
            text: nasty.to_string()
        },
        "byte-exact round trip required"
    );
}

#[test]
fn persists_under_a_windows_path_with_chinese_and_spaces() {
    let scratch = Scratch::new("path");
    let path = scratch.db_path();
    // The path must contain a space and non-ASCII to be a meaningful test.
    assert!(path.display().to_string().contains(' '));
    {
        let store = Store::open(&path).expect("open at cjk+space path");
        store
            .save_session("s", &meta(5), &[user("C:\\Program Files\\应用 data")])
            .expect("save");
        store.checkpoint().expect("checkpoint");
        assert_eq!(store.integrity_check().expect("check ran"), Integrity::Ok);
    }
    assert!(path.exists(), "database file should have been created");
    let reopened = Store::open(&path).expect("reopen same path");
    let messages = load(&reopened, "s");
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].blocks[0],
        ContentBlock::Text {
            text: "C:\\Program Files\\应用 data".to_string()
        }
    );
}

#[test]
fn data_durably_survives_reopen() {
    let scratch = Scratch::new("durability");
    let path = scratch.db_path();
    let original = vec![
        user("first 消息"),
        ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "answer".to_string(),
        }]),
    ];
    {
        let store = Store::open(&path).expect("open");
        store
            .save_session("dup", &meta(10), &original)
            .expect("save");
    } // store dropped, WAL checkpointed on close
    let store = Store::open(&path).expect("reopen");
    assert_eq!(load(&store, "dup"), original);
    assert_eq!(store.schema_version().expect("version"), 3);
}

#[test]
fn migration_is_idempotent_across_reopens() {
    let scratch = Scratch::new("migration");
    let path = scratch.db_path();
    for _ in 0..3 {
        let store = Store::open(&path).expect("open");
        store
            .save_session("m", &meta(1), &[user("x")])
            .expect("save");
        assert_eq!(store.schema_version().expect("v"), 3);
    }
    let store = Store::open(&path).expect("final open");
    assert_eq!(load(&store, "m").len(), 1, "re-save keeps one copy");
}

#[test]
fn migrates_v1_database_and_preserves_pinned_flag() {
    // Build a genuine pre-pin (version 1) database by hand: the old schema had
    // no `pinned` column. Then open it through `Store` so the migration runs,
    // and confirm pins round-trip and old rows read back as unpinned.
    let scratch = Scratch::new("migrate_v1");
    let path = scratch.db_path();
    {
        let conn = rusqlite::Connection::open(&path).expect("raw open");
        conn.execute_batch(
            "PRAGMA user_version=1;
             CREATE TABLE sessions (
                 id INTEGER PRIMARY KEY,
                 session_id TEXT NOT NULL UNIQUE,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 source_path TEXT,
                 provider TEXT,
                 model TEXT
             );
             CREATE TABLE messages (
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
                 UNIQUE(session_row, seq)
             );
             CREATE INDEX idx_messages_session ON messages(session_row);
             CREATE VIRTUAL TABLE messages_fts USING fts5(search_text, tokenize='trigram');
             INSERT INTO sessions (session_id, created_at, updated_at) VALUES ('old', 1, 1);
             INSERT INTO messages (session_row, seq, role, blocks_json, search_text) \
                 VALUES (1, 0, 'user', '[{\"type\":\"text\",\"text\":\"legacy\"}]', 'legacy');",
        )
        .expect("create v1 schema");
    }
    let store = Store::open(&path).expect("open triggers migration");
    assert_eq!(store.schema_version().expect("migrated"), 3);
    // v3 drops `idx_messages_session`: `UNIQUE(session_row, seq)` already backs
    // every query filtering on `session_row`, so the v1 database's copy must be
    // gone after the upgrade rather than left behind for upgraded files.
    {
        let probe = rusqlite::Connection::open(&path).expect("probe open");
        let leftover: i64 = probe
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_messages_session';",
                [],
                |row| row.get(0),
            )
            .expect("probe query");
        assert_eq!(leftover, 0, "migration must drop the redundant index");
    }
    let legacy = load(&store, "old");
    assert_eq!(legacy.len(), 1, "pre-existing row survives ALTER");
    assert!(!legacy[0].pinned, "old rows default to unpinned");
    // Newly saved pins persist across the migrated schema.
    store
        .save_session(
            "new",
            &meta(2),
            &[user("keep me").with_pinned(true), user("normal")],
        )
        .expect("save with pin");
    let fresh = load(&store, "new");
    assert!(fresh[0].pinned, "pin must round-trip");
    assert!(!fresh[1].pinned, "unpinned stays unpinned");
}

#[test]
fn fts_trigram_finds_cjk_and_ascii_terms() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session(
            "docs",
            &meta(1),
            &[
                user("请检索我的中文笔记内容"),
                user("please index this configuration thoroughly"),
            ],
        )
        .expect("save");

    // >= 3 code points => FTS path.
    let hits = store.search("中文笔", None, 10).expect("search cjk");
    assert!(
        hits.iter().any(|h| h.method == SearchMethod::Fts),
        "3-char CJK should use trigram FTS: {hits:?}"
    );
    let ascii = store
        .search("configuration", None, 10)
        .expect("search ascii");
    assert_eq!(ascii.len(), 1, "unique ascii phrase");
    assert_eq!(ascii[0].seq, 1);
}

#[test]
fn like_fallback_matches_short_two_char_cjk() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session("d", &meta(1), &[user("请检索我的中文笔记内容")])
        .expect("save");
    // 2 code points => trigram cannot match, must fall back to LIKE.
    let hits = store.search("笔记", None, 10).expect("search short");
    assert_eq!(hits.len(), 1, "short CJK found via LIKE");
    assert_eq!(hits[0].method, SearchMethod::Like);
}

#[test]
fn like_escaping_treats_wildcards_literally() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session(
            "w",
            &meta(1),
            &[user("literal x_y here"), user("axay no underscore")],
        )
        .expect("save");
    // 2-char query "x_" => LIKE path; '_' must be literal, not a wildcard.
    let hits = store.search("x_", None, 10).expect("search");
    assert_eq!(hits.len(), 1, "only the real underscore matches: {hits:?}");
    assert!(hits[0].snippet.contains("x_y"));
}

#[test]
fn search_never_errors_on_operator_like_input() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session(
            "q",
            &meta(1),
            &[user("quoting \"hello\" and stars * and cols:")],
        )
        .expect("save");
    for q in ["\"hello\"", "*", "a:b", "(x)", "^y", "-z", "\""] {
        // Must not return Err regardless of FTS syntax characters.
        let _ = store.search(q, None, 10).unwrap_or_else(|e| {
            panic!("query {q:?} must not error: {e}");
        });
    }
}

#[test]
fn search_can_scope_to_one_session() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session("a", &meta(1), &[user("shared needle here")])
        .expect("save");
    store
        .save_session("b", &meta(1), &[user("shared needle here")])
        .expect("save");
    let all = store.search("shared needle", None, 10).expect("all");
    assert_eq!(all.len(), 2);
    let scoped = store
        .search("shared needle", Some("a"), 10)
        .expect("scoped");
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].session_id, "a");
}

#[test]
fn empty_query_returns_no_hits() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session("e", &meta(1), &[user("anything")])
        .expect("save");
    assert_eq!(store.search("   ", None, 10).expect("empty").len(), 0);
}

#[test]
fn tool_inputs_and_outputs_are_searchable() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session(
            "t",
            &meta(1),
            &[
                ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                    id: "1".to_string(),
                    name: "grep".to_string(),
                    input: "needle_in_tool_input".to_string(),
                }]),
                ConversationMessage::tool_result("1", "grep", "needle_in_tool_output", false),
            ],
        )
        .expect("save");
    assert!(store
        .search("needle_in_tool_input", None, 10)
        .expect("in")
        .iter()
        .any(|h| h.role == MessageRole::Assistant));
    assert!(store
        .search("needle_in_tool_output", None, 10)
        .expect("out")
        .iter()
        .any(|h| h.role == MessageRole::Tool));
}

#[test]
fn list_orders_by_recency_with_count_and_preview() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session("old", &meta(100), &[user("earlier note")])
        .expect("save old");
    store
        .save_session("new", &meta(200), &[user("later note"), user("second")])
        .expect("save new");
    let list = store.list_sessions(10).expect("list");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].session_id, "new", "newest activity first");
    assert_eq!(list[0].message_count, 2);
    assert_eq!(list[0].preview, "later note");
}

#[test]
fn usage_totals_aggregate_globally_and_scoped() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session(
            "u",
            &meta(1),
            &[
                user("no usage"),
                ConversationMessage::assistant_with_usage(
                    vec![ContentBlock::Text {
                        text: "a".to_string(),
                    }],
                    Some(usage(10, 3)),
                ),
                ConversationMessage::assistant_with_usage(
                    vec![ContentBlock::Text {
                        text: "b".to_string(),
                    }],
                    Some(usage(5, 2)),
                ),
            ],
        )
        .expect("save");
    let all = store.usage_totals(None).expect("totals");
    assert_eq!(all.messages_with_usage, 2);
    assert_eq!(all.input_tokens, 15);
    assert_eq!(all.output_tokens, 5);
    let scoped = store.usage_totals(Some("u")).expect("scoped");
    assert_eq!(scoped.input_tokens, 15);
    let none = store.usage_totals(Some("missing")).expect("missing");
    assert_eq!(none.messages_with_usage, 0);
}

#[test]
fn saving_again_replaces_messages_and_resyncs_fts() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session("r", &meta(1), &[user("obsolete phrase zzz")])
        .expect("first");
    store
        .save_session("r", &meta(1), &[user("fresh phrase yyy")])
        .expect("second");
    assert_eq!(load(&store, "r").len(), 1, "old messages replaced");
    assert!(
        store
            .search("obsolete phrase", None, 10)
            .expect("s1")
            .is_empty(),
        "stale FTS entry must be gone"
    );
    assert_eq!(store.search("fresh phrase", None, 10).expect("s2").len(), 1);
}

#[test]
fn delete_session_removes_messages_and_fts() {
    let store = Store::open_in_memory().expect("open");
    store
        .save_session("del", &meta(1), &[user("temporary content xyzzy")])
        .expect("save");
    assert!(store.delete_session("del").expect("delete"));
    assert!(
        !store.delete_session("del").expect("second delete"),
        "idempotent"
    );
    assert!(store.load_session("del").expect("load").is_none());
    assert!(
        store
            .search("temporary content", None, 10)
            .expect("s")
            .is_empty(),
        "cascade + fts cleanup"
    );
    assert!(!store
        .session_ids()
        .expect("ids")
        .contains(&"del".to_string()));
}

#[test]
fn unicode_session_id_round_trips() {
    let store = Store::open_in_memory().expect("open");
    let id = "会话 id with 空格";
    store
        .save_session(id, &meta(1), &[user("body")])
        .expect("save");
    assert_eq!(load(&store, id).len(), 1);
    assert_eq!(
        store.search("body", Some(id), 10).expect("s")[0].session_id,
        id
    );
}

#[test]
fn large_message_round_trips() {
    let store = Store::open_in_memory().expect("open");
    let big = "汉a".repeat(50_000);
    store
        .save_session("big", &meta(1), &[user(&big)])
        .expect("save big");
    let restored = load(&store, "big");
    assert_eq!(restored[0].blocks[0], ContentBlock::Text { text: big });
}

#[test]
fn open_creates_parent_dir_missing_errors_cleanly() {
    // Opening a DB whose parent does not exist must surface an error, not panic.
    let missing = Path::new(std::env::temp_dir().as_os_str())
        .join("no such dir 中文")
        .join("x.db");
    let result = Store::open(&missing);
    assert!(result.is_err(), "opening under a missing dir must error");
}

#[test]
fn integrity_and_quick_check_pass_on_a_written_file_db() {
    // Exercises the real PRAGMA against an on-disk file with FTS5 shadow tables,
    // across save -> reopen, the path `hf doctor` relies on being non-false-positive.
    let scratch = Scratch::new("integrity");
    let path = scratch.db_path();
    {
        let store = Store::open(&path).expect("open");
        for i in 0..5 {
            store
                .save_session(
                    &format!("s{i}"),
                    &meta(i),
                    &[user(&format!("会话 {i} notes with spaces and CJK"))],
                )
                .expect("save");
        }
        assert_eq!(store.integrity_check().expect("full ran"), Integrity::Ok);
    }
    let reopened = Store::open(&path).expect("reopen");
    assert_eq!(reopened.quick_check().expect("quick ran"), Integrity::Ok);
    assert_eq!(
        reopened.integrity_check().expect("full ran"),
        Integrity::Ok,
        "a durably written db must pass a full scan after reopen"
    );
}

#[test]
fn integrity_check_flags_a_truncated_database() {
    // A risk test that must actually bite: a healthy file that loses pages from
    // the end no longer matches its header page count, so `integrity_check` (via
    // a read-only handle) must NOT report a clean database. Deterministic: we
    // checkpoint, then halve the file so every trailing page is gone.
    let scratch = Scratch::new("truncated");
    let path = scratch.db_path();
    {
        let store = Store::open(&path).expect("open");
        for i in 0..8 {
            store
                .save_session(
                    &format!("s{i}"),
                    &meta(i),
                    &[user(&format!("content for session {i} 中文"))],
                )
                .expect("save");
        }
        store.checkpoint().expect("checkpoint");
    }
    let full = fs::metadata(&path).expect("metadata").len();
    assert!(full > 4096, "need multiple pages to truncate meaningfully");
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open for truncate")
        .set_len(full / 2)
        .expect("truncate");
    let outcome = Store::open_read_only(&path).map(|store| store.integrity_check());
    assert!(
        !matches!(outcome, Ok(Ok(Integrity::Ok))),
        "a truncated database must never pass as sound (got {outcome:?})"
    );
}

#[test]
fn read_only_handle_cannot_mutate() {
    // Proves the safety property `hf search`/`hf doctor` rely on: a reader can
    // query the database but is structurally incapable of writing to it.
    let scratch = Scratch::new("readonly");
    let path = scratch.db_path();
    {
        let store = Store::open(&path).expect("open rw");
        store
            .save_session("x", &meta(1), &[user("existing row")])
            .expect("save");
    }
    let reader = Store::open_read_only(&path).expect("open ro");
    assert_eq!(
        reader
            .load_session("x")
            .expect("load")
            .map(|s| s.messages.len()),
        Some(1),
        "reads work on a read-only handle"
    );
    assert!(
        reader
            .save_session("y", &meta(2), &[user("should fail")])
            .is_err(),
        "writes must be rejected on a read-only handle"
    );
    assert!(
        reader.load_session("y").expect("recheck").is_none(),
        "the rejected write left no trace"
    );
    assert_eq!(reader.integrity_check().expect("check"), Integrity::Ok);
}

#[test]
fn wal_lets_two_open_handles_share_one_file() {
    // Cross-handle visibility is the real risk of a shared system database:
    // two read-write connections must both persist and see each other's commits.
    let scratch = Scratch::new("concurrency");
    let path = scratch.db_path();
    let a = Store::open(&path).expect("open a");
    let b = Store::open(&path).expect("open b");
    a.save_session("a", &meta(1), &[user("alpha unique phrase")])
        .expect("a save");
    b.save_session("b", &meta(2), &[user("beta unique phrase")])
        .expect("b save");
    let ids = a.session_ids().expect("ids");
    assert!(
        ids.contains(&"a".to_string()) && ids.contains(&"b".to_string()),
        "both sessions visible across handles: {ids:?}"
    );
    assert_eq!(
        a.search("beta unique", None, 10).expect("search").len(),
        1,
        "b's committed write is readable from a"
    );
    assert_eq!(a.integrity_check().expect("check"), Integrity::Ok);
}

/// A list of `n` distinguishable messages for append-vs-full comparisons.
fn msgs(prefix: &str, n: usize) -> Vec<ConversationMessage> {
    (0..n)
        .map(|i| user(&format!("{prefix} message {i} 中文内容")))
        .collect()
}

#[test]
fn incremental_append_equals_full_replace() {
    let all = msgs("grow", 10);
    // Control: one full save of every message.
    let control = Store::open_in_memory().expect("control");
    control
        .save_session("s", &meta(1), &all)
        .expect("control save");

    // Under test: save a base, then append the remaining turns incrementally.
    let store = Store::open_in_memory().expect("store");
    let split = 4;
    store
        .save_session("s", &meta(1), &all[..split])
        .expect("base save");
    store
        .append_messages("s", &meta(2), split, &all[split..], split)
        .expect("append tail");

    // The reconstructed transcript (order + content) must equal both the
    // source and a byte-for-byte full replace — proving seq continues correctly
    // and the shared row-writer emits identical bytes.
    assert_eq!(load(&store, "s"), all, "append reconstructs exactly");
    assert_eq!(load(&store, "s"), load(&control, "s"));
    assert_eq!(
        store.integrity_check().expect("check"),
        Integrity::Ok,
        "FTS shadow stays consistent after append"
    );
    assert!(
        !store
            .search("grow message 9", None, 10)
            .expect("search appended")
            .is_empty(),
        "appended tail must be searchable"
    );
}

#[test]
fn append_rejects_base_count_drift_without_writing() {
    let store = Store::open_in_memory().expect("store");
    store
        .save_session("s", &meta(1), &msgs("keep", 3))
        .expect("base");
    // Caller believes 7 rows exist; only 3 do (e.g. a compaction shortened it).
    let err = store
        .append_messages("s", &meta(2), 7, &msgs("new", 2), 7)
        .expect_err("must reject on drift");
    assert!(
        matches!(
            err,
            StoreError::Drift {
                expected: 7,
                found: 3
            }
        ),
        "unexpected error: {err}"
    );
    assert_eq!(load(&store, "s").len(), 3, "original rows untouched");
}

#[test]
fn append_rejects_inconsistent_first_seq_before_touching_db() {
    let store = Store::open_in_memory().expect("store");
    store
        .save_session("s", &meta(1), &msgs("keep", 3))
        .expect("base");
    // first_seq (5) disagrees with expected_existing (3): a caller bug, rejected
    // without a connection round-trip.
    let err = store
        .append_messages("s", &meta(2), 5, &msgs("new", 1), 3)
        .expect_err("mismatched seq/base rejected");
    assert!(matches!(err, StoreError::Drift { .. }));
    assert_eq!(load(&store, "s").len(), 3);
}

#[test]
fn append_rolls_back_header_when_base_missing() {
    // A fresh DB has zero rows for the id; an append expecting a prior base must
    // fail and leave no orphan session header (the transaction rolls back).
    let store = Store::open_in_memory().expect("store");
    let err = store
        .append_messages("ghost", &meta(1), 5, &msgs("new", 1), 5)
        .expect_err("missing base rejected");
    assert!(matches!(
        err,
        StoreError::Drift {
            expected: 5,
            found: 0
        }
    ));
    assert!(
        store.load_session("ghost").expect("load").is_none(),
        "rejected append leaves no orphan header row"
    );
}

#[test]
fn append_empty_tail_refreshes_header_only() {
    let store = Store::open_in_memory().expect("store");
    let base = msgs("same", 3);
    store.save_session("s", &meta(1), &base).expect("base");
    store
        .append_messages("s", &meta(9), 3, &[], 3)
        .expect("empty append ok");
    assert_eq!(load(&store, "s"), base, "no rows added");
}
