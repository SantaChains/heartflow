use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use glob::Pattern;
use ignore::WalkState;
use nucleo_matcher::{Config, Matcher, Utf32Str};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextFilePayload {
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "numLines")]
    pub num_lines: usize,
    #[serde(rename = "startLine")]
    pub start_line: usize,
    #[serde(rename = "totalLines")]
    pub total_lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    pub file: TextFilePayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredPatchHunk {
    #[serde(rename = "oldStart")]
    pub old_start: usize,
    #[serde(rename = "oldLines")]
    pub old_lines: usize,
    #[serde(rename = "newStart")]
    pub new_start: usize,
    #[serde(rename = "newLines")]
    pub new_lines: usize,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
    #[serde(rename = "originalFile")]
    pub original_file: Option<String>,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditFileOutput {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "oldString")]
    pub old_string: String,
    #[serde(rename = "newString")]
    pub new_string: String,
    #[serde(rename = "originalFile")]
    pub original_file: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
    #[serde(rename = "userModified")]
    pub user_modified: bool,
    #[serde(rename = "replaceAll")]
    pub replace_all: bool,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobSearchOutput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchFilesOutput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchInput {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(rename = "output_mode")]
    pub output_mode: Option<String>,
    #[serde(rename = "-B")]
    pub before: Option<usize>,
    #[serde(rename = "-A")]
    pub after: Option<usize>,
    #[serde(rename = "-C")]
    pub context_short: Option<usize>,
    pub context: Option<usize>,
    #[serde(rename = "-n")]
    pub line_numbers: Option<bool>,
    #[serde(rename = "-i")]
    pub case_insensitive: Option<bool>,
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    pub multiline: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchOutput {
    pub mode: Option<String>,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub content: Option<String>,
    #[serde(rename = "numLines")]
    pub num_lines: Option<usize>,
    #[serde(rename = "numMatches")]
    pub num_matches: Option<usize>,
    #[serde(rename = "appliedLimit")]
    pub applied_limit: Option<usize>,
    #[serde(rename = "appliedOffset")]
    pub applied_offset: Option<usize>,
}

/// Read cap: refuse to pull oversized files into the model context.
const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
/// Default line window when the caller does not pass an explicit limit.
const DEFAULT_READ_LINES: usize = 2_000;
/// Wait before the single retry of a read interrupted by another process.
const READ_RETRY_DELAY: Duration = Duration::from_millis(50);
/// Directories that are never worth searching (VCS and build caches).
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
    ".idea",
    ".vscode",
];
/// Files larger than this are treated as data blobs, not searchable text.
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
/// Bytes sniffed from a file's head for binary detection (ripgrep's NUL
/// heuristic): one NUL byte in the first 8 KiB disqualifies the file.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Hard ceiling on content-mode lines collected across one grep call,
/// bounding worst-case memory no matter how many workers hit matches.
const MAX_GREP_CONTENT_LINES: usize = 50_000;
/// Hard ceiling on files examined per search to bound worst-case latency.
const MAX_SEARCH_FILES: usize = 20_000;
/// Default number of fuzzy file hits `search_files` returns.
const DEFAULT_SEARCH_LIMIT: usize = 50;
/// Most fuzzy file hits `search_files` may return in one call.
const MAX_SEARCH_LIMIT: usize = 500;

/// Run an idempotent read, retrying once on transient OS failures.
///
/// Editors, indexers and virus scanners briefly hold files open while an
/// agent reads them; one short retry turns that race into a success.
/// Windows sharing/lock violations surface as `WouldBlock`, as does
/// `EAGAIN` on unix; `Interrupted` covers interrupted syscalls. Permanent
/// failures (not found, invalid input, real permission errors) return
/// immediately without a delay.
fn with_read_retry<T>(op: impl Fn() -> io::Result<T>) -> io::Result<T> {
    op().or_else(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
        ) {
            std::thread::sleep(READ_RETRY_DELAY);
            op()
        } else {
            Err(error)
        }
    })
}

pub fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> io::Result<ReadFileOutput> {
    with_read_retry(|| read_file_once(path, offset, limit))
}

fn read_file_once(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> io::Result<ReadFileOutput> {
    let absolute_path = normalize_path(path)?;
    let metadata = fs::metadata(&absolute_path)?;
    if metadata.len() > MAX_READ_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file exceeds the {} byte read cap ({} bytes); read it with the bash tool instead",
                MAX_READ_BYTES,
                metadata.len()
            ),
        ));
    }
    let raw_content = read_text_lossy(&absolute_path)?;
    let content = strip_bom(&raw_content);
    let lines: Vec<&str> = content.lines().collect();
    let start_index = offset.unwrap_or(0).min(lines.len());
    let end_index = limit.map_or_else(
        || (start_index + DEFAULT_READ_LINES).min(lines.len()),
        |limit| start_index.saturating_add(limit).min(lines.len()),
    );
    let selected = lines[start_index..end_index].join("\n");

    Ok(ReadFileOutput {
        kind: String::from("text"),
        file: TextFilePayload {
            file_path: absolute_path.to_string_lossy().into_owned(),
            content: selected,
            num_lines: end_index.saturating_sub(start_index),
            start_line: start_index.saturating_add(1),
            total_lines: lines.len(),
        },
    })
}

/// Read a file as text, degrading through charset detection instead of failing
/// outright. `fs::read_to_string` alone rejects every byte sequence that is not
/// UTF-8, which on a zh-CN Windows box means GBK logs and UTF-16 output from
/// `PowerShell` redirection are not merely mis-decoded but wholly unreadable.
fn read_text_lossy(path: &Path) -> io::Result<String> {
    Ok(decode_text(&fs::read(path)?))
}

/// Decode bytes to text. A byte-order mark is honoured first (authoritative,
/// and `chardetng`'s statistical guess is unreliable for the short UTF-16 files
/// Windows tools emit), then valid UTF-8 is returned unchanged; only genuinely
/// non-UTF-8 input is sniffed and decoded through the detected legacy encoding,
/// so CJK content survives rather than collapsing into replacement characters
/// under a lossy UTF-8 pass. Mirrors `bash::decode_output`.
fn decode_text(bytes: &[u8]) -> String {
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding.decode(&bytes[bom_len..]).0.into_owned();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    // `allow_utf8: false` is deliberate: the bytes are known non-UTF-8 here, so
    // forcing a legacy candidate avoids re-emitting the same bytes lossily.
    detector.guess(None, false).decode(bytes).0.into_owned()
}

pub fn write_file(path: &str, content: &str) -> io::Result<WriteFileOutput> {
    let absolute_path = normalize_path_allow_missing(path)?;
    // Read bytes, not `read_to_string`: a non-UTF-8 file *exists* and is being
    // replaced whole, and reporting that as a "create" would tell the caller
    // there is nothing to restore.
    let original_bytes = fs::read(&absolute_path).ok();
    let original_file = original_bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(|text| strip_bom(text).to_string());
    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_text_atomic(&absolute_path, content)?;

    Ok(WriteFileOutput {
        kind: if original_bytes.is_some() {
            String::from("update")
        } else {
            String::from("create")
        },
        file_path: absolute_path.to_string_lossy().into_owned(),
        content: content.to_owned(),
        structured_patch: make_patch(original_file.as_deref().unwrap_or(""), content),
        original_file,
        git_diff: None,
    })
}

pub fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> io::Result<EditFileOutput> {
    let absolute_path = normalize_path(path)?;
    // `fs::read` first so a non-UTF-8 file produces a message that names the
    // file and the way out, not a bare `read_to_string` decoding error.
    let raw = fs::read(&absolute_path)?;
    let decoded = std::str::from_utf8(&raw).map_err(|_| {
        invalid_input(format!(
            "{} is not valid UTF-8; edit_file edits text only (write_file replaces \
             the whole file)",
            absolute_path.display()
        ))
    })?;
    let original_file = strip_bom(decoded).to_string();
    if old_string == new_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "old_string and new_string must differ",
        ));
    }
    if !original_file.contains(old_string) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "old_string not found in file{}",
                nearest_snippet(&original_file, old_string).unwrap_or_default()
            ),
        ));
    }
    // Ambiguity is refused rather than silently resolved to the first hit: the
    // caller wrote `old_string` believing it identified one site, and editing a
    // different one of `matches` sites is a wrong edit that looks like success.
    // `apply_patch` already refuses the same input; this keeps the two tools
    // from disagreeing about what a non-unique `old_string` means.
    let matches = original_file.matches(old_string).count();
    if !replace_all && !old_string.is_empty() && matches > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "old_string matches {matches} times in {}; set replace_all or make it unique",
                absolute_path.display()
            ),
        ));
    }

    let updated = if replace_all {
        original_file.replace(old_string, new_string)
    } else {
        original_file.replacen(old_string, new_string, 1)
    };
    write_text_atomic(&absolute_path, &updated)?;

    Ok(EditFileOutput {
        file_path: absolute_path.to_string_lossy().into_owned(),
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        original_file: original_file.clone(),
        structured_patch: make_patch(&original_file, &updated),
        user_modified: false,
        replace_all,
        git_diff: None,
    })
}

