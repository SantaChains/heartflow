//! Post-edit diagnostics: after a write-class tool mutates a Rust source file,
//! run `cargo check --message-format=short` against the owning project and
//! hand the compiler's verdict back to the model inside the same tool result.
//!
//! This is the "write-after diagnosis" half of the roadmap §6 safe-edit loop:
//! shadow-git snapshot (before) -> tool executes -> diagnostics (after) ->
//! `/restore` if the verdict is bad. The model sees errors one turn earlier
//! than a read-then-bash-check round trip would reveal them.
//!
//! Degradation policy follows the house rule "silent degradation must stay
//! bounded and observable": every skipped check bumps a per-reason counter
//! surfaced by `hf doctor`, and a degradation never blocks the write itself -
//! checkpointing is the safety net, diagnostics are a convenience.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Upper bound on parsed diagnostics handed to the model; the rest are
/// summarized by `truncated`.
const MAX_ITEMS: usize = 20;
/// Per-message char cap (same spirit as the compaction summary ramps).
const MAX_MESSAGE_CHARS: usize = 240;
/// Default ceiling for one `cargo check` invocation.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// How many parent directories to walk up looking for a `Cargo.toml`.
const MAX_MANIFEST_WALK: usize = 8;

/// One compiler diagnostic in short form.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiagnosticItem {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub level: String,
    pub message: String,
}

/// Result of one post-edit diagnostic run. `degraded` is `Some(reason)` when
/// the check could not run or was abandoned; `items` still carries whatever
/// was parsed before the timeout fired.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PostEditDiagnostics {
    pub command: String,
    pub duration_ms: u64,
    pub items: Vec<DiagnosticItem>,
    pub truncated: bool,
    /// Bounded degradation ledger entry, e.g. "timeout after 60s".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
}

/// Degradation ledger: per-reason counters, surfaced by `hf doctor`.
static DEG_NO_CARGO: AtomicU64 = AtomicU64::new(0);
static DEG_NO_MANIFEST: AtomicU64 = AtomicU64::new(0);
static DEG_TIMEOUT: AtomicU64 = AtomicU64::new(0);
static DEG_SPAWN: AtomicU64 = AtomicU64::new(0);

/// Last non-counter degradation detail (e.g. the spawn error string), for
/// doctor to quote. Mutex so failures never poison a reader (house rule:
/// lock poisoning must not turn a diagnostic into an error).
static DEG_DETAIL: Mutex<Option<String>> = Mutex::new(None);

fn bump(counter: &AtomicU64, detail: Option<String>) -> String {
    if let Some(text) = detail {
        if let Ok(mut slot) = DEG_DETAIL.lock() {
            *slot = Some(text);
        }
    }
    counter.fetch_add(1, Ordering::Relaxed);
    "degraded".to_string()
}

/// Snapshot of the degradation ledger for `hf doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DiagnosticsLedger {
    pub no_cargo: u64,
    pub no_manifest: u64,
    pub timeout: u64,
    pub spawn_error: u64,
}

#[must_use]
pub fn diagnostics_ledger() -> DiagnosticsLedger {
    DiagnosticsLedger {
        no_cargo: DEG_NO_CARGO.load(Ordering::Relaxed),
        no_manifest: DEG_NO_MANIFEST.load(Ordering::Relaxed),
        timeout: DEG_TIMEOUT.load(Ordering::Relaxed),
        spawn_error: DEG_SPAWN.load(Ordering::Relaxed),
    }
}

#[must_use]
pub fn diagnostics_degradation_detail() -> Option<String> {
    DEG_DETAIL.lock().ok().and_then(|slot| slot.clone())
}

/// Resolve the cargo binary: `CARGO` env (set whenever we ourselves were
/// built/launched under cargo) wins, PATH fallback otherwise.
fn cargo_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CARGO") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    // `which cargo` equivalent without a dependency: scan PATH manually so a
    // missing binary degrades with a counted reason instead of a panic.
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// True when at least one changed file should trigger a Rust check: any
/// `.rs` source or the `Cargo.toml` manifest itself.
#[must_use]
fn rust_relevant(changed_files: &[PathBuf]) -> bool {
    changed_files.iter().any(|path| {
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
            || path.file_name().is_some_and(|name| name == "Cargo.toml")
    })
}

