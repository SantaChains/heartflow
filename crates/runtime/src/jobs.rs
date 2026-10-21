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

fn job_dir(root: &Path, id: u64) -> PathBuf {
    root.join(format!("job-{id}"))
}

/// Best-effort disk write; a failed write is counted (doctor-visible) and
/// never blocks the job — the in-memory record is the source of truth.
fn persist(root: &Option<PathBuf>, id: u64, status_json: &str) {
    let Some(root) = root else { return };
    let dir = job_dir(root, id);
    if std::fs::create_dir_all(&dir).is_err() {
        DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if std::fs::write(dir.join("status.json"), status_json).is_err() {
        DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
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
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut file) => {
            if file.write_all(line.as_bytes()).is_err() {
                DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(_) => {
            DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
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
pub fn reconcile_stale_records() -> usize {
    let Some(root) = jobs_root() else { return 0 };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };
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
        record.status = JobStatus::Stopped;
        record.note = Some(
            "process exited before completion - output is partial; the task was NOT finished"
                .to_string(),
        );
        if let Ok(json) = serde_json::to_string_pretty(&record) {
            if std::fs::write(&status_path, json).is_ok() {
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
        std::thread::sleep(Duration::from_millis(120));
        // Only one slot: exactly one runner started; the other stays queued.
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

    #[test]
    fn failed_runner_carries_note_and_keeps_list_bounded() {
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
}