/// One change inside an `apply_patch` batch: a precise string replacement, or a
/// whole-file create/overwrite when `old_string` is empty.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PatchChange {
    pub path: String,
    #[serde(default)]
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// Per-file outcome of `apply_patch`, carrying a real unified-diff hunk set.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PatchFileResult {
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub kind: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ApplyPatchOutput {
    #[serde(rename = "filesChanged")]
    pub files_changed: usize,
    pub results: Vec<PatchFileResult>,
}

/// A single in-memory file being assembled across a batch (before any write).
struct PreparedFile {
    absolute_path: PathBuf,
    /// The on-disk bytes as first seen; `None` means the file did not exist.
    ///
    /// Bytes rather than `String` on purpose. Reading through `read_to_string`
    /// made "does not exist" and "exists but is not UTF-8" indistinguishable,
    /// and the rollback resolves `None` by *deleting* the file — so one failed
    /// write turned an overwritten binary file into a missing one.
    original_bytes: Option<Vec<u8>>,
    /// The same content decoded as text, when it is valid UTF-8; drives the diff.
    original: Option<String>,
    content: String,
}

/// Apply a batch of precise edits across one or more files in a single call —
/// the transactional, multi-file counterpart to `edit_file`. Every change is
/// validated against the running content *before* anything is written, so a
/// missing or ambiguous `old_string` aborts the whole batch with nothing on
/// disk. An empty `old_string` creates or overwrites the file with `new_string`.
/// Multiple changes may target the same file and are applied in order. Each
/// written file reports real unified-diff hunks.
pub fn apply_patch(changes: &[PatchChange]) -> io::Result<ApplyPatchOutput> {
    if changes.is_empty() {
        return Err(invalid_input("no changes supplied"));
    }

    let mut order: Vec<PathBuf> = Vec::new();
    let mut by_path: HashMap<PathBuf, PreparedFile> = HashMap::new();

    for change in changes {
        let absolute_path = normalize_path_allow_missing(&change.path)?;
        let entry = by_path.entry(absolute_path.clone()).or_insert_with(|| {
            order.push(absolute_path.clone());
            let original_bytes = fs::read(&absolute_path).ok();
            let original = original_bytes
                .as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .map(|text| strip_bom(text).to_string());
            PreparedFile {
                absolute_path: absolute_path.clone(),
                content: original.clone().unwrap_or_default(),
                original,
                original_bytes,
            }
        });

        if change.old_string.is_empty() {
            // Whole-file create/overwrite.
            entry.content.clone_from(&change.new_string);
            continue;
        }
        if change.old_string == change.new_string {
            return Err(invalid_input(format!(
                "old_string and new_string must differ for {}",
                change.path
            )));
        }
        // A file that exists but does not decode as UTF-8 can never contain the
        // `old_string`. Say that plainly instead of "old_string not found",
        // which would send the caller off to re-read a file it can never match.
        if entry.original_bytes.is_some() && entry.original.is_none() {
            return Err(invalid_input(format!(
                "{} is not valid UTF-8; apply_patch edits text only. Use an empty old_string \
                 to replace the whole file.",
                change.path
            )));
        }
        let matches = entry.content.matches(change.old_string.as_str()).count();
        if matches == 0 {
            return Err(not_found(format!(
                "old_string not found in {}{}",
                change.path,
                nearest_snippet(&entry.content, &change.old_string).unwrap_or_default()
            )));
        }
        if matches > 1 && !change.replace_all {
            return Err(invalid_input(format!(
                "old_string matches {matches} times in {}; set replace_all or make it unique",
                change.path
            )));
        }
        entry.content = if change.replace_all {
            entry
                .content
                .replace(&change.old_string, &change.new_string)
        } else {
            entry
                .content
                .replacen(&change.old_string, &change.new_string, 1)
        };
    }

    // Everything validated: now write, in first-seen order. Validation above is
    // what makes the batch atomic against *bad input*; this loop is what makes
    // it atomic against *bad timing*. Without a journal, an I/O failure partway
    // through leaves the caller holding an error it cannot act on — the files it
    // had already changed still hold their new content, so the `old_string`
    // values it would retry with no longer exist. Roll the batch back so the
    // failure returns the workspace to the state the caller still believes in.
    let mut results = Vec::with_capacity(order.len());
    let mut written: Vec<&PathBuf> = Vec::with_capacity(order.len());
    for absolute_path in &order {
        let prepared = &by_path[absolute_path];
        if let Some(parent) = prepared.absolute_path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                return Err(write_failure(
                    error,
                    &prepared.absolute_path,
                    &written,
                    &by_path,
                ));
            }
        }
        if let Err(error) = write_text_atomic(&prepared.absolute_path, &prepared.content) {
            return Err(write_failure(
                error,
                &prepared.absolute_path,
                &written,
                &by_path,
            ));
        }
        written.push(absolute_path);
        // Existence comes from the byte read, not from whether the content
        // decoded as text: an existing-but-binary file is an *update*. Its
        // hunks are omitted rather than drawn against an empty string, which
        // would claim the file used to be empty.
        let (kind, structured_patch) = match (&prepared.original, &prepared.original_bytes) {
            (Some(original), _) => (
                String::from("update"),
                make_patch(original, &prepared.content),
            ),
            (None, Some(_)) => (String::from("update"), Vec::new()),
            (None, None) => (String::from("create"), make_patch("", &prepared.content)),
        };
        results.push(PatchFileResult {
            file_path: prepared.absolute_path.to_string_lossy().into_owned(),
            kind,
            structured_patch,
        });
    }

    let files_changed = results.len();
    Ok(ApplyPatchOutput {
        files_changed,
        results,
    })
}

/// Undo a half-applied batch and describe the outcome.
///
/// Restoration is best-effort, but its own failures are surfaced rather than
/// swallowed: the caller retries based on this message, so silently dropping a
/// failed restore would turn the report into a lie and reintroduce exactly the
/// ambiguity the rollback exists to remove.
fn write_failure(
    error: io::Error,
    failed_path: &Path,
    written: &[&PathBuf],
    by_path: &HashMap<PathBuf, PreparedFile>,
) -> io::Error {
    let mut restore_errors = Vec::new();
    for path in written.iter().rev() {
        let prepared = &by_path[*path];
        let restore = match &prepared.original_bytes {
            // Byte-for-byte, so a non-UTF-8 file comes back unchanged.
            Some(original) => write_bytes_atomic(path, original),
            // The file did not exist before this batch, so its creation is the
            // thing to undo. A concurrent delete is fine — the goal is absence.
            None => fs::remove_file(path).or_else(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(error)
                }
            }),
        };
        if let Err(error) = restore {
            restore_errors.push(format!("{}: {error}", path.display()));
        }
    }

    let mut message = format!(
        "write failed for {}: {error}; rolled back {} previously written file(s), \
         so the batch left nothing on disk",
        failed_path.display(),
        written.len()
    );
    if !restore_errors.is_empty() {
        message.push_str(&format!(
            "; rollback failures: {}",
            restore_errors.join(", ")
        ));
    }
    io::Error::new(error.kind(), message)
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn not_found(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, message.into())
}

/// Atomic text write: stage the content in a sibling temp file, fsync, then
/// rename over the target. A crash mid-write leaves the previous content
/// intact instead of a truncated file. (`std::fs::rename` replaces existing
/// files on both Unix and Windows.)
fn write_text_atomic(path: &Path, content: &str) -> io::Result<()> {
    write_bytes_atomic(path, content.as_bytes())
}

