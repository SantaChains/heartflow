//! Shadow-git checkpoints (roadmap §6/§4, easy-llm-cli checkpointing 借鉴).
//!
//! Before a write-class tool mutates the project, the whole project state is
//! committed to a **separate, private git repository** under
//! `~/.heartflow/checkpoints/<project-hash>` — the user's own git repo (if
//! any) is never touched, and `/restore` can bring the tracked files back.
//!
//! Isolation rules:
//! - the shadow repo is created `--bare` and always addressed with explicit
//!   `--git-dir` + `--work-tree`, so no cwd inference can leak into the user's
//!   repository;
//! - `core.autocrlf=false` is forced per-invocation (a global `autocrlf=true`
//!   would silently rewrite line endings on restore — a data-corruption class
//!   bug, not a cosmetic one);
//! - identity, gpg signing and hooks are overridden so a snapshot never
//!   depends on user git configuration;
//! - `.heartflow/` (our own session data) is excluded via the shadow repo's
//!   `info/exclude`; the project's own `.gitignore` is respected as usual.
//!
//! Failure policy: a checkpoint is a safety net, not a gate. If git is
//! missing or any step fails, the tool call proceeds and the failure is
//! counted + warned (degradation, never a blocked write) — but the failure
//! counter is surfaced by `hf doctor` so a persistently broken checkpoint
//! layer is visible.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

static CHECKPOINT_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);
static SNAPSHOT_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Inject the checkpoints root (CLI assembly layer; typically
/// `~/.heartflow/checkpoints`). `None` disables checkpointing entirely —
/// the default, so tests and library consumers never touch the filesystem.
pub fn set_checkpoint_root(root: Option<PathBuf>) {
    if let Ok(mut guard) = CHECKPOINT_ROOT.write() {
        *guard = root;
    }
}

/// How many snapshot attempts failed since process start. Zero does not mean
/// snapshots are happening — pair with [`set_checkpoint_root`] being set.
#[must_use]
pub fn snapshot_failures() -> u64 {
    SNAPSHOT_FAILURES.load(Ordering::Relaxed)
}

/// One restorable snapshot: short sha, commit time, subject line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub sha: String,
    pub time: String,
    pub subject: String,
}

