use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

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

pub fn read_file(
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
    let raw_content = fs::read_to_string(&absolute_path)?;
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

pub fn write_file(path: &str, content: &str) -> io::Result<WriteFileOutput> {
    let absolute_path = normalize_path_allow_missing(path)?;
    let original_file = fs::read_to_string(&absolute_path).ok();
    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_text_atomic(&absolute_path, content)?;

    Ok(WriteFileOutput {
        kind: if original_file.is_some() {
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
    let original_file = strip_bom(&fs::read_to_string(&absolute_path)?).to_string();
    if old_string == new_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "old_string and new_string must differ",
        ));
    }
    if !original_file.contains(old_string) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "old_string not found in file",
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
    /// On-disk content the first time this path was seen; `None` for new files.
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
            let original = fs::read_to_string(&absolute_path)
                .ok()
                .map(|text| strip_bom(&text).to_string());
            PreparedFile {
                absolute_path: absolute_path.clone(),
                content: original.clone().unwrap_or_default(),
                original,
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
        let matches = entry.content.matches(change.old_string.as_str()).count();
        if matches == 0 {
            return Err(not_found(format!(
                "old_string not found in {}",
                change.path
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

    // Everything validated: now write, in first-seen order.
    let mut results = Vec::with_capacity(order.len());
    for absolute_path in &order {
        let prepared = &by_path[absolute_path];
        if let Some(parent) = prepared.absolute_path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_text_atomic(&prepared.absolute_path, &prepared.content)?;
        let (kind, structured_patch) = match &prepared.original {
            Some(original) => (
                String::from("update"),
                make_patch(original, &prepared.content),
            ),
            None => (String::from("create"), make_patch("", &prepared.content)),
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
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

pub fn glob_search(pattern: &str, path: Option<&str>) -> io::Result<GlobSearchOutput> {
    let started = Instant::now();
    let base_dir = path
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let search_pattern = if Path::new(pattern).is_absolute() {
        pattern.to_owned()
    } else {
        base_dir.join(pattern).to_string_lossy().into_owned()
    };

    let mut matches = Vec::new();
    let entries = glob::glob(&search_pattern)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    for entry in entries.flatten() {
        if entry.is_file() {
            matches.push(entry);
        }
    }

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
        .unwrap_or("files_with_matches")
        .trim()
        .to_owned();
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
                if let Ok(mut sink) = results.lock() {
                    sink.push(scan);
                }
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
                if content_budget
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
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
                    if let Ok(mut sink) = files.lock() {
                        sink.push(entry.path().to_path_buf());
                    }
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

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        apply_patch, edit_file, glob_search, grep_search, make_patch, read_file, search_files,
        write_file, ApplyPatchOutput, GrepSearchInput, PatchChange,
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
    fn globs_and_greps_directory() {
        let dir = temp_path("search-dir");
        std::fs::create_dir_all(&dir).expect("directory should be created");
        let file = dir.join("demo.rs");
        write_file(
            file.to_string_lossy().as_ref(),
            "fn main() {\n println!(\"hello\");\n}\n",
        )
        .expect("file write should succeed");

        let globbed = glob_search("**/*.rs", Some(dir.to_string_lossy().as_ref()))
            .expect("glob should succeed");
        assert_eq!(globbed.num_files, 1);

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
}
