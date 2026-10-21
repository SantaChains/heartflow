//! Scheduled runs: roadmap §1 "pi 的 job" item 6 — due work triggered by an
//! external launcher, never a resident daemon (pi-subagents
//! `scheduled-runs.ts` source-verified 2026-10-21; `mission.run-due` 纪律).
//!
//! The REPL drains due schedules at startup, at each turn boundary and via
//! `/schedule run-due`; each due schedule becomes a normal subagent job
//! (`JobRegistry::spawn`), so scheduling adds no execution machinery — it is
//! a due-computation + ledger over the existing job system.
//!
//! Semantics locked to the upstream contract where it matters:
//! - **`overlap: "skip"` is the only overlap semantics** (upstream quote:
//!   不排队、不并行): a schedule with an unfinished run in history is
//!   skipped, and the skip is recorded as history, not swallowed.
//! - **`catchUp: none | latest`**: a missed `Once` either launches on the
//!   next drain (`Latest`) or is consumed as `SkippedCatchUp` (`None`).
//! - History is bounded (`MAX_HISTORY`); the whole store is one atomic
//!   `schedules.json` (temp + rename, size-capped), mirroring
//!   `jobs.rs`/`mission.rs` — the in-memory map is the source of truth and
//!   disk failures are counted for `hf doctor`.
//!
//! Honest gaps (recorded, not hidden): trigger specs support relative
//! (`+30m`) and epoch-ms times only — local wall-clock `HH:MM`/IANA zones
//! need `chrono-tz` (roadmap dependency-evaluation item); `calendar` triggers
//! are therefore not expressible yet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

/// Per-schedule history cap (pi `MAX_HISTORY=100`).
pub const MAX_HISTORY: usize = 100;
/// Cap on total schedules; `add` refuses beyond this instead of growing the
/// store file without bounds.
pub const MAX_SCHEDULES: usize = 100;
/// `schedules.json` size cap (same rationale as the mission state cap).
pub const SCHEDULES_FILE_LIMIT: usize = 256 * 1024;

/// What fires a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleTrigger {
    /// Fire once at (or after, per `catch_up`) this epoch-millisecond.
    Once { at_ms: u64 },
    /// Fire every N seconds of wall clock since the last launch.
    Interval { every_secs: u64 },
}

/// What to do with occurrences missed while nobody was draining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatchUp {
    /// Missed occurrences are consumed as `SkippedCatchUp` (the common
    /// choice: a stale "run the nightly audit" should not fire at 9am).
    #[default]
    None,
    /// The most recent missed occurrence launches on the next drain.
    Latest,
}

/// Who asked for this drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DueReason {
    /// Automatic: REPL startup / turn-boundary drain.
    Timer,
    /// `/schedule run-due` — the external-launcher form.
    RunDue,
    /// `/schedule run <id>` — a single manual launch.
    Manual,
}

impl DueReason {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            DueReason::Timer => "timer",
            DueReason::RunDue => "run-due",
            DueReason::Manual => "manual",
        }
    }
}

/// One recorded (or skipped) launch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScheduleRun {
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    pub outcome: RunOutcome,
    pub reason: DueReason,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// Launched, job still running.
    Started,
    /// Overlap skip: a previous run had not finished.
    SkippedOverlap,
    /// A missed `Once` consumed under `catchUp: none`.
    SkippedCatchUp,
    Complete,
    Failed {
        error: String,
    },
}

/// The schedule record; also the `schedules.json` entry schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Schedule {
    pub id: u64,
    pub name: String,
    pub prompt: String,
    pub trigger: ScheduleTrigger,
    #[serde(default)]
    pub catch_up: CatchUp,
    /// Success does not wake the supervisor (result still lands in the job
    /// log); failures always wake.
    #[serde(default)]
    pub quiet: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<u64>,
    #[serde(default)]
    pub history: Vec<ScheduleRun>,
    pub created_ms: u64,
}

fn default_true() -> bool {
    true
}

