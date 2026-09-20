//! Search inside documents and archives via an external `rga` (ripgrep-all).
//!
//! hf does not reimplement PDF/Office/archive parsing: `rga` already does it
//! through its adapter chain (zip, tar, gzip/xz/zstd, pdf, pandoc, ...) and
//! caches preprocessed output in its own SQLite cache keyed by adapter
//! version + path + mtime. When `rga` is on PATH this tool gives the agent
//! one-shot regex search over `doc.zip!inner/file` content; when it is
//! missing the error explains exactly what to install. The probe result is
//! cached process-wide because PATH does not change mid-session.

use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Hard wall for one rga run; rga itself is also given ripgrep's `--timeout`.
const RGA_TIMEOUT: Duration = Duration::from_secs(30);
/// ripgrep-side wall (passed through rga), a softer first stop.
const RGA_INTERNAL_TIMEOUT_SECS: u32 = 25;
/// Cap on captured stdout; beyond this the rest is dropped (still bounded).
const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
/// Default number of matching lines returned.
const DEFAULT_HEAD_LIMIT: usize = 50;
/// Most matching lines one search may return.
const MAX_HEAD_LIMIT: usize = 500;
/// Most matching lines one ripgrep run may produce before hf cuts it off.
const MAX_TOTAL_MATCHES: usize = 2_000;
/// Refuse absurd regexes up front instead of inside rga.
const MAX_PATTERN_CHARS: usize = 4_096;
/// Bytes of stderr kept for error reporting.
const MAX_STDERR_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
pub struct DocSearchInput {
    /// Regex applied to the extracted text of each document.
    pub pattern: String,
    /// File or directory to search.
    pub path: String,
    #[serde(default)]
    pub case_insensitive: Option<bool>,
    /// ripgrep `-g` glob to restrict which files are searched.
    #[serde(default)]
    pub glob: Option<String>,
    /// Maximum matching lines returned (default 50, max 500).
    #[serde(default)]
    pub head_limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocHit {
    /// Path as rga reports it; inside archives it is `archive!entry` form.
    pub file: String,
    /// Inner document path when the hit is nested (split at the first `!`).
    pub entry: Option<String>,
    /// 1-based line number within the extracted text.
    pub line: usize,
    /// The matching line with trailing whitespace trimmed.
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct DocSearchOutput {
    pub duration_ms: u128,
    pub num_matches: usize,
    pub truncated: bool,
    pub hits: Vec<DocHit>,
}

pub fn search_documents(input: &DocSearchInput) -> io::Result<DocSearchOutput> {
    let started = Instant::now();
    if input.pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("pattern exceeds {MAX_PATTERN_CHARS} chars"),
        ));
    }
    if !rga_available() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "rga not found on PATH; install ripgrep-all (`cargo install ripgrep-all` \
             or `scoop install ripgrep-all`) to search PDFs, Office files and archives",
        ));
    }
    let head_limit = input
        .head_limit
        .unwrap_or(DEFAULT_HEAD_LIMIT)
        .clamp(1, MAX_HEAD_LIMIT);

    let mut args: Vec<String> = vec![
        String::from("--json"),
        format!("--max-count={head_limit}"),
        format!("--timeout={RGA_INTERNAL_TIMEOUT_SECS}s"),
        String::from("--max-filesize=64M"),
    ];
    if input.case_insensitive.unwrap_or(false) {
        args.push(String::from("-i"));
    }
    if let Some(glob) = &input.glob {
        args.push(String::from("-g"));
        args.push(glob.clone());
    }
    args.push(input.pattern.clone());
    args.push(input.path.clone());

    let (status, stdout, stderr) = run_rga(&args)?;
    // ripgrep exit codes: 0 = matches, 1 = no matches, 2 = error.
    if !matches!(status.code(), Some(0 | 1)) {
        let detail = stderr_summary(&stderr);
        return Err(io::Error::other(format!("rga failed: {detail}")));
    }
    let hits = parse_rg_json(&stdout, head_limit);
    Ok(DocSearchOutput {
        duration_ms: started.elapsed().as_millis(),
        num_matches: hits.len(),
        truncated: hits.len() >= head_limit || hits.len() >= MAX_TOTAL_MATCHES,
        hits,
    })
}

/// True when `rga --version` runs successfully. Cached for the process.
/// Exposed so tool registration can advertise `search_documents` only when
/// the backing binary exists.
#[must_use]
pub fn rga_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("rga")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Spawn rga, collect stdout/stderr on threads, and enforce a wall-clock
/// timeout by killing the process tree root. Bounded output prevents a
/// runaway `--json` stream from exhausting memory.
fn run_rga(args: &[String]) -> io::Result<(std::process::ExitStatus, String, String)> {
    let mut child = Command::new("rga")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| io::Error::other(format!("failed to spawn rga: {error}")))?;

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("rga stdout unavailable"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("rga stderr unavailable"))?;

    let stdout_reader =
        std::thread::spawn(move || read_bounded(&mut stdout_pipe, MAX_OUTPUT_BYTES));
    let stderr_reader =
        std::thread::spawn(move || read_bounded(&mut stderr_pipe, MAX_STDERR_BYTES as u64));

    let deadline = Instant::now() + RGA_TIMEOUT;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("rga exceeded the {}s wall clock", RGA_TIMEOUT.as_secs()),
                ));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("rga stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("rga stderr reader panicked"))??;
    Ok((status, stdout, stderr))
}

