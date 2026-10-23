//! Subagent jobs: the roadmap §1 "提交后不必原地等" task unit, modeled on
//! pi-subagents' async job contract (source-verified 2026-10-21, plus one
//! upstream re-check the same day).
//!
//! A job is a worker conversation running off-thread with its own isolated
//! context (the supervisor = the main REPL session). The registry owns the
//! lifecycle; execution is delegated to a [`JobRunner`] so the runtime crate
//! stays free of transport concerns and offline tests can script runners.
//!
//! Faithful-subset discipline (what is deliberately NOT here yet):
//! - status set is the intersection of pi's async-run set
//!   (queued/running/complete/failed/partial/paused/rejected/stopped) and its
//!   agent set (queued/running/completed/steered/aborted/stopped/error):
//!   `queued/running/complete/failed/stopped`. `partial`/`paused` need turn
//!   budgeting this MVP does not have; `steered` needs P-D steer delivery.
//! - workers cannot spawn jobs (depth = 1). pi guards recursion with spawn
//!   counters; the guard here is structural — no spawn surface exists.
//! - stop is cooperative at completion (see [`StopFlag`]): the in-flight LLM
//!   call is not interrupted (abort boundary, roadmap §4). The result of a
//!   stopped job is discarded and the terminal note says the output was NOT
//!   finished (pi's status-note discipline).
//! - events.jsonl is append-only JSON lines; the byte-cursor incremental
//!   reader pi uses is unnecessary at this scale (files are small, reads are
//!   tail-only), so this MVP reads tails directly.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::SystemTime;

/// Concurrent worker cap (semaphore semantics; pi enforces the same via its
/// concurrency queue). Override with `HEARTFLOW_JOB_SLOTS`.
pub const DEFAULT_JOB_SLOTS: usize = 3;
/// Cap on queued+running jobs; beyond this `spawn` refuses instead of
/// silently growing (pi `DEFAULT_MAX_PENDING=20`).
pub const DEFAULT_MAX_PENDING: usize = 20;
/// Terminal jobs kept visible in [`JobRegistry::list`] (pi fleet view keeps
/// the most recent 20 terminal runs).
pub const TERMINAL_KEEP: usize = 20;
/// Wall-clock cap on a *running* job (2026-10-21 research batch: counterpoint
/// arms 30 min for long external work at launch time — "a cap that exists only
/// in your polling plan is not a cap"; `codexmon` enforces the same WallSec
/// tier). Without it a hung worker stays `Running` forever and burns a slot.
/// Override with `HEARTFLOW_JOB_TIMEOUT_SECS` (0 disables); the cap starts at
/// the `Queued → Running` flip, not at spawn, so a queued job is not charged
/// for waiting.
pub const DEFAULT_JOB_WALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Job lifecycle. `Stopped` covers both user stop and process-exit
/// reconciliation; `Failed` is a runner error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Complete,
    Failed,
    Stopped,
}

impl JobStatus {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Complete => "complete",
            JobStatus::Failed => "failed",
            JobStatus::Stopped => "stopped",
        }
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Complete | JobStatus::Failed | JobStatus::Stopped
        )
    }
}

/// One job's durable snapshot; also the on-disk `status.json` schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobRecord {
    pub id: u64,
    pub label: String,
    pub status: JobStatus,
    /// First 80 chars of the prompt, for humans scanning `/jobs`.
    pub prompt_head: String,
    /// Owning process id. Job ids are process-local sequences, so two
    /// heartflow processes sharing `JOBS_ROOT` would collide on `job-<id>`;
    /// the pid namespaces the directory AND tells reconcile whom a live job
    /// belongs to (a second process must not terminalize the first one's
    /// running jobs). `0` marks records from before the field existed.
    #[serde(default)]
    pub pid: u32,
    /// Terminal note (pi status-note discipline): stopped/failed jobs say the
    /// output was partial so the supervisor never mistakes a cut-off for a
    /// finished task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_head: Option<String>,
}

/// Why the supervisor is being pinged (ds4/pi reason 三分法, minimal form):
/// progress never wakes the supervisor; results and decision needs do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Progress,
    NeedDecision,
    Result,
}

#[derive(Debug)]
pub struct WorkerMessage {
    pub job_id: u64,
    pub kind: MessageKind,
    pub text: String,
}

/// Cooperative stop flag. The runner checks it between units of work; the
/// in-flight LLM call is NOT interrupted (abort boundary, roadmap §4).
#[derive(Debug, Clone, Default)]
pub struct StopFlag(Arc<AtomicBool>);

impl StopFlag {
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The actual work. Implementations build their own isolated conversation
/// (own session, own client) and run the prompt to completion. `emit` is the
/// live channel to the supervisor; the return value is the final result text.
pub trait JobRunner: Send + 'static {
    fn run(
        &mut self,
        prompt: &str,
        stop: &StopFlag,
        emit: &mut dyn FnMut(MessageKind, &str),
    ) -> Result<String, String>;
}

#[derive(Debug)]
struct JobHandle {
    record: Mutex<JobRecord>,
    stop: StopFlag,
}

/// Error reasons for [`JobRegistry::spawn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// queued+running at `max_pending`; the caller should tell the operator to
    /// wait or stop a job (pi: pending cap 20).
    Saturated,
    // Workers cannot spawn workers (depth = 1) — there is no spawn surface
    // handed to runners at all, so no `DepthExceeded` variant exists: the
    // guard is structural, not a runtime check.
}