impl Schedule {
    /// A run is in flight when history holds an unfinished `Started` entry.
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.history
            .iter()
            .any(|run| run.outcome == RunOutcome::Started && run.finished_ms.is_none())
    }

    /// Trigger arithmetic ignoring the overlap guard (the overlap skip is
    /// recorded by `run_due`, not folded silently into "not due").
    #[must_use]
    pub fn trigger_due(&self, now_ms: u64) -> bool {
        if !self.enabled {
            return false;
        }
        match self.trigger {
            ScheduleTrigger::Once { at_ms } => self.last_run_ms.is_none() && now_ms >= at_ms,
            ScheduleTrigger::Interval { every_secs } => {
                let period = every_secs.saturating_mul(1000);
                period > 0
                    && self
                        .last_run_ms
                        .is_none_or(|last| now_ms.saturating_sub(last) >= period)
            }
        }
    }

    /// Full due check for observation surfaces (`hf doctor`, listings):
    /// trigger due AND nothing in flight.
    #[must_use]
    pub fn is_due(&self, now_ms: u64) -> bool {
        self.trigger_due(now_ms) && !self.in_flight()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("no schedule #{0}")]
    NotFound(u64),
    #[error("schedule store is full ({MAX_SCHEDULES}); delete one with /schedule delete")]
    Full,
    #[error("bad trigger spec `{0}`")]
    BadSpec(String),
}

static SCHEDULES_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);
static DISK_WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Point the store at a directory (e.g. `~/.heartflow/sessions/schedules`).
/// `None` disables persistence (in-memory only; tests).
pub fn set_schedules_root(root: Option<PathBuf>) {
    if let Ok(mut slot) = SCHEDULES_ROOT.write() {
        *slot = root;
    }
}

#[must_use]
pub fn schedule_disk_write_failures() -> u64 {
    DISK_WRITE_FAILURES.load(Ordering::Relaxed)
}

fn schedules_root() -> Option<PathBuf> {
    SCHEDULES_ROOT.read().ok().and_then(|slot| slot.clone())
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    next_id: u64,
    schedules: Vec<Schedule>,
}

/// In-process schedule store; `Mutex` over the map is the single writer.
#[derive(Debug, Default)]
pub struct ScheduleStore {
    schedules: Mutex<BTreeMap<u64, Schedule>>,
    next_id: AtomicU64,
    corrupted_on_load: Mutex<bool>,
}

