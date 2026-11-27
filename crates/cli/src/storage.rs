//! Persistence subsystem for one conversation: session save/restore,
//! incremental credential redaction, the searchable `SQLite` history mirror,
//! the runtime credential registry, and conversation-id rotation. Extracted
//! verbatim from `main.rs` (Phase 1 thinning) with no behavior change; all
//! state is instance-scoped behind `SessionShared`, never a process singleton.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::{
    redact_messages, redact_session, CompactionAudit, ConversationMessage, ConversationRuntime,
    SegmentWriter, Session, SessionJournal,
};
use store::{SessionMeta, Store, StoreError};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

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

/// Single-line topic snippet of a saved session: the first user text message,
/// whitespace-collapsed and capped. Backs `/sessions` rows (a bare `<id>.json`
/// path is unreadable as a list entry) and `/sessions FILTER` + `/open TOPIC`
/// matching. Read/parse failures degrade to `None` — a listing row just shows
/// the path; a broken segment never fails the whole listing.
pub(crate) fn session_topic(path: &Path) -> Option<String> {
    let (session, _) = Session::load_with_segment(path).ok()?;
    let text = session
        .messages
        .iter()
        .find(|message| message.role == runtime::MessageRole::User)
        .and_then(|message| {
            message.blocks.iter().find_map(|block| match block {
                runtime::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
        })?;
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!one_line.is_empty()).then(|| runtime::truncate_chars(&one_line, 96))
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
    /// Where compaction archives go; `None` = the default under
    /// [`sessions_dir`]. Set when a snapshot outside that directory is
    /// resumed, so its archive travels with the snapshot.
    pub(crate) archive_root: Option<PathBuf>,
    /// Full text of tool outputs folded during rendering, indexed for `/expand`.
    pub(crate) expandable: Vec<String>,
    /// 联网三态 (roadmap C-1): shared with the tool executor so both the REPL
    /// command and the TUI keybind flip it live — no runtime rebuild.
    pub(crate) web_access: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Session permission mode ("workspace-write" default); mirrored here so
    /// `run_turn_interactive` can inject the mode's prompt segment without
    /// threading the string through every call site.
    pub(crate) mode: String,
    /// Display price for the active model (USD per 1M tokens, in/out) from the
    /// catalog, mirrored here so the per-turn usage line can show cost without
    /// threading the catalog through every call site (same pattern as `mode`).
    pub(crate) price: Option<(f64, f64)>,
    /// Append-only segment writer for this conversation. Created lazily by
    /// [`ensure_journal`] when a runtime is built; shared by every runtime
    /// rebuild so there is exactly one writer (and one open file) per session.
    pub(crate) journal: Option<Arc<DiskJournal>>,
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

/// Mirror the active model's display price (USD per 1M tokens, in/out) into the
/// shared state so the per-turn usage line can render cost. Called at REPL
/// startup and on every `/model`//`/provider` switch.
pub(crate) fn set_display_price(state: &SessionShared, price: Option<(f64, f64)>) {
    lock_state(state).price = price;
}

/// The mirrored display price, if the active model has one in the catalog.
pub(crate) fn display_price(state: &SessionShared) -> Option<(f64, f64)> {
    lock_state(state).price
}

pub(crate) fn save_session(state: &SessionShared, session: &Session) -> io::Result<PathBuf> {
    let dir = sessions_dir();
    fs::create_dir_all(&dir)?;
    // Owner-only traversal (security.dev.md D9): covers the snapshots plus
    // missions/schedules/queue siblings under the same root. Best effort.
    runtime::harden_dir(&dir);
    // Never persist a live credential: scrub the transcript that reaches disk
    // (both the authoritative JSON and the SQLite mirror) on a clone, so the
    // in-memory session that talks to the provider is left verbatim.
    let secrets = registered_secrets(state);
    let mut redacted = redact_for_save(state, session, &secrets);
    // The cached redaction prefix may predate this turn's runtime; the timing
    // figure is metadata, not message content, so copy it through unchanged.
    redacted.runtime_ms = session.runtime_ms;
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
///
/// This is the turn-boundary persistence point, so it owns the segment order:
/// 1. flush the journal (nothing may still be queued when the snapshot moves),
/// 2. atomically replace the snapshot,
/// 3. reset the segment header to the saved length — only after the rename
///    succeeded, so a crash in between leaves a stale (rejected) segment rather
///    than a segment describing a snapshot that never landed.
pub(crate) async fn save_session_async(
    state: &SessionShared,
    session: &Session,
) -> io::Result<PathBuf> {
    let session = session.clone();
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        // Clone the handle out before flushing: the writer thread locks the
        // same state to redact, and holding the guard here would deadlock.
        let journal = lock_state(&state).journal.clone();
        if let Some(journal) = &journal {
            // Recovery is best-effort: a segment failure must never block the
            // authoritative snapshot save.
            if let Err(error) = journal.flush() {
                warn_journal(&error);
            }
        }
        let path = save_session(&state, &session)?;
        if let Some(journal) = &journal {
            if let Err(error) = journal.rotate(session.messages.len()) {
                warn_journal(&error);
            }
        }
        Ok(path)
    })
    .await
    .map_err(|error| io::Error::other(error.to_string()))?
}

/// Turn-boundary persistence, injected into the TUI loop (`tui::drive_loop`)
/// as a port so the loop itself names no disk path: production binds
/// [`DiskSaver`], tests bind a recorder and never touch the real home
/// directory. One call per finished turn, success or failure.
pub(crate) trait TurnSaver {
    async fn save(&mut self, state: &SessionShared, session: &Session) -> io::Result<PathBuf>;
}

// ---------------------------------------------------------------------------
// Compaction archive. The snapshot is a per-turn cache that each save
// overwrites, so a compaction's dropped region would otherwise exist only in
// memory. Every compaction writes its dropped messages to
// `sessions/archive/<id>/NNNN.json` before the snapshot is saved; the archive
// is append-only and nothing here ever deletes or rewrites a record.
// ---------------------------------------------------------------------------

/// Root of the append-only compaction archive (normal sessions live directly
/// under the sessions directory).
pub(crate) fn archive_dir() -> PathBuf {
    sessions_dir().join("archive")
}

/// Archive root for the active conversation. A resumed snapshot may live
/// outside the sessions directory (`hf --resume <path>`), and its archive
/// belongs beside it, not under the default home tree.
fn archive_root(state: &SessionShared) -> PathBuf {
    lock_state(state)
        .archive_root
        .clone()
        .unwrap_or_else(archive_dir)
}

/// Warn only once per process: a failing disk must not print per compaction.
static ARCHIVE_WARNED: AtomicBool = AtomicBool::new(false);

/// Drain the runtime's dropped regions into the archive. Callers invoke this
/// *before* saving the snapshot: afterwards the compacted transcript is the
/// only live copy. Failures warn once and never fail the turn — losing the
/// archive is bad, losing the turn is worse.
pub(crate) fn archive_pending<C, T>(state: &SessionShared, runtime: &mut ConversationRuntime<C, T>)
where
    C: runtime::ApiClient,
    T: runtime::ToolExecutor,
{
    archive_audits(state, runtime.take_compaction_audit());
}

/// Same contract as [`archive_pending`] for callers that hold the audits
/// directly (the `--run /compact` path, which never builds a runtime).
pub(crate) fn archive_audits(state: &SessionShared, audits: Vec<CompactionAudit>) {
    for audit in audits {
        if let Err(error) = archive_compaction(state, &audit) {
            if !ARCHIVE_WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("warning: could not archive compacted messages: {error}");
                tracing::warn!(
                    target: "heartflow::storage",
                    %error,
                    "could not archive compacted messages; the snapshot is still saved"
                );
            }
        }
    }
}