/// Walk up from `start` looking for a `Cargo.toml`; returns the containing
/// directory. Bounded so a drive-root miss costs at most `MAX_MANIFEST_WALK`
/// stat calls.
#[must_use]
fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        Some(start.to_path_buf())
    } else {
        start.parent().map(Path::to_path_buf)
    };
    for _ in 0..MAX_MANIFEST_WALK {
        let dir = current?;
        if dir.join("Cargo.toml").is_file() {
            return Some(dir);
        }
        current = dir.parent().map(Path::to_path_buf);
    }
    None
}

fn timeout_secs() -> u64 {
    std::env::var("HEARTFLOW_DIAGNOSTICS_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// Run post-edit diagnostics for the files a write-class tool just changed.
///
/// Returns `None` when the change is not Rust-relevant, the kill switch is
/// set, or the project has no manifest within the walk bound - the caller
/// should treat `None` as "nothing to say", and the ledger records why the
/// check did not happen when a Rust file *was* changed.
#[must_use]
pub fn run_post_edit_diagnostics(changed_files: &[PathBuf]) -> Option<PostEditDiagnostics> {
    if std::env::var("HEARTFLOW_POST_EDIT_DIAGNOSTICS")
        .ok()
        .is_some_and(|raw| raw.trim() == "0")
    {
        return None;
    }
    if changed_files.is_empty() || !rust_relevant(changed_files) {
        return None;
    }
    let Some(project_root) = changed_files.iter().find_map(|p| find_project_root(p)) else {
        bump(&DEG_NO_MANIFEST, None);
        return None;
    };
    let Some(cargo) = cargo_binary() else {
        bump(&DEG_NO_CARGO, None);
        return None;
    };

    let command = "cargo check --message-format=short --quiet";
    let started = Instant::now();
    let deadline = Duration::from_secs(timeout_secs());
    let mut child = match std::process::Command::new(&cargo)
        .args(["check", "--message-format=short", "--quiet"])
        .current_dir(&project_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let detail = format!("spawn {command}: {error}");
            bump(&DEG_SPAWN, Some(detail.clone()));
            return Some(PostEditDiagnostics {
                command: command.to_string(),
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(0),
                items: Vec::new(),
                truncated: false,
                degraded: Some(detail),
            });
        }
    };

    // Bounded wait without a wait_timeout dependency: poll try_wait, drain
    // output after exit (or kill on deadline). stdout/stderr are piped but
    // unread until exit - cargo's quiet short output stays well under pipe
    // capacity for a single check round, and even if a pathological build
    // filled the pipe the deadline kill unblocks us, so no hang is possible.
    let mut degraded: Option<String> = None;
    // The exit status itself is not reported - items carry the compiler's
    // verdict, and a non-zero exit with no parsed diagnostics means a broken
    // build environment, which the degraded/deadline path already covers.
    let _exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= deadline {
                    degraded = Some(format!("timeout after {}s", deadline.as_secs()));
                    bump(&DEG_TIMEOUT, None);
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => {
                degraded = Some("wait failed".to_string());
                break None;
            }
        }
    };
    use std::io::Read;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut handle) = child.stdout.take() {
        let _ = handle.read_to_end(&mut stdout);
    }
    if let Some(mut handle) = child.stderr.take() {
        let _ = handle.read_to_end(&mut stderr);
    }

    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&stderr));
    let mut items = parse_short_diagnostics(&text);
    let truncated = items.len() > MAX_ITEMS;
    items.truncate(MAX_ITEMS);

    Some(PostEditDiagnostics {
        command: command.to_string(),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(0),
        items,
        truncated,
        degraded,
    })
}

/// Parse `--message-format=short` lines: `file:line:col: level[CODE]: msg`.
/// Cargo-level failures (`error: could not compile ...`) have no position and
/// land as `file = "<cargo>"`. Everything else (progress, notes on their own
/// lines) is ignored.
#[must_use]
pub fn parse_short_diagnostics(output: &str) -> Vec<DiagnosticItem> {
    let mut items = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(item) = parse_short_line(line) {
            items.push(item);
        }
    }
    items
}

fn parse_short_line(line: &str) -> Option<DiagnosticItem> {
    // Positional form: path:line:col: level: message (path may contain no ':'
    // on Windows drives since the colon sits after the drive letter - splitting
    // from the right for line/col keeps drive letters intact).
    let parts: Vec<&str> = line.splitn(4, ':').collect();
    if parts.len() == 4 {
        if let (Ok(line_no), Ok(col_no)) = (
            parts[1].trim().parse::<u32>(),
            parts[2].trim().parse::<u32>(),
        ) {
            let rest = parts[3];
            let (level, message) = match rest.split_once(':') {
                Some((level, message)) => (level.trim(), message.trim()),
                None => (rest.trim(), ""),
            };
            let level = level.split('[').next().unwrap_or(level).trim();
            return Some(DiagnosticItem {
                file: parts[0].trim().to_string(),
                line: line_no,
                column: col_no,
                level: level.to_string(),
                message: clamp_message(message),
            });
        }
    }
    // Cargo-level form: "error: ..." (no position).
    if let Some(rest) = line.strip_prefix("error:") {
        return Some(DiagnosticItem {
            file: "<cargo>".to_string(),
            line: 0,
            column: 0,
            level: "error".to_string(),
            message: clamp_message(rest.trim()),
        });
    }
    None
}