impl ScheduleStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reload from disk. One file holds the whole store; a parse failure
    /// leaves the store empty and flags `corrupted_on_load` (doctor-visible).
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let store = Self::new();
        let Ok(raw) = std::fs::read_to_string(root.join("schedules.json")) else {
            return store;
        };
        match serde_json::from_str::<StoreFile>(&raw) {
            Ok(file) => {
                for schedule in file.schedules {
                    store.next_id.fetch_max(schedule.id + 1, Ordering::Relaxed);
                    store
                        .schedules
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(schedule.id, schedule);
                }
                store.next_id.fetch_max(file.next_id, Ordering::Relaxed);
            }
            Err(_) => {
                *store
                    .corrupted_on_load
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            }
        }
        store
    }

    #[must_use]
    pub fn corrupted_on_load(&self) -> bool {
        *self
            .corrupted_on_load
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn add(
        &self,
        name: &str,
        prompt: &str,
        trigger: ScheduleTrigger,
        catch_up: CatchUp,
        quiet: bool,
    ) -> Result<Schedule, ScheduleError> {
        {
            let map = self
                .schedules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if map.len() >= MAX_SCHEDULES {
                return Err(ScheduleError::Full);
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let schedule = Schedule {
            id,
            name: name.trim().to_string(),
            prompt: prompt.trim().to_string(),
            trigger,
            catch_up,
            quiet,
            enabled: true,
            last_run_ms: None,
            history: Vec::new(),
            created_ms: now_ms(),
        };
        self.schedules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, schedule.clone());
        self.persist();
        Ok(schedule)
    }

    #[must_use]
    pub fn list(&self) -> Vec<Schedule> {
        self.schedules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn get(&self, id: u64) -> Option<Schedule> {
        self.schedules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    pub fn pause(&self, id: u64) -> Result<Schedule, ScheduleError> {
        self.mutate(id, |s| s.enabled = false)
    }

    pub fn resume(&self, id: u64) -> Result<Schedule, ScheduleError> {
        self.mutate(id, |s| s.enabled = true)
    }

    pub fn delete(&self, id: u64) -> Result<Schedule, ScheduleError> {
        let removed = self
            .schedules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
            .ok_or(ScheduleError::NotFound(id))?;
        self.persist();
        Ok(removed)
    }

    /// The launcher entry point: snapshot of due schedules. Reserves each
    /// launch by stamping `last_run_ms` *now* (so a caller that dies before
    /// `record_launched` cannot cause an infinite refire within one period),
    /// records skip outcomes for overlap/catch-up, and returns the schedules
    /// the caller should spawn as jobs. Callers MUST follow up with
    /// [`Self::record_launched`] or [`Self::record_spawn_refused`].
    pub fn run_due(&self, now_ms: u64, reason: DueReason) -> Vec<Schedule> {
        let mut launches = Vec::new();
        {
            let mut map = self
                .schedules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for schedule in map.values_mut() {
                if !schedule.trigger_due(now_ms) {
                    continue;
                }
                if schedule.in_flight() {
                    // Overlap is only reachable for intervals (a Once with a
                    // last_run stamp is never trigger-due again).
                    Self::push_run(
                        schedule,
                        ScheduleRun {
                            started_ms: now_ms,
                            finished_ms: Some(now_ms),
                            outcome: RunOutcome::SkippedOverlap,
                            reason,
                        },
                    );
                    continue;
                }
                if let ScheduleTrigger::Once { at_ms } = schedule.trigger {
                    if schedule.catch_up == CatchUp::None && now_ms > at_ms {
                        // Missed and not wanted: consume it with an honest
                        // history entry instead of silently dropping it.
                        schedule.last_run_ms = Some(now_ms);
                        Self::push_run(
                            schedule,
                            ScheduleRun {
                                started_ms: at_ms,
                                finished_ms: Some(now_ms),
                                outcome: RunOutcome::SkippedCatchUp,
                                reason,
                            },
                        );
                        continue;
                    }
                }
                // Reserve the launch slot; the Started history entry is
                // appended by `record_launched`.
                schedule.last_run_ms = Some(now_ms);
                launches.push(schedule.clone());
            }
        }
        if !launches.is_empty() {
            self.persist();
        }
        launches
    }

    /// Append the `Started` history entry for a reserved launch.
    pub fn record_launched(&self, id: u64, started_ms: u64, reason: DueReason) {
        self.mutate(id, |s| {
            Self::push_run(
                s,
                ScheduleRun {
                    started_ms,
                    finished_ms: None,
                    outcome: RunOutcome::Started,
                    reason,
                },
            );
        })
        .ok();
    }

    /// The spawn itself was refused (e.g. backlog full): record the failure
    /// as a finished run so the reservation is explained. `last_run_ms` stays
    /// stamped, so an interval retries after its period, not every tick.
    pub fn record_spawn_refused(&self, id: u64, error: &str) {
        self.mutate(id, |s| {
            let started_ms = s.last_run_ms.unwrap_or_else(now_ms);
            Self::push_run(
                s,
                ScheduleRun {
                    started_ms,
                    finished_ms: Some(started_ms),
                    outcome: RunOutcome::Failed {
                        error: error.to_string(),
                    },
                    reason: DueReason::Timer,
                },
            );
        })
        .ok();
    }

    /// Close the matching unfinished `Started` entry with a terminal outcome.
    pub fn record_finished(&self, id: u64, started_ms: u64, outcome: RunOutcome, finished_ms: u64) {
        self.mutate(id, |s| {
            if let Some(run) = s
                .history
                .iter_mut()
                .find(|run| run.started_ms == started_ms && run.finished_ms.is_none())
            {
                run.finished_ms = Some(finished_ms);
                run.outcome = outcome;
            }
        })
        .ok();
    }

    fn mutate(&self, id: u64, f: impl FnOnce(&mut Schedule)) -> Result<Schedule, ScheduleError> {
        let snapshot = {
            let mut map = self
                .schedules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let schedule = map.get_mut(&id).ok_or(ScheduleError::NotFound(id))?;
            f(schedule);
            schedule.clone()
        };
        self.persist();
        Ok(snapshot)
    }

    fn push_run(schedule: &mut Schedule, run: ScheduleRun) {
        schedule.history.push(run);
        let keep_from = schedule.history.len().saturating_sub(MAX_HISTORY);
        schedule.history.drain(..keep_from);
    }

    /// Whole-store atomic write (temp + rename) under the cross-process
    /// sidecar lock, size-capped, failures counted — the in-memory map is
    /// the source of truth. A lock timeout is counted like a write failure:
    /// skipping the write is the honest degradation, writing unguarded is
    /// not an option.
    fn persist(&self) {
        let Some(root) = schedules_root() else { return };
        if std::fs::create_dir_all(&root).is_err() {
            DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let map = self
            .schedules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = StoreFile {
            next_id: self.next_id.load(Ordering::Relaxed),
            schedules: map.values().cloned().collect(),
        };
        drop(map);
        let Ok(json) = serde_json::to_string_pretty(&file) else {
            DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if json.len() > SCHEDULES_FILE_LIMIT {
            DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let final_path = root.join("schedules.json");
        let tmp_path = root.join("schedules.json.tmp");
        let write_ok = crate::filelock::with_exclusive(
            &crate::filelock::lock_path(&final_path),
            crate::filelock::DEFAULT_LOCK_TIMEOUT,
            || {
                std::fs::write(&tmp_path, json.as_bytes()).and_then(|()| {
                    let _ = std::fs::remove_file(&final_path);
                    std::fs::rename(&tmp_path, &final_path)
                })
            },
        );
        match write_ok {
            Ok(Ok(())) => {}
            // Either the lock timed out (closure never ran) or the write
            // itself failed inside the lock; clean the tmp either way.
            Ok(Err(_)) | Err(_) => {
                let _ = std::fs::remove_file(&tmp_path);
                DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

use crate::epoch_ms as now_ms;

/// Parse a duration spec for the CLI: `90` / `90s` / `30m` / `2h` / `1d`,
/// or a relative `+30m` for `Once`. Returns seconds.
pub fn parse_duration_spec(raw: &str) -> Result<u64, ScheduleError> {
    let raw = raw.trim().trim_start_matches('+');
    let (digits, unit) = raw.split_at(raw.len().saturating_sub(1));
    let multiplier: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ if raw.chars().all(|c| c.is_ascii_digit()) => {
            return raw
                .parse::<u64>()
                .map_err(|_| ScheduleError::BadSpec(raw.to_string()));
        }
        _ => return Err(ScheduleError::BadSpec(raw.to_string())),
    };
    digits
        .parse::<u64>()
        .map(|n| n.saturating_mul(multiplier))
        .map_err(|_| ScheduleError::BadSpec(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_schedules_root` is a process global; tests that persist serialize
    /// on this lock and reset the root before releasing (mission.rs pattern).
    static ROOT_LOCK: Mutex<()> = Mutex::new(());

    fn root_guard() -> std::sync::MutexGuard<'static, ()> {
        ROOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hf-sched-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn interval_store(every_secs: u64) -> (ScheduleStore, u64) {
        let store = ScheduleStore::new();
        let made = store
            .add(
                "tick",
                "say hi",
                ScheduleTrigger::Interval { every_secs },
                CatchUp::None,
                false,
            )
            .expect("add");
        (store, made.id)
    }

    /// Boundary arithmetic at the edges of u64 (chaos/limit discipline): a
    /// zero-period interval must be permanently not-due (otherwise "every 0
    /// seconds" would flood every tick forever), and epoch-zero / u64::MAX
    /// timestamps must saturate instead of overflowing.
    #[test]
    fn trigger_arithmetic_saturates_at_u64_boundaries() {
        let _guard = root_guard();
        // every_secs = 0: never fires, forever.
        let (store, id) = interval_store(0);
        assert!(
            !store.get(id).expect("present").trigger_due(u64::MAX),
            "zero-period interval must never fire"
        );
        // Once at the epoch: due from now = 0.
        let store = ScheduleStore::new();
        let made = store
            .add(
                "epoch",
                "p",
                ScheduleTrigger::Once { at_ms: 0 },
                CatchUp::None,
                false,
            )
            .expect("add");
        assert!(store.get(made.id).expect("present").trigger_due(0));
        // Once at u64::MAX: fires exactly at the boundary, never before.
        let store = ScheduleStore::new();
        let made = store
            .add(
                "far",
                "p",
                ScheduleTrigger::Once { at_ms: u64::MAX },
                CatchUp::None,
                false,
            )
            .expect("add");
        assert!(!store
            .get(made.id)
            .expect("present")
            .trigger_due(u64::MAX - 1));
        assert!(store.get(made.id).expect("present").trigger_due(u64::MAX));
        // A huge interval books once at u64::MAX and the saturating
        // subtraction keeps it honest afterwards (no overflow, no refire).
        let (store, id) = interval_store(u64::MAX / 1000 - 1);
        assert_eq!(
            store.run_due(u64::MAX, DueReason::Timer).len(),
            1,
            "first tick at the boundary fires"
        );
        assert!(
            store.run_due(u64::MAX, DueReason::Timer).is_empty(),
            "saturating arithmetic must not refire a huge period"
        );
    }

    #[test]
    fn interval_fires_once_per_period_and_records_history() {
        let _guard = root_guard();        let (store, id) = interval_store(60);
        // Nothing due at t=0 (first tick fires immediately: last_run is None).
        let due = store.run_due(0, DueReason::Timer);
        assert_eq!(due.len(), 1);
        store.record_launched(id, 0, DueReason::Timer);
        // Well inside the period: not due, no refire even with a "missed" feel.
        assert!(store.run_due(30_000, DueReason::Timer).is_empty());
        assert!(
            store.get(id).expect("exists").in_flight(),
            "Started entry open"
        );
        // Overlap: still in flight at the next period boundary → skipped and
        // the skip is recorded, not swallowed.
        let due = store.run_due(61_000, DueReason::Timer);
        assert!(due.is_empty());
        let schedule = store.get(id).expect("exists");
        assert!(matches!(
            schedule.history.last().expect("skip recorded").outcome,
            RunOutcome::SkippedOverlap
        ));
        // Finish, then the next period fires again.
        store.record_finished(id, 0, RunOutcome::Complete, 62_000);
        assert_eq!(store.run_due(121_000, DueReason::Timer).len(), 1);
    }

    #[test]
    fn once_respects_catch_up_none_and_latest() {
        let _guard = root_guard();
        let store = ScheduleStore::new();
        let none_id = store
            .add(
                "stale",
                "p",
                ScheduleTrigger::Once { at_ms: 1_000 },
                CatchUp::None,
                false,
            )
            .expect("add")
            .id;
        let latest_id = store
            .add(
                "fresh",
                "p",
                ScheduleTrigger::Once { at_ms: 1_000 },
                CatchUp::Latest,
                false,
            )
            .expect("add")
            .id;
        // Not due before the moment.
        assert!(store.run_due(500, DueReason::Timer).is_empty());
        // After: catch_up None consumes with an honest skip; Latest launches.
        let due = store.run_due(2_000, DueReason::RunDue);
        assert_eq!(due.len(), 1, "only the Latest schedule launches");
        assert_eq!(due[0].id, latest_id);
        let skipped = store.get(none_id).expect("exists");
        assert!(matches!(
            skipped.history.last().expect("skip recorded").outcome,
            RunOutcome::SkippedCatchUp
        ));
        // Both are consumed: nothing refires.
        assert!(store.run_due(9_000, DueReason::Timer).is_empty());
    }

    #[test]
    fn spawn_refusal_records_failure_and_prevents_tick_refire() {
        let _guard = root_guard();
        let (store, id) = interval_store(60);
        assert_eq!(store.run_due(0, DueReason::Timer).len(), 1, "reserved");
        store.record_spawn_refused(id, "backlog full");
        // The same tick must not hand the schedule out again; the retry waits
        // for the next period.
        assert!(store.run_due(1_000, DueReason::Timer).is_empty());
        assert!(matches!(
            store
                .get(id)
                .expect("exists")
                .history
                .last()
                .expect("failure recorded")
                .outcome,
            RunOutcome::Failed { .. }
        ));
    }

    #[test]
    fn reservation_survives_a_missing_record_launched() {
        let _guard = root_guard();
        let (store, _id) = interval_store(60);
        assert_eq!(store.run_due(0, DueReason::Timer).len(), 1);
        // Caller crashed before record_launched: the last_run stamp still
        // prevents an every-tick refire.
        assert!(store.run_due(10_000, DueReason::Timer).is_empty());
        assert_eq!(store.run_due(60_000, DueReason::Timer).len(), 1);
    }

    #[test]
    fn pause_resume_delete_roundtrip() {
        let _guard = root_guard();
        let (store, id) = interval_store(60);
        store.pause(id).expect("pause");
        assert!(!store.get(id).expect("exists").is_due(999_999));
        assert!(store.run_due(999_999, DueReason::Timer).is_empty());
        store.resume(id).expect("resume");
        assert_eq!(store.run_due(999_999, DueReason::Timer).len(), 1);
        store.delete(id).expect("delete");
        assert!(matches!(store.pause(id), Err(ScheduleError::NotFound(_))));
    }

    #[test]
    fn history_is_bounded() {
        let _guard = root_guard();
        let (store, id) = interval_store(1);
        for tick in 0_u64..(MAX_HISTORY as u64 + 20) {
            let now = tick * 2_000;
            if !store.run_due(now, DueReason::Timer).is_empty() {
                store.record_launched(id, now, DueReason::Timer);
                store.record_finished(id, now, RunOutcome::Complete, now);
            }
        }
        assert_eq!(store.get(id).expect("exists").history.len(), MAX_HISTORY);
    }

    #[test]
    fn persistence_roundtrip_and_corruption_flag() {
        let _guard = root_guard();
        let root = temp_root("roundtrip");
        set_schedules_root(Some(root.clone()));
        let store = ScheduleStore::new();
        store
            .add(
                "kept",
                "prompt",
                ScheduleTrigger::Interval { every_secs: 5 },
                CatchUp::Latest,
                true,
            )
            .expect("add");
        drop(store);

        let reloaded = ScheduleStore::load(&root);
        assert!(!reloaded.corrupted_on_load());
        assert_eq!(reloaded.list().len(), 1);
        let kept = &reloaded.list()[0];
        assert_eq!(kept.name, "kept");
        assert!(kept.quiet);

        // Corrupt the file; the next load flags it instead of crashing.
        std::fs::write(root.join("schedules.json"), "{not json").expect("corrupt");
        let broken = ScheduleStore::load(&root);
        assert!(broken.corrupted_on_load());
        assert!(broken.list().is_empty());

        set_schedules_root(None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_duration_spec_units() {
        assert_eq!(parse_duration_spec("90").expect("plain"), 90);
        assert_eq!(parse_duration_spec("90s").expect("secs"), 90);
        assert_eq!(parse_duration_spec("30m").expect("mins"), 1_800);
        assert_eq!(parse_duration_spec("+2h").expect("plus-hours"), 7_200);
        assert_eq!(parse_duration_spec("1d").expect("days"), 86_400);
        assert!(matches!(
            parse_duration_spec("abc"),
            Err(ScheduleError::BadSpec(_))
        ));
        assert!(matches!(
            parse_duration_spec("1x"),
            Err(ScheduleError::BadSpec(_))
        ));
    }
}