/// FNV-1a 64 over the canonical project path. Std's `DefaultHasher` is only
/// stable within one compiler version, which would orphan every checkpoint
/// directory on a toolchain bump — a hand-rolled FNV is stable forever.
#[must_use]
fn project_hash(cwd: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in cwd.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn checkpoint_root() -> Option<PathBuf> {
    CHECKPOINT_ROOT.read().ok()?.clone()
}

fn shadow_git_dir(cwd: &Path) -> Option<PathBuf> {
    let root = checkpoint_root()?;
    Some(root.join(project_hash(cwd)))
}

/// Run one git invocation against the shadow repo. Every call pins the
/// isolation flags; nothing inherits the user's identity/hooks/autocrlf.
fn git(git_dir: &Path, work_tree: &Path, args: &[&str]) -> io::Result<std::process::Output> {
    Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .arg(format!("--work-tree={}", work_tree.display()))
        .args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.fileMode=false",
            "-c",
            "commit.gpgsign=false",
            // Non-ASCII paths (common in this user base) must come back raw,
            // never octal-escaped and quoted, or path parsing breaks silently.
            "-c",
            "core.quotepath=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "heartflow")
        .env("GIT_AUTHOR_EMAIL", "heartflow@localhost")
        .env("GIT_COMMITTER_NAME", "heartflow")
        .env("GIT_COMMITTER_EMAIL", "heartflow@localhost")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(work_tree)
        .output()
}

fn git_ok(output: &std::process::Output) -> bool {
    output.status.success()
}

/// Initialize the bare shadow repo for `cwd` if it does not exist yet.
fn ensure_shadow_repo(git_dir: &Path, _cwd: &Path) -> io::Result<()> {
    if git_dir.join("HEAD").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(git_dir)?;
    // `git init --bare` rejects `--work-tree` (fatal), so init gets its own
    // minimal invocation without the shared helper's work-tree flag; every
    // later call pins both flags explicitly.
    let output = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .args(["init", "-q", "--bare", "-b", "main"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !git_ok(&output) {
        return Err(io::Error::other(format!(
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    // Our own session data must never enter a snapshot, regardless of what
    // the project's .gitignore covers.
    let info = git_dir.join("info");
    std::fs::create_dir_all(&info)?;
    std::fs::write(info.join("exclude"), ".heartflow/\n")?;
    Ok(())
}

/// Snapshot the project before `tool` mutates it. Resolves the process cwd
/// itself so the file-tools call sites stay one line. Returns the snapshot
/// sha, or `None` when checkpointing is disabled / nothing changed since the
/// last snapshot / the snapshot failed (warned + counted, never blocking).
pub fn maybe_snapshot_cwd(tool: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    maybe_snapshot(&cwd, tool)
}

/// [`maybe_snapshot_cwd`] with an explicit project directory (testable).
pub fn maybe_snapshot(cwd: &Path, tool: &str) -> Option<String> {
    let git_dir = shadow_git_dir(cwd)?;
    match snapshot(&git_dir, cwd, tool) {
        Ok(sha) => sha,
        Err(error) => {
            SNAPSHOT_FAILURES.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                tool,
                error = %error,
                "checkpoint snapshot failed; the write proceeds without a restore point"
            );
            None
        }
    }
}

fn snapshot(git_dir: &Path, cwd: &Path, tool: &str) -> io::Result<Option<String>> {
    ensure_shadow_repo(git_dir, cwd)?;
    // `git add -A` reads .gitignore from the work tree, so the project's own
    // ignore rules (target/, node_modules/, ...) bound the snapshot cost.
    let added = git(git_dir, cwd, &["add", "-A", "--"])?;
    if !git_ok(&added) {
        return Err(io::Error::other(format!(
            "git add failed: {}",
            String::from_utf8_lossy(&added.stderr).trim()
        )));
    }
    let committed = git(
        git_dir,
        cwd,
        &[
            "commit",
            "-q",
            "-m",
            &format!("before {tool}"),
            "--no-verify",
        ],
    )?;
    if !git_ok(&committed) {
        // "nothing to commit" is git's clean-tree exit (status message on
        // stdout even under -q, sometimes stderr depending on version) - the
        // previous snapshot already captures this exact state.
        let stderr = String::from_utf8_lossy(&committed.stderr);
        let stdout = String::from_utf8_lossy(&committed.stdout);
        if stderr.contains("nothing to commit") || stdout.contains("nothing to commit") {
            return Ok(None);
        }
        return Err(io::Error::other(format!(
            "git commit failed: {}",
            stderr.trim()
        )));
    }
    // Full sha: it is the canonical key `/restore` resolves and what
    // `list_snapshots` reports (%H); callers truncate for display only.
    let head = git(git_dir, cwd, &["rev-parse", "HEAD"])?;
    if !git_ok(&head) {
        return Err(io::Error::other("git rev-parse failed after commit"));
    }
    Ok(Some(
        String::from_utf8_lossy(&head.stdout).trim().to_string(),
    ))
}

/// List the most recent snapshots for `cwd`, newest first.
///
/// # Errors
/// Propagates when the shadow repo exists but git fails against it.
pub fn list_snapshots(cwd: &Path) -> io::Result<Vec<Snapshot>> {
    let Some(git_dir) = shadow_git_dir(cwd) else {
        return Ok(Vec::new());
    };
    if !git_dir.join("HEAD").exists() {
        return Ok(Vec::new());
    }
    // %x09 separates fields so tabs survive subjects containing spaces.
    let log = git(
        &git_dir,
        cwd,
        &["log", "--format=%H%x09%ci%x09%s", "-n", "50"],
    )?;
    if !git_ok(&log) {
        return Err(io::Error::other(format!(
            "git log failed: {}",
            String::from_utf8_lossy(&log.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&log.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let sha = fields.next()?.trim().to_string();
            let time = fields.next()?.trim().to_string();
            let subject = fields.next()?.trim().to_string();
            if sha.is_empty() {
                return None;
            }
            Some(Snapshot { sha, time, subject })
        })
        .collect())
}

/// Restore the tracked files of `cwd` to the snapshot `sha` (a full or short
/// hex sha, or a resolvable revision like `main~1`). Files *created* after
/// the snapshot are deliberately left in place — removing them would mean
/// running a `git clean` over the user's project, which this MVP refuses to
/// do; the caller should surface that limit in its output.
///
/// Returns the number of files the snapshot tree tracks.
///
/// # Errors
/// Unknown revision, or git failure against the shadow repo.
pub fn restore_snapshot(cwd: &Path, sha: &str) -> io::Result<usize> {
    let git_dir = shadow_git_dir(cwd).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "checkpointing is disabled (no checkpoints root configured)",
        )
    })?;
    if !git_dir.join("HEAD").exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no snapshots exist for this project yet",
        ));
    }
    let resolved = git(
        &git_dir,
        cwd,
        &["rev-parse", "--verify", &format!("{sha}^{{commit}}")],
    )?;
    if !git_ok(&resolved) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown snapshot revision: {sha}"),
        ));
    }
    let resolved_sha = String::from_utf8_lossy(&resolved.stdout).trim().to_string();

    let read_tree = git(&git_dir, cwd, &["read-tree", &resolved_sha])?;
    if !git_ok(&read_tree) {
        return Err(io::Error::other(format!(
            "git read-tree failed: {}",
            String::from_utf8_lossy(&read_tree.stderr).trim()
        )));
    }
    let checkout = git(&git_dir, cwd, &["checkout-index", "-a", "-f", "--"])?;
    if !git_ok(&checkout) {
        return Err(io::Error::other(format!(
            "git checkout-index failed: {}",
            String::from_utf8_lossy(&checkout.stderr).trim()
        )));
    }
    let listing = git(
        &git_dir,
        cwd,
        &["ls-tree", "-r", "--name-only", &resolved_sha],
    )?;
    if !git_ok(&listing) {
        return Err(io::Error::other("git ls-tree failed"));
    }
    Ok(String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count())
}