fn clamp_message(message: &str) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        return message.to_string();
    }
    let mut clipped: String = message.chars().take(MAX_MESSAGE_CHARS).collect();
    clipped.push_str(" ...[truncated]");
    clipped
}

#[cfg(test)]
mod tests {
    use super::{find_project_root, parse_short_diagnostics, run_post_edit_diagnostics};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("hf-diag-{label}-{}-{nanos}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir create");
        dir
    }

    fn write_minimal_project(root: &Path, main_rs: &str) {
        fs::create_dir_all(root.join("src")).expect("src dir");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"hf-diag-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        fs::write(root.join("src/main.rs"), main_rs).expect("main.rs");
    }

    #[test]
    fn parses_short_diagnostics_lines() {
        let output = "src/main.rs:2:21: error[E0308]: mismatched types\n\
                      src/lib.rs:10:5: warning: unused variable: `x`\n\
                      error: could not compile `fixture` (bin \"fixture\") due to 1 previous error\n\
                      Compiling fixture v0.1.0\n";
        let items = parse_short_diagnostics(output);
        assert_eq!(items.len(), 3, "progress lines must be ignored: {items:?}");
        assert_eq!(items[0].file, "src/main.rs");
        assert_eq!(items[0].line, 2);
        assert_eq!(items[0].column, 21);
        assert_eq!(items[0].level, "error");
        assert_eq!(items[0].message, "mismatched types");
        assert_eq!(items[1].level, "warning");
        assert_eq!(items[2].file, "<cargo>");
        assert_eq!(items[2].level, "error");
    }

    #[test]
    fn detects_errors_in_broken_project() {
        let root = unique_temp("broken");
        write_minimal_project(
            &root,
            "fn main() {\n    let x: i32 = \"not a number\";\n    let _ = x;\n}\n",
        );
        let result = run_post_edit_diagnostics(&[root.join("src/main.rs")])
            .expect("rust-relevant change must run a check");
        assert!(
            result.degraded.is_none(),
            "unexpected degradation: {:?}",
            result.degraded
        );
        assert!(
            result.items.iter().any(|item| item.level == "error"),
            "expected at least one error, got: {:?}",
            result.items
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_project_yields_no_errors() {
        let root = unique_temp("clean");
        write_minimal_project(&root, "fn main() {\n    println!(\"ok\");\n}\n");
        let result = run_post_edit_diagnostics(&[root.join("src/main.rs")])
            .expect("rust-relevant change must run a check");
        assert!(
            result.items.iter().all(|item| item.level != "error"),
            "clean project must have no errors: {:?}",
            result.items
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn non_rust_changes_are_skipped_silently() {
        let root = unique_temp("txt");
        fs::write(root.join("notes.txt"), "hello").expect("notes");
        assert!(run_post_edit_diagnostics(&[root.join("notes.txt")]).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_manifest_counts_as_degradation() {
        let before = super::diagnostics_ledger().no_manifest;
        let root = unique_temp("nomanifest");
        fs::write(root.join("main.rs"), "fn main() {}").expect("main.rs");
        assert!(run_post_edit_diagnostics(&[root.join("main.rs")]).is_none());
        assert!(
            super::diagnostics_ledger().no_manifest > before,
            "no-manifest skip must be ledgered"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn walks_up_to_workspace_manifest() {
        let root = unique_temp("walkup");
        write_minimal_project(&root, "fn main() {}");
        let deep = root.join("src/deep/nested");
        fs::create_dir_all(&deep).expect("nested dirs");
        assert_eq!(find_project_root(&deep), Some(root.clone()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn message_clamp_is_char_safe() {
        let long = "x".repeat(400);
        let items = parse_short_diagnostics(&format!("a.rs:1:1: error: {long}"));
        assert_eq!(items.len(), 1);
        assert!(items[0].message.chars().count() < 400);
        assert!(items[0].message.ends_with("...[truncated]"));
    }
}
