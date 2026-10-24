//! Mission ledger: the roadmap §1/§4 durable task unit the supervisor owns
//! ("监工全权负责项目的耐久任务单元"), fusing pi-subagents' mission contract
//! (durable state + receipts) with kimi-code's goal semantics (source-verified
//! in `.workbuddy/kimi-code/packages/agent-core-v2/src/features/goal/`).
//!
//! Disciplines enforced here in code, not by prompt (D1.1-D1.3):
//! - **anti-early-complete**: `complete` passes only when *every* explicit
//!   criterion verifies. Mechanical criteria (`CommandSucceeds`, `FileMatches`)
//!   are re-verified at complete time; manual ones (`HumanApproved`,
//!   `NoDiagnostics`) require an explicit `approve_criterion` receipt. Weak
//!   evidence alone (`FreeText`, or a mission with zero criteria) never
//!   completes — "弱/间接证据不算完成" is a type error here, not a hope.
//! - **anti-early-give-up**: `mark_blocked` only flips the status to
//!   `Blocked` on the third *consecutive* claim; any productive
//!   [`MissionLedger::record_turn`] resets the streak ("同一阻塞连续 ≥3 个
//!   mission 回合" — one setback is not blocked).
//! - **budget, not kill**: any axis at ≥90% emits a needs-attention reminder
//!   once the mission is over the line; an exhausted axis sets a flag and
//!   *stops reminding* but never terminates the mission (pi-subagents
//!   missions.md:42-64 — big projects outlive a single session's budget).
//!
//! Storage: one `state.json` per mission under the missions root, written
//! atomically (temp + rename) with a 256 KiB cap (pi missions.md:24) and a
//! bounded receipt list; receipts also append to an `events.jsonl` stream.
//! Mirroring `jobs.rs`: the in-memory map is the source of truth, disk is
//! best-effort, failures are counted for `hf doctor`. Cross-process file
//! locking is deliberately NOT here yet (single-process MVP, honest gap).

use std::collections::BTreeMap;
use std::io::Write as _;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

/// Per-mission state file cap (pi-subagents `missions.md:24`). A mission that
/// cannot fit is a modeling bug (receipts are bounded), so persisting refuses
/// rather than truncating.
pub const MISSION_STATE_LIMIT: usize = 256 * 1024;
/// Receipts kept inside `state.json`; older ones drop off (the `events.jsonl`
/// stream remains complete).
pub const RECEIPT_KEEP: usize = 50;
/// Consecutive blocked claims required before the status flips (D1.1).
pub const BLOCKED_STREAK_NEEDED: u32 = 3;
/// Budget fraction that triggers a needs-attention reminder (D1.2).
pub const BUDGET_ATTENTION_FRACTION: f64 = 0.90;
/// Timeout for a `CommandSucceeds` verification run.
pub const CRITERION_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Mission lifecycle (kimi `GoalStatus`, four states).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MissionStatus {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl MissionStatus {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MissionStatus::Active => "active",
            MissionStatus::Paused => "paused",
            MissionStatus::Blocked => "blocked",
            MissionStatus::Complete => "complete",
        }
    }
}

/// One explicit completion criterion (roadmap §1 设计规格). Mechanical kinds
/// re-verify at `complete` time; manual kinds need an approval receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompletionCriterion {
    /// The command exits 0 (re-verified at complete time).
    CommandSucceeds { cmd: String },
    /// The file exists and matches the regex (re-verified at complete time).
    FileMatches { path: String, pattern: String },
    /// LSP/compiler clean — manual in this MVP (rust-analyzer integration is
    /// deliberately out; an explicit approval receipt stands in honestly).
    NoDiagnostics { scope: String },
    /// A human or supervisor explicitly approved.
    HumanApproved,
    /// The model's own claim. Weak evidence: never completes a mission alone.
    FreeText { check: String },
}

impl CompletionCriterion {
    /// Mechanical kinds re-verify themselves; manual kinds need a receipt.
    #[must_use]
    pub fn is_mechanical(&self) -> bool {
        matches!(
            self,
            CompletionCriterion::CommandSucceeds { .. } | CompletionCriterion::FileMatches { .. }
        )
    }

    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            CompletionCriterion::CommandSucceeds { cmd } => format!("command `{cmd}` exits 0"),
            CompletionCriterion::FileMatches { path, pattern } => {
                format!("`{path}` matches /{pattern}/")
            }
            CompletionCriterion::NoDiagnostics { scope } => format!("no diagnostics in {scope}"),
            CompletionCriterion::HumanApproved => String::from("human approval"),
            CompletionCriterion::FreeText { check } => format!("claimed: {check}"),
        }
    }
}

/// Per-criterion verification state, parallel to `criteria`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionState {
    Pending,
    Satisfied { evidence: String },
}

/// Three-axis budget (kimi `GoalBudgetLimits`); `None` = unlimited.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct MissionBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct MissionUsage {
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub turns: u64,
}

/// A durable, human-readable event appended to the mission (bounded view).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Receipt {
    pub ts_ms: u64,
    pub kind: String,
    pub text: String,
}

/// The durable task unit; also the on-disk `state.json` schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Mission {
    pub id: u64,
    pub objective: String,
    pub criteria: Vec<CompletionCriterion>,
    /// Parallel to `criteria`.
    pub states: Vec<CriterionState>,
    pub status: MissionStatus,
    #[serde(default)]
    pub budget: MissionBudget,
    #[serde(default)]
    pub used: MissionUsage,
    #[serde(default)]
    pub blocked_streak: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// Set when any budget axis is exhausted; reminders stop but the mission
    /// stays open (D1.2 "停提醒但不关").
    #[serde(default)]
    pub budget_exhausted: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub receipts: Vec<Receipt>,
}

impl Mission {
    /// Wall-clock used so far (derived, not stored).
    #[must_use]
    pub fn wall_ms(&self) -> u64 {
        self.updated_ms.saturating_sub(self.created_ms)
    }
}