static JOBS_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);
static DISK_WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Point the registry at a directory (e.g. `~/.heartflow/sessions/jobs`).
/// `None` disables disk persistence (registry stays in-memory; tests).
pub fn set_jobs_root(root: Option<PathBuf>) {
    if let Ok(mut slot) = JOBS_ROOT.write() {
        *slot = root;
    }
}

#[must_use]
pub fn disk_write_failures() -> u64 {
    DISK_WRITE_FAILURES.load(Ordering::Relaxed)
}

fn jobs_root() -> Option<PathBuf> {
    JOBS_ROOT.read().ok().and_then(|slot| slot.clone())
}

/// Namespace a job directory by the *writing* process's pid. Job ids are
/// process-local sequences starting at 1, so the pid is what keeps two
/// heartflow processes sharing one root from writing over each other's
/// `status.json`. Layout: `job-<pid>-<id>` (legacy dirs are `job-<id>` with
/// `record.pid == 0`).
fn job_dir(root: &Path, id: u64) -> PathBuf {
    root.join(format!("job-{}-{id}", std::process::id()))
}

/// Best-effort disk write under the cross-process sidecar lock; a failed
/// write (or a lock timeout) is counted (doctor-visible) and never blocks
/// the job — the in-memory record is the source of truth. The write is
/// temp+rename: a bare `fs::write` here used to leave a torn `status.json`
/// behind if the process died mid-write, which then breaks reload-time
/// reconciliation.
fn persist(root: &Option<PathBuf>, id: u64, status_json: &str) {
    let Some(root) = root else { return };
    let dir = job_dir(root, id);
    persist_status_at(&dir.join("status.json"), status_json);
}

