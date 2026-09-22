//! Persistence subsystem for one conversation: session save/restore,
//! incremental credential redaction, the searchable `SQLite` history mirror,
//! the runtime credential registry, and conversation-id rotation. Extracted
//! verbatim from `main.rs` (Phase 1 thinning) with no behavior change; all
//! state is instance-scoped behind `SessionShared`, never a process singleton.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::{redact_messages, redact_session, Session};
use store::{SessionMeta, Store, StoreError};

pub(crate) fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn sessions_dir() -> PathBuf {
    home_dir().join(".heartflow").join("sessions")
}

/// System-level `SQLite` history database (searchable mirror of saved sessions).
pub(crate) fn store_path() -> PathBuf {
    home_dir().join(".heartflow").join("heartflow.db")
}

pub(crate) fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

pub(crate) fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default()
}

/// Restore a saved session: the snapshot file plus any append-only segment
/// beside it. A segment that cannot be placed on the snapshot never fails the
/// restore — the snapshot is complete on its own — so it is dropped with a note
/// on stderr. See `Session::load_with_segment`.
pub(crate) fn load_saved_session(path: &Path) -> Result<Session, String> {
    let (session, segment) = Session::load_with_segment(path).map_err(|error| error.to_string())?;
    if let Some(warning) = segment.warning() {
        eprintln!("{}: {warning}", path.display());
    }
    Ok(session)
}

/// Instance-scoped session state, replacing the former process-global statics.
/// One `hf` run owns exactly one of these; a future parallel section (Phase 6)
/// owns its own, so nothing here may assume a process singleton. Held behind
/// `Arc<Mutex<_>>` because the async save path (`spawn_blocking`) touches it
/// from a blocking thread while the REPL thread may register a secret or note a
/// mirror rewrite.
#[derive(Default)]
pub(crate) struct SessionState {
    /// Cached redacted transcript prefix so each save scrubs only new messages.
    pub(crate) save_cache: Option<SaveTracker>,
    /// Credential literals learned this run, replayed against every save.
    pub(crate) secrets: Vec<String>,
    /// Lazily opened history-store handle, reused across turns.
    pub(crate) store: Option<Store>,
    /// Whether the store open has been attempted (former `OnceLock` semantics).
    pub(crate) store_opened: bool,
    /// Incremental `SQLite` mirror bookkeeping.
    pub(crate) mirror: Option<MirrorTracker>,
    /// Stable on-disk identity of the conversation this state persists.
    pub(crate) session_id: Option<String>,
    /// Full text of tool outputs folded during rendering, indexed for `/expand`.
    pub(crate) expandable: Vec<String>,
}

/// Shared handle to one conversation's mutable state.
pub(crate) type SessionShared = Arc<Mutex<SessionState>>;

/// Lock the shared session state, recovering a poisoned guard instead of
/// surrendering the work.
///
/// Poison means an earlier holder panicked while the guard was live. Every field
/// of [`SessionState`] is an independent `Option`/`Vec`/`String` with no
/// cross-field invariant a half-finished write could corrupt, so a poisoned
/// value is still perfectly usable. Refusing to take it is never the safer
/// choice here: it silently drops a credential out of the redaction set, drops
/// search results, or routes a save at the wrong conversation. This is the same
/// `PoisonError::into_inner` idiom already used by `tools/todo.rs`,
/// `provider/cassette.rs` and `cli/theme.rs`.
///
/// The two sites that genuinely have a safer fallback keep their explicit `Err`
/// arm and say why: [`redact_for_save`] redoes the always-correct full scrub,
/// and the folded-output renderer prints the whole output rather than nothing.
pub(crate) fn lock_state(state: &SessionShared) -> std::sync::MutexGuard<'_, SessionState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A fresh, empty state handle for one conversation/section.
pub(crate) fn new_session_state() -> SessionShared {
    Arc::new(Mutex::new(SessionState::default()))
}

pub(crate) fn save_session(state: &SessionShared, session: &Session) -> io::Result<PathBuf> {
    let dir = sessions_dir();
    fs::create_dir_all(&dir)?;
    // Never persist a live credential: scrub the transcript that reaches disk
    // (both the authoritative JSON and the SQLite mirror) on a clone, so the
    // in-memory session that talks to the provider is left verbatim.
    let secrets = registered_secrets(state);
    let redacted = redact_for_save(state, session, &secrets);
    // One file per conversation, keyed by the stable id (see current_session_id):
    // each turn overwrites it via the atomic temp+rename in `save_to_path`, so a
    // crash keeps the last complete snapshot rather than a truncated tail.
    let path = dir.join(format!("{}.json", current_session_id(state)));
    redacted
        .save_to_path(&path)
        .map_err(|error| io::Error::other(error.to_string()))?;
    // Best-effort mirror into the searchable store; the JSON file stays
    // authoritative, so a store failure must never fail the save.
    mirror_to_store(state, &path, &redacted);
    Ok(path)
}