/// Archive one dropped region; `None` when there was nothing to drop.
fn archive_compaction(
    state: &SessionShared,
    audit: &CompactionAudit,
) -> io::Result<Option<PathBuf>> {
    if audit.messages.is_empty() {
        return Ok(None);
    }
    // Same scrub rule as the snapshot (security.dev.md D9): the archive must
    // never persist a live credential either.
    let messages = redact_messages(&audit.messages, &registered_secrets(state));
    let session_id = current_session_id(state);
    let dir = archive_root(state).join(&session_id);
    fs::create_dir_all(&dir)?;
    runtime::harden_dir(&dir);
    let epoch = next_epoch(&dir)?;
    let ts = u64::try_from(unix_millis()).unwrap_or(u64::MAX);
    let removed = messages.len();
    let record = serde_json::json!({
        "version": 1,
        "ts": ts,
        "session": session_id,
        "epoch": epoch,
        "reason": &audit.reason,
        "removed": removed,
        "messages": messages,
    });
    let path = dir.join(format!("{epoch:04}.json"));
    let bytes = serde_json::to_vec_pretty(&record).map_err(io::Error::other)?;
    runtime::write_atomic(&path, &bytes)?;
    append_manifest(&dir, &path, epoch, ts, removed, &audit.reason)?;
    Ok(Some(path))
}