/// Locked temp+rename write of one `status.json`, usable from any path (the
/// rotation/reconcile writers included — a bare `fs::write` here used to be
/// able to leave a torn file that then breaks the next reconciliation).
fn persist_status_at(final_path: &Path, status_json: &str) {
    let Some(dir) = final_path.parent() else {
        DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let tmp_path = final_path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        std::fs::write(&tmp_path, status_json)?;
        let _ = std::fs::remove_file(final_path);
        std::fs::rename(&tmp_path, final_path)
    };
    match crate::filelock::with_exclusive(
        &crate::filelock::lock_path(final_path),
        crate::filelock::DEFAULT_LOCK_TIMEOUT,
        write,
    ) {
        Ok(Ok(())) => {}
        Ok(Err(_)) | Err(_) => {
            let _ = std::fs::remove_file(&tmp_path);
            DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn append_event(root: &Option<PathBuf>, id: u64, kind: &str, text: &str) {
    let Some(root) = root else { return };
    let path = job_dir(root, id).join("events.jsonl");
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let line = format!(
        "{{\"ts\":{nanos},\"kind\":\"{kind}\",\"text\":{}}}\n",
        serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
    );
    // Append under the cross-process lock (see mission.rs append_event):
    // unlocked appends from two processes can interleave mid-line.
    let appended = crate::filelock::with_exclusive(
        &crate::filelock::lock_path(&path),
        crate::filelock::DEFAULT_LOCK_TIMEOUT,
        || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut file| file.write_all(line.as_bytes()))
                .is_ok()
        },
    );
    if !matches!(appended, Ok(true)) {
        DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Registry of subagent jobs for this process. Cloning shares the mailbox and
/// the job map (Arcs inside), so the REPL can hold one handle and poll it.
#[derive(Debug, Clone)]
pub struct JobRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Debug)]
struct RegistryInner {
    slots: usize,
    max_pending: usize,
    wall_timeout: Option<std::time::Duration>,
    active: Arc<Mutex<usize>>,
    next_id: AtomicU64,
    tx: mpsc::Sender<WorkerMessage>,
    rx: Mutex<mpsc::Receiver<WorkerMessage>>,
    jobs: Mutex<BTreeMap<u64, Arc<JobHandle>>>,
}

impl JobRegistry {
    /// Registry with the given concurrency and no wall-clock timeout; call
    /// once per REPL and pass clones where needed.
    #[must_use]
    pub fn new(slots: usize, max_pending: usize) -> Self {
        Self::with_wall_timeout(slots, max_pending, None)
    }

    /// Registry with a per-job wall-clock timeout (see
    /// [`DEFAULT_JOB_WALL_TIMEOUT`]). `None` restores the unbounded MVP
    /// behavior.
    #[must_use]
    pub fn with_wall_timeout(
        slots: usize,
        max_pending: usize,
        wall_timeout: Option<std::time::Duration>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            inner: Arc::new(RegistryInner {
                slots: slots.max(1),
                max_pending: max_pending.max(1),
                wall_timeout,
                active: Arc::new(Mutex::new(0)),
                next_id: AtomicU64::new(1),
                tx,
                rx: Mutex::new(rx),
                jobs: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    #[must_use]
    pub fn slots(&self) -> usize {
        self.inner.slots
    }

    /// Spawn a worker job. Returns its id. The job starts `Queued`; the
    /// runner thread acquires a slot before executing and flips to `Running`.
    ///
    /// # Errors
    /// [`SpawnError::Saturated`] when queued+running hits the pending cap.
    pub fn spawn(
        &self,
        label: &str,
        prompt: &str,
        runner: Box<dyn JobRunner>,
    ) -> Result<u64, SpawnError> {
        // Lock poisoning on the registry means a worker panicked mid-record;
        // recovering with the intact fields is the house rule (污染后恢复),
        // a poisoned map is still a valid map.
        let mut jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let queued_plus_running = jobs
            .values()
            .filter(|handle| {
                handle
                    .record
                    .lock()
                    .map(|record| !record.status.is_terminal())
                    .unwrap_or(false)
            })
            .count();
        if queued_plus_running >= self.inner.max_pending {
            return Err(SpawnError::Saturated);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let prompt_head: String = prompt.trim().chars().take(80).collect();
        let record = JobRecord {
            id,
            label: label.trim().to_string(),
            status: JobStatus::Queued,
            prompt_head,
            pid: std::process::id(),
            note: None,
            result_head: None,
        };
        let status_json =
            serde_json::to_string_pretty(&record).unwrap_or_else(|_| "{}".to_string());
        let root = jobs_root();
        persist(&root, id, &status_json);
        append_event(&root, id, "queued", &format!("label={}", record.label));

        let handle = Arc::new(JobHandle {
            record: Mutex::new(record),
            stop: StopFlag::default(),
        });
        jobs.insert(id, handle);
        drop(jobs);
        // Worker thread: acquire a slot (parking the job as Queued), flip to
        // Running, execute, terminalize. Detached on purpose — the registry
        // record is the only observable handle.
        let registry = self.clone();
        let prompt_owned = prompt.trim().to_string();
        let mut runner = runner;
        let _ = std::thread::Builder::new()
            .name(format!("hf-job-{id}"))
            .spawn(move || {
                // Slot acquisition: spin on the active counter until below
                // the cap, then claim one slot atomically under the lock.
                loop {
                    if registry.stopped(id) {
                        registry.terminalize(id, JobStatus::Stopped, None);
                        return;
                    }
                    let acquired = {
                        let mut active = registry
                            .inner
                            .active
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if *active < registry.inner.slots {
                            *active += 1;
                            true
                        } else {
                            false
                        }
                    };
                    if acquired {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                registry.set_status(id, JobStatus::Running);
                append_event(&jobs_root(), id, "running", "");

                // Wall-clock watchdog (2026-10-21 research batch): a cap armed
                // at the moment work starts, not applied retroactively when
                // someone happens to notice. Cooperative — the flag is what
                // the runner observes; an LLM call already in flight finishes
                // first, exactly like an operator stop.
                if let Some(wall) = registry.inner.wall_timeout {
                    let registry = registry.clone();
                    let _ = std::thread::Builder::new()
                        .name(format!("hf-job-watchdog-{id}"))
                        .spawn(move || {
                            std::thread::sleep(wall);
                            // Only a still-Running job is ours to stop; an
                            // already-terminal job keeps its own (earlier)
                            // verdict, and terminalize is idempotent anyway.
                            if registry.status_of(id) == Some(JobStatus::Running) {
                                registry.stop(id);
                                append_event(&jobs_root(), id, "timeout", "");
                                registry.terminalize_note(
                                    id,
                                    JobStatus::Stopped,
                                    None,
                                    Some(format!(
                                        "wall-clock timeout after {}s - output is \
                                         partial; the task was NOT finished (timed out)",
                                        wall.as_secs()
                                    )),
                                );
                            }
                        });
                }

                let tx = registry.inner.tx.clone();
                let mut emit = |kind: MessageKind, text: &str| {
                    append_event(&jobs_root(), id, kind_label(kind), text);
                    let _ = tx.send(WorkerMessage {
                        job_id: id,
                        kind,
                        text: text.to_string(),
                    });
                };

                let outcome = runner.run(&prompt_owned, &registry.stop_flag(id), &mut emit);
                if let Ok(mut active) = registry.inner.active.lock() {
                    *active = active.saturating_sub(1);
                }
                match outcome {
                    Ok(result) if registry.stopped(id) => {
                        // The runner finished its in-flight work after the
                        // stop: discard the result, terminalize honestly.
                        registry.terminalize(id, JobStatus::Stopped, None);
                    }
                    Err(_) if registry.stopped(id) => {
                        // A stop-aware runner bails with an error; that is a
                        // stop, not a failure.
                        registry.terminalize(id, JobStatus::Stopped, None);
                    }
                    Ok(result) => {
                        let head: String = result.trim().chars().take(160).collect();
                        append_event(&jobs_root(), id, "result", &result);
                        // The supervisor wake: a completed result becomes a
                        // message (Result/NeedDecision are the only two kinds
                        // that wake the supervisor loop).
                        let _ = tx.send(WorkerMessage {
                            job_id: id,
                            kind: MessageKind::Result,
                            text: result,
                        });
                        registry.terminalize(id, JobStatus::Complete, Some(head));
                    }
                    Err(error) => {
                        append_event(&jobs_root(), id, "failed", &error);
                        let _ = tx.send(WorkerMessage {
                            job_id: id,
                            kind: MessageKind::NeedDecision,
                            text: format!("job failed: {error}"),
                        });
                        registry.terminalize(id, JobStatus::Failed, None);
                    }
                }
            });
        Ok(id)
    }

    fn stopped(&self, id: u64) -> bool {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .is_some_and(|handle| handle.stop.is_stopped())
    }

    fn stop_flag(&self, id: u64) -> StopFlag {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .map(|handle| handle.stop.clone())
            .unwrap_or_default()
    }

    fn set_status(&self, id: u64, status: JobStatus) {
        if let Ok(jobs) = self.inner.jobs.lock() {
            if let Some(handle) = jobs.get(&id) {
                if let Ok(mut record) = handle.record.lock() {
                    record.status = status;
                    let json =
                        serde_json::to_string_pretty(&*record).unwrap_or_else(|_| "{}".to_string());
                    persist(&jobs_root(), id, &json);
                }
            }
        }
    }

    fn status_of(&self, id: u64) -> Option<JobStatus> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .and_then(|handle| handle.record.lock().ok().map(|record| record.status))
    }

    fn terminalize(&self, id: u64, status: JobStatus, result_head: Option<String>) {
        let note = match status {
            JobStatus::Stopped => Some(
                "stopped before completion - output is partial; the task was NOT finished"
                    .to_string(),
            ),
            JobStatus::Failed => Some("worker errored - see /jobs log for the failure".to_string()),
            _ => None,
        };
        self.terminalize_note(id, status, result_head, note);
    }

    /// Terminalize with an explicit note, idempotently: the first terminal
    /// verdict wins. This is what keeps a watchdog-issued timeout verdict from
    /// being overwritten when the cooperative runner unwinds moments later —
    /// the stop path in the worker thread calls [`Self::terminalize`] again,
    /// and a second Stopped write would be harmless, but a late `Ok(result)`
    /// reaching `Complete` would be a lie.
    fn terminalize_note(
        &self,
        id: u64,
        status: JobStatus,
        result_head: Option<String>,
        note: Option<String>,
    ) {
        if let Ok(jobs) = self.inner.jobs.lock() {
            if let Some(handle) = jobs.get(&id) {
                if let Ok(mut record) = handle.record.lock() {
                    if record.status.is_terminal() {
                        return;
                    }
                    record.status = status;
                    record.note = note;
                    record.result_head = result_head;
                    let json =
                        serde_json::to_string_pretty(&*record).unwrap_or_else(|_| "{}".to_string());
                    persist(&jobs_root(), id, &json);
                }
            }
        }
        append_event(&jobs_root(), id, status.label(), "");
    }

    /// Shutdown honesty (roadmap §4 停机安全): terminalize every
    /// non-terminal job with an explicit shutdown note, so a restart — or the
    /// operator reading `status.json` directly — sees the truth immediately
    /// instead of waiting for next-start reconciliation. Idempotent by way of
    /// [`Self::terminalize_note`]'s first-verdict-wins discipline: a worker
    /// finishing microseconds after the host decided to exit cannot overwrite
    /// the shutdown verdict. Returns how many jobs were finalized.
    #[must_use]
    pub fn finalize_on_shutdown(&self, note: &str) -> usize {
        let ids: Vec<u64> = match self.inner.jobs.lock() {
            Ok(jobs) => jobs
                .iter()
                .filter_map(|(id, handle)| {
                    let non_terminal = handle
                        .record
                        .lock()
                        .map(|record| !record.status.is_terminal())
                        .unwrap_or(false);
                    non_terminal.then_some(*id)
                })
                .collect(),
            Err(_) => return 0,
        };
        for id in &ids {
            self.terminalize_note(*id, JobStatus::Stopped, None, Some(note.to_string()));
        }
        ids.len()
    }

    /// Non-blocking drain of worker messages. The caller (supervisor loop)
    /// decides what wakes a turn: only `Result`/`NeedDecision` do (progress
    /// updates are already in events.jsonl).
    #[must_use]
    pub fn poll(&self) -> Vec<WorkerMessage> {
        let Some(rx) = self.inner.rx.lock().ok() else {
            return Vec::new();
        };
        let mut messages = Vec::new();
        while let Ok(message) = rx.try_recv() {
            messages.push(message);
        }
        messages
    }

    /// Ask a job to stop. Cooperative: the current LLM call finishes, its
    /// result is discarded, and the job terminalizes as `Stopped`.
    pub fn stop(&self, id: u64) -> bool {
        let Ok(jobs) = self.inner.jobs.lock() else {
            return false;
        };
        let Some(handle) = jobs.get(&id) else {
            return false;
        };
        handle.stop.stop();
        true
    }

    /// All jobs: non-terminal first (id ascending), then terminal
    /// newest-first capped at [`TERMINAL_KEEP`].
    #[must_use]
    pub fn list(&self) -> Vec<JobRecord> {
        let jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut active = Vec::new();
        let mut terminal = Vec::new();
        for handle in jobs.values() {
            let Ok(record) = handle.record.lock() else {
                continue;
            };
            if record.status.is_terminal() {
                terminal.push(record.clone());
            } else {
                active.push(record.clone());
            }
        }
        terminal.reverse(); // newest first by id order of the map
        terminal.truncate(TERMINAL_KEEP);
        active.extend(terminal);
        active
    }

    /// Last `lines` event texts for a job, oldest last.
    #[must_use]
    pub fn log_tail(&self, id: u64, lines: usize) -> Vec<String> {
        let Some(root) = jobs_root() else {
            return Vec::new();
        };
        let path = job_dir(&root, id).join("events.jsonl");
        let Ok(content) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let mut all: Vec<String> = content.lines().map(str::to_string).collect();
        all.reverse();
        all.truncate(lines.max(1));
        all.reverse();
        all
    }

    /// Whether `id` exists and is still non-terminal.
    #[must_use]
    pub fn is_active(&self, id: u64) -> bool {
        let Ok(jobs) = self.inner.jobs.lock() else {
            return false;
        };
        jobs.get(&id).is_some_and(|handle| {
            handle
                .record
                .lock()
                .map(|record| !record.status.is_terminal())
                .unwrap_or(false)
        })
    }
}

fn kind_label(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Progress => "progress",
        MessageKind::NeedDecision => "need_decision",
        MessageKind::Result => "result",
    }
}

/// Process-exit reconciliation: disk shows the *intended* state, but a job
/// whose process died is not running. Scanning on registry startup, any
/// `status.json` still queued/running is re-filed as `Stopped` with the pi
/// status-note so a stale record can never masquerade as live work.
/// Whether a process id is still running, cross-platform without new deps.
///
/// Deliberately conservative: an *undeterminable* pid (the probe command
/// itself fails) reports alive. Stopping a job whose owner may still be
/// running is the result-changing direction ("结果会变错" 的降级); leaving a
/// dead owner's job in `running` until the next reconcile is recoverable.
/// Residual risk, recorded honestly: a dead owner's pid can be reused by an
/// unrelated process, leaving the job stranded as `running` — pi's
/// process-terminal proof is the fuller answer if that ever bites.
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        // `tasklist` filters by exact pid and prints one CSV line whose shape
        // is `"image.exe","<pid>","session",...` - the pid is the second
        // quoted field, so match `,"<pid>",` rather than anything looser
        // (image names and memory figures could otherwise collide). A dead
        // pid yields only the unquoted "INFO:" line.
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.contains(&format!(",\"{pid}\","))),
            Err(_) => true, // cannot prove dead -> treat as alive
        }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .output()
            .map(|output| output.status.success())
            .unwrap_or(true)
    }
}

/// Startup reconciliation over the shared jobs root. Ownership rules
/// (§4 cross-process fix):
///
/// - a job owned by *this* pid is a previous incarnation of this process
///   that died mid-job -> terminalize (the original behavior);
/// - a legacy record (`pid == 0`, from before namespacing) cannot be
///   attributed -> terminalize, exactly as the old code did;
/// - a job owned by a *foreign* pid is terminalized only when that process
///   is verifiably dead — a second heartflow process starting must never
///   stop the first one's live jobs (the collision this fixes).
pub fn reconcile_stale_records() -> usize {
    let Some(root) = jobs_root() else { return 0 };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };
    let own_pid = std::process::id();
    let mut repaired = 0;
    for entry in entries.flatten() {
        let status_path = entry.path().join("status.json");
        let Ok(content) = std::fs::read_to_string(&status_path) else {
            continue;
        };
        let Ok(mut record) = serde_json::from_str::<JobRecord>(&content) else {
            continue;
        };
        if record.status.is_terminal() {
            continue;
        }
        if record.pid != 0 && record.pid != own_pid && process_alive(record.pid) {
            // Another live process owns this job; hands off.
            continue;
        }
        record.status = JobStatus::Stopped;
        record.note = Some(if record.pid == 0 || record.pid == own_pid {
            "process exited before completion - output is partial; the task was NOT finished"
                .to_string()
        } else {
            format!(
                "owning process {} exited before completion - output is partial; the task was NOT finished",
                record.pid
            )
        });
        if let Ok(json) = serde_json::to_string_pretty(&record) {
            persist_status_at(&status_path, &json);
            // The helper counts failures on the same doctor-visible counter;
            // a successful rewrite repairs exactly one record.
            if std::fs::read_to_string(&status_path)
                .map(|text| text == json)
                .unwrap_or(false)
            {
                repaired += 1;
            }
        }
    }
    repaired
}