/// Async wrapper around [`save_session`]: the whole chain (regex scrub,
/// multi-MB serialize, file rename, `SQLite` mirror) is blocking work, so it
/// runs on the blocking pool instead of stalling a reactor worker mid-turn.
pub(crate) async fn save_session_async(
    state: &SessionShared,
    session: &Session,
) -> io::Result<PathBuf> {
    let session = session.clone();
    let state = state.clone();
    tokio::task::spawn_blocking(move || save_session(&state, &session))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

/// Per-process redaction cache: the redacted prefix of the current transcript,
/// so each save scrubs only the newly added messages instead of re-running the
/// four structural regexes over the whole (multi-MB) transcript every turn.
///
/// A single hf run is one conversation, so the tracker needs no id: it is
/// invalidated whenever the cached prefix stops being a trustworthy base —
/// a compaction shortened the transcript, or a pin toggle / task-context reset
/// mutated rows in place (`note_mirror_rewrite` forces both caches, whose
/// invalidation sources are identical). Credentials registered at runtime build
/// time always predate the messages that follow, so extending the prefix with
/// the current literal set stays sound.
pub(crate) struct SaveTracker {
    pub(crate) last_len: usize,
    pub(crate) force_full: bool,
    pub(crate) redacted: Session,
}

impl SaveTracker {
    /// Whether this save may extend the cached redacted prefix. Sound only when
    /// the transcript strictly grows past an already-scrubbed base and no
    /// in-place rewrite was forced.
    #[must_use]
    pub(crate) fn can_extend(&self, len: usize) -> bool {
        !self.force_full && self.last_len > 0 && len > self.last_len
    }
}

/// Scrub the transcript for persistence, extending the cached redacted prefix
/// when possible. The clone handed back is what reaches disk and the mirror;
/// the tracker keeps its own authoritative copy so a failed save simply leaves
/// the cache intact and the next save redoes the same extension.
pub(crate) fn redact_for_save(
    state: &SessionShared,
    session: &Session,
    secrets: &[String],
) -> Session {
    let len = session.messages.len();
    let Ok(mut guard) = state.lock() else {
        // Poisoned lock: the always-correct full scrub.
        return redact_session(session, secrets);
    };
    if let Some(tracker) = guard.save_cache.as_mut() {
        if tracker.can_extend(len) {
            // Scrub only the new tail: the cached prefix was already
            // scrubbed, and credentials registered at runtime build time
            // always predate the messages that follow.
            tracker.redacted.messages.extend(redact_messages(
                &session.messages[tracker.last_len..],
                secrets,
            ));
            tracker.last_len = len;
            return tracker.redacted.clone();
        }
    }
    let redacted = redact_session(session, secrets);
    guard.save_cache = Some(SaveTracker {
        last_len: len,
        force_full: false,
        redacted: redacted.clone(),
    });
    redacted
}

/// Credential literals learned this run (the resolved API key / auth token).
/// Registered when a runtime is built and replayed against every saved
/// transcript so a key that matches no structural redaction pattern is still
/// scrubbed before it reaches disk. De-duplicated; a redeployed provider just
/// re-registers its (identical) value.
pub(crate) fn register_secret(state: &SessionShared, value: &str) {
    let value = value.trim();
    // Mirrors redact_text's floor: a shorter string is ordinary text.
    if value.len() < 4 {
        return;
    }
    let mut guard = lock_state(state);
    if !guard.secrets.iter().any(|existing| existing == value) {
        guard.secrets.push(value.to_string());
    }
}

pub(crate) fn registered_secrets(state: &SessionShared) -> Vec<String> {
    lock_state(state).secrets.clone()
}

/// Mirror the transcript into the searchable history store, opening the shared
/// handle on first write. Reusing one connection across turns avoids re-running
/// the pragmas and migration on every save; WAL still lets other processes
/// read/search safely.
pub(crate) fn mirror_to_store(state: &SessionShared, json_path: &Path, session: &Session) {
    let now = unix_secs();
    let meta = SessionMeta {
        created_at: now,
        updated_at: now,
        source_path: Some(json_path.display().to_string()),
        provider: None,
        model: None,
    };
    let mut guard = lock_state(state);
    let SessionState {
        store,
        store_opened,
        mirror,
        session_id,
        ..
    } = &mut *guard;
    // Open the store handle once (former `OnceLock`), keeping it for the run.
    if !*store_opened {
        *store_opened = true;
        let path = store_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        *store = Store::open(&path).ok();
    }
    // Store unavailable (e.g. unwritable home): the JSON file is authoritative,
    // so a mirror failure is logged and never blocks the save.
    let Some(store) = store.as_ref() else {
        return;
    };
    // Mint the stable id if absent, mirroring `current_session_id` semantics;
    // the lock is already held, so it is inlined rather than re-locking.
    let id = if let Some(existing) = session_id {
        existing.clone()
    } else {
        let minted = new_session_id();
        *session_id = Some(minted.clone());
        minted
    };
    let tracker = mirror.get_or_insert_with(|| MirrorTracker {
        id,
        last_len: 0,
        force_full: true,
    });

    // Append is only sound when the transcript is a strict extension of the last
    // mirrored prefix; a shrink (compaction) or forced rewrite (pin/task reset)
    // falls through to the full rewrite, which is always correct.
    let len = session.messages.len();
    let can_append = tracker.can_append(len);
    let result = if can_append {
        store.append_messages(
            &tracker.id,
            &meta,
            tracker.last_len,
            &session.messages[tracker.last_len..],
            tracker.last_len,
        )
    } else {
        store.save_session(&tracker.id, &meta, &session.messages)
    };

    match result {
        Ok(()) => {
            tracker.last_len = len;
            tracker.force_full = false;
        }
        // The stored base moved under us (another process rewrote it, or the DB
        // was rebuilt): recover once with a full rewrite, which cannot drift.
        Err(StoreError::Drift { .. }) => {
            if store
                .save_session(&tracker.id, &meta, &session.messages)
                .is_ok()
            {
                tracker.last_len = len;
                tracker.force_full = false;
            } else {
                tracing::debug!("history store mirror fallback failed");
            }
        }
        Err(error) => tracing::debug!("history store mirror failed: {error}"),
    }
}

/// Per-process mirror bookkeeping so the `SQLite` history is written incrementally
/// instead of re-dumping the whole transcript every turn.
///
/// The JSON transcript stays authoritative and keeps its per-save file naming;
/// this governs only the *mirror*. A single stable `id` represents the current
/// conversation inside the history DB, so search returns one row per
/// conversation rather than a snapshot per turn. `last_len` is how many messages
/// are already mirrored under `id`; `force_full` marks that the stored prefix is
/// no longer a trustworthy base (an in-place pin toggle, a compaction, or a
/// task-context reset changed/shortened already-mirrored rows), so the next
/// write must be a full rewrite rather than a tail append.
pub(crate) struct MirrorTracker {
    pub(crate) id: String,
    pub(crate) last_len: usize,
    pub(crate) force_full: bool,
}

impl MirrorTracker {
    /// Whether this turn's mirror may be an incremental append onto the existing
    /// base. Sound only when the transcript strictly extends the last mirrored
    /// prefix: no rewrite was forced (pin/compaction/task-reset), at least one
    /// row is already stored (otherwise there is no base to extend), and the
    /// session did not shrink (a shorter transcript means earlier rows changed).
    /// When false, the caller performs a full `save_session` rewrite.
    #[must_use]
    pub(crate) fn can_append(&self, len: usize) -> bool {
        !self.force_full && self.last_len > 0 && len >= self.last_len
    }
}

/// Force the next mirror to fully rewrite. Called after any operation that
/// mutates already-mirrored rows without necessarily growing the transcript.
pub(crate) fn note_mirror_rewrite(state: &SessionShared) {
    let mut guard = lock_state(state);
    if let Some(tracker) = guard.mirror.as_mut() {
        tracker.force_full = true;
    }
    // The JSON snapshot's redaction cache shares every invalidation source
    // (pin toggle / compaction / task reset change rows in place), so it is
    // forced here rather than threading a second note call through each site.
    if let Some(tracker) = guard.save_cache.as_mut() {
        tracker.force_full = true;
    }
}

/// A fresh conversation id: start millis then PID, unique across concurrent
/// `hf` processes sharing the history DB and sortable by recency.
pub(crate) fn new_session_id() -> String {
    format!("{}-{}", unix_millis(), std::process::id())
}

/// The active conversation id, minted on first use.
pub(crate) fn current_session_id(state: &SessionShared) -> String {
    lock_state(state)
        .session_id
        .get_or_insert_with(new_session_id)
        .clone()
}

/// Rebind persistence to `id`: point future saves at that conversation and drop
/// the mirror tracker so the next write re-initializes (full, not append) under
/// the same id, keeping the JSON file and the `SQLite` row keyed identically.
pub(crate) fn rebind_conversation(state: &SessionShared, id: String) {
    let mut guard = lock_state(state);
    guard.session_id = Some(id);
    guard.mirror = None;
}

/// Start a brand-new conversation (after `/clear`): a fresh id for both the
/// transcript file and the history mirror.
pub(crate) fn rotate_session_id(state: &SessionShared) {
    rebind_conversation(state, new_session_id());
}

/// Adopt the conversation persisted at `path` (resume / `/open`): continuing it
/// overwrites the same file and updates the same history row instead of
/// branching into a new one.
pub(crate) fn adopt_session_path(state: &SessionShared, path: &Path) {
    let id = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(new_session_id);
    rebind_conversation(state, id);
}

pub(crate) fn list_sessions() -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(sessions_dir())
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    entries.sort_by_key(|path| {
        std::cmp::Reverse(
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    });
    entries
}