/// The byte-level primitive behind [`write_text_atomic`]. Rollback restores the
/// exact bytes it read, so a binary file that a batch overwrote comes back
/// byte-identical instead of being re-encoded through `&str`.
fn write_bytes_atomic(path: &Path, content: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let name = path.file_name().map_or_else(
        || std::ffi::OsString::from("hf-file"),
        std::ffi::OsStr::to_os_string,
    );
    let tmp_path = path.with_file_name(format!(
        ".{}.hf-tmp-{}-{}",
        name.to_string_lossy(),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));

    let result = (|| {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// One or more glob patterns over the shared `ignore` crawler (ripgrep's
/// `globset` semantics). A single pattern keeps the literal-prefix walk-root
/// optimization; several patterns walk the common base and match with one
/// `GlobSet`, so cost grows with files visited, not with pattern count.
pub fn glob_search(patterns: &[String], path: Option<&str>) -> io::Result<GlobSearchOutput> {
    with_read_retry(|| glob_search_once(patterns, path))
}

/// Split one glob into its literal directory prefix (walk root candidate)
/// and the wildcarded remainder joined with the platform separator.
fn split_glob(full: &str) -> (std::path::PathBuf, Option<String>) {
    let mut root = std::path::PathBuf::new();
    let mut rest: Vec<String> = Vec::new();
    let mut wildcard_seen = false;
    for component in Path::new(full).components() {
        let text = component.as_os_str().to_string_lossy();
        if wildcard_seen || text.contains(['*', '?', '[', ']']) {
            wildcard_seen = true;
            rest.push(text.into_owned());
        } else {
            root.push(component.as_os_str());
        }
    }
    let separator = if cfg!(windows) { '\\' } else { '/' };
    let rest = if rest.is_empty() {
        None
    } else {
        Some(rest.join(&separator.to_string()))
    };
    (root, rest)
}

fn glob_search_once(patterns: &[String], path: Option<&str>) -> io::Result<GlobSearchOutput> {
    let started = Instant::now();
    if patterns.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "at least one glob pattern is required",
        ));
    }
    let base_dir = path
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let absolutize = |pattern: &str| -> String {
        if Path::new(pattern).is_absolute() {
            pattern.to_owned()
        } else {
            base_dir.join(pattern).to_string_lossy().into_owned()
        }
    };

    let mut matches = if patterns.len() == 1 {
        collect_single_glob(&absolutize(&patterns[0]))?
    } else {
        collect_multi_glob(&base_dir, patterns.iter().map(|p| absolutize(p)))?
    };

    matches.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(Reverse)
    });

    let truncated = matches.len() > 100;
    let filenames = matches
        .into_iter()
        .take(100)
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    Ok(GlobSearchOutput {
        duration_ms: started.elapsed().as_millis(),
        num_files: filenames.len(),
        filenames,
        truncated,
    })
}

/// Single pattern: walk only the literal prefix of the glob, so a query like
/// `crates/**/*.rs` never leaves `crates/`.
fn collect_single_glob(full: &str) -> io::Result<Vec<std::path::PathBuf>> {
    let (root, rest) = split_glob(full);
    let Some(rest_pattern) = rest else {
        // Literal path with no wildcard at all.
        return Ok(if fs::metadata(&root).is_ok_and(|m| m.is_file()) {
            vec![root]
        } else {
            Vec::new()
        });
    };
    let glob_set = globset::GlobSetBuilder::new()
        .add(
            globset::GlobBuilder::new(&rest_pattern)
                .literal_separator(true)
                .build()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?,
        )
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    Ok(collect_glob_matches(&root, |relative, _| {
        glob_set.is_match(relative)
    }))
}

/// Several patterns in one crawl: relative globs match entries under the
/// base; absolute globs cover out-of-base roots. Matching cost grows with
/// files visited, not with pattern count (aho-corasick prefilter inside).
fn collect_multi_glob<I: IntoIterator<Item = String>>(
    base_dir: &Path,
    full_patterns: I,
) -> io::Result<Vec<std::path::PathBuf>> {
    let build_glob = |pattern: &str| -> io::Result<globset::Glob> {
        globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))
    };
    let mut relative_globs = globset::GlobSetBuilder::new();
    let mut absolute_globs = globset::GlobSetBuilder::new();
    for full in full_patterns {
        match Path::new(&full).strip_prefix(base_dir) {
            Ok(relative) => relative_globs.add(build_glob(&relative.to_string_lossy())?),
            Err(_) => absolute_globs.add(build_glob(&full)?),
        };
    }
    let relative_set = relative_globs
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let absolute_set = absolute_globs
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    Ok(collect_glob_matches(base_dir, |relative, absolute| {
        relative_set.is_match(relative) || absolute_set.is_match(absolute)
    }))
}

/// Non-interactive fuzzy file finder — the machine-friendly answer to `fzf`.
/// Ranks files under `path` by an fzf-style fuzzy match of `query` against each
/// path (nucleo is the library the fzf engine is built on) and returns the best
/// `limit` absolute paths, best match first. It reuses the shared `ignore`
/// crawler, so `.gitignore`/build caches are respected without a human TTY.
pub fn search_files(
    query: &str,
    path: Option<&str>,
    limit: Option<usize>,
) -> io::Result<SearchFilesOutput> {
    with_read_retry(|| search_files_once(query, path, limit))
}

fn search_files_once(
    query: &str,
    path: Option<&str>,
    limit: Option<usize>,
) -> io::Result<SearchFilesOutput> {
    let started = Instant::now();
    if query.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "search query must not be empty",
        ));
    }
    let base = path
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let cap = limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);

    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(query, &mut needle_buf);

    let mut scored: Vec<(u32, String)> = Vec::new();
    for candidate in collect_search_files(&base) {
        // Score against the path relative to the search root (matches folder and
        // file names, like `fzf`), but hand back the absolute path for reuse.
        let display = candidate
            .strip_prefix(&base)
            .unwrap_or(candidate.as_path())
            .to_string_lossy();
        let mut haystack_buf = Vec::with_capacity(display.chars().count());
        let haystack = Utf32Str::new(&display, &mut haystack_buf);
        if let Some(score) = matcher.fuzzy_match(haystack, needle) {
            scored.push((u32::from(score), candidate.to_string_lossy().into_owned()));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let truncated = scored.len() > cap;
    let filenames = scored
        .into_iter()
        .take(cap)
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    Ok(SearchFilesOutput {
        duration_ms: started.elapsed().as_millis(),
        num_files: filenames.len(),
        filenames,
        truncated,
    })
}

pub fn grep_search(input: &GrepSearchInput) -> io::Result<GrepSearchOutput> {
    with_read_retry(|| grep_search_once(input))
}

fn grep_search_once(input: &GrepSearchInput) -> io::Result<GrepSearchOutput> {
    let base_path = input
        .path
        .as_deref()
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);

    let regex = RegexBuilder::new(&input.pattern)
        .case_insensitive(input.case_insensitive.unwrap_or(false))
        .dot_matches_new_line(input.multiline.unwrap_or(false))
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let glob_filter = input
        .glob
        .as_deref()
        .map(Pattern::new)
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let file_type = input.file_type.as_deref();
    let output_mode = input
        .output_mode
        .as_deref()
        .map(str::trim)
        .unwrap_or("files_with_matches")
        .to_owned();
    // Validate the mode instead of letting anything unrecognised fall through to
    // `files_with_matches`: a caller that asked for `count` and mistyped it got a
    // silent file list with `num_matches: null`, which reads as "no matches" and
    // sends the search in the wrong direction.
    if !matches!(
        output_mode.as_str(),
        "files_with_matches" | "content" | "count"
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unknown output_mode `{output_mode}`; expected one of \
                 `files_with_matches`, `content`, `count`"
            ),
        ));
    }
    let context = input.context.or(input.context_short).unwrap_or(0);

    // ripgrep's shape: the whole per-file pipeline (filter -> read -> sniff ->
    // regex) runs on the walker's worker threads, so cold-page IO and matching
    // overlap across cores. Each visitor accumulates thread-local hits; the
    // single shared sink is touched once per file.
    let results = Mutex::new(Vec::<FileScan>::new());
    let seen_files = AtomicUsize::new(0);
    let content_budget = AtomicUsize::new(MAX_GREP_CONTENT_LINES);

    build_search_walker(&base_path).run(|| {
        // Shared state is captured by reference (`Mutex`/`AtomicUsize` are
        // `Sync`), so every per-thread visitor sees the same sink.
        Box::new(|entry: Result<ignore::DirEntry, ignore::Error>| {
            let Ok(entry) = entry else {
                return WalkState::Continue; // one unreadable entry never kills the walk
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            // Bound the walk like the sequential crawler did.
            if seen_files.fetch_add(1, Ordering::Relaxed) >= MAX_SEARCH_FILES {
                return WalkState::Quit;
            }
            let path = entry.path();
            if !matches_optional_filters(path, glob_filter.as_ref(), file_type) {
                return WalkState::Continue;
            }
            if let Some(scan) =
                scan_file(path, &regex, input, &output_mode, context, &content_budget)
            {
                results
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(scan);
            }
            WalkState::Continue
        })
    });

    let mut scans = results.into_inner().unwrap_or_default();
    // Deterministic output regardless of worker scheduling: group by path.
    scans.sort_unstable_by(|a, b| {
        a.path
            .as_os_str()
            .as_encoded_bytes()
            .cmp(b.path.as_os_str().as_encoded_bytes())
    });

    let mut filenames = Vec::with_capacity(scans.len());
    let mut content_lines = Vec::new();
    let mut total_matches = 0usize;
    for scan in &scans {
        filenames.push(scan.path.to_string_lossy().into_owned());
        if output_mode == "count" {
            total_matches += scan.count;
        } else if output_mode == "content" {
            content_lines.extend(scan.content_lines.iter().cloned());
        } else {
            total_matches += scan.line_matches;
        }
    }

    let (filenames, applied_limit, applied_offset) =
        apply_limit(filenames, input.head_limit, input.offset);
    let mode_label = if output_mode == "content" {
        let (lines, limit, offset) = apply_limit(content_lines, input.head_limit, input.offset);
        return Ok(GrepSearchOutput {
            mode: Some(output_mode),
            num_files: filenames.len(),
            filenames,
            num_lines: Some(lines.len()),
            content: Some(lines.join("\n")),
            num_matches: None,
            applied_limit: limit,
            applied_offset: offset,
        });
    } else {
        None
    };

    Ok(GrepSearchOutput {
        mode: Some(output_mode.clone()),
        num_files: filenames.len(),
        filenames,
        content: mode_label,
        num_lines: None,
        num_matches: (output_mode == "count").then_some(total_matches),
        applied_limit,
        applied_offset,
    })
}

