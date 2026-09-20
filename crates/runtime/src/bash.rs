use std::env;
use std::fs;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;
use tokio::runtime::Builder;
use tokio::time::timeout;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BashCommandInput {
    pub command: String,
    pub timeout: Option<u64>,
    pub description: Option<String>,
    #[serde(rename = "run_in_background")]
    pub run_in_background: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BashCommandOutput {
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "rawOutputPath")]
    pub raw_output_path: Option<String>,
    pub interrupted: bool,
    #[serde(rename = "isImage")]
    pub is_image: Option<bool>,
    #[serde(rename = "backgroundTaskId")]
    pub background_task_id: Option<String>,
    #[serde(rename = "backgroundedByUser")]
    pub backgrounded_by_user: Option<bool>,
    #[serde(rename = "assistantAutoBackgrounded")]
    pub assistant_auto_backgrounded: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "returnCodeInterpretation")]
    pub return_code_interpretation: Option<String>,
    #[serde(rename = "noOutputExpected")]
    pub no_output_expected: Option<bool>,
    #[serde(rename = "structuredContent")]
    pub structured_content: Option<Vec<serde_json::Value>>,
    #[serde(rename = "persistedOutputPath")]
    pub persisted_output_path: Option<String>,
    #[serde(rename = "persistedOutputSize")]
    pub persisted_output_size: Option<u64>,
}

pub fn execute_bash(input: BashCommandInput) -> io::Result<BashCommandOutput> {
    let (program, args) = current_shell();
    let wrapped = wrap_command_for_encoding(&args, &input.command);
    if input.run_in_background.unwrap_or(false) {
        return spawn_background(&program, &args, &wrapped, &input);
    }

    let runtime = Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(execute_bash_async(input, wrapped))
}

/// Foreground-command timeout ceiling; the model may override it per call.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

/// Background tasks tee stdout/stderr into a shared log file (returned as
/// `raw_output_path`) instead of discarding them, so the agent can read
/// progress later with `read_file` and stop the task by PID with bash.
fn spawn_background(
    program: &str,
    args: &[&str],
    wrapped: &str,
    input: &BashCommandInput,
) -> io::Result<BashCommandOutput> {
    let mut spawn = Command::new(program);
    for arg in args {
        spawn.arg(arg);
    }
    // The log file must be openable before spawn (both output streams point
    // at it), so it gets a unique pre-generated name; the PID is returned as
    // the task id and the log path as `raw_output_path`.
    let (log_file, log_path) = tempfile_log()?;

    let child = spawn
        .arg(wrapped)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log_file
                .try_clone()
                .map_err(|error| io::Error::other(error.to_string()))?,
        ))
        .stderr(Stdio::from(log_file))
        .spawn()?;

    let pid = child.id();
    // Unix keeps the child as a zombie until reaped; a detached reaper thread
    // collects it (Windows has no zombies and reaps on handle close).
    #[cfg(unix)]
    {
        let mut reaper = child;
        std::thread::spawn(move || {
            let _ = reaper.wait();
        });
    }
    #[cfg(not(unix))]
    drop(child);

    Ok(background_output(input, pid, &log_path.to_string_lossy()))
}

fn background_output(input: &BashCommandInput, pid: u32, log_path: &str) -> BashCommandOutput {
    BashCommandOutput {
        stdout: format!(
            "background task started: pid {pid}, log at {log_path} \
             (read it later with read_file; stop with `kill`/`Stop-Process -Id {pid}`)"
        ),
        stderr: String::new(),
        raw_output_path: Some(log_path.to_string()),
        interrupted: false,
        is_image: None,
        background_task_id: Some(pid.to_string()),
        backgrounded_by_user: Some(false),
        assistant_auto_backgrounded: Some(false),
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation: None,
        no_output_expected: Some(false),
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
    }
}