/// Everything that can refuse a mission transition. Every variant names the
/// discipline that produced it — the caller can print it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionError {
    NotFound(u64),
    /// The transition needs an Active mission.
    NotActive {
        id: u64,
        status: MissionStatus,
    },
    /// Approve target is out of range, or is a mechanical criterion (those
    /// verify themselves — an approval receipt would be fake evidence).
    NotApprovable {
        id: u64,
        index: usize,
        why: &'static str,
    },
    /// Anti-early-complete: these criteria did not verify.
    Unsatisfied {
        id: u64,
        pending: Vec<String>,
    },
    /// Anti-early-complete: every satisfied criterion is FreeText (or the
    /// mission has no criteria at all) — weak evidence never completes.
    WeakOnly {
        id: u64,
    },
}

impl std::fmt::Display for MissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MissionError::NotFound(id) => write!(f, "no mission m{id}"),
            MissionError::NotActive { id, status } => {
                write!(f, "mission m{id} is {}, not active", status.label())
            }
            MissionError::NotApprovable { id, index, why } => {
                write!(f, "criterion {index} of m{id} is not approvable: {why}")
            }
            MissionError::Unsatisfied { id, pending } => {
                write!(f, "mission m{id} is not done yet, unverified: ")?;
                for (i, what) in pending.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{what}")?;
                }
                Ok(())
            }
            MissionError::WeakOnly { id } => write!(
                f,
                "mission m{id} has only weak (claimed) evidence - add at least one \
                 mechanical criterion or an explicit approval before completing"
            ),
        }
    }
}

static MISSIONS_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);
static DISK_WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Point the ledger at a directory (e.g. `~/.heartflow/sessions/missions`).
/// `None` disables disk persistence (in-memory only; tests).
pub fn set_missions_root(root: Option<PathBuf>) {
    if let Ok(mut slot) = MISSIONS_ROOT.write() {
        *slot = root;
    }
}

#[must_use]
pub fn mission_disk_write_failures() -> u64 {
    DISK_WRITE_FAILURES.load(Ordering::Relaxed)
}

fn missions_root() -> Option<PathBuf> {
    MISSIONS_ROOT.read().ok().and_then(|slot| slot.clone())
}

use crate::epoch_ms as now_ms;

/// In-process ledger. `Mutex` over the map is the single writer (roadmap §4
/// ds4 single-writer discipline at mission granularity).
#[derive(Debug, Default)]
pub struct MissionLedger {
    missions: Mutex<BTreeMap<u64, Mission>>,
    next_id: AtomicU64,
    /// state.json files that existed but could not be parsed at `load`.
    corrupted_on_load: Mutex<Vec<u64>>,
}