#[cfg(test)]
mod tests {
    use super::{
        JobRecord, JobRegistry, JobRunner, JobStatus, MessageKind, SpawnError, StopFlag,
        DEFAULT_MAX_PENDING, TERMINAL_KEEP,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// `set_jobs_root` mutates a process-global; tests that persist (or
    /// reconcile over the shared root) must serialize, or one test's records
    /// land in another test's root mid-assert. Every test takes the lock for
    /// its whole body (mission.rs ROOT_LOCK pattern).
    static ROOT_LOCK: Mutex<()> = Mutex::new(());

    fn root_guard() -> std::sync::MutexGuard<'static, ()> {
        ROOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runner that blocks until released — drives slot/saturation tests.
    struct GateRunner {
        gate: Arc<Mutex<bool>>,
        started: Arc<AtomicUsize>,
    }

    impl JobRunner for GateRunner {
        fn run(
            &mut self,
            _prompt: &str,
            stop: &StopFlag,
            emit: &mut dyn FnMut(MessageKind, &str),
        ) -> Result<String, String> {
            self.started.fetch_add(1, Ordering::Relaxed);
            emit(MessageKind::Progress, "worker started");
            loop {
                if stop.is_stopped() {
                    return Err("stopped".to_string());
                }
                if *self
                    .gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok("worker done".to_string())
        }
    }

    fn gate() -> Arc<Mutex<bool>> {
        Arc::new(Mutex::new(false))
    }

    fn release(gate: &Arc<Mutex<bool>>) {
        if let Ok(mut slot) = gate.lock() {
            *slot = true;
        }
    }

    fn drain_until(registry: &JobRegistry, job_id: u64, status: JobStatus) -> bool {
        for _ in 0..100 {
            let hit = registry
                .list()
                .iter()
                .any(|record| record.id == job_id && record.status == status);
            if hit {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn lifecycle_runs_through_slots_and_terminalizes() {
        let _guard = root_guard();
        set_jobs_root_for_test("lifecycle");
        let registry = JobRegistry::new(2, DEFAULT_MAX_PENDING);
        let gate = gate();
        let started = Arc::new(AtomicUsize::new(0));
        let id = registry
            .spawn(
                "research",
                "study the thing",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone(),
                }),
            )
            .expect("spawn");
        assert!(
            drain_until(&registry, id, JobStatus::Running),
            "must reach running"
        );
        release(&gate);
        assert!(drain_until(&registry, id, JobStatus::Complete));
        let record = registry
            .list()
            .into_iter()
            .find(|record: &JobRecord| record.id == id)
            .expect("record");
        assert_eq!(record.status, JobStatus::Complete);
        assert_eq!(record.result_head.as_deref(), Some("worker done"));
        assert!(record.note.is_none(), "complete jobs carry no note");
        // The result message reached the supervisor mailbox.
        let messages = registry.poll();
        assert!(messages
            .iter()
            .any(|m| m.job_id == id && m.kind == MessageKind::Result && m.text == "worker done"));
        clear_jobs_root_for_test();
    }

    #[test]
    fn excess_jobs_queue_behind_full_slots() {
        let _guard = root_guard();
        set_jobs_root_for_test("slots");
        let registry = JobRegistry::new(1, DEFAULT_MAX_PENDING);
        let gate = gate();
        let started = Arc::new(AtomicUsize::new(0));
        let first = registry
            .spawn(
                "a",
                "a",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone(),
                }),
            )
            .expect("first");
        let second = registry
            .spawn(
                "b",
                "b",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone(),
                }),
            )
            .expect("second");
        // Only one slot: exactly one runner started; the other stays queued.
        // Poll to that state (bounded) instead of sleeping a fixed slice —
        // under a full parallel test run the worker thread may not be
        // scheduled within any fixed window, and the invariant is the state,
        // not the clock.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while started.load(Ordering::Relaxed) != 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(started.load(Ordering::Relaxed), 1);
        let statuses: Vec<_> = registry
            .list()
            .into_iter()
            .map(|r| (r.id, r.status))
            .collect();
        assert!(statuses.contains(&(first, JobStatus::Running)));
        assert!(statuses.contains(&(second, JobStatus::Queued)));
        release(&gate);
        assert!(drain_until(&registry, second, JobStatus::Complete));
        clear_jobs_root_for_test();
    }

    #[test]
    fn pending_cap_refuses_instead_of_growing() {
        let _guard = root_guard();
        set_jobs_root_for_test("cap");
        // One slot, two allowed non-terminal: first runs, second queues,
        // third is refused at the cap.
        let registry = JobRegistry::new(1, 2);
        let gate = gate();
        let started = Arc::new(AtomicUsize::new(0));
        registry
            .spawn(
                "a",
                "a",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone(),
                }),
            )
            .expect("first");
        registry
            .spawn(
                "b",
                "b",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone(),
                }),
            )
            .expect("second (queued)");
        assert_eq!(
            registry.spawn(
                "c",
                "c",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone()
                })
            ),
            Err(SpawnError::Saturated),
            "third spawn must refuse at the pending cap"
        );
        release(&gate);
        clear_jobs_root_for_test();
    }

    #[test]
    fn stop_discards_result_and_appends_the_partial_note() {
        let _guard = root_guard();
        set_jobs_root_for_test("stop");
        let registry = JobRegistry::new(2, DEFAULT_MAX_PENDING);
        let gate = gate();
        let started = Arc::new(AtomicUsize::new(0));
        let id = registry
            .spawn(
                "long",
                "long task",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone(),
                }),
            )
            .expect("spawn");
        assert!(drain_until(&registry, id, JobStatus::Running));
        assert!(registry.stop(id), "stop must find the job");
        release(&gate);
        assert!(drain_until(&registry, id, JobStatus::Stopped));
        let record = registry
            .list()
            .into_iter()
            .find(|record: &JobRecord| record.id == id)
            .expect("record");
        assert_eq!(record.status, JobStatus::Stopped);
        assert!(
            record
                .note
                .as_deref()
                .is_some_and(|note| note.contains("NOT finished")),
            "stopped jobs must say the task was not finished: {:?}",
            record.note
        );
        assert!(
            record.result_head.is_none(),
            "stopped results are discarded"
        );
        clear_jobs_root_for_test();
    }

    /// Shutdown honesty: finalize_on_shutdown retires a mid-run job with the
    /// shutdown note, is idempotent, and the verdict survives a late
    /// completion (first-verdict-wins — the host decided to exit; a worker
    /// unwinding microseconds later may not claim Complete afterwards).
    #[test]
    fn finalize_on_shutdown_honest_and_unoverwriteable() {
        let _guard = root_guard();
        set_jobs_root_for_test("shutdown");
        let registry = JobRegistry::new(2, DEFAULT_MAX_PENDING);
        let gate = gate();
        let started = Arc::new(AtomicUsize::new(0));
        let id = registry
            .spawn(
                "long",
                "long task",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone(),
                }),
            )
            .expect("spawn");
        assert!(drain_until(&registry, id, JobStatus::Running));
        assert_eq!(
            registry.finalize_on_shutdown("session exited while the job was running"),
            1,
            "one running job finalized"
        );
        // Idempotent: the second call finds nothing non-terminal.
        assert_eq!(registry.finalize_on_shutdown("again"), 0);
        let record = registry
            .list()
            .into_iter()
            .find(|record: &JobRecord| record.id == id)
            .expect("record");
        assert_eq!(record.status, JobStatus::Stopped);
        assert_eq!(
            record.note.as_deref(),
            Some("session exited while the job was running"),
            "shutdown note must be the custom one"
        );
        // The "late completion": the runner actually finished after the host
        // wrote its verdict. First verdict wins — status stays Stopped.
        release(&gate);
        drain_until(&registry, id, JobStatus::Complete);
        let record = registry
            .list()
            .into_iter()
            .find(|record: &JobRecord| record.id == id)
            .expect("record");
        assert_eq!(
            record.status,
            JobStatus::Stopped,
            "a shutdown verdict may not be overwritten by a late completion"
        );
        assert_eq!(
            record.note.as_deref(),
            Some("session exited while the job was running")
        );
        clear_jobs_root_for_test();
    }

    #[test]
    fn failed_runner_carries_note_and_keeps_list_bounded() {
        let _guard = root_guard();
        set_jobs_root_for_test("failed");
        let registry = JobRegistry::new(1, DEFAULT_MAX_PENDING);
        struct Boom;
        impl JobRunner for Boom {
            fn run(
                &mut self,
                _prompt: &str,
                _stop: &StopFlag,
                _emit: &mut dyn FnMut(MessageKind, &str),
            ) -> Result<String, String> {
                Err("exploded".to_string())
            }
        }
        let id = registry
            .spawn("boom", "boom", Box::new(Boom))
            .expect("spawn");
        assert!(drain_until(&registry, id, JobStatus::Failed));
        let record = registry
            .list()
            .into_iter()
            .find(|record: &JobRecord| record.id == id)
            .expect("record");
        assert!(record
            .note
            .as_deref()
            .is_some_and(|note| note.contains("errored")));
        // Terminal keep cap: spawn+finish many, listing stays bounded.
        for _ in 0..(TERMINAL_KEEP + 5) {
            let id = registry
                .spawn("boom", "boom", Box::new(Boom))
                .expect("spawn");
            assert!(drain_until(&registry, id, JobStatus::Failed));
        }
        let terminal_count = registry
            .list()
            .into_iter()
            .filter(|record| record.status.is_terminal())
            .count();
        assert!(
            terminal_count <= TERMINAL_KEEP,
            "fleet view must stay bounded: {terminal_count}"
        );
        clear_jobs_root_for_test();
    }

    /// The wall-clock watchdog must retire a stuck running job, and the
    /// verdict must survive the cooperative runner's own unwind (first
    /// terminal verdict wins — a late completion may not overwrite a timeout).
    #[test]
    fn wall_timeout_stops_a_stuck_running_job_and_wins_the_verdict() {
        let _guard = root_guard();
        set_jobs_root_for_test("wall");
        let registry = JobRegistry::with_wall_timeout(
            1,
            DEFAULT_MAX_PENDING,
            Some(Duration::from_millis(150)),
        );
        let gate = gate();
        let started = Arc::new(AtomicUsize::new(0));
        let id = registry
            .spawn(
                "stuck",
                "never finishes on its own",
                Box::new(GateRunner {
                    gate: gate.clone(),
                    started: started.clone(),
                }),
            )
            .expect("spawn");
        assert!(drain_until(&registry, id, JobStatus::Running));
        assert!(drain_until(&registry, id, JobStatus::Stopped));
        let record = registry
            .list()
            .into_iter()
            .find(|record: &JobRecord| record.id == id)
            .expect("record");
        assert!(
            record
                .note
                .as_deref()
                .is_some_and(|note| note.contains("wall-clock timeout")),
            "timeout verdict must carry the wall-clock note: {:?}",
            record.note
        );
        // The cooperative runner unwinds after the watchdog fired (its stop
        // check sees the flag). Neither its `Err`-as-stop path nor a released
        // gate may overwrite the timeout verdict.
        release(&gate);
        std::thread::sleep(Duration::from_millis(250));
        let record = registry
            .list()
            .into_iter()
            .find(|record: &JobRecord| record.id == id)
            .expect("record after unwind");
        assert_eq!(record.status, JobStatus::Stopped, "verdict is final");
        assert!(
            record.result_head.is_none(),
            "timed-out results are discarded"
        );
        clear_jobs_root_for_test();
    }

    // Disk-root plumbing shared by the tests above: a unique temp dir per
    // label, registered through the same static the CLI uses.
    fn set_jobs_root_for_test(label: &str) {
        let dir = std::env::temp_dir().join(format!(
            "hf-jobs-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("jobs temp dir");
        super::set_jobs_root(Some(dir));
    }

    fn clear_jobs_root_for_test() {
        super::set_jobs_root(None);
    }

    /// A child process that stays alive until killed — stands in for "another
    /// live heartflow process owning a job".
    fn spawn_sleeper() -> std::process::Child {
        #[cfg(windows)]
        let mut command = {
            let mut command = std::process::Command::new("ping");
            command.args(["-n", "30", "127.0.0.1"]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            command
        };
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command.spawn().expect("sleeper child")
    }

    fn write_record(
        root: &std::path::Path,
        dir_name: &str,
        record: &super::JobRecord,
    ) -> std::path::PathBuf {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).expect("foreign job dir");
        let path = dir.join("status.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(record).expect("serialize record"),
        )
        .expect("write foreign record");
        path
    }

    fn running_record(id: u64, pid: u32) -> super::JobRecord {
        super::JobRecord {
            id,
            label: format!("job {id}"),
            status: super::JobStatus::Running,
            prompt_head: "foreign work".to_string(),
            pid,
            note: None,
            result_head: None,
        }
    }

    /// Runner that finishes immediately — drives the namespace test.
    struct QuickRunner {
        done: Arc<AtomicUsize>,
    }

    impl JobRunner for QuickRunner {
        fn run(
            &mut self,
            _prompt: &str,
            _stop: &StopFlag,
            _emit: &mut dyn FnMut(MessageKind, &str),
        ) -> Result<String, String> {
            self.done.fetch_add(1, Ordering::SeqCst);
            Ok("done".to_string())
        }
    }

    #[test]
    fn job_dirs_carry_the_pid_namespace() {
        let _guard = root_guard();
        set_jobs_root_for_test("namespace");
        let registry = JobRegistry::new(1, DEFAULT_MAX_PENDING);
        let done = Arc::new(AtomicUsize::new(0));
        registry
            .spawn(
                "ns",
                "finish now",
                Box::new(QuickRunner {
                    done: Arc::clone(&done),
                }),
            )
            .expect("spawn");
        // The invariant under test is the spawn-time write, so poll for the
        // file itself (bounded) - under a fully loaded test run the slot
        // thread may take arbitrarily long, and any fixed sleep window races.
        let root = super::jobs_root().expect("root set");
        let dir = root.join(format!("job-{}-1", std::process::id()));
        let status_path = dir.join("status.json");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Ok(text) = std::fs::read_to_string(&status_path) {
                break text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the namespaced status.json never appeared: {}",
                status_path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.contains(&format!("\"pid\": {}", std::process::id())));
        // The runner finishing is auxiliary; poll it (bounded) because under
        // a loaded suite the worker thread may start arbitrarily late.
        let done_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while done.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < done_deadline,
                "the runner never finished"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        clear_jobs_root_for_test();
    }

    #[test]
    fn reconcile_leaves_a_live_foreign_job_alone() {
        let _guard = root_guard();
        set_jobs_root_for_test("foreign-live");
        let root = super::jobs_root().expect("root set");
        let mut sleeper = spawn_sleeper();
        let path = write_record(
            &root,
            &format!("job-{}-9", sleeper.id()),
            &running_record(9, sleeper.id()),
        );
        let repaired = super::reconcile_stale_records();
        let content = std::fs::read_to_string(&path).expect("record intact");
        assert!(
            content.contains("\"running\""),
            "a live foreign owner's job must not be terminalized: {content}"
        );
        assert_eq!(repaired, 0, "nothing was repaired for the live foreign job");
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        clear_jobs_root_for_test();
    }

    #[test]
    fn reconcile_stops_a_dead_foreign_job_with_the_owner_named() {
        let _guard = root_guard();
        set_jobs_root_for_test("foreign-dead");
        let root = super::jobs_root().expect("root set");
        // A child that exits immediately: after wait() its pid is dead.
        let mut short_lived = {
            #[cfg(windows)]
            {
                let mut command = std::process::Command::new("cmd");
                command.args(["/C", "exit", "0"]);
                command
            }
            #[cfg(not(windows))]
            {
                let mut command = std::process::Command::new("true");
                command
            }
        };
        let dead_pid = short_lived.spawn().expect("child ran").id();
        // Give the child a moment to exit so its pid is genuinely dead.
        std::thread::sleep(Duration::from_millis(300));
        let path = write_record(
            &root,
            &format!("job-{dead_pid}-9"),
            &running_record(9, dead_pid),
        );
        let repaired = super::reconcile_stale_records();
        assert_eq!(repaired, 1);
        let content = std::fs::read_to_string(&path).expect("record readable");
        assert!(content.contains("\"stopped\""));
        assert!(
            content.contains(&format!("owning process {dead_pid} exited")),
            "the note names the dead owner: {content}"
        );
        clear_jobs_root_for_test();
    }

    #[test]
    fn reconcile_stops_own_and_legacy_records_as_before() {
        let _guard = root_guard();
        set_jobs_root_for_test("own-legacy");
        let root = super::jobs_root().expect("root set");
        // Our own pid: a previous incarnation of this process.
        let own = write_record(
            &root,
            &format!("job-{}-1", std::process::id()),
            &running_record(1, std::process::id()),
        );
        // Legacy layout: no pid in the name, none in the record.
        let legacy = write_record(&root, "job-7", &running_record(7, 0));
        assert_eq!(super::reconcile_stale_records(), 2);
        for (path, label) in [(own, "own"), (legacy, "legacy")] {
            let content = std::fs::read_to_string(path).expect("record readable");
            assert!(content.contains("\"stopped\""), "{label} terminalized");
            assert!(
                content.contains("process exited before completion"),
                "{label} keeps the original note: {content}"
            );
        }
        clear_jobs_root_for_test();
    }
}