/// A unique, pre-opened log file in the shared background-log directory;
/// returned with its path because `File` alone does not expose it.
fn tempfile_log() -> io::Result<(std::fs::File, std::path::PathBuf)> {
    let dir = env::temp_dir().join("heartflow-bg");
    fs::create_dir_all(&dir)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for attempt in 0..64u32 {
        let path = dir.join(format!("bg-{nanos}-{attempt}.log"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other("could not create a background log file"))
}

async fn execute_bash_async(
    input: BashCommandInput,
    wrapped: String,
) -> io::Result<BashCommandOutput> {
    let (program, args) = current_shell();
    let mut command = TokioCommand::new(&program);
    for arg in args {
        command.arg(arg);
    }
    command.arg(&wrapped);
    // A timed-out or dropped future must not leave an orphaned child running.
    command.kill_on_drop(true);

    let timeout_ms = input
        .timeout
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);
    let output_result = match timeout(Duration::from_millis(timeout_ms), command.output()).await {
        Ok(result) => (result?, false),
        Err(_) => {
            return Ok(BashCommandOutput {
                stdout: String::new(),
                stderr: format!("Command exceeded timeout of {timeout_ms} ms"),
                raw_output_path: None,
                interrupted: true,
                is_image: None,
                background_task_id: None,
                backgrounded_by_user: None,
                assistant_auto_backgrounded: None,
                dangerously_disable_sandbox: input.dangerously_disable_sandbox,
                return_code_interpretation: Some(String::from("timeout")),
                no_output_expected: Some(true),
                structured_content: None,
                persisted_output_path: None,
                persisted_output_size: None,
            });
        }
    };

    let (output, interrupted) = output_result;
    let stdout = decode_output(&output.stdout);
    let stderr = decode_output(&output.stderr);
    let no_output_expected = Some(stdout.trim().is_empty() && stderr.trim().is_empty());
    let return_code_interpretation = output.status.code().and_then(|code| {
        if code == 0 {
            None
        } else {
            Some(format!("exit_code:{code}"))
        }
    });

    Ok(BashCommandOutput {
        stdout,
        stderr,
        raw_output_path: None,
        interrupted,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation,
        no_output_expected,
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
    })
}

/// Decode child-process bytes to text. A UTF-8 BOM is stripped; valid UTF-8 is
/// returned unchanged (the common case once the PowerShell UTF-8 prefix applies).
/// Only when the bytes are *not* UTF-8 do we sniff the charset (e.g. GBK on a
/// legacy zh-CN console) and decode through it, so CJK output survives intact
/// instead of collapsing into replacement characters under `from_utf8_lossy`.
fn decode_output(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    decode_with(bytes, guess_encoding(bytes))
}

/// Sniff the most probable legacy encoding for bytes already known to be
/// non-UTF-8. `allow_utf8: false` is deliberate: a UTF-8 verdict is impossible
/// here, so forcing a legacy candidate avoids re-emitting the same bytes lossy.
fn guess_encoding(bytes: &[u8]) -> &'static encoding_rs::Encoding {
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    detector.guess(None, false)
}

fn decode_with(bytes: &[u8], encoding: &'static encoding_rs::Encoding) -> String {
    encoding.decode(bytes).0.into_owned()
}

/// Force UTF-8 output encoding for PowerShell so Chinese text survives the
/// pipe (Windows PowerShell defaults to the console code page, often GBK).
fn wrap_command_for_encoding(args: &[&str], command: &str) -> String {
    let is_power_shell = args.iter().any(|arg| arg.eq_ignore_ascii_case("-Command"));
    if is_power_shell {
        let prefix = "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
                      $OutputEncoding=[System.Text.Encoding]::UTF8; ";
        format!("{prefix}{command}")
    } else {
        command.to_string()
    }
}

/// Resolve the shell for the current invocation: `HEARTFLOW_SHELL` override,
/// else platform default (PowerShell on Windows, `sh -lc` elsewhere).
fn current_shell() -> (String, Vec<&'static str>) {
    resolve_shell(env::var("HEARTFLOW_SHELL").ok().as_deref(), cfg!(windows))
}