/// Epoch = one past the highest epoch already on disk, counting both manifest
/// lines and record file names. Reading file names too keeps append-only
/// intact on the failure path: a record written before a failed manifest
/// append is never overwritten by the next compaction.
fn next_epoch(dir: &Path) -> io::Result<u64> {
    let manifest = dir.join("manifest.jsonl");
    let lines = match fs::read_to_string(&manifest) {
        Ok(text) => text.lines().filter(|line| !line.trim().is_empty()).count(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    let highest_file = match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "json") {
                    return None;
                }
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    let seen = highest_file.max(u64::try_from(lines).unwrap_or(u64::MAX));
    Ok(seen.saturating_add(1))
}

/// Append one single-line manifest entry (`O_APPEND` + flush). The manifest is
/// the index a reader replays; the record files stay self-describing.
fn append_manifest(
    dir: &Path,
    path: &Path,
    epoch: u64,
    ts: u64,
    removed: usize,
    reason: &str,
) -> io::Result<()> {
    use std::io::Write;
    let line = serde_json::json!({
        "version": 1,
        "ts": ts,
        "epoch": epoch,
        "file": path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
        "removed": removed,
        "bytes": fs::metadata(path).map_or(0, |meta| meta.len()),
        "reason": reason,
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("manifest.jsonl"))?;
    writeln!(file, "{line}")?;
    file.flush()
}

/// Archive totals for `hf doctor`. Orphans are the two broken pairings a
/// failed archive append can leave: a record file with no manifest line, and
/// a manifest line whose record file is missing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArchiveStats {
    pub(crate) sessions: usize,
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    pub(crate) latest_ts: Option<u64>,
    pub(crate) orphans: usize,
}

pub(crate) fn archive_stats() -> ArchiveStats {
    let mut stats = ArchiveStats::default();
    let Ok(sessions) = fs::read_dir(archive_dir()) else {
        return stats;
    };
    for session in sessions.flatten() {
        let dir = session.path();
        if !dir.is_dir() {
            continue;
        }
        stats.sessions += 1;
        let mut referenced: HashSet<String> = HashSet::new();
        if let Ok(text) = fs::read_to_string(dir.join("manifest.jsonl")) {
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    stats.orphans += 1;
                    continue;
                };
                if let Some(file) = value.get("file").and_then(|v| v.as_str()) {
                    referenced.insert(file.to_string());
                }
                if let Some(ts) = value.get("ts").and_then(serde_json::Value::as_u64) {
                    stats.latest_ts = Some(stats.latest_ts.map_or(ts, |seen| seen.max(ts)));
                }
            }
        }
        let mut present: HashSet<String> = HashSet::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "json") {
                    continue;
                }
                let Some(name) = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                else {
                    continue;
                };
                stats.files += 1;
                stats.bytes += fs::metadata(&path).map_or(0, |meta| meta.len());
                present.insert(name);
            }
        }
        stats.orphans += referenced.symmetric_difference(&present).count();
    }
    stats
}

/// The production [`TurnSaver`]: the real on-disk snapshot save, offloaded to
/// the blocking pool by [`save_session_async`].
pub(crate) struct DiskSaver;

impl TurnSaver for DiskSaver {
    async fn save(&mut self, state: &SessionShared, session: &Session) -> io::Result<PathBuf> {
        save_session_async(state, session).await
    }
}

// ---------------------------------------------------------------------------
// Active segment journal. Every message a turn appends is written to
// `<id>.jsonl` beside the snapshot before it enters memory, so a crash mid-turn
// recovers the completed work. The snapshot stays authoritative: the segment
// only carries messages appended after the last save and is reset at every
// turn boundary.
// ---------------------------------------------------------------------------

