//! End-to-end tests that spawn the real `hf` binary. The offline path
//! `hf --resume <file> --run <slash-command>` never touches a provider, so the
//! binary can be driven exactly like a user would (restore, mutate, persist)
//! with nothing but the filesystem.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::Session;

/// Nanos alone can collide when tests start in the same tick; a per-process
/// counter makes the name unique no matter how cargo schedules the tests.
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

fn temp_session_path(tag: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("hf-e2e-{tag}-{nanos}-{id}.json"))
}

/// A hand-written transcript in the on-disk format; `version` plus one message
/// per entry. `count` is padded with numbered user/assistant turns.
fn session_json(count: usize) -> String {
    let messages = (0..count)
        .map(|index| {
            let role = if index % 2 == 0 { "user" } else { "assistant" };
            format!(
                "{{\"role\": \"{role}\", \"blocks\": [{{\"type\": \"text\", \"text\": \"turn {index}\"}}]}}"
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\"version\": 1, \"messages\": [{messages}]}}")
}

fn write_session(path: &Path, count: usize) {
    fs::write(path, session_json(count)).expect("session snapshot should write");
}

fn hf(session: &Path, command: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hf"))
        .arg(format!("--resume={}", session.display()))
        .arg(format!("--run={command}"))
        .output()
        .expect("hf binary should spawn")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn load(path: &Path) -> Session {
    Session::load_from_path(path).expect("resumed session should reload")
}

#[test]
fn compact_resume_round_trip() {
    let path = temp_session_path("compact");
    write_session(&path, 6);
    let out = hf(&path, "/compact");

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Compacted 2 messages"));
    let session = load(&path);
    fs::remove_file(&path).ok();

    // Four recent turns survive verbatim behind one system summary.
    assert_eq!(session.messages.len(), 5);
    assert_eq!(session.messages[0].role, runtime::MessageRole::System);
    assert_eq!(
        session.messages[1].blocks[0],
        runtime::ContentBlock::Text {
            text: "turn 2".to_string(),
        }
    );
}

#[test]
fn pin_resume_round_trip_toggles() {
    let path = temp_session_path("pin");
    write_session(&path, 2);
    let pinned = hf(&path, "/pin");
    assert!(pinned.status.success());
    assert!(stdout(&pinned).contains("pinned the last message"));
    assert!(load(&path).messages[1].pinned);

    // Toggling again through a fresh process clears the flag on disk.
    let unpinned = hf(&path, "/pin");
    assert!(unpinned.status.success());
    assert!(stdout(&unpinned).contains("unpinned the last message"));
    let session = load(&path);
    fs::remove_file(&path).ok();
    assert!(!session.messages[1].pinned);
}

#[test]
fn resume_applies_append_only_segment() {
    let path = temp_session_path("segment");
    write_session(&path, 2);
    // A tail appended after the snapshot: one message beside the .json file.
    let segment = path.with_extension("jsonl");
    fs::write(
        &segment,
        "{\"base\": 2, \"version\": 1}\n\
         {\"role\": \"user\", \"blocks\": [{\"type\": \"text\", \"text\": \"appended\"}]}\n",
    )
    .expect("segment should write");

    // The /pin runs against snapshot + segment and persists all three back
    // into the snapshot; the merge is invisible above the slash command.
    let out = hf(&path, "/pin");
    let loaded = load(&path);
    fs::remove_file(&path).ok();
    fs::remove_file(&segment).ok();

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(
        loaded.messages[2].blocks[0],
        runtime::ContentBlock::Text {
            text: "appended".to_string(),
        }
    );
    assert!(loaded.messages[2].pinned);
}

#[test]
fn restore_failure_exits_nonzero() {
    let path = temp_session_path("missing");
    let out = hf(&path, "/compact");
    fs::remove_file(&path).ok();

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("failed to restore session"));
}

#[test]
fn unknown_command_suggests_the_closest() {
    let path = temp_session_path("typo");
    write_session(&path, 2);
    let out = hf(&path, "/compct");
    fs::remove_file(&path).ok();

    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("did you mean /compact"));
}