impl MissionLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reload from disk (REPL startup / after compaction — "恢复优先于新动作").
    /// Corrupted files are skipped and counted, never crash the session.
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let ledger = Self::new();
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => return ledger,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name
                .strip_prefix("mission-")
                .and_then(|rest| rest.parse::<u64>().ok())
            else {
                continue;
            };
            let state_path = entry.path().join("state.json");
            // A non-UTF-8 file is corrupt, not absent — count it in the
            // ledger so the operator can find and fix it (a silent skip here
            // once made a broken state file permanently invisible).
            let raw = match std::fs::read(&state_path) {
                Ok(raw) => match String::from_utf8(raw) {
                    Ok(raw) => raw,
                    Err(_) => {
                        ledger
                            .corrupted_on_load
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(id);
                        continue;
                    }
                },
                Err(_) => continue,
            };
            match serde_json::from_str::<Mission>(&raw) {
                Ok(mission) => {
                    ledger.next_id.fetch_max(mission.id + 1, Ordering::Relaxed);
                    ledger
                        .missions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(mission.id, mission);
                }
                Err(_) => {
                    ledger
                        .corrupted_on_load
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(id);
                }
            }
        }
        ledger
    }

    /// Ids whose state.json existed but failed to parse at load.
    #[must_use]
    pub fn corrupted_on_load(&self) -> Vec<u64> {
        self.corrupted_on_load
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Create a mission. Zero criteria is allowed (the human-gated shape) but
    /// `complete` will refuse it until at least one criterion exists.
    pub fn create(
        &self,
        objective: &str,
        criteria: Vec<CompletionCriterion>,
        budget: MissionBudget,
    ) -> Mission {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let ts = now_ms();
        let states = criteria.iter().map(|_| CriterionState::Pending).collect();
        let mission = Mission {
            id,
            objective: objective.trim().to_string(),
            criteria,
            states,
            status: MissionStatus::Active,
            budget,
            used: MissionUsage::default(),
            blocked_streak: 0,
            blocked_reason: None,
            budget_exhausted: false,
            created_ms: ts,
            updated_ms: ts,
            receipts: vec![Receipt {
                ts_ms: ts,
                kind: String::from("created"),
                text: String::from("mission opened"),
            }],
        };
        self.missions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, mission.clone());
        self.record_receipt(
            mission.id,
            "created",
            &format!("objective: {}", mission.objective),
        );
        mission
    }

    /// All missions, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<Mission> {
        self.missions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn get(&self, id: u64) -> Option<Mission> {
        self.missions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Pause (Active or Blocked → Paused).
    ///
    /// # Errors
    /// Fails with [`MissionError::NotFound`] for an unknown id, or
    /// [`MissionError::NotActive`] when the mission is neither Active nor
    /// Blocked.
    pub fn pause(&self, id: u64) -> Result<Mission, MissionError> {
        self.transition(id, |m| {
            if !matches!(m.status, MissionStatus::Active | MissionStatus::Blocked) {
                return Err(MissionError::NotActive {
                    id,
                    status: m.status,
                });
            }
            m.status = MissionStatus::Paused;
            Ok(())
        })
    }

    /// Resume (Paused or Blocked → Active); a blocked streak does not survive
    /// a resume — the intervention resets the give-up counter.
    ///
    /// # Errors
    /// Fails with [`MissionError::NotFound`] for an unknown id, or
    /// [`MissionError::NotActive`] when the mission is neither Paused nor
    /// Blocked.
    pub fn resume(&self, id: u64) -> Result<Mission, MissionError> {
        self.transition(id, |m| {
            if !matches!(m.status, MissionStatus::Paused | MissionStatus::Blocked) {
                return Err(MissionError::NotActive {
                    id,
                    status: m.status,
                });
            }
            m.status = MissionStatus::Active;
            m.blocked_streak = 0;
            m.blocked_reason = None;
            Ok(())
        })
    }

    /// Account one productive turn. Resets the blocked streak (productive work
    /// contradicts "same blocker"), advances usage, and returns the
    /// needs-attention reminder when a budget axis crosses 90% — including the
    /// turn that exhausts it (the supervisor must hear "wrap up" once); after
    /// that, reminders stop but the mission stays open (D1.2 "停提醒但不关").
    ///
    /// # Errors
    /// Fails with [`MissionError::NotFound`] for an unknown id, or
    /// [`MissionError::NotActive`] when the mission is not Active.
    pub fn record_turn(
        &self,
        id: u64,
        tokens_used: u64,
    ) -> Result<(Mission, Option<String>), MissionError> {
        let was_exhausted = self.get(id).is_some_and(|m| m.budget_exhausted);
        let mission = self.transition(id, |m| {
            if m.status != MissionStatus::Active {
                return Err(MissionError::NotActive {
                    id,
                    status: m.status,
                });
            }
            m.used.turns = m.used.turns.saturating_add(1);
            m.used.tokens = m.used.tokens.saturating_add(tokens_used);
            m.blocked_streak = 0;
            m.blocked_reason = None;
            if self.budget_axes_exhausted(m) {
                m.budget_exhausted = true;
            }
            Ok(())
        })?;
        let attention = if mission.budget_exhausted && was_exhausted {
            None
        } else {
            budget_attention(&mission)
        };
        self.record_receipt(
            mission.id,
            "turn",
            &format!("turn {} (+{tokens_used} tokens)", mission.used.turns),
        );
        Ok((mission, attention))
    }

    /// Claim the mission is blocked. Only the third *consecutive* claim flips
    /// the status (D1.1 anti-early-give-up); earlier claims are receipts.
    ///
    /// # Errors
    /// Fails with [`MissionError::NotFound`] for an unknown id, or
    /// [`MissionError::NotActive`] when the mission is not Active. Earlier
    /// claims (below the streak) succeed and return `flipped: false`.
    pub fn mark_blocked(&self, id: u64, reason: &str) -> Result<(Mission, bool), MissionError> {
        let mut flipped = false;
        let mission = self.transition(id, |m| {
            if m.status != MissionStatus::Active {
                return Err(MissionError::NotActive {
                    id,
                    status: m.status,
                });
            }
            m.blocked_streak = m.blocked_streak.saturating_add(1);
            m.blocked_reason = Some(reason.trim().to_string());
            if m.blocked_streak >= BLOCKED_STREAK_NEEDED {
                m.status = MissionStatus::Blocked;
                flipped = true;
            }
            Ok(())
        })?;
        self.record_receipt(
            mission.id,
            if flipped { "blocked" } else { "blocked-claim" },
            &format!(
                "claim {}/{}: {}",
                mission.blocked_streak, BLOCKED_STREAK_NEEDED, reason
            ),
        );
        Ok((mission, flipped))
    }

    /// Record explicit evidence for a *manual* criterion. Mechanical criteria
    /// refuse approvals — they re-verify at complete time, and an approval
    /// receipt for a self-verifying criterion would be fake evidence.
    ///
    /// # Errors
    /// Fails with [`MissionError::NotFound`] for an unknown id,
    /// [`MissionError::NotApprovable`] when the index is out of range or the
    /// criterion is mechanical, or [`MissionError::NotActive`] when the
    /// mission is not Active.
    pub fn approve_criterion(
        &self,
        id: u64,
        index: usize,
        evidence: &str,
    ) -> Result<Mission, MissionError> {
        let mission = self.transition(id, |m| {
            let Some(criterion) = m.criteria.get(index) else {
                return Err(MissionError::NotApprovable {
                    id,
                    index,
                    why: "no such criterion",
                });
            };
            if criterion.is_mechanical() {
                return Err(MissionError::NotApprovable {
                    id,
                    index,
                    why: "mechanical criteria verify themselves at complete time",
                });
            }
            if m.status != MissionStatus::Active {
                return Err(MissionError::NotActive {
                    id,
                    status: m.status,
                });
            }
            m.states[index] = CriterionState::Satisfied {
                evidence: evidence.trim().to_string(),
            };
            Ok(())
        })?;
        self.record_receipt(
            mission.id,
            "approved",
            &format!("criterion {index}: {}", mission.criteria[index].describe()),
        );
        Ok(mission)
    }

    /// Add a criterion to an Active mission (CLI authoring path). Returns the
    /// new criterion's index.
    ///
    /// # Errors
    /// Fails with [`MissionError::NotFound`] for an unknown id, or
    /// [`MissionError::NotActive`] when the mission is not Active.
    pub fn add_criterion(
        &self,
        id: u64,
        criterion: CompletionCriterion,
    ) -> Result<(Mission, usize), MissionError> {
        let mission = self.transition(id, |m| {
            if m.status != MissionStatus::Active {
                return Err(MissionError::NotActive {
                    id,
                    status: m.status,
                });
            }
            m.criteria.push(criterion);
            m.states.push(CriterionState::Pending);
            Ok(())
        })?;
        let index = mission.criteria.len() - 1;
        self.record_receipt(
            mission.id,
            "criterion",
            &format!("added [{index}] {}", mission.criteria[index].describe()),
        );
        Ok((mission, index))
    }

    /// Complete the mission — the anti-early-complete gate. Mechanical
    /// criteria re-verify *now*; manual criteria must already carry an
    /// approval; weak-only missions (or criteria-less ones) refuse.
    ///
    /// # Errors
    /// Fails with [`MissionError::NotFound`] for an unknown id,
    /// [`MissionError::NotActive`] when the mission is not Active,
    /// [`MissionError::WeakOnly`] when no strong criterion exists, or
    /// [`MissionError::Unsatisfied`] listing the criteria that failed
    /// re-verification.
    pub fn complete(&self, id: u64) -> Result<Mission, MissionError> {
        let mission = self.transition(id, |m| {
            if m.status != MissionStatus::Active {
                return Err(MissionError::NotActive {
                    id,
                    status: m.status,
                });
            }
            // Weak-only gate first so a criteria-less or claimed-only mission
            // gets the honest refusal even if nothing else is pending. Strong
            // evidence = a mechanical criterion exists (it re-verifies below)
            // or a manual one already carries its approval receipt. FreeText
            // is the ONLY weak kind and never completes a mission alone.
            let any_strong = m.criteria.iter().zip(&m.states).any(|(c, s)| {
                c.is_mechanical()
                    || matches!(
                        (c, s),
                        (
                            CompletionCriterion::HumanApproved
                                | CompletionCriterion::NoDiagnostics { .. },
                            CriterionState::Satisfied { .. }
                        )
                    )
            });
            if m.criteria.is_empty() || !any_strong {
                return Err(MissionError::WeakOnly { id });
            }
            // Re-verify everything now.
            let mut pending = Vec::new();
            for (index, criterion) in m.criteria.iter().enumerate() {
                let satisfied = match &m.states[index] {
                    CriterionState::Satisfied { .. } => true,
                    CriterionState::Pending => match criterion {
                        CompletionCriterion::CommandSucceeds { cmd } => {
                            command_succeeds(cmd, CRITERION_COMMAND_TIMEOUT)
                        }
                        CompletionCriterion::FileMatches { path, pattern } => {
                            file_matches(path, pattern)
                        }
                        CompletionCriterion::NoDiagnostics { .. }
                        | CompletionCriterion::HumanApproved
                        | CompletionCriterion::FreeText { .. } => false,
                    },
                };
                if !satisfied {
                    pending.push(criterion.describe());
                }
            }
            if !pending.is_empty() {
                return Err(MissionError::Unsatisfied { id, pending });
            }
            m.status = MissionStatus::Complete;
            m.updated_ms = now_ms();
            Ok(())
        })?;
        self.record_receipt(mission.id, "completed", "all criteria verified");
        Ok(mission)
    }

    fn transition(
        &self,
        id: u64,
        f: impl FnOnce(&mut Mission) -> Result<(), MissionError>,
    ) -> Result<Mission, MissionError> {
        let mut map = self
            .missions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mission = map.get_mut(&id).ok_or(MissionError::NotFound(id))?;
        f(mission)?;
        mission.updated_ms = now_ms();
        let snapshot = mission.clone();
        drop(map);
        self.persist(&snapshot);
        Ok(snapshot)
    }

    fn budget_axes_exhausted(&self, m: &Mission) -> bool {
        let used_frac = |used: u64, limit: Option<u64>| {
            limit.map_or(0.0, |limit| {
                if limit == 0 {
                    0.0
                } else {
                    used as f64 / limit as f64
                }
            })
        };
        let wall = m.wall_ms();
        used_frac(m.used.tokens, m.budget.tokens) >= 1.0
            || used_frac(m.used.turns, m.budget.turns) >= 1.0
            || used_frac(wall, m.budget.wall_ms) >= 1.0
    }

    fn record_receipt(&self, id: u64, kind: &str, text: &str) {
        let receipt = Receipt {
            ts_ms: now_ms(),
            kind: kind.to_string(),
            text: text.to_string(),
        };
        // Persist the map's state *after* the receipt lands in it — persisting
        // the caller's snapshot would leave state.json one receipt behind and
        // a terminal receipt (e.g. "completed") would never reach disk.
        let snapshot = {
            let mut map = self
                .missions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(m) = map.get_mut(&id) else { return };
            m.receipts.push(receipt);
            let keep_from = m.receipts.len().saturating_sub(RECEIPT_KEEP);
            m.receipts.drain(..keep_from);
            m.clone()
        };
        self.persist(&snapshot);
        self.append_event(id, kind, text);
    }

    /// Best-effort disk write: atomic temp+rename, capped, failures counted —
    /// the in-memory map is the source of truth (jobs.rs discipline).
    fn persist(&self, mission: &Mission) {
        let Some(root) = missions_root() else { return };
        let dir = root.join(format!("mission-{}", mission.id));
        if std::fs::create_dir_all(&dir).is_err() {
            DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Ok(json) = serde_json::to_string_pretty(mission) else {
            DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if json.len() > MISSION_STATE_LIMIT {
            // Refuse, don't truncate: a truncated state.json is worse than a
            // missing one (the loader would have to guess).
            DISK_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let final_path = dir.join("state.json");
        let tmp_path = dir.join("state.json.tmp");
        let write_ok = crate::filelock::with_exclusive(
            &crate::filelock::lock_path(&final_path),
            crate::filelock::DEFAULT_LOCK_TIMEOUT,
            || {
                std::fs::write(&tmp_path, json.as_bytes()).and_then(|()| {
                    // Windows rename fails onto an existing file; clear first.
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

    fn append_event(&self, id: u64, kind: &str, text: &str) {
        let Some(root) = missions_root() else { return };
        let path = root.join(format!("mission-{id}")).join("events.jsonl");
        let line = format!(
            "{{\"ts\":{},\"kind\":\"{kind}\",\"text\":{}}}\n",
            now_ms(),
            serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
        );
        // Append under the cross-process lock: two processes appending to
        // the same events.jsonl may otherwise interleave mid-line. A timed
        // out append is counted like any other disk failure — the ledger is
        // observability, the in-memory mission stays authoritative.
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
}

/// D1.2 reminder text for the first axis at ≥90% (None when under or when the
/// caller already knows the mission is exhausted).
#[must_use]
pub fn budget_attention(mission: &Mission) -> Option<String> {
    let axes: [(&str, f64, Option<u64>); 3] = [
        ("tokens", mission.used.tokens as f64, mission.budget.tokens),
        ("turns", mission.used.turns as f64, mission.budget.turns),
        (
            "wall clock",
            mission.wall_ms() as f64,
            mission.budget.wall_ms,
        ),
    ];
    for (name, used, limit) in axes {
        let Some(limit) = limit else { continue };
        if limit == 0 {
            continue;
        }
        let fraction = used / limit as f64;
        if fraction >= BUDGET_ATTENTION_FRACTION {
            return Some(format!(
                "mission m{} budget nearly exhausted on {name}: {:.0}% used ({used:.0}/{limit}) - wrap up the current slice or raise the budget",
                mission.id,
                fraction * 100.0
            ));
        }
    }
    None
}

/// Build the per-turn mission reminder (D1.3, kimi goal injection): the open
/// missions as one system-prompt section — status, criteria progress, budget
/// remaining and the `goal_continue` contract ("the previous turn did not
/// finish this mission; continue it"). Returns `None` when nothing is open.
/// Budget-exhausted missions are skipped entirely (D1.2: reminders stop, the
/// mission stays open); Paused missions get a one-line mention without the
/// continue nudge — pausing is a deliberate choice the ledger should honor.
#[must_use]
pub fn turn_reminder(missions: &[Mission]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for mission in missions {
        if mission.budget_exhausted || mission.status == MissionStatus::Complete {
            continue;
        }
        let satisfied = mission
            .states
            .iter()
            .filter(|state| matches!(state, CriterionState::Satisfied { .. }))
            .count();
        let total = mission.criteria.len();
        let budget = budget_summary(mission);
        match mission.status {
            MissionStatus::Active => {
                let mut line = format!(
                    "- m{} ACTIVE: {} — criteria {satisfied}/{total} satisfied — {budget}",
                    mission.id, mission.objective
                );
                if let Some(attention) = budget_attention(mission) {
                    line.push_str(&format!(" — {attention}"));
                }
                line.push_str(
                    " — goal_continue: this mission is not finished; keep working on it this turn",
                );
                lines.push(line);
            }
            MissionStatus::Blocked => {
                let reason = mission
                    .blocked_reason
                    .as_deref()
                    .unwrap_or("unspecified blocker");
                lines.push(format!(
                    "- m{} BLOCKED ({reason}): {} — criteria {satisfied}/{total} satisfied — {budget} — decide this turn: resolve the blocker, re-scope the mission, or pause it",
                    mission.id, mission.objective
                ));
            }
            MissionStatus::Paused => lines.push(format!(
                "- m{} PAUSED: {} — no continue nudge; resume explicitly when it is back on the agenda",
                mission.id, mission.objective
            )),
            MissionStatus::Complete => continue,
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = String::from(
        "Mission ledger (per-turn reminder). The runtime injected this section: unfinished missions continue by default, completion requires every criterion verified. Update the ledger via /mission rather than declaring success in prose.\n",
    );
    out.push_str(&lines.join("\n"));
    Some(out)
}

/// Compact used/limit readout over the three budget axes; unlimited axes
/// report usage only, so the line never hides that a budget exists.
fn budget_summary(mission: &Mission) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(match mission.budget.tokens {
        Some(limit) => format!("tokens {}/{}", mission.used.tokens, limit),
        None => format!("tokens {}", mission.used.tokens),
    });
    parts.push(match mission.budget.turns {
        Some(limit) => format!("turns {}/{}", mission.used.turns, limit),
        None => format!("turns {}", mission.used.turns),
    });
    parts.push(match mission.budget.wall_ms {
        Some(limit) => format!("wall {}m/{}m", mission.wall_ms() / 60_000, limit / 60_000),
        None => format!("wall {}m", mission.wall_ms() / 60_000),
    });
    parts.join(", ")
}

/// Auto-account one finished turn against the single active mission (§11.3
/// mission 回合自动记账; the REPL's manual `/mission record` stays available).
/// Attribution is only safe with exactly one active mission — with several,
/// whose budget a turn burned is a guess, so the ledger is left untouched.
/// Returns the needs-attention reminder, if any (the caller surfaces it).
pub fn auto_record_turn(root: &Path, tokens_used: u64) -> Option<String> {
    let ledger = MissionLedger::load(root);
    let active: Vec<u64> = ledger
        .list()
        .iter()
        .filter(|mission| mission.status == MissionStatus::Active)
        .map(|mission| mission.id)
        .collect();
    let [only] = active.as_slice() else {
        return None;
    };
    ledger
        .record_turn(*only, tokens_used)
        .ok()
        .and_then(|(_, attention)| attention)
}

/// Run `cmd` through the platform shell and report exit status 0, with a
/// poll-based timeout (diagnostics.rs pattern). Output is discarded: criteria
/// verify *that* the command succeeded, not what it printed.
#[must_use]
pub fn command_succeeds(cmd: &str, timeout: Duration) -> bool {
    let mut command = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
    if cfg!(windows) {
        command.args(["/C", cmd]);
        #[cfg(windows)]
        {
            // No console flash from a background verification.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
    } else {
        command.arg("-c").arg(cmd);
    }
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

/// `FileMatches` verification: the file must exist and its UTF-8-lossy text
/// must match the regex. A bad pattern counts as unverified (fail-closed) —
/// the refusal carries the criterion description so the model can fix it.
#[must_use]
pub fn file_matches(path: &str, pattern: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(re) = regex::Regex::new(pattern) else {
        return false;
    };
    re.is_match(&contents)
}

/// Disk-level summary for `hf doctor` (which runs before any ledger exists in
/// that process): counts by status straight from state.json files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MissionSummary {
    pub active: usize,
    pub paused: usize,
    pub blocked: usize,
    pub complete: usize,
    pub budget_exhausted: usize,
    pub corrupted: usize,
}

#[must_use]
pub fn summarize_on_disk(root: &Path) -> MissionSummary {
    let mut summary = MissionSummary::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return summary;
    };
    for entry in entries.flatten() {
        let state = entry.path().join("state.json");
        // Same rule as load(): non-UTF-8 is corrupt, not absent.
        let raw = match std::fs::read(&state) {
            Ok(raw) => match String::from_utf8(raw) {
                Ok(raw) => raw,
                Err(_) => {
                    summary.corrupted += 1;
                    continue;
                }
            },
            Err(_) => continue,
        };
        match serde_json::from_str::<Mission>(&raw) {
            Ok(mission) => match mission.status {
                MissionStatus::Active => summary.active += 1,
                MissionStatus::Paused => summary.paused += 1,
                MissionStatus::Blocked => summary.blocked += 1,
                MissionStatus::Complete => summary.complete += 1,
            },
            Err(_) => summary.corrupted += 1,
        }
        if matches!(
            serde_json::from_str::<Mission>(&raw),
            Ok(m) if m.budget_exhausted
        ) {
            summary.budget_exhausted += 1;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_missions_root` mutates a process-global; tests that persist (or
    /// merely trigger persistence) must serialize or one test's missions land
    /// in another test's temp root. Every test takes the lock for its whole
    /// body and resets the root to `None` before releasing.
    static ROOT_LOCK: Mutex<()> = Mutex::new(());

    fn root_guard() -> std::sync::MutexGuard<'static, ()> {
        ROOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hf-mission-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn drop_root(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn cmd_criterion(cmd: &str) -> CompletionCriterion {
        CompletionCriterion::CommandSucceeds {
            cmd: cmd.to_string(),
        }
    }

    fn free_criterion(text: &str) -> CompletionCriterion {
        CompletionCriterion::FreeText {
            check: text.to_string(),
        }
    }

    fn in_memory_mission(id: u64, objective: &str, status: MissionStatus) -> Mission {
        Mission {
            id,
            objective: objective.to_string(),
            criteria: Vec::new(),
            states: Vec::new(),
            status,
            budget: MissionBudget::default(),
            used: MissionUsage::default(),
            blocked_streak: 0,
            blocked_reason: None,
            budget_exhausted: false,
            created_ms: 0,
            updated_ms: 0,
            receipts: Vec::new(),
        }
    }

    #[test]
    fn turn_reminder_renders_each_open_state_and_skips_closed_work() {
        let mut active = in_memory_mission(1, "ship the queue UI", MissionStatus::Active);
        active.criteria = vec![
            free_criterion("feels done"),
            free_criterion("also done"),
            free_criterion("nope"),
        ];
        active.states = vec![
            CriterionState::Satisfied {
                evidence: "exit 0".to_string(),
            },
            CriterionState::Satisfied {
                evidence: "matched".to_string(),
            },
            CriterionState::Pending,
        ];
        active.budget.turns = Some(10);
        let paused = in_memory_mission(2, "parked idea", MissionStatus::Paused);
        let mut blocked = in_memory_mission(99, "unblock CI", MissionStatus::Blocked);
        blocked.blocked_reason = Some("api quota".to_string());
        let mut exhausted = in_memory_mission(100, "over budget", MissionStatus::Active);
        exhausted.budget_exhausted = true;
        let complete = in_memory_mission(101, "done deal", MissionStatus::Complete);

        let reminder =
            turn_reminder(&[active, paused, blocked, exhausted.clone(), complete.clone()])
                .expect("open work");
        assert!(reminder.contains("m1 ACTIVE: ship the queue UI"));
        assert!(reminder.contains("criteria 2/3"));
        assert!(reminder.contains("turns 0/10"));
        assert!(reminder.contains("goal_continue"));
        assert!(reminder.contains("m99 BLOCKED (api quota): unblock CI"));
        assert!(reminder.contains("m2 PAUSED: parked idea"));
        assert!(
            !reminder.contains("m100"),
            "budget-exhausted: reminders stop"
        );
        assert!(!reminder.contains("m101"), "complete missions are closed");

        assert_eq!(turn_reminder(&[]), None, "nothing open, no section");
        assert_eq!(
            turn_reminder(&[exhausted, complete]),
            None,
            "only closed work is also nothing to remind about"
        );
    }

    #[test]
    fn auto_record_turn_accounts_the_single_active_mission_only() {
        let _guard = root_guard();
        let root = temp_root("auto-record");
        set_missions_root(Some(root.clone()));
        let ledger = MissionLedger::new();
        ledger.create(
            "single active mission",
            Vec::new(),
            MissionBudget {
                turns: Some(2),
                ..MissionBudget::default()
            },
        );

        // Exactly one active mission: the turn is attributed and the budget
        // attention fires once the axis crosses 90%.
        assert!(
            auto_record_turn(&root, 5).is_none(),
            "1/2 turns is under 90%"
        );
        let after = auto_record_turn(&root, 5);
        assert_eq!(
            after,
            Some(
                "mission m1 budget nearly exhausted on turns: 100% used (2/2) - wrap up the current slice or raise the budget"
                    .to_string()
            )
        );
        // A third call keeps recording usage but stops reminding (D1.2).
        assert!(
            auto_record_turn(&root, 5).is_none(),
            "reminders stop after exhaustion"
        );
        assert_eq!(
            MissionLedger::load(&root)
                .get(1)
                .expect("still there")
                .used
                .turns,
            3,
            "the mission stays open and keeps accounting"
        );

        // Two active missions: attribution is a guess, so nothing is written.
        ledger.create(
            "second active mission",
            Vec::new(),
            MissionBudget::default(),
        );
        let snapshot = |missions: &[Mission]| -> Vec<(u64, u64)> {
            missions
                .iter()
                .map(|mission| (mission.id, mission.used.turns))
                .collect()
        };
        let before = snapshot(&MissionLedger::load(&root).list());
        assert!(auto_record_turn(&root, 100).is_none());
        let after_two = snapshot(&MissionLedger::load(&root).list());
        assert_eq!(before, after_two, "ambiguous attribution writes nothing");
        set_missions_root(None);
        drop_root(&root);
    }

    #[test]
    fn persistence_roundtrip_recovers_missions_and_next_id() {
        let _guard = root_guard();
        let root = temp_root("roundtrip");
        set_missions_root(Some(root.clone()));
        let ledger = MissionLedger::new();
        let made = ledger.create(
            "ship the parser",
            vec![cmd_criterion("exit 0")],
            MissionBudget::default(),
        );
        let second = ledger.create("second", vec![], MissionBudget::default());
        drop(ledger);

        let reloaded = MissionLedger::load(&root);
        assert_eq!(reloaded.list().len(), 2);
        assert_eq!(
            reloaded.get(made.id).map(|m| m.objective.clone()),
            Some("ship the parser".into())
        );
        // Next id continues after the max, so a reload never reissues ids.
        let third = reloaded.create("third", vec![], MissionBudget::default());
        assert!(third.id > second.id);
        drop_root(&root);
        set_missions_root(None);
    }

    /// Chaos: every flavor of on-disk corruption (garbage, truncation
    /// mid-write, wrong types, valid-JSON-wrong-shape) must be skipped and
    /// counted — never crash the session, never poison a healthy sibling,
    /// never regress `next_id` past surviving missions.
    #[test]
    fn corrupt_state_files_are_skipped_and_counted_not_crashes() {
        let _guard = root_guard();
        let root = temp_root("chaos");
        set_missions_root(Some(root.clone()));
        // A healthy mission survives alongside the broken ones.
        let ledger = MissionLedger::new();
        let healthy = ledger.create("healthy", vec![], MissionBudget::default());
        drop(ledger);
        // mission-11: plain garbage.
        let dir = root.join("mission-11");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("state.json"), b"\x00\xff not json at all").expect("garbage");
        // mission-12: truncated mid-write (power loss during temp+rename-less
        // writer; our own writer is atomic but the disk format must tolerate
        // what other tools did).
        let dir = root.join("mission-12");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("state.json"), r#"{"id":12,"objective":"tru"#).expect("truncated");
        // mission-13: wrong types where u64/string are expected.
        let dir = root.join("mission-13");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("state.json"),
            r#"{"id":"thirteen","objective":42,"status":[]}"#,
        )
        .expect("wrong types");
        // mission-14: valid JSON, wrong shape entirely.
        let dir = root.join("mission-14");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("state.json"), "[1,2,3]").expect("array not object");
        // Non-UTF-8 bytes in a directory name that parses as an id: skipped.
        // mission-15: state.json missing entirely.
        std::fs::create_dir_all(root.join("mission-15")).expect("dir");

        let reloaded = MissionLedger::load(&root);
        let mut corrupted = reloaded.corrupted_on_load();
        corrupted.sort_unstable();
        assert_eq!(
            corrupted,
            vec![11, 12, 13, 14],
            "garbage/truncated/wrong-type/wrong-shape all counted"
        );
        assert!(
            reloaded.get(healthy.id).is_some(),
            "a healthy sibling must survive the chaos"
        );
        // next_id stays past the healthy mission regardless of corrupt ids.
        let next = reloaded.create("next", vec![], MissionBudget::default());
        assert!(next.id > healthy.id);
        // summarize_on_disk sees the corrupt files instead of lying.
        let summary = summarize_on_disk(&root);
        assert_eq!(summary.corrupted, 4);
        drop_root(&root);
        set_missions_root(None);
    }

    /// A runaway worker reporting `u64::MAX` tokens must saturate the
    /// accounting (never overflow-panic mid-session) and exhaust the budget
    /// honestly; repeat turns stay saturated at the boundary.
    #[test]
    fn budget_accounting_saturates_at_u64_max() {
        let _guard = root_guard();
        let root = temp_root("saturation");
        set_missions_root(Some(root.clone()));
        let ledger = MissionLedger::new();
        let made = ledger.create(
            "soak",
            vec![],
            MissionBudget {
                tokens: Some(1),
                turns: None,
                wall_ms: None,
            },
        );
        let (mission, _) = ledger
            .record_turn(made.id, u64::MAX)
            .expect("first turn ok");
        assert_eq!(mission.used.tokens, u64::MAX, "saturating, not wrapping");
        assert!(mission.budget_exhausted, "1-token budget is exhausted");
        let (mission, _) = ledger
            .record_turn(made.id, u64::MAX)
            .expect("later turns still accepted");
        assert_eq!(mission.used.tokens, u64::MAX, "stays at the boundary");
        drop_root(&root);
        set_missions_root(None);
    }

    #[test]
    fn complete_refuses_while_criteria_pending() {
        let _guard = root_guard();
        let ledger = MissionLedger::new();
        let made = ledger.create(
            "verify me",
            vec![cmd_criterion("exit 1"), CompletionCriterion::HumanApproved],
            MissionBudget::default(),
        );
        match ledger.complete(made.id) {
            Err(MissionError::Unsatisfied { pending, .. }) => {
                assert_eq!(pending.len(), 2, "both unverified criteria listed");
            }
            other => panic!("expected Unsatisfied, got {other:?}"),
        }
    }

    #[test]
    fn weak_only_evidence_never_completes() {
        let _guard = root_guard();
        let ledger = MissionLedger::new();
        // Case 1: a single FreeText criterion, approved — still refuses.
        let made = ledger.create(
            "claimed done",
            vec![free_criterion("i think it works")],
            MissionBudget::default(),
        );
        ledger
            .approve_criterion(made.id, 0, "model asserts success")
            .expect("approve free");
        assert!(matches!(
            ledger.complete(made.id),
            Err(MissionError::WeakOnly { .. })
        ));
        // Case 2: zero criteria — human-gated shape, also refuses.
        let bare = ledger.create("no criteria", vec![], MissionBudget::default());
        assert!(matches!(
            ledger.complete(bare.id),
            Err(MissionError::WeakOnly { .. })
        ));
    }

    #[test]
    fn mechanical_criteria_verify_at_complete_time() {
        let _guard = root_guard();
        let ledger = MissionLedger::new();
        let made = ledger.create(
            "mechanical",
            vec![cmd_criterion("exit 0"), CompletionCriterion::HumanApproved],
            MissionBudget::default(),
        );
        // The human-approved one needs its receipt first.
        assert!(matches!(
            ledger.complete(made.id),
            Err(MissionError::Unsatisfied { .. })
        ));
        ledger
            .approve_criterion(made.id, 1, "supervisor signed off")
            .expect("approve");
        let done = ledger
            .complete(made.id)
            .expect("completes with exit 0 + approval");
        assert_eq!(done.status, MissionStatus::Complete);
        // Completed missions refuse further transitions.
        assert!(matches!(
            ledger.pause(made.id),
            Err(MissionError::NotActive { .. })
        ));
    }

    #[test]
    fn file_matches_verifies_content() {
        let root = temp_root("filematch");
        let file = root.join("proof.txt");
        std::fs::write(&file, "all tests passed: 17/17").expect("seed file");
        assert!(file_matches(file.to_str().expect("utf8 path"), r"17/17"));
        assert!(!file_matches(file.to_str().expect("utf8 path"), r"18/18"));
        // Fail-closed: missing file and bad pattern both verify false.
        assert!(!file_matches(
            root.join("nope.txt").to_str().expect("utf8"),
            "x"
        ));
        assert!(
            !file_matches(file.to_str().expect("utf8"), "(["),
            "bad regex refuses"
        );
        drop_root(&root);
    }

    #[test]
    fn approvals_only_accept_manual_criteria() {
        let _guard = root_guard();
        let ledger = MissionLedger::new();
        let made = ledger.create(
            "gate",
            vec![cmd_criterion("exit 0")],
            MissionBudget::default(),
        );
        assert!(matches!(
            ledger.approve_criterion(made.id, 0, "bogus"),
            Err(MissionError::NotApprovable { why, .. }) if why.contains("mechanical")
        ));
        assert!(matches!(
            ledger.approve_criterion(made.id, 9, "bogus"),
            Err(MissionError::NotApprovable { .. })
        ));
    }

    #[test]
    fn blocked_needs_three_consecutive_claims_and_productive_turn_resets() {
        let _guard = root_guard();
        let ledger = MissionLedger::new();
        let made = ledger.create("stuck?", vec![], MissionBudget::default());
        let (_, flipped1) = ledger
            .mark_blocked(made.id, "build broken")
            .expect("claim 1");
        let (_, flipped2) = ledger
            .mark_blocked(made.id, "build broken")
            .expect("claim 2");
        assert!(!flipped1 && !flipped2, "two claims do not flip");
        assert_eq!(
            ledger.get(made.id).expect("exists").status,
            MissionStatus::Active
        );
        // A productive turn resets the streak: one turn of progress and the
        // counter starts over.
        ledger.record_turn(made.id, 100).expect("turn");
        let (_, flipped3) = ledger
            .mark_blocked(made.id, "still broken")
            .expect("claim 3 (fresh)");
        assert!(!flipped3, "streak was reset by the turn");
        ledger
            .mark_blocked(made.id, "still broken")
            .expect("claim 4");
        let (mission, flipped5) = ledger
            .mark_blocked(made.id, "still broken")
            .expect("claim 5");
        assert!(flipped5, "third consecutive claim flips");
        assert_eq!(mission.status, MissionStatus::Blocked);
        // Resume clears both the status and the streak.
        let resumed = ledger.resume(made.id).expect("resume");
        assert_eq!(resumed.status, MissionStatus::Active);
        assert_eq!(resumed.blocked_streak, 0);
    }

    #[test]
    fn budget_reminds_at_ninety_percent_then_goes_quiet_when_exhausted() {
        let _guard = root_guard();
        let ledger = MissionLedger::new();
        let made = ledger.create(
            "budgeted",
            vec![],
            MissionBudget {
                tokens: None,
                turns: Some(2),
                wall_ms: None,
            },
        );
        let (_, attention1) = ledger.record_turn(made.id, 10).expect("turn 1");
        assert!(attention1.is_none(), "1/2 turns is below 90%");
        let (_, attention2) = ledger.record_turn(made.id, 10).expect("turn 2");
        assert!(attention2.is_some(), "2/2 turns crosses the attention line");
        let mission = ledger.get(made.id).expect("exists");
        assert!(mission.budget_exhausted, "100% of an axis sets the flag");
        // 停提醒但不关: the mission stays Active and reminders stop.
        let (_, attention3) = ledger.record_turn(made.id, 10).expect("turn 3");
        assert!(attention3.is_none(), "exhausted missions stop reminding");
        assert_eq!(
            ledger.get(made.id).expect("exists").status,
            MissionStatus::Active
        );
    }

    #[test]
    fn paused_missions_refuse_turns_and_complete() {
        let _guard = root_guard();
        let ledger = MissionLedger::new();
        let made = ledger.create("parked", vec![], MissionBudget::default());
        ledger.pause(made.id).expect("pause");
        assert!(matches!(
            ledger.record_turn(made.id, 1),
            Err(MissionError::NotActive { .. })
        ));
        assert!(matches!(
            ledger.complete(made.id),
            Err(MissionError::NotActive { .. })
        ));
        ledger.resume(made.id).expect("resume");
        assert!(ledger.record_turn(made.id, 1).is_ok());
    }

    #[test]
    fn state_file_cap_refuses_to_write_oversized_missions() {
        let _guard = root_guard();
        let root = temp_root("cap");
        set_missions_root(Some(root.clone()));
        let ledger = MissionLedger::new();
        let bloated = "x".repeat(MISSION_STATE_LIMIT + 1024);
        let made = ledger.create(&bloated, vec![], MissionBudget::default());
        // In-memory source of truth still works...
        assert!(ledger.get(made.id).is_some());
        // ...but the disk write was refused and counted, not truncated.
        assert!(mission_disk_write_failures() > 0);
        assert!(!root
            .join(format!("mission-{}", made.id))
            .join("state.json")
            .exists());
        drop_root(&root);
        set_missions_root(None);
    }

    #[test]
    fn command_succeeds_distinguishes_exit_codes_and_timeouts() {
        assert!(command_succeeds("exit 0", Duration::from_secs(10)));
        assert!(!command_succeeds("exit 3", Duration::from_secs(10)));
        // A sleeping command is killed at the timeout, reported honestly false.
        let started = std::time::Instant::now();
        assert!(!command_succeeds(
            if cfg!(windows) {
                "ping -n 30 127.0.0.1"
            } else {
                "sleep 30"
            },
            Duration::from_millis(300)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout actually bounds the run"
        );
    }

    #[test]
    fn summarize_on_disk_counts_statuses() {
        let _guard = root_guard();
        let root = temp_root("summary");
        set_missions_root(Some(root.clone()));
        let ledger = MissionLedger::new();
        let active = ledger.create("active one", vec![], MissionBudget::default());
        let done = ledger.create(
            "done one",
            vec![cmd_criterion("exit 0")],
            MissionBudget::default(),
        );
        ledger.complete(done.id).expect("completes");
        ledger.pause(active.id).expect("pause");
        drop(ledger);
        let summary = summarize_on_disk(&root);
        assert_eq!(
            (summary.active, summary.paused, summary.complete),
            (0, 1, 1)
        );
        drop_root(&root);
        set_missions_root(None);
    }
}