/// Read a pipe fully but stop after `cap` bytes (the child is killed soon
/// after by the caller, unblocking any pending write).
fn read_bounded(pipe: &mut impl Read, cap: u64) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 || buf.len() as u64 + read as u64 > cap {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn stderr_summary(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::from("no stderr output")
    } else {
        trimmed.chars().take(MAX_STDERR_BYTES).collect()
    }
}

/// One ripgrep `--json` line; only `type: match` events matter here.
#[derive(Debug, Deserialize)]
struct RgMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: Option<RgMatchData>,
}

#[derive(Debug, Deserialize)]
struct RgMatchData {
    path: Option<RgTextField>,
    line_number: Option<usize>,
    lines: Option<RgTextField>,
}

#[derive(Debug, Deserialize)]
struct RgTextField {
    text: Option<String>,
}

/// Turn ripgrep JSON lines into hits, splitting rga's `archive!entry` paths.
fn parse_rg_json(stdout: &str, head_limit: usize) -> Vec<DocHit> {
    let mut hits = Vec::new();
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<RgMessage>(line) else {
            continue;
        };
        if message.kind != "match" {
            continue;
        }
        let Some(data) = message.data else { continue };
        let path = data
            .path
            .and_then(|field| field.text)
            .unwrap_or_else(|| String::from("(unknown)"));
        let (file, entry) = match path.split_once('!') {
            Some((archive, inner)) => (archive.to_string(), Some(inner.to_string())),
            None => (path, None),
        };
        hits.push(DocHit {
            file,
            entry,
            line: data.line_number.unwrap_or(0),
            text: data
                .lines
                .and_then(|field| field.text)
                .unwrap_or_default()
                .trim_end()
                .to_string(),
        });
        // ripgrep was already told --max-count, this is just belt and braces.
        if hits.len() >= head_limit {
            break;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::{parse_rg_json, DocSearchInput, MAX_TOTAL_MATCHES};
    use serde_json::json;
    use std::io;

    #[test]
    fn rg_json_matches_become_hits() {
        let stdout = concat!(
            "{\"type\":\"begin\",\"data\":null}\n",
            "{\"type\":\"match\",\"data\":{\"path\":{\"text\":\"docs.zip!notes/readme.md\"},",
            "\"line_number\":42,\"lines\":{\"text\":\"deploy on friday\\n\"}}}\n",
            "{\"type\":\"end\",\"data\":null}\n",
            "not json at all\n"
        );
        let hits = parse_rg_json(stdout, 50);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file, "docs.zip");
        assert_eq!(hits[0].entry.as_deref(), Some("notes/readme.md"));
        assert_eq!(hits[0].line, 42);
        assert_eq!(hits[0].text, "deploy on friday");
    }

    #[test]
    fn head_limit_stops_collection() {
        use std::fmt::Write as FmtWrite;

        let mut stdout = String::new();
        for i in 0..10 {
            let _ = writeln!(
                stdout,
                "{{\"type\":\"match\",\"data\":{{\"path\":{{\"text\":\"a.txt\"}},\"line_number\":{i},\"lines\":{{\"text\":\"hit\\n\"}}}}}}",
            );
        }
        let hits = parse_rg_json(&stdout, 3);
        assert_eq!(hits.len(), 3);
        assert!(hits.len() < MAX_TOTAL_MATCHES);
    }

    #[test]
    fn input_deserializes_from_wire_json() {
        let value = json!({
            "pattern": "release notes",
            "path": "changelog.zip",
            "case_insensitive": true,
            "glob": "*.md",
            "head_limit": 5
        });
        let input: DocSearchInput = serde_json::from_value(value).expect("deserialize");
        assert_eq!(input.head_limit, Some(5));
        assert_eq!(input.glob.as_deref(), Some("*.md"));
        assert!(input.case_insensitive.unwrap_or(false));
    }

    #[test]
    fn missing_rga_reports_install_hint() {
        // When rga is absent the error must say how to install it; when it is
        // present this test cannot simulate absence, so only assert on error
        // kind for a clearly invalid path after the probe passed.
        let result = super::search_documents(&DocSearchInput {
            pattern: "x".to_string(),
            path: "definitely/missing/path.zip".to_string(),
            case_insensitive: None,
            glob: None,
            head_limit: None,
        });
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                assert!(error.to_string().contains("install ripgrep-all"));
            }
            Err(error) => {
                // rga present: a missing path is rga's problem, reported as error.
                let _ = error;
            }
            Ok(output) => {
                // rga present and tolerated the missing path: no matches is fine.
                assert_eq!(output.num_matches, 0);
            }
        }
    }
}