/// Windows defaults to `pwsh` when installed and falls back to `powershell`;
/// other platforms use `sh -lc`. PowerShell-like programs get
/// `-NoLogo -NoProfile -Command`; everything else gets `-lc`.
fn resolve_shell(explicit: Option<&str>, windows: bool) -> (String, Vec<&'static str>) {
    let program = match explicit.map(str::trim).filter(|shell| !shell.is_empty()) {
        Some(shell) => shell.to_string(),
        None if windows && exists_on_path("pwsh") => String::from("pwsh"),
        None if windows => String::from("powershell"),
        None => String::from("sh"),
    };
    let lowered = program.to_ascii_lowercase();
    let args = if lowered.contains("pwsh") || lowered.contains("powershell") {
        vec!["-NoLogo", "-NoProfile", "-Command"]
    } else {
        vec!["-lc"]
    };
    (program, args)
}

/// Whether an executable resolves on `PATH` (Windows-aware `.exe`). Reused by
/// the prompt builder so the agent is only pointed at CLI tools actually
/// installed on the host.
#[must_use]
pub(crate) fn exists_on_path(program: &str) -> bool {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(format!("{program}{suffix}")))
            .any(|candidate| candidate.is_file())
    })
}

/// Heuristic blast-radius classifier for a shell command. `true` means the
/// command looks destructive or privilege-changing and should be confirmed even
/// in an otherwise permissive workspace; `false` means it can auto-run.
///
/// Deliberately conservative toward flagging: an unknown command is treated as
/// safe only when it matches none of the destructive signatures below.
#[must_use]
pub fn is_dangerous_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let has = |needle: &str| lower.contains(needle);

    // Privilege escalation.
    if words
        .iter()
        .any(|w| matches!(*w, "sudo" | "doas" | "runas"))
    {
        return true;
    }
    // Fork bomb.
    if has(":(){") {
        return true;
    }
    // Piping a remote script straight into a shell.
    if (has("curl") || has("wget"))
        && (has("| sh")
            || has("|bash")
            || has("| sh")
            || has("|zsh")
            || has("|pwsh")
            || has("|powershell")
            || has("| ps1"))
    {
        return true;
    }
    // Disk and filesystem destruction.
    if has("mkfs") || has("fdisk") || has("dd if=") || has("shred") || has("diskpart") {
        return true;
    }
    if has("> /dev/sd") || has(">\\\\.\\physicaldrive") || words.contains(&"format") {
        return true;
    }
    // Forced or recursive deletes.
    let recursive = has("-r") || has("--recursive") || has("/s") || has("-recurse");
    let force = has("-f") || has("--force") || has("/f") || has("-force");
    let deletes = words
        .iter()
        .any(|w| matches!(*w, "rm" | "del" | "erase" | "rd" | "rmdir" | "remove-item"));
    if deletes && (recursive || force) {
        return true;
    }
    // Permission and ownership storms over a tree.
    if (has("chmod") || has("chown")) && (has("-r") || has("--recursive")) {
        return true;
    }
    // Destructive git operations.
    if has("git push")
        && words
            .iter()
            .any(|w| matches!(*w, "-f" | "--force" | "--force-with-lease"))
    {
        return true;
    }
    if has("git reset --hard") || has("git clean -fd") || has("git clean -df") {
        return true;
    }
    // Power / system state.
    if words
        .iter()
        .any(|w| matches!(*w, "shutdown" | "reboot" | "poweroff" | "halt"))
        || has("init 0")
        || has("init 6")
    {
        return true;
    }
    // Windows registry / Defender tampering.
    has("reg delete") || has("set-mppreference")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{execute_bash, is_dangerous_command, resolve_shell, BashCommandInput};

    #[test]
    fn flags_destructive_commands() {
        for cmd in [
            "rm -rf /",
            "sudo apt install foo",
            "curl https://x.sh | sh",
            "git push --force origin main",
            "git reset --hard",
            "mkfs.ext4 /dev/sda1",
            "shutdown -h now",
            ":(){ :|:& };:",
            "Remove-Item -Recurse -Force C:\\proj",
            "chmod -R 777 /etc",
            "reg delete HKLM\\Foo",
        ] {
            assert!(is_dangerous_command(cmd), "should flag: {cmd}");
        }
    }

    #[test]
    fn leaves_ordinary_commands_unflagged() {
        for cmd in [
            "ls -la",
            "echo hello",
            "cargo test --workspace",
            "git status",
            "git push origin main",
            "grep -rn pattern src",
            "rm build/output.txt",
        ] {
            assert!(!is_dangerous_command(cmd), "should allow: {cmd}");
        }
    }

    #[test]
    fn resolves_powershell_on_windows() {
        let (program, args) = resolve_shell(None, true);
        assert!(program == "pwsh" || program == "powershell");
        assert!(args.contains(&"-Command"));
    }

    #[test]
    fn resolves_sh_elsewhere() {
        let (program, args) = resolve_shell(None, false);
        assert_eq!(program, "sh");
        assert_eq!(args, vec!["-lc"]);
    }

    #[test]
    fn honours_explicit_shell_override() {
        let (program, args) = resolve_shell(Some("C:\\Tools\\pwsh.exe"), false);
        assert_eq!(program, "C:\\Tools\\pwsh.exe");
        assert!(args.contains(&"-Command"));

        let (program, args) = resolve_shell(Some("bash"), true);
        assert_eq!(program, "bash");
        assert_eq!(args, vec!["-lc"]);
    }

    #[test]
    fn background_task_logs_output_and_returns_pid() {
        let output = execute_bash(BashCommandInput {
            command: String::from("echo bg-marker"),
            timeout: None,
            description: None,
            run_in_background: Some(true),
            dangerously_disable_sandbox: Some(false),
        })
        .expect("background spawn should succeed");

        let task_id = output.background_task_id.expect("pid as task id");
        assert_ne!(task_id, "");
        let log_path = output.raw_output_path.expect("log path");
        // The shell cold start can exceed a second on a loaded machine; poll
        // for the marker instead of sleeping a fixed amount.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut log = String::new();
        while Instant::now() < deadline {
            log = std::fs::read_to_string(&log_path).unwrap_or_default();
            if log.contains("bg-marker") {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("log never contained the marker; content: {log:?}");
    }

    #[test]
    fn executes_simple_command() {
        let output = execute_bash(BashCommandInput {
            command: String::from("echo hello"),
            // PowerShell cold starts can exceed one second on a loaded machine.
            timeout: Some(10_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
        })
        .expect("shell command should execute");

        assert_eq!(output.stdout.trim(), "hello");
        assert!(!output.interrupted);
    }

    #[test]
    fn wraps_power_shell_commands_with_utf8_prefix() {
        let wrapped = super::wrap_command_for_encoding(
            &["-NoLogo", "-NoProfile", "-Command"],
            "Get-Content x",
        );
        assert!(wrapped.starts_with("[Console]::OutputEncoding="));
        assert!(wrapped.ends_with("Get-Content x"));

        let untouched = super::wrap_command_for_encoding(&["-lc"], "ls -la");
        assert_eq!(untouched, "ls -la");
    }

    #[test]
    fn strips_utf8_bom_from_output() {
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice("中文输出".as_bytes());
        assert_eq!(super::decode_output(&bytes), "中文输出");
        assert_eq!(super::decode_output("plain".as_bytes()), "plain");
    }

    #[test]
    fn explicit_charset_roundtrips() {
        let text = "中文测试";
        let (bytes, _, _) = encoding_rs::GBK.encode(text);
        assert_eq!(super::decode_with(&bytes, encoding_rs::GBK), text);
    }

    #[test]
    fn non_utf8_output_is_decoded_without_replacement_chars() {
        // Legacy zh-CN console bytes: valid GBK, invalid UTF-8. The whole point
        // of the sniff+decode path is these no longer shred into U+FFFD.
        let (gbk, _, _) = encoding_rs::GBK.encode("中文测试输出内容，包含常用标点符号。");
        assert_ne!(super::guess_encoding(&gbk), encoding_rs::UTF_8);
        let decoded = super::decode_output(&gbk);
        assert!(!decoded.contains('\u{FFFD}'), "{decoded}");
    }

    #[cfg(windows)]
    #[test]
    fn emits_chinese_output_as_utf8() {
        let output = execute_bash(BashCommandInput {
            command: String::from("Write-Output '中文输出测试'"),
            timeout: Some(10_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
        })
        .expect("shell command should execute");
        assert_eq!(output.stdout.trim(), "中文输出测试");
    }
}
