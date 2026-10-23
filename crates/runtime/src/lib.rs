mod agent_assets;
mod bash;
mod checkpoint;
mod compact;
mod config;
mod conversation;
mod diagnostics;
mod doc_search;
mod file_ops;
mod filelock;
mod jobs;
mod mission;
mod permissions;
mod prompt;
mod redact;
mod schedule;
mod schema;
mod session;
mod usage;

pub use agent_assets::{discover_rules, discover_skills, RuleFile, SkillSummary};
pub use bash::{execute_bash, is_dangerous_command, BashCommandInput, BashCommandOutput};
pub use checkpoint::{
    list_snapshots, purge_new_files_since, restore_snapshot, set_checkpoint_root,
    snapshot_failures, PurgeReport, Snapshot,
};
pub use compact::estimate_tokens_from;
pub use compact::{
    compact_session_in_place, compaction_stats, estimate_session_tokens, format_compact_summary,
    get_compact_continuation_message, is_context_overflow_error, plan_compaction_cut,
    set_compact_stats_path, should_compact, truncate_chars, CompactEvent, CompactionConfig,
    CompactionResult, CompactionStats, CutPlan, SoftTrigger, SummaryMode, ThresholdKind,
    TokenCalibration,
};
pub use config::{
    ConfigEntry, ConfigError, ConfigLoader, ConfigSource, RuntimeConfig,
    HEARTFLOW_SETTINGS_SCHEMA_NAME,
};
pub use conversation::{
    AgentEvent, ApiClient, ApiRequest, ConversationRuntime, RuntimeError, StaticToolExecutor,
    SteerMailbox, ToolError, ToolExecutor, ToolSpec, TurnStream, TurnSummary,
};
pub use diagnostics::{
    diagnostics_degradation_detail, diagnostics_ledger, run_post_edit_diagnostics, DiagnosticItem,
    DiagnosticsLedger, PostEditDiagnostics,
};
pub use doc_search::{rga_available, search_documents, DocSearchInput, DocSearchOutput};
pub use file_ops::{
    apply_patch, edit_file, escapes_workspace, glob_search, grep_search, read_file, search_files,
    write_file, ApplyPatchOutput, EditFileOutput, GlobSearchOutput, GrepSearchInput,
    GrepSearchOutput, PatchChange, PatchFileResult, ReadFileOutput, SearchFilesOutput,
    StructuredPatchHunk, TextFilePayload, WriteFileOutput,
};
pub use filelock::{lock_path, with_exclusive, LockError, DEFAULT_LOCK_TIMEOUT};
pub use jobs::{
    disk_write_failures, reconcile_stale_records, set_jobs_root, JobRecord, JobRegistry, JobRunner,
    JobStatus, MessageKind, SpawnError, StopFlag, WorkerMessage, DEFAULT_JOB_SLOTS,
    DEFAULT_JOB_WALL_TIMEOUT, DEFAULT_MAX_PENDING,
};
pub use mission::{
    auto_record_turn, budget_attention, command_succeeds, file_matches,
    mission_disk_write_failures, set_missions_root, summarize_on_disk, turn_reminder,
    CompletionCriterion, CriterionState, Mission, MissionBudget, MissionError, MissionLedger,
    MissionStatus, MissionSummary, Receipt, BLOCKED_STREAK_NEEDED, BUDGET_ATTENTION_FRACTION,
    CRITERION_COMMAND_TIMEOUT, MISSION_STATE_LIMIT, RECEIPT_KEEP,
};
pub use permissions::{
    PermissionMode, PermissionOutcome, PermissionPolicy, PermissionPromptDecision,
    PermissionPrompter, PermissionRequest,
};
pub use prompt::{
    load_system_prompt, ContextFile, ProjectContext, PromptBuildError, SystemPromptBuilder,
    FRONTIER_MODEL_NAME, SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
pub use redact::{redact_messages, redact_session, redact_text, REDACTED};
pub use schedule::{
    parse_duration_spec, schedule_disk_write_failures, set_schedules_root, CatchUp, DueReason,
    RunOutcome, Schedule, ScheduleError, ScheduleRun, ScheduleStore, ScheduleTrigger, MAX_HISTORY,
    MAX_SCHEDULES, SCHEDULES_FILE_LIMIT,
};
pub use schema::{normalize_tool_schema, validate_tool_input};
pub use session::{
    ContentBlock, ConversationMessage, MessageRole, SegmentRead, Session, SessionError,
};
pub use usage::{TokenUsage, UsageTracker};

/// Milliseconds since the Unix epoch, saturated at 0 if the clock steps
/// backwards past the epoch. The single clock for every durable-store
/// timestamp (jobs, missions, schedules, queue) and the idle watchdog —
/// one place documents the `u128 → u64` cast: `as_millis` cannot exceed
/// u64 until year ~292 million, and saturation is the safe direction for
/// the subtraction-only consumers (a bogus 0 makes elapsed look enormous
/// for one tick, never negative).
#[must_use]
pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