/// One file's grep result, produced entirely on a walker thread and only then
/// handed to the shared sink.
struct FileScan {
    path: PathBuf,
    /// `count`-mode regex match count.
    count: usize,
    /// Number of matching lines (all modes except `count`).
    line_matches: usize,
    /// Rendered content-mode lines (path:line: prefix + text, context included).
    content_lines: Vec<String>,
}

/// Read + scan one file for the grep pipeline. Returns `None` for filtered-out,
/// oversized, binary, or non-UTF-8 files. The regex is shared (`Regex` is
/// `Sync`); the content budget bounds worst-case memory across all workers.
fn scan_file(
    path: &Path,
    regex: &Regex,
    input: &GrepSearchInput,
    output_mode: &str,
    context: usize,
    content_budget: &AtomicUsize,
) -> Option<FileScan> {
    // Skip oversized files before pulling them into RAM.
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_GREP_FILE_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    // Binary sniff: a NUL byte in the first 8 KiB (ripgrep's heuristic) means
    // the file is not worth regexing or feeding to a model.
    let sniff_len = bytes.len().min(BINARY_SNIFF_BYTES);
    if memchr::memchr(0, &bytes[..sniff_len]).is_some() {
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    if output_mode == "count" {
        let count = regex.find_iter(text).count();
        return (count > 0).then(|| FileScan {
            path: path.to_path_buf(),
            count,
            line_matches: 0,
            content_lines: Vec::new(),
        });
    }

    // `files_with_matches` (the default) only reports *how many* lines match,
    // never *which* ones, so it counts in one pass and allocates nothing per
    // match. Only `content` mode needs the indices, because those address the
    // per-match context windows below.
    if output_mode != "content" {
        let line_matches = text.lines().filter(|line| regex.is_match(line)).count();
        return (line_matches > 0).then(|| FileScan {
            path: path.to_path_buf(),
            count: 0,
            line_matches,
            content_lines: Vec::new(),
        });
    }

    let matched_lines: Vec<usize> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| regex.is_match(line))
        .map(|(index, _)| index)
        .collect();
    if matched_lines.is_empty() {
        return None;
    }

    let mut content_lines = Vec::new();
    if output_mode == "content" {
        let lines: Vec<&str> = text.lines().collect();
        for index in &matched_lines {
            let start = index.saturating_sub(input.before.unwrap_or(context));
            let end = (index + input.after.unwrap_or(context) + 1).min(lines.len());
            for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                // Budget guard: 0 means exhausted (never decrement through it).
                // `fetch_update` is the stable CAS loop; on 0 it errs untouched.
                if content_budget
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_err()
                {
                    return Some(FileScan {
                        path: path.to_path_buf(),
                        count: 0,
                        line_matches: matched_lines.len(),
                        content_lines,
                    });
                }
                let prefix = if input.line_numbers.unwrap_or(true) {
                    format!("{}:{}:", path.to_string_lossy(), i + 1)
                } else {
                    format!("{}:", path.to_string_lossy())
                };
                content_lines.push(format!("{prefix}{line}"));
            }
        }
    }

    Some(FileScan {
        path: path.to_path_buf(),
        count: 0,
        line_matches: matched_lines.len(),
        content_lines,
    })
}

/// Shared crawler for grep/fuzzy-search: ripgrep's `ignore` walker, parallel
/// build. Honours `.gitignore`/`.ignore` even outside a git checkout
/// (`require_git(false)`), always skips `.git`; hidden files stay searchable to
/// preserve prior behaviour, and `SKIP_DIRS` prunes forgotten build caches.
fn build_search_walker(base_path: &Path) -> ignore::WalkParallel {
    let mut builder = ignore::WalkBuilder::new(base_path);
    builder
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| {
            !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| SKIP_DIRS.contains(&name))
        });
    builder.build_parallel()
}