/// Result of an explicit [`purge_new_files_since`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PurgeReport {
    /// Snapshot capturing the purged files right before deletion - the purge
    /// itself is reversible via `/restore` on this sha.
    pub backup_sha: String,
    /// Paths removed, relative to the project root.
    pub removed: Vec<String>,
    /// Paths that could not be removed, as `path: error` entries.
    pub failed: Vec<String>,
}

/// Remove files that exist on disk but are absent from snapshot `sha` - the
/// explicit opt-in complement to [`restore_snapshot`]'s deliberate refusal to
/// touch post-snapshot creations.
///
/// Safety design (the MVP's hard requirement):
/// 1. the purge never runs as a blanket `git clean`; the file set is the
///    exact difference between the snapshot tree and what git currently sees
///    (tracked + untracked, `.gitignore`d files like `target/` untouched);
/// 2. a backup snapshot commits the current state *before* anything is
///    deleted, so every removal is reversible through the normal
///    `/restore <backup_sha>` path.
///
/// # Errors
/// Unknown revision, checkpointing disabled, or git failure - in which case
/// nothing on disk has been modified.
pub fn purge_new_files_since(cwd: &Path, sha: &str) -> io::Result<PurgeReport> {
    let git_dir = shadow_git_dir(cwd).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "checkpointing is disabled (no checkpoints root configured)",
        )
    })?;
    if !git_dir.join("HEAD").exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no snapshots exist for this project yet",
        ));
    }
    let resolved = git(
        &git_dir,
        cwd,
        &["rev-parse", "--verify", &format!("{sha}^{{commit}}")],
    )?;
    if !git_ok(&resolved) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown snapshot revision: {sha}"),
        ));
    }
    let target = String::from_utf8_lossy(&resolved.stdout).trim().to_string();

    // Safety net first: commit the state about to be purged. `snapshot`
    // returns Ok(None) on "nothing to commit", which means HEAD already
    // holds exactly this state - HEAD is then the backup.
    let backup = match snapshot(&git_dir, cwd, "purge-backup")? {
        Some(sha) => sha,
        None => {
            let head = git(&git_dir, cwd, &["rev-parse", "HEAD"])?;
            if !git_ok(&head) {
                return Err(io::Error::other("git rev-parse failed"));
            }
            String::from_utf8_lossy(&head.stdout).trim().to_string()
        }
    };

    // Current disk view: everything git tracks or sees untracked, minus
    // ignored files (the project's .gitignore + our info/exclude rules).
    let disk = git(
        &git_dir,
        cwd,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    if !git_ok(&disk) {
        return Err(io::Error::other(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&disk.stderr).trim()
        )));
    }
    // The target snapshot's tree. `-z` on both sides keeps paths with
    // spaces/quotes intact (quotepath=false keeps non-ASCII readable).
    let tracked = git(
        &git_dir,
        cwd,
        &["ls-tree", "-r", "--name-only", "-z", &target],
    )?;
    if !git_ok(&tracked) {
        return Err(io::Error::other(format!(
            "git ls-tree failed: {}",
            String::from_utf8_lossy(&tracked.stderr).trim()
        )));
    }
    let target_files: std::collections::HashSet<String> = String::from_utf8_lossy(&tracked.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect();

    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for path in String::from_utf8_lossy(&disk.stdout).split('\0') {
        if path.is_empty() || target_files.contains(path) {
            continue;
        }
        match std::fs::remove_file(cwd.join(path)) {
            Ok(()) => removed.push(path.to_string()),
            // Directories show up here when a whole tree was created after
            // the snapshot; empty dirs are left in place on purpose (harmless,
            // and rmdir-ing trees the user may have touched is over-reach).
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) => failed.push(format!("{path}: {error}")),
        }
    }
    Ok(PurgeReport {
        backup_sha: backup,
        removed,
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The checkpoint root is a process-global; tests that set it must
    /// serialize against each other.
    static ROOT_LOCK: Mutex<()> = Mutex::new(());

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("hf-checkpoint-test-{label}-{}", std::process::id()))
            .join(format!(
                "{}",
                u128::from(SNAPSHOT_FAILURES.load(Ordering::Relaxed)) + rand_counter()
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // A monotonic-ish counter without pulling a rand dependency: nanos are
    // unique enough for temp-dir names inside one test process.
    fn rand_counter() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    #[test]
    fn project_hash_is_stable_and_input_sensitive() {
        let a = project_hash(Path::new("D:/proj/one"));
        assert_eq!(a, project_hash(Path::new("D:/proj/one")));
        assert_ne!(a, project_hash(Path::new("D:/proj/two")));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn disabled_root_means_no_snapshots() {
        let _guard = ROOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_checkpoint_root(None);
        let dir = unique_temp_dir("disabled");
        assert!(maybe_snapshot(&dir, "write_file").is_none());
        assert!(list_snapshots(&dir).unwrap().is_empty());
        assert!(restore_snapshot(&dir, "HEAD").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_list_and_restore_round_trip() {
        let _guard = ROOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let project = unique_temp_dir("project");
        let root = unique_temp_dir("root");
        set_checkpoint_root(Some(root.clone()));

        std::fs::write(project.join("a.txt"), "version one\n").unwrap();
        let first = maybe_snapshot(&project, "write_file").expect("first snapshot");
        assert!(!first.is_empty());

        // Second snapshot with unchanged tree: no new commit.
        assert!(maybe_snapshot(&project, "write_file").is_none());

        std::fs::write(project.join("a.txt"), "version two\n").unwrap();
        std::fs::write(project.join("b.txt"), "added later\n").unwrap();
        let second = maybe_snapshot(&project, "edit_file").expect("second snapshot");
        assert_ne!(first, second);

        let snapshots = list_snapshots(&project).unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].sha, second, "newest first");
        assert!(snapshots[1].subject.contains("write_file"));
        assert!(snapshots[0].subject.contains("edit_file"));

        // Restore the first snapshot: a.txt reverts, b.txt (created after) is
        // deliberately left in place by the MVP restore.
        let file_count = restore_snapshot(&project, &first).unwrap();
        assert_eq!(file_count, 1);
        assert_eq!(
            std::fs::read_to_string(project.join("a.txt")).unwrap(),
            "version one\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("b.txt")).unwrap(),
            "added later\n"
        );

        // Unknown revision is an error, not a panic.
        assert!(restore_snapshot(&project, "deadbeef").is_err());

        set_checkpoint_root(None);
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn purge_removes_new_files_and_keeps_a_backup() {
        let _guard = ROOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let project = unique_temp_dir("purge");
        let root = unique_temp_dir("purge-root");
        set_checkpoint_root(Some(root.clone()));

        std::fs::write(project.join("keep.txt"), "kept\n").unwrap();
        // Ignore rules predate the snapshot: real projects have them from the
        // start, and the purge must respect them for files like *.log.
        std::fs::write(project.join(".gitignore"), "*.log\n").unwrap();
        let first = maybe_snapshot(&project, "write_file").expect("first snapshot");

        // Files created after the target snapshot: one tracked later, one
        // never snapshotted before the purge's own backup, one nested.
        std::fs::write(project.join("b.txt"), "added later\n").unwrap();
        std::fs::create_dir_all(project.join("sub/dir")).unwrap();
        std::fs::write(project.join("sub/dir/new.txt"), "nested\n").unwrap();
        std::fs::write(project.join("ignored.log"), "noise\n").unwrap();
        maybe_snapshot(&project, "edit_file").expect("second snapshot");

        let report = purge_new_files_since(&project, &first).expect("purge");
        assert!(
            !report.backup_sha.is_empty() && report.backup_sha != first,
            "backup must be a fresh commit of the purged state"
        );
        let mut removed = report.removed.clone();
        removed.sort();
        // keep.txt is in the target tree; ignored.log is invisible to git and
        // must survive untouched.
        assert_eq!(removed, vec!["b.txt", "sub/dir/new.txt"]);
        assert!(
            report.failed.is_empty(),
            "unexpected failures: {:?}",
            report.failed
        );
        assert!(!project.join("b.txt").exists());
        assert!(!project.join("sub/dir/new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(project.join("keep.txt")).unwrap(),
            "kept\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("ignored.log")).unwrap(),
            "noise\n"
        );

        // The purge is reversible: restoring the backup sha brings the
        // purged files back byte-for-byte.
        restore_snapshot(&project, &report.backup_sha).unwrap();
        assert_eq!(
            std::fs::read_to_string(project.join("b.txt")).unwrap(),
            "added later\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("sub/dir/new.txt")).unwrap(),
            "nested\n"
        );

        // Unknown revision refuses before touching anything.
        assert!(purge_new_files_since(&project, "deadbeef").is_err());
        assert!(
            project.join("b.txt").exists(),
            "failed purge must not delete"
        );

        set_checkpoint_root(None);
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_respects_gitignore_and_excludes_heartflow_dir() {
        let _guard = ROOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let project = unique_temp_dir("ignore");
        let root = unique_temp_dir("ignore-root");
        set_checkpoint_root(Some(root.clone()));

        std::fs::write(project.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::create_dir_all(project.join(".heartflow")).unwrap();
        std::fs::write(project.join(".heartflow").join("state.json"), "{}").unwrap();
        std::fs::write(project.join("ignored.txt"), "junk\n").unwrap();
        std::fs::write(project.join("kept.txt"), "real\n").unwrap();

        maybe_snapshot(&project, "write_file").expect("snapshot");

        let git_dir = root.join(project_hash(&project));
        let tree = git(
            &git_dir,
            &project,
            &["ls-tree", "-r", "--name-only", "HEAD"],
        )
        .unwrap();
        let listed = String::from_utf8_lossy(&tree.stdout);
        assert!(listed.contains("kept.txt"));
        assert!(listed.contains(".gitignore"));
        assert!(
            !listed.contains("ignored.txt"),
            "gitignore must bind the snapshot"
        );
        assert!(
            !listed.contains(".heartflow"),
            "session data must never be snapshotted"
        );

        set_checkpoint_root(None);
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&root);
    }
}