/// Segment path for a conversation: same directory and stem as its snapshot.
fn segment_path(id: &str) -> PathBuf {
    sessions_dir().join(format!("{id}.jsonl"))
}

enum JournalCmd {
    Record {
        ts: u64,
        message: ConversationMessage,
    },
    Flush(oneshot::Sender<io::Result<()>>),
    Rotate {
        base: usize,
        ack: oneshot::Sender<io::Result<()>>,
    },
}

/// Non-blocking journal handle. [`SessionJournal::record`] only enqueues;
/// one dedicated thread owns the open segment file, redacts new messages with
/// the same rules as the snapshot, and services flush/rotate at turn
/// boundaries. `record` must never block a turn, which is why no locking,
/// redaction or I/O happens on the caller's thread.
pub(crate) struct DiskJournal {
    tx: tokio_mpsc::UnboundedSender<JournalCmd>,
    /// Snapshot length the segment extends. Updated on rotate and on every
    /// runtime rebuild; read only when (re)opening the file.
    base: Arc<AtomicUsize>,
}

impl DiskJournal {
    fn start(state: &SessionShared, base: usize) -> Self {
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        let shared_base = Arc::new(AtomicUsize::new(base));
        let _ = std::thread::Builder::new()
            .name(String::from("hf-session-journal"))
            .spawn({
                let state = Arc::clone(state);
                let shared_base = Arc::clone(&shared_base);
                move || journal_writer_loop(state, rx, shared_base)
            });
        Self {
            tx,
            base: shared_base,
        }
    }

    /// Point the segment at a new snapshot length. Called when a runtime is
    /// built (the session may have grown since the last rotate) — only the
    /// next open reads it, so a live writer is never disturbed.
    fn rebase(&self, base: usize) {
        self.base.store(base, Ordering::SeqCst);
    }

    /// Reset the segment for the snapshot just saved and drop the writer's open
    /// handle. Blocking; call only from the blocking persistence path.
    fn rotate(&self, base: usize) -> io::Result<()> {
        let (ack, done) = oneshot::channel();
        self.tx
            .send(JournalCmd::Rotate { base, ack })
            .map_err(|_| io::Error::other("session journal writer stopped"))?;
        done.blocking_recv()
            .map_err(|_| io::Error::other("session journal writer stopped"))?
    }
}

impl SessionJournal for DiskJournal {
    fn record(&self, ts_ms: u64, message: &ConversationMessage) {
        // Unbounded: a send that fails means the writer thread is gone, and a
        // lost recovery record must never fail the turn that produced it.
        let _ = self.tx.send(JournalCmd::Record {
            ts: ts_ms,
            message: message.clone(),
        });
    }

    fn flush(&self) -> io::Result<()> {
        let (ack, done) = oneshot::channel();
        self.tx
            .send(JournalCmd::Flush(ack))
            .map_err(|_| io::Error::other("session journal writer stopped"))?;
        done.blocking_recv()
            .map_err(|_| io::Error::other("session journal writer stopped"))?
    }
}

/// Warn once per process: a failing disk must not print per message.
static JOURNAL_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_journal(error: &io::Error) {
    if !JOURNAL_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("warning: could not write the session segment: {error}");
        tracing::warn!(
            target: "heartflow::storage",
            %error,
            "session segment write failed; the snapshot at the turn boundary is still authoritative"
        );
    }
}