/// Walk `root` in parallel and collect every *file* the predicate accepts,
/// handing it both the path relative to `root` (for relative globs) and the
/// full path (for absolute ones).
///
/// Shares the grep crawler deliberately: `.gitignore` / `.ignore` handling, the
/// `SKIP_DIRS` prune, and the visited-file budget now behave identically across
/// every search tool. The glob path previously ran its own serial `WalkBuilder`,
/// which is how the two configurations drifted apart — and why a query against
/// a build tree could stall where grep would have pruned it.
fn collect_glob_matches<F>(root: &Path, accept: F) -> Vec<PathBuf>
where
    F: Fn(&Path, &Path) -> bool + Send + Sync,
{
    let matches = Mutex::new(Vec::<PathBuf>::new());
    let seen = AtomicUsize::new(0);
    build_search_walker(root).run(|| {
        Box::new(|entry: Result<ignore::DirEntry, ignore::Error>| {
            let Ok(entry) = entry else {
                return WalkState::Continue; // one unreadable entry never kills the walk
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            if seen.fetch_add(1, Ordering::Relaxed) >= MAX_SEARCH_FILES {
                return WalkState::Quit;
            }
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(path);
            if accept(relative, path) {
                matches
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(path.to_path_buf());
            }
            WalkState::Continue
        })
    });
    matches.into_inner().unwrap_or_default()
}

fn collect_search_files(base_path: &Path) -> Vec<PathBuf> {
    if base_path.is_file() {
        return vec![base_path.to_path_buf()];
    }

    // Parallel crawl (the same WalkParallel ripgrep uses): directory IO is the
    // bottleneck for the fuzzy finder, so spread it across cores. Errors on
    // single entries are skipped rather than aborting the whole enumeration.
    let files = Mutex::new(Vec::<PathBuf>::new());
    let seen = AtomicUsize::new(0);
    build_search_walker(base_path).run(|| {
        Box::new(|entry: Result<ignore::DirEntry, ignore::Error>| {
            if let Ok(entry) = entry {
                if entry.file_type().is_some_and(|t| t.is_file())
                    && seen.fetch_add(1, Ordering::Relaxed) < MAX_SEARCH_FILES
                {
                    files
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(entry.path().to_path_buf());
                } else if entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });
    files.into_inner().unwrap_or_default()
}

fn matches_optional_filters(
    path: &Path,
    glob_filter: Option<&Pattern>,
    file_type: Option<&str>,
) -> bool {
    if let Some(glob_filter) = glob_filter {
        let path_string = path.to_string_lossy();
        if !glob_filter.matches(&path_string) && !glob_filter.matches_path(path) {
            return false;
        }
    }

    if let Some(file_type) = file_type {
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension != Some(file_type) {
            return false;
        }
    }

    true
}

fn apply_limit<T>(
    items: Vec<T>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> (Vec<T>, Option<usize>, Option<usize>) {
    let offset_value = offset.unwrap_or(0);
    let mut items = items.into_iter().skip(offset_value).collect::<Vec<_>>();
    let explicit_limit = limit.unwrap_or(250);
    if explicit_limit == 0 {
        return (items, None, (offset_value > 0).then_some(offset_value));
    }

    let truncated = items.len() > explicit_limit;
    items.truncate(explicit_limit);
    (
        items,
        truncated.then_some(explicit_limit),
        (offset_value > 0).then_some(offset_value),
    )
}

/// "Did you mean" hint for a failed exact replacement. Scans every window of
/// the same line count as `old_string`: a cheap jaro-winkler pass ranks all
/// candidates, the top few get a precise normalized-levenshtein score. Below
/// the threshold nothing is returned - a wrong guess costs more than no hint.
/// Returns `"; closest text at line N (82% similar):\n..."` ready to append.
#[must_use]
fn nearest_snippet(content: &str, old_string: &str) -> Option<String> {
    const MIN_SIMILARITY: f64 = 0.7;
    const COARSE_THRESHOLD: f64 = 0.5;
    const MAX_WINDOWS: usize = 50_000;

    let needle = old_string.trim_end();
    if needle.is_empty() {
        return None;
    }
    let span = needle.lines().count().max(1);
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() < span || lines.len() - span + 1 > MAX_WINDOWS {
        return None;
    }

    // A single-line needle is the overwhelmingly common case, and there the
    // window *is* the line — borrowing it avoids one `String` allocation per
    // candidate, up to `MAX_WINDOWS` of them before any ranking happens.
    // Multi-line needles still materialise the joined window; the algorithm is
    // unchanged, only the allocation count.
    let window_at = |start: usize| -> Cow<'_, str> {
        if span == 1 {
            Cow::Borrowed(lines[start])
        } else {
            Cow::Owned(lines[start..start + span].join("\n"))
        }
    };
    let mut coarse: Vec<(usize, f64)> = (0..=lines.len() - span)
        .map(|start| (start, strsim::jaro_winkler(needle, &window_at(start))))
        .collect();
    coarse.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut best: Option<(usize, f64)> = None;
    for (start, score) in coarse.into_iter().take(10) {
        if score < COARSE_THRESHOLD {
            break;
        }
        let fine = strsim::normalized_levenshtein(needle, &window_at(start));
        if best.is_none_or(|(_, top)| fine > top) {
            best = Some((start, fine));
        }
    }
    let (start, similarity) = best?;
    if similarity < MIN_SIMILARITY {
        return None;
    }
    let mut snippet = window_at(start).into_owned();
    if snippet.len() > 500 {
        snippet.truncate(500);
        snippet.push_str("...");
    }
    Some(format!(
        "; closest text at line {} ({:.0}% similar):\n{snippet}",
        start + 1,
        similarity * 100.0
    ))
}

/// Build real line-level diff hunks (with surrounding context) between the
/// original and updated text. Uses `similar` (Myers) rather than a whole-file
/// `-old/+new` dump, so callers and the model see a minimal, reviewable patch.
/// Adjacent edits closer than twice the context radius collapse into one hunk;
/// an unchanged file yields no hunks at all.
fn make_patch(original: &str, updated: &str) -> Vec<StructuredPatchHunk> {
    const CONTEXT: usize = 3;
    let diff = TextDiff::from_lines(original, updated);
    let changes: Vec<_> = diff.iter_all_changes().collect();

    // Positions of the lines that actually changed.
    let interesting: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| change.tag() != ChangeTag::Equal)
        .map(|(index, _)| index)
        .collect();
    if interesting.is_empty() {
        return Vec::new();
    }

    // Group change positions whose context windows would touch each other.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for index in interesting {
        match groups.last_mut() {
            Some(last) if index - last.1 <= CONTEXT * 2 => last.1 = index,
            _ => groups.push((index, index)),
        }
    }

    let mut hunks = Vec::new();
    let mut prev_end: Option<usize> = None;
    for (first, last) in groups {
        let start = prev_end
            .map_or(first, |end| end + 1)
            .max(first.saturating_sub(CONTEXT));
        let end = (last + CONTEXT).min(changes.len() - 1);
        prev_end = Some(end);

        let slice = &changes[start..=end];
        let mut lines = Vec::with_capacity(slice.len());
        let mut old_lines = 0usize;
        let mut new_lines = 0usize;
        let mut old_start: Option<usize> = None;
        let mut new_start: Option<usize> = None;
        for change in slice {
            let prefix = match change.tag() {
                ChangeTag::Equal => ' ',
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
            };
            // `similar` reports 0-based line indices; the patch format is 1-based.
            if let Some(index) = change.old_index() {
                old_lines += 1;
                old_start = Some(old_start.unwrap_or(index));
            }
            if let Some(index) = change.new_index() {
                new_lines += 1;
                new_start = Some(new_start.unwrap_or(index));
            }
            let text = change.value().trim_end_matches(['\r', '\n']);
            lines.push(format!("{prefix}{text}"));
        }

        hunks.push(StructuredPatchHunk {
            old_start: old_start.unwrap_or(0) + 1,
            old_lines,
            new_start: new_start.unwrap_or(0) + 1,
            new_lines,
            lines,
        });
    }
    fold_oversized_hunks(hunks)
}

/// Cap the diff the model receives. A whole-file rewrite can legitimately
/// produce thousands of diff lines; echoing them all back burns context
/// without adding review value (the write already succeeded and the model
/// knows its own content). Past the cap, keep head/tail of each hunk and a
/// factual omission note; hunk line counts stay truthful.
fn fold_oversized_hunks(mut hunks: Vec<StructuredPatchHunk>) -> Vec<StructuredPatchHunk> {
    const MAX_DIFF_LINES: usize = 400;
    const KEEP_HEAD: usize = 12;
    const KEEP_TAIL: usize = 8;

    let total: usize = hunks.iter().map(|hunk| hunk.lines.len()).sum();
    if total <= MAX_DIFF_LINES {
        return hunks;
    }
    for hunk in &mut hunks {
        if hunk.lines.len() <= KEEP_HEAD + KEEP_TAIL {
            continue;
        }
        let omitted = hunk.lines.len() - KEEP_HEAD - KEEP_TAIL;
        let mut folded = Vec::with_capacity(KEEP_HEAD + KEEP_TAIL + 1);
        folded.extend(hunk.lines[..KEEP_HEAD].iter().cloned());
        folded.push(format!(
            "... {omitted} diff lines omitted (the edit itself was applied in full) ..."
        ));
        folded.extend(hunk.lines[hunk.lines.len() - KEEP_TAIL..].iter().cloned());
        hunk.lines = folded;
    }
    hunks
}

/// Strip a leading UTF-8 BOM so first-line matching and JSON parsing work.
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{FEFF}').unwrap_or(content)
}

/// `canonicalize` returns `\\?\C:\...` on Windows; that prefix breaks
/// display, comparison, and most shell tools. Produce a clean absolute path.
fn clean_canonical(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{stripped}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

fn normalize_path(path: &str) -> io::Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    candidate.canonicalize().map(clean_canonical)
}

fn normalize_path_allow_missing(path: &str) -> io::Result<PathBuf> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()?.join(path)
    };

    if let Ok(canonical) = candidate.canonicalize() {
        return Ok(clean_canonical(canonical));
    }

    if let Some(parent) = candidate.parent() {
        let canonical_parent = parent
            .canonicalize()
            .map_or_else(|_| parent.to_path_buf(), clean_canonical);
        if let Some(name) = candidate.file_name() {
            return Ok(canonical_parent.join(name));
        }
    }

    Ok(candidate)
}

/// Whether a model-supplied path resolves somewhere OUTSIDE `root`.
///
/// This is the check behind `workspace-write`'s write confinement: the mode
/// name promises that mutations stay in the workspace, and before this existed
/// nothing enforced it — `write_file`/`edit_file`/`apply_patch` were plain
/// `Allow`, so `../../etc/hosts` or `C:\Users\me\.ssh\config` went through
/// untouched.
///
/// Correctness notes, in the order the work happens:
/// - a relative path is taken against `root`;
/// - `..` is **not** folded lexically: it is resolved by the filesystem during
///   canonicalization. POSIX resolves `..` *after* symlink substitution, so
///   folding `ws/link/../x` to `ws/x` before probing would bless a write that
///   actually lands beside the link's target — a platform-conditional bypass.
///   (Windows' Win32 layer folds `..` lexically before the object manager sees
///   the path, so `canonicalize` already matches real resolution there.)
/// - the **longest existing ancestor** is canonicalized, so a symlinked
///   directory cannot launder an escape (`ws/link -> /etc`, `ws/link/passwd`).
///   The final component is allowed not to exist yet — writes create files —
///   which is exactly why the ancestor, not the whole path, is resolved;
/// - comparison is component-wise and case-insensitive on Windows, so
///   `C:\ws` does not "contain" `C:\ws2`.
///
/// Fails **closed**: a candidate that cannot be proven to stay inside `root` is
/// reported as an escape, so the failure mode is "ask the human", never
/// "silently allow". That includes a `root` that cannot itself be resolved to
/// an absolute path (deleted cwd, empty path): against an empty base every
/// candidate would pass, so the gate reports escape instead.
///
/// Both sides are resolved the same way, so a `root` that does not exist yet
/// (`.heartflow/plans`, before the first plan is written) is still usable: only
/// the components that exist are canonicalized on either side.
#[must_use]
pub fn escapes_workspace(root: &Path, candidate: impl AsRef<Path>) -> bool {
    let root_real = resolve_existing_prefix(root);
    // A root that cannot be resolved to an absolute path proves nothing:
    // `has_prefix` against an empty base accepts every candidate.
    if !root_real.is_absolute() {
        return true;
    }
    let raw = candidate.as_ref();
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let resolved = resolve_existing_prefix(&joined);
    !has_prefix(&resolved, &root_real)
}

/// Canonicalize the longest existing prefix of `path`, then re-append the
/// components that do not exist yet. Mirrors what a create would do while still
/// resolving every symlink on the way down.
///
/// Falls back to returning `path` unchanged when nothing along it exists, which
/// the caller reports as an escape.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut probe = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut resolved = clean_canonical(real);
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        let Some(name) = probe.file_name().map(std::ffi::OsStr::to_os_string) else {
            return path.to_path_buf();
        };
        tail.push(name);
        if !probe.pop() {
            return path.to_path_buf();
        }
    }
}

/// Component-wise prefix test. Windows paths are case-insensitive, so compare
/// each component lowercased there; a string-level test would wrongly accept
/// `C:\ws2` as living under `C:\ws`.
fn has_prefix(path: &Path, base: &Path) -> bool {
    let mut rest = path.components();
    for expected in base.components() {
        match rest.next() {
            Some(actual) if component_eq(actual, expected) => {}
            _ => return false,
        }
    }
    true
}