// The writer thread owns its `SessionShared`/`base` handles for the process
// lifetime: passing them by value is the point (they must be `'static`), not an
// accidental clone.
#[allow(clippy::needless_pass_by_value)]
fn journal_writer_loop(
    state: SessionShared,
    mut rx: tokio_mpsc::UnboundedReceiver<JournalCmd>,
    base: Arc<AtomicUsize>,
) {
    let mut sink: Option<(String, SegmentWriter)> = None;
    while let Some(command) = rx.blocking_recv() {
        match command {
            JournalCmd::Record { ts, message } => {
                let id = current_session_id(&state);
                if sink.as_ref().is_none_or(|(open_id, _)| open_id != &id) {
                    // A rotated/opened conversation points at a different file;
                    // drop the stale handle instead of writing across ids.
                    sink = None;
                    match SegmentWriter::open(segment_path(&id), base.load(Ordering::SeqCst)) {
                        Ok(writer) => sink = Some((id, writer)),
                        Err(error) => {
                            warn_journal(&error);
                            continue;
                        }
                    }
                }
                let Some((_, writer)) = sink.as_mut() else {
                    continue;
                };
                // Same scrub rule as the snapshot (security.dev.md D9): the
                // segment must never persist a live credential either.
                let secrets = registered_secrets(&state);
                let scrubbed = redact_messages(std::slice::from_ref(&message), &secrets);
                let message = scrubbed.into_iter().next().unwrap_or(message);
                if let Err(error) = writer.append(ts, &message) {
                    warn_journal(&error);
                }
            }
            JournalCmd::Flush(ack) => {
                let result = match sink.as_mut() {
                    Some((_, writer)) => writer.sync(),
                    None => Ok(()),
                };
                let _ = ack.send(result);
            }
            JournalCmd::Rotate {
                base: new_base,
                ack,
            } => {
                let mut result = match sink.take() {
                    Some((_, mut writer)) => writer.sync(),
                    None => Ok(()),
                };
                if result.is_ok() {
                    let path = segment_path(&current_session_id(&state));
                    // No segment yet (nothing journalled): do not create an
                    // empty file just to reset its header.
                    if path.exists() {
                        result = runtime::reset_segment(&path, new_base);
                    }
                }
                base.store(new_base, Ordering::SeqCst);
                let _ = ack.send(result);
            }
        }
    }
}

/// The journal for this conversation, created on first use and shared by every
/// runtime rebuild. `base` is the current snapshot length: it only matters when
/// the segment file has to be created or rebuilt.
pub(crate) fn ensure_journal(state: &SessionShared, base: usize) -> Arc<DiskJournal> {
    let mut guard = lock_state(state);
    if let Some(journal) = &guard.journal {
        journal.rebase(base);
        return Arc::clone(journal);
    }
    let journal = Arc::new(DiskJournal::start(state, base));
    guard.journal = Some(Arc::clone(&journal));
    journal
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
    // A fresh conversation is saved under the sessions directory again.
    lock_state(state).archive_root = None;
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
    // The archive belongs beside the snapshot it documents.
    let archive = path.parent().map(|parent| parent.join("archive"));
    rebind_conversation(state, id);
    lock_state(state).archive_root = archive;
}

/// On-disk conversation footprint for `hf doctor`, which runs in its own
/// process and cannot see live counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SessionStats {
    pub(crate) snapshots: usize,
    pub(crate) snapshot_bytes: u64,
    pub(crate) segments: usize,
    pub(crate) segment_bytes: u64,
    /// Newest snapshot: id, accumulated turn runtime, message count, and age in
    /// seconds since its last write.
    pub(crate) newest: Option<(String, u64, usize, u64)>,
}

/// Scan the sessions directory. Snapshot names are `<millis>-<pid>.json`, which
/// is what separates them from sibling config files (e.g.
/// `compaction-stats.json`).
pub(crate) fn session_stats() -> SessionStats {
    let mut stats = SessionStats::default();
    let Ok(entries) = fs::read_dir(sessions_dir()) else {
        return stats;
    };
    let mut newest_mtime: Option<SystemTime> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("jsonl") => {
                stats.segments += 1;
                stats.segment_bytes += fs::metadata(&path).map_or(0, |meta| meta.len());
            }
            Some("json") if is_snapshot_stem(&name) => {
                stats.snapshots += 1;
                stats.snapshot_bytes += fs::metadata(&path).map_or(0, |meta| meta.len());
                let Ok(modified) = fs::metadata(&path).and_then(|meta| meta.modified()) else {
                    continue;
                };
                if newest_mtime.is_none_or(|seen| modified > seen) {
                    let id = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let session = Session::load_from_path(&path).ok();
                    stats.newest = Some((
                        id,
                        session.as_ref().map_or(0, |session| session.runtime_ms),
                        session.as_ref().map_or(0, |session| session.messages.len()),
                        SystemTime::now()
                            .duration_since(modified)
                            .map_or(0, |age| age.as_secs()),
                    ));
                    newest_mtime = Some(modified);
                }
            }
            _ => {}
        }
    }
    stats
}

fn is_snapshot_stem(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let Some((millis, pid)) = stem.split_once('-') else {
        return false;
    };
    millis.parse::<u64>().is_ok() && pid.parse::<u32>().is_ok()
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