fn component_eq(left: std::path::Component<'_>, right: std::path::Component<'_>) -> bool {
    if cfg!(windows) {
        left.as_os_str().to_string_lossy().to_lowercase()
            == right.as_os_str().to_string_lossy().to_lowercase()
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        apply_patch, edit_file, escapes_workspace, glob_search, grep_search, make_patch, read_file,
        search_files, with_read_retry, write_file, ApplyPatchOutput, GrepSearchInput, PatchChange,
    };

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir().join(format!("clawd-native-{name}-{unique}"))
    }

    #[test]
    fn reads_and_writes_files() {
        let path = temp_path("read-write.txt");
        let write_output = write_file(path.to_string_lossy().as_ref(), "one\ntwo\nthree")
            .expect("write should succeed");
        assert_eq!(write_output.kind, "create");

        let read_output = read_file(path.to_string_lossy().as_ref(), Some(1), Some(1))
            .expect("read should succeed");
        assert_eq!(read_output.file.content, "two");
    }

    #[test]
    fn transient_read_errors_are_retried_once() {
        use std::io;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let attempts = AtomicUsize::new(0);
        let result: io::Result<u32> = with_read_retry(|| {
            let attempt = attempts.fetch_add(1, SeqCst);
            if attempt == 0 {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                Ok(7)
            }
        });
        assert_eq!(result.expect("retry should succeed"), 7);
        assert_eq!(attempts.load(SeqCst), 2);
    }

    #[test]
    fn permanent_read_errors_are_not_retried() {
        use std::io;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let attempts = AtomicUsize::new(0);
        let result: io::Result<u32> = with_read_retry(|| {
            attempts.fetch_add(1, SeqCst);
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert!(result.is_err());
        assert_eq!(attempts.load(SeqCst), 1);
    }

    #[test]
    fn edits_file_contents() {
        let path = temp_path("edit.txt");
        write_file(path.to_string_lossy().as_ref(), "alpha beta alpha")
            .expect("initial write should succeed");
        let output = edit_file(path.to_string_lossy().as_ref(), "alpha", "omega", true)
            .expect("edit should succeed");
        assert!(output.replace_all);
    }

    #[test]
    fn search_files_ranks_fuzzy_matches() {
        let dir = temp_path("fuzzy-dir");
        std::fs::create_dir_all(dir.join("src")).expect("src dir");
        std::fs::create_dir_all(dir.join("docs")).expect("docs dir");
        write_file(
            dir.join("src")
                .join("handler.rs")
                .to_string_lossy()
                .as_ref(),
            "x",
        )
        .expect("write handler");
        write_file(
            dir.join("docs")
                .join("README.md")
                .to_string_lossy()
                .as_ref(),
            "y",
        )
        .expect("write readme");

        let output = search_files("hndlr", Some(dir.to_string_lossy().as_ref()), None)
            .expect("fuzzy search should succeed");
        assert_eq!(output.num_files, 1, "only handler.rs matches hndlr");
        assert!(output.filenames[0].ends_with("handler.rs"));
    }

    #[test]
    fn search_files_rejects_empty_query() {
        assert!(search_files("   ", None, None).is_err());
    }

    #[test]
    fn apply_patch_edits_multiple_files_atomically() {
        let dir = temp_path("patch-dir");
        std::fs::create_dir_all(&dir).expect("dir");
        let a = dir.join("a.txt");
        write_file(a.to_string_lossy().as_ref(), "hello world").expect("write a");
        let b = dir.join("nested").join("b.txt");

        let output: ApplyPatchOutput = apply_patch(&[
            PatchChange {
                path: a.to_string_lossy().into_owned(),
                old_string: "world".to_string(),
                new_string: "there".to_string(),
                replace_all: false,
            },
            PatchChange {
                path: b.to_string_lossy().into_owned(),
                old_string: String::new(),
                new_string: "created".to_string(),
                replace_all: false,
            },
        ])
        .expect("patch should apply");

        assert_eq!(output.files_changed, 2);
        assert_eq!(output.results[0].kind, "update");
        assert_eq!(output.results[1].kind, "create");
        assert_eq!(
            read_file(a.to_string_lossy().as_ref(), None, None)
                .expect("read a")
                .file
                .content,
            "hello there"
        );
        assert_eq!(
            read_file(b.to_string_lossy().as_ref(), None, None)
                .expect("read b")
                .file
                .content,
            "created"
        );
    }

    #[test]
    fn apply_patch_rolls_back_when_a_later_write_fails() {
        let dir = temp_path("patch-rollback");
        std::fs::create_dir_all(&dir).expect("dir");
        let first = dir.join("first.txt");
        write_file(first.to_string_lossy().as_ref(), "hello world").expect("write first");

        // A plain *file* sitting where the second change needs a directory:
        // `create_dir_all` fails only after `first.txt` has been rewritten, so
        // the batch reaches the write loop and dies halfway through it.
        let blocker = dir.join("blocked");
        write_file(blocker.to_string_lossy().as_ref(), "not a directory").expect("write blocker");
        let second = blocker.join("nested.txt");

        let error = apply_patch(&[
            PatchChange {
                path: first.to_string_lossy().into_owned(),
                old_string: "world".to_string(),
                new_string: "there".to_string(),
                replace_all: false,
            },
            PatchChange {
                path: second.to_string_lossy().into_owned(),
                old_string: String::new(),
                new_string: "created".to_string(),
                replace_all: false,
            },
        ])
        .expect_err("the second write must fail");

        assert!(
            error.to_string().contains("rolled back"),
            "error should report the rollback: {error}"
        );
        // The batch must be a no-op from the caller's point of view: the first
        // file's pre-batch content is back, so its `old_string` still exists and
        // a retry remains valid.
        assert_eq!(
            read_file(first.to_string_lossy().as_ref(), None, None)
                .expect("read first")
                .file
                .content,
            "hello world"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_rollback_deletes_files_the_batch_created() {
        let dir = temp_path("patch-rollback-create");
        std::fs::create_dir_all(&dir).expect("dir");

        // Nothing exists yet, so the rollback has to *remove* rather than
        // restore — a create-then-fail batch must not leave a stray file behind.
        let created = dir.join("brand-new.txt");
        let blocker = dir.join("blocked");
        write_file(blocker.to_string_lossy().as_ref(), "not a directory").expect("write blocker");

        let error = apply_patch(&[
            PatchChange {
                path: created.to_string_lossy().into_owned(),
                old_string: String::new(),
                new_string: "should not survive".to_string(),
                replace_all: false,
            },
            PatchChange {
                path: blocker.join("nested.txt").to_string_lossy().into_owned(),
                old_string: String::new(),
                new_string: "created".to_string(),
                replace_all: false,
            },
        ])
        .expect_err("the second write must fail");

        assert!(
            error.to_string().contains("rolled back"),
            "error should report the rollback: {error}"
        );
        assert!(
            !created.exists(),
            "a rolled-back creation must not leave a file on disk"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bytes that decode as neither UTF-8 nor any BOM-prefixed encoding: `0xFF`
    /// is never a valid UTF-8 lead byte.
    fn binary_bytes() -> Vec<u8> {
        vec![0x00, 0x01, 0xFF, 0xFE, 0x7F, 0x80]
    }

    #[test]
    fn apply_patch_rollback_restores_a_binary_file_byte_for_byte() {
        let dir = temp_path("patch-rollback-binary");
        std::fs::create_dir_all(&dir).expect("dir");
        let binary = dir.join("image.bin");
        let original = binary_bytes();
        std::fs::write(&binary, &original).expect("write binary");

        let blocker = dir.join("blocked");
        write_file(blocker.to_string_lossy().as_ref(), "not a directory").expect("write blocker");

        // Whole-file replace on the binary (nothing can match inside it), then a
        // write that cannot succeed. The rollback used to treat "exists but does
        // not decode" as "did not exist" and *deleted* the file.
        let error = apply_patch(&[
            PatchChange {
                path: binary.to_string_lossy().into_owned(),
                old_string: String::new(),
                new_string: "clobbered".to_string(),
                replace_all: false,
            },
            PatchChange {
                path: blocker.join("nested.txt").to_string_lossy().into_owned(),
                old_string: String::new(),
                new_string: "created".to_string(),
                replace_all: false,
            },
        ])
        .expect_err("the second write must fail");

        assert!(
            error.to_string().contains("rolled back"),
            "error should report the rollback: {error}"
        );
        assert!(
            binary.exists(),
            "a rolled-back overwrite must not delete the file"
        );
        assert_eq!(
            std::fs::read(&binary).expect("read back"),
            original,
            "the bytes must come back unchanged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_patch_refuses_to_match_inside_a_binary_file() {
        let dir = temp_path("patch-binary-match");
        std::fs::create_dir_all(&dir).expect("dir");
        let binary = dir.join("blob.bin");
        std::fs::write(&binary, binary_bytes()).expect("write binary");

        let error = apply_patch(&[PatchChange {
            path: binary.to_string_lossy().into_owned(),
            old_string: "anything".to_string(),
            new_string: "else".to_string(),
            replace_all: false,
        }])
        .expect_err("matching inside a binary file must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("not valid UTF-8"),
            "the message names the real problem: {error}"
        );
        // Refused up front: nothing was written.
        assert_eq!(std::fs::read(&binary).expect("read back"), binary_bytes());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_reports_a_non_utf8_overwrite_as_an_update() {
        let file = temp_path("overwrite-binary.bin");
        std::fs::write(&file, binary_bytes()).expect("write binary");

        let output = write_file(file.to_string_lossy().as_ref(), "now text").expect("write over");
        // It existed, so it is an update — calling it a "create" would tell the
        // caller there is nothing to put back.
        assert_eq!(output.kind, "update");
        assert!(
            output.original_file.is_none(),
            "binary has no text to quote"
        );

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn edit_file_names_the_file_when_it_is_not_utf8() {
        let file = temp_path("edit-binary.bin");
        std::fs::write(&file, binary_bytes()).expect("write binary");

        let error = edit_file(file.to_string_lossy().as_ref(), "a", "b", false)
            .expect_err("editing a binary file must be refused");
        let message = error.to_string();
        assert!(
            message.contains("not valid UTF-8") && message.contains("edit-binary.bin"),
            "the message names both the problem and the file: {message}"
        );
        assert_eq!(std::fs::read(&file).expect("read back"), binary_bytes());

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn edit_file_refuses_an_ambiguous_old_string() {
        let file = temp_path("edit-ambiguous.txt");
        write_file(file.to_string_lossy().as_ref(), "let x = 1;\nlet y = 2;\n")
            .expect("write file");

        let error = edit_file(file.to_string_lossy().as_ref(), "let ", "const ", false)
            .expect_err("two matches with replace_all=false must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("matches 2 times"),
            "the error counts the matches: {error}"
        );
        // Refused, not half-applied. (`read_file` reports a trimmed trailing
        // newline, which is what the round-trip below compares against.)
        assert_eq!(
            read_file(file.to_string_lossy().as_ref(), None, None)
                .expect("read back")
                .file
                .content,
            "let x = 1;\nlet y = 2;"
        );

        // A unique `old_string` still works, and `replace_all` still replaces all.
        let output = edit_file(file.to_string_lossy().as_ref(), "let x", "let z", false)
            .expect("a unique match succeeds");
        assert_eq!(output.new_string, "let z");
        let output = edit_file(file.to_string_lossy().as_ref(), "let ", "const ", true)
            .expect("replace_all accepts many matches");
        assert!(output.replace_all);

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn read_file_decodes_non_utf8_instead_of_failing() {
        // UTF-16LE with a BOM is what `PowerShell > file` redirection writes on
        // Windows. The BOM makes the decode exact rather than statistical.
        let utf16 = temp_path("utf16.txt");
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "你好 world".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&utf16, &bytes).expect("write utf16");
        assert_eq!(
            read_file(utf16.to_string_lossy().as_ref(), None, None)
                .expect("utf16 read should succeed")
                .file
                .content,
            "你好 world"
        );

        // GBK has no BOM, so the charset detector has to earn its keep. Before
        // the fallback existed this read failed outright with InvalidData.
        let gbk = temp_path("gbk.txt");
        let (encoded, _, _) = encoding_rs::GBK.encode("中文日志内容测试");
        std::fs::write(&gbk, &encoded).expect("write gbk");
        assert_eq!(
            read_file(gbk.to_string_lossy().as_ref(), None, None)
                .expect("gbk read should succeed")
                .file
                .content,
            "中文日志内容测试"
        );

        let _ = std::fs::remove_file(&utf16);
        let _ = std::fs::remove_file(&gbk);
    }

    #[test]
    fn apply_patch_aborts_without_writing_on_missing_string() {
        let dir = temp_path("patch-abort");
        std::fs::create_dir_all(&dir).expect("dir");
        let a = dir.join("a.txt");
        write_file(a.to_string_lossy().as_ref(), "keep me").expect("write a");
        let b = dir.join("b.txt");

        let error = apply_patch(&[
            PatchChange {
                path: a.to_string_lossy().into_owned(),
                old_string: "keep".to_string(),
                new_string: "changed".to_string(),
                replace_all: false,
            },
            // Second change can't be satisfied -> whole batch must roll back.
            PatchChange {
                path: b.to_string_lossy().into_owned(),
                old_string: "absent".to_string(),
                new_string: "x".to_string(),
                replace_all: false,
            },
        ])
        .expect_err("missing old_string must abort");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        // The first file was validated but must not have been written.
        assert_eq!(
            read_file(a.to_string_lossy().as_ref(), None, None)
                .expect("read a")
                .file
                .content,
            "keep me"
        );
        assert!(!b.exists(), "second file must not be created on abort");
    }

    #[test]
    fn make_patch_emits_minimal_hunk_with_context() {
        let original = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\n";
        // Change only the first line; with 14 untouched lines the diff should be
        // a single small hunk, not a whole-file -old/+new dump.
        let updated = "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\n";
        let hunks = make_patch(original, updated);
        assert_eq!(hunks.len(), 1, "one change region -> one hunk");
        let hunk = &hunks[0];
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.old_lines, 4);
        assert_eq!(hunk.new_lines, 4);
        assert_eq!(hunk.lines[0], "-a");
        assert_eq!(hunk.lines[1], "+A");
        assert_eq!(hunk.lines[2], " b");
    }

    #[test]
    fn make_patch_splits_distant_edits_and_ignores_noop() {
        let original = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n";
        // Two edits near opposite ends, far more than 2*context apart.
        let updated = "one\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\ntwelve\n";
        assert_eq!(make_patch(original, updated).len(), 2, "two distant hunks");
        // Identical text produces no hunks at all.
        assert_eq!(make_patch(original, original).len(), 0);
    }

    #[test]
    fn failed_edit_hints_at_the_closest_snippet() {
        let content = "fn alpha() {\n    body_alpha();\n}\n\nfn beta() {\n    body_beta();\n}\n";
        // One character off from the real `beta` block: similarity is high
        // enough to surface a hint with the correct line number.
        let hint = super::nearest_snippet(content, "fn beta() {\n    body_alpba();\n}\n")
            .expect("close match should hint");
        assert!(hint.contains("line 5"), "hint: {hint}");
        assert!(hint.contains("similar"));

        // Unrelated text stays silent - no wrong guesses.
        assert_eq!(
            super::nearest_snippet(content, "class completely_unrelated_signature"),
            None
        );
        assert_eq!(
            super::nearest_snippet("tiny", "x".repeat(50).as_str()),
            None
        );
    }

    #[test]
    fn oversized_diffs_fold_with_an_omission_note() {
        // 600 distinct changed lines exceed the 400-line cap: the hunk must
        // fold to head + note + tail instead of echoing everything.
        let original = String::new();
        let mut updated = String::new();
        for i in 0..600 {
            use std::fmt::Write as _;
            let _ = writeln!(updated, "line {i}");
        }
        let hunks = make_patch(&original, &updated);
        assert_eq!(hunks.len(), 1);
        let total: usize = hunks[0].lines.len();
        assert!(total < 30, "hunk should fold, got {total} lines");
        assert!(
            hunks[0]
                .lines
                .iter()
                .any(|line| line.contains("diff lines omitted")),
            "missing omission note"
        );
        // The truthful counts are untouched by folding.
        assert_eq!(hunks[0].new_lines, 600);
    }

    #[test]
    fn globs_and_greps_directory() {
        let dir = temp_path("search-dir");
        std::fs::create_dir_all(&dir).expect("directory should be created");
        let file = dir.join("demo.rs");
        write_file(
            file.to_string_lossy().as_ref(),
            "fn main() {\n println!(\"hello\");\n}\n",
        )
        .expect("file write should succeed");

        let globbed = glob_search(
            &[String::from("**/*.rs")],
            Some(dir.to_string_lossy().as_ref()),
        )
        .expect("glob should succeed");
        assert_eq!(globbed.num_files, 1);

        // Multi-pattern searches match in one crawl via a single GlobSet.
        std::fs::write(dir.join("other.toml"), b"[x]\n").expect("toml");
        let multi = glob_search(
            &[String::from("**/*.rs"), String::from("**/*.toml")],
            Some(dir.to_string_lossy().as_ref()),
        )
        .expect("multi glob should succeed");
        assert_eq!(multi.num_files, 2);

        let grep_output = grep_search(&GrepSearchInput {
            pattern: String::from("hello"),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: Some(String::from("**/*.rs")),
            output_mode: Some(String::from("content")),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: Some(true),
            case_insensitive: Some(false),
            file_type: None,
            head_limit: Some(10),
            offset: Some(0),
            multiline: Some(false),
        })
        .expect("grep should succeed");
        assert!(grep_output.content.unwrap_or_default().contains("hello"));
    }

    #[test]
    fn glob_skips_build_cache_directories() {
        let dir = temp_path("glob-skip");
        let cache = dir.join("target").join("debug");
        std::fs::create_dir_all(&cache).expect("cache dir should be created");
        write_file(
            cache.join("artifact.rs").to_string_lossy().as_ref(),
            "compiled",
        )
        .expect("cache file write should succeed");
        let source = dir.join("src");
        std::fs::create_dir_all(&source).expect("src dir should be created");
        write_file(source.join("main.rs").to_string_lossy().as_ref(), "source")
            .expect("source file write should succeed");

        let dir_string = dir.to_string_lossy().into_owned();
        let output = glob_search(&["**/*.rs".to_string()], Some(dir_string.as_str()))
            .expect("glob should succeed");

        let listing = output.filenames.join("\n");
        assert!(
            listing.contains("main.rs"),
            "source file must be found: {listing}"
        );
        assert!(
            !listing.contains("artifact.rs"),
            "glob now shares the grep crawler, so build caches are pruned: {listing}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_skips_build_cache_directories() {
        let dir = temp_path("grep-skip");
        let cache = dir.join("target").join("debug");
        std::fs::create_dir_all(&cache).expect("cache dir should be created");
        write_file(
            cache.join("generated.rs").to_string_lossy().as_ref(),
            "needle inside cache",
        )
        .expect("cache file write should succeed");
        let source = dir.join("src");
        std::fs::create_dir_all(&source).expect("src dir should be created");
        write_file(
            source.join("main.rs").to_string_lossy().as_ref(),
            "needle in source",
        )
        .expect("source file write should succeed");

        let output = grep_search(&GrepSearchInput {
            pattern: String::from("needle"),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: None,
            output_mode: Some(String::from("files_with_matches")),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: None,
            case_insensitive: None,
            file_type: None,
            head_limit: None,
            offset: None,
            multiline: None,
        })
        .expect("grep should succeed");

        assert_eq!(output.filenames.len(), 1);
        assert!(output.filenames[0]
            .replace('\\', "/")
            .contains("src/main.rs"));
    }

    #[test]
    fn grep_respects_gitignore() {
        let dir = temp_path("grep-ignore");
        std::fs::create_dir_all(&dir).expect("dir should be created");
        write_file(
            dir.join(".gitignore").to_string_lossy().as_ref(),
            "ignored.rs\n",
        )
        .expect("gitignore write should succeed");
        write_file(
            dir.join("ignored.rs").to_string_lossy().as_ref(),
            "needle hidden by ignore",
        )
        .expect("ignored file write should succeed");
        write_file(
            dir.join("kept.rs").to_string_lossy().as_ref(),
            "needle visible",
        )
        .expect("kept file write should succeed");

        let output = grep_search(&GrepSearchInput {
            pattern: String::from("needle"),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: None,
            output_mode: Some(String::from("files_with_matches")),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: None,
            case_insensitive: None,
            file_type: None,
            head_limit: None,
            offset: None,
            multiline: None,
        })
        .expect("grep should succeed");

        assert_eq!(output.filenames.len(), 1, "gitignored file must be skipped");
        assert!(output.filenames[0].replace('\\', "/").ends_with("kept.rs"));
    }

    #[test]
    fn unknown_grep_output_mode_is_refused() {
        let dir = scratch_dir("grep-mode");
        std::fs::write(dir.join("a.rs"), "needle\n").expect("write file");

        let error = grep_search(&GrepSearchInput {
            pattern: "needle".to_string(),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: None,
            output_mode: Some(String::from("counts")),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: None,
            case_insensitive: None,
            file_type: None,
            head_limit: None,
            offset: None,
            multiline: None,
        })
        .expect_err("a mistyped mode must not pass as files_with_matches");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(
            message.contains("counts") && message.contains("files_with_matches"),
            "the error names the bad value and the valid ones: {message}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_strips_utf8_bom_and_windows_prefixes() {
        let path = temp_path("bom.txt");
        write_file(path.to_string_lossy().as_ref(), "header").expect("write should succeed");
        // Re-add a BOM the way many Windows editors store files.
        let raw = std::fs::read(&path).expect("read raw should succeed");
        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(&raw);
        std::fs::write(&path, &with_bom).expect("rewrite with bom should succeed");

        let output =
            read_file(path.to_string_lossy().as_ref(), None, Some(1)).expect("read should succeed");
        assert_eq!(output.file.content, "header");
        assert!(!output.file.file_path.starts_with("\\\\?\\"));
        let _ = std::fs::remove_file(&path);
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |delta| delta.as_nanos());
        let dir = std::env::temp_dir().join(format!("hf-escape-{tag}-{unique}"));
        std::fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    /// `symlink_dir`, or an error when the platform refuses (Windows needs
    /// Developer Mode or an elevated token, so this is expected to skip there).
    #[cfg(unix)]
    fn make_dir_symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn make_dir_symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[test]
    fn writes_inside_the_workspace_are_not_escapes() {
        let root = scratch_dir("inside");
        assert!(!escapes_workspace(&root, "a.txt"));
        // The whole path may be missing: only the existing ancestor is resolved.
        assert!(!escapes_workspace(&root, "sub/deep/a.txt"));
        // `..` that cancels out stays inside.
        assert!(!escapes_workspace(&root, "sub/../b.txt"));
        // The root itself, given absolutely, is inside.
        assert!(!escapes_workspace(&root, root.to_string_lossy().as_ref()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parent_traversal_is_detected() {
        let root = scratch_dir("parent");
        assert!(escapes_workspace(&root, "../outside.txt"));
        assert!(escapes_workspace(&root, "sub/../../outside.txt"));
        assert!(escapes_workspace(&root, ".."));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn absolute_paths_are_judged_by_location() {
        let root = scratch_dir("absolute");
        let inside = root.join("x.txt");
        assert!(!escapes_workspace(&root, inside.to_string_lossy().as_ref()));
        let outside = std::env::temp_dir().join("hf-outside-probe.txt");
        assert!(escapes_workspace(&root, outside.to_string_lossy().as_ref()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A component-wise test, not a string prefix: `ws2` is not under `ws`.
    #[test]
    fn sibling_with_a_shared_name_prefix_is_not_inside() {
        let base = scratch_dir("prefix");
        let root = base.join("ws");
        std::fs::create_dir_all(&root).expect("root should be creatable");
        let sibling = base.join("ws2").join("hit.txt");
        assert!(escapes_workspace(&root, sibling.to_string_lossy().as_ref()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A not-yet-created root must stay usable — plan mode writes
    /// `.heartflow/plans/` before that directory exists.
    #[test]
    fn a_missing_root_is_resolved_lexically_and_still_confines() {
        let base = scratch_dir("missing-root");
        let root = base.join(".heartflow").join("plans");
        assert!(!escapes_workspace(&root, "plan.md"));
        assert!(escapes_workspace(&root, "../../src/lib.rs"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_symlinked_directory_cannot_launder_an_escape() {
        let base = scratch_dir("symlink");
        let root = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("root should be creatable");
        std::fs::create_dir_all(&outside).expect("outside should be creatable");

        let link = root.join("link");
        if make_dir_symlink(&outside, &link).is_err() {
            // Unprivileged Windows: asserted on Unix, skipped here.
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        assert!(
            escapes_workspace(&root, "link/secret.txt"),
            "a symlink pointing out of the workspace is an escape"
        );

        // Control: the same shape pointing back inside stays inside.
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).expect("inner should be creatable");
        let inner_link = root.join("inner-link");
        if make_dir_symlink(&inner, &inner_link).is_ok() {
            assert!(!escapes_workspace(&root, "inner-link/ok.txt"));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// POSIX resolves `..` *after* symlink substitution, so `link/../x` must
    /// not be folded to `x` before probing — the write would actually land
    /// beside the link's target. (Windows folds `..` lexically in Win32 before
    /// the object manager sees the path, so real resolution matches the gate
    /// there; this bypass only existed on Unix.)
    #[cfg(unix)]
    #[test]
    fn parent_dotdot_through_a_symlink_is_an_escape() {
        let base = scratch_dir("symlink-dotdot");
        let root = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("root should be creatable");
        std::fs::create_dir_all(&outside).expect("outside should be creatable");

        let link = root.join("link");
        if make_dir_symlink(&outside, &link).is_err() {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        assert!(
            escapes_workspace(&root, "link/../secret.txt"),
            ".. resolved after a symlink is judged at the link's target"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Regression: the cwd-unresolvable fallback used to lexical-normalize
    /// `.` to an empty path, against which every candidate passed.
    #[test]
    fn a_relative_root_still_confines() {
        // A temp-dir path outside the (relative) cwd is an escape.
        assert!(escapes_workspace(
            std::path::Path::new("."),
            std::env::temp_dir().join("hf-rel-root-out.txt")
        ));
        // A candidate under the cwd stays inside.
        assert!(!escapes_workspace(
            std::path::Path::new("."),
            "hf-rel-root-in.txt"
        ));
    }
}
