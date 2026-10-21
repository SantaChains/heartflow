//! Cross-process advisory file locks for the durable stores
//! (`schedules.json`, `mission-*/state.json`, `job-*/status.json`,
//! `queue.json`).
//!
//! # Why this exists
//!
//! Every durable store already persists through atomic temp+rename writes.
//! That makes *one* write indivisible, but it does not make *two writers*
//! safe: two `hf` processes sharing a root interleave as
//! `P1 writes tmp → P2 truncates tmp → P1 renames` — and P1 renames P2's
//! bytes under P1's name, while P2's own rename fails outright. A
//! temp+rename is a single-writer primitive; the sidecar lock is what
//! serializes writers.
//!
//! # Why a sidecar lock file
//!
//! The lock must never be taken on the data file itself: the persistence
//! discipline renames a temp file *over* it, which swaps the underlying
//! file object out from under the handle — a lock held on the old object
//! protects nothing afterwards. `<data>.lock` is a stable name whose whole
//! lifetime is the lock's own.
//!
//! # Why an OS advisory lock
//!
//! `O_CREAT|O_EXCL` lock files survive a crash as stale locks — the one
//! failure mode a long-running process must not have. An OS advisory lock
//! (Windows `LockFileEx`, Unix `flock`, both inside the battle-tested
//! [`fd_lock`] crate) is released by the kernel when the holder dies, so
//! there is no stale-lock problem at all. The unsafe platform code lives
//! inside `fd-lock` because this workspace runs with `unsafe_code = forbid`.
//!
//! # Honest limits (recorded, not hidden)
//!
//! - This serializes *writes*; it does not make the stores a multi-writer
//!   database. Each process still keeps its own in-memory copy as the
//!   source of truth, so a concurrent-writer world is "last persist wins"
//!   per store. The real single-writer story remains roadmap P2.
//! - Readers do **not** take the lock: a `temp+rename` reader always sees
//!   one whole snapshot, possibly a stale one, which is acceptable
//!   (at-least-once over at-most-once). On Windows a concurrent reader can
//!   make a writer's `remove_file` on the final path fail; that writer
//!   counts a disk failure (doctor-visible) and self-heals on its next
//!   persist. Locking reads would make startup depend on another process's
//!   critical section for no correctness gain.
//! - **Never nest** `with_exclusive` calls on the same path from one
//!   thread: the inner attempt cannot distinguish "my own process holds
//!   it" from "another process holds it" and would burn the whole timeout.
//!   Current call sites are flat (no persist inside a persist).
//!
//! # Failure semantics
//!
//! [`with_exclusive`] waits up to the given timeout (backing off 1 ms →
//! 50 ms), then reports [`LockError::Timeout`] instead of writing
//! unguarded. Call sites turn every lock failure into the same
//! "disk write failure" counter a failed `fs::write` would hit: the
//! in-memory state keeps working, `hf doctor` surfaces the degradation.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default bounded wait for a write lock. Write critical sections are
/// sub-millisecond (small JSON files); five seconds only ever elapses when
/// another holder is wedged (stopped under a debugger, for example), and
/// skipping the write is the honest degradation there.
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a [`with_exclusive`] call gave up without running its closure.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// The lock was still held elsewhere after the timeout elapsed.
    #[error("file lock timed out: another holder did not release within the budget")]
    Timeout,
    /// Opening or locking the sidecar file failed (permissions, full disk,
    /// antivirus interference, ...).
    #[error("file lock error: {0}")]
    Io(std::io::Error),
}

impl PartialEq for LockError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Timeout, Self::Timeout) => true,
            // `io::Error` is not `PartialEq`; the error *kind* is the
            // meaningful identity here (two "file not found" errors are
            // the same failure for a caller deciding what to do next).
            (Self::Io(a), Self::Io(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

impl Eq for LockError {}

/// The sidecar lock path for a data file: `queue.json` → `queue.json.lock`.
/// A sibling of the data file, never the data file itself (see the module
/// docs for why rename-based persistence makes that mandatory).
#[must_use]
pub fn lock_path(data_path: &Path) -> PathBuf {
    let mut name = data_path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// Run `f` while holding an exclusive cross-process lock on
/// [`lock_path(path)`].
///
/// Bounded wait: polls a non-blocking exclusive acquire with 1 ms → 50 ms
/// backoff until `timeout` elapses. On [`LockError::Timeout`] (or an
/// underlying I/O error) `f` is **not** run — callers must treat the work
/// as skipped, never fall through to an unguarded write.
///
/// If `f` panics, the lock is released during unwind; the lock file itself
/// is intentionally left behind (empty, reusable) — recreating it is
/// cheaper than deleting it on every write.
///
/// # Errors
///
/// [`LockError::Timeout`] when the lock stays held elsewhere for the whole
/// timeout; [`LockError::Io`] when the sidecar file cannot be opened or
/// locked. Neither leaves partial effects from `f`.
pub fn with_exclusive<T>(
    path: &Path,
    timeout: Duration,
    f: impl FnOnce() -> T,
) -> Result<T, LockError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(LockError::Io)?;
    let mut lock = fd_lock::RwLock::new(file);
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(1);
    loop {
        match lock.try_write() {
            // Run the critical section, then release before returning so a
            // long-held guard can never outlive this call by accident.
            Ok(guard) => {
                let out = f();
                drop(guard);
                return Ok(out);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(LockError::Timeout);
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            Err(err) => return Err(LockError::Io(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hf-filelock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn lock_path_appends_suffix() {
        assert_eq!(
            lock_path(Path::new("/x/queue.json")),
            PathBuf::from("/x/queue.json.lock")
        );
    }

    #[test]
    fn mutual_exclusion_and_release_in_process() {
        let dir = temp_dir("mutex");
        let path = dir.join("x.lock");
        let entered = AtomicUsize::new(0);
        let out = with_exclusive(&path, Duration::from_secs(1), || {
            entered.fetch_add(1, Ordering::Relaxed);
            // A second handle — even from this same process — must see the
            // lock as held; two short non-blocking attempts both bounce.
            for _ in 0..2 {
                let attempt = with_exclusive(&path, Duration::from_millis(10), || {
                    panic!("nested acquisition must not run");
                });
                assert!(matches!(attempt, Err(LockError::Timeout)));
            }
            42
        });
        assert_eq!(out, Ok(42));
        assert_eq!(entered.load(Ordering::Relaxed), 1);
        // Released on return: a fresh acquisition succeeds immediately.
        assert_eq!(with_exclusive(&path, Duration::from_secs(1), || 7), Ok(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bounded_timeout_is_honored_and_closure_skipped() {
        let dir = temp_dir("timeout");
        let path = dir.join("x.lock");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .expect("open holder");
        let mut held = fd_lock::RwLock::new(held);
        let _guard = held.try_write().expect("self-acquire");
        let start = Instant::now();
        let attempt = with_exclusive(&path, Duration::from_millis(120), || {
            panic!("timeout path must not run the closure");
        });
        assert!(matches!(attempt, Err(LockError::Timeout)));
        assert!(start.elapsed() >= Duration::from_millis(120));
        drop(_guard);
        // After the holder drops, acquisition works again.
        assert_eq!(with_exclusive(&path, Duration::from_secs(1), || 1), Ok(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two threads doing read-modify-write on a shared counter file, each
    /// iteration inside `with_exclusive`. With the lock the final value is
    /// exactly `2 * ITERS`; without it, lost updates are near-certain —
    /// this is the property the persist call sites actually rely on.
    #[test]
    fn concurrent_writers_serialize() {
        const ITERS: usize = 60;
        let dir = temp_dir("serialize");
        let data = dir.join("counter");
        std::fs::write(&data, b"0").expect("seed counter");
        let lock = lock_path(&data);
        let worker = |data: PathBuf| {
            let lock = lock.clone();
            move || {
                for _ in 0..ITERS {
                    with_exclusive(&lock, Duration::from_secs(5), || {
                        let cur: usize = std::fs::read_to_string(&data)
                            .ok()
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(0);
                        std::fs::write(&data, format!("{}", cur + 1)).expect("counter write");
                    })
                    .expect("lock contention should resolve within 5s");
                }
            }
        };
        let t1 = std::thread::spawn(worker(data.clone()));
        let t2 = std::thread::spawn(worker(data.clone()));
        t1.join().expect("t1");
        t2.join().expect("t2");
        let final_count: usize = std::fs::read_to_string(&data)
            .expect("counter exists")
            .trim()
            .parse()
            .expect("counter digits");
        assert_eq!(final_count, 2 * ITERS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// True cross-process proof: a child *process* (re-executed test
    /// binary) holds the lock; the parent's bounded attempt times out and,
    /// once the child exits, the kernel-released lock is acquirable.
    ///
    /// Protocol: the parent picks the sandbox dir and passes it to the
    /// child via `HF_FILELOCK_DIR` (the child's own pid differs, so the
    /// dir must be communicated, not derived); the child takes the lock,
    /// signals `child-holds`, waits for `parent-released`, then exits.
    #[test]
    fn cross_process_mutual_exclusion() {
        if let Ok(dir) = std::env::var("HF_FILELOCK_DIR") {
            // Child mode: take the lock, tell the parent, wait to be told.
            let dir = PathBuf::from(dir);
            let path = dir.join("x.lock");
            let holds = dir.join("child-holds");
            let release = dir.join("parent-released");
            let outcome = with_exclusive(&path, DEFAULT_LOCK_TIMEOUT, || {
                std::fs::write(&holds, b"1").expect("sentinel");
                let deadline = Instant::now() + Duration::from_secs(15);
                while !release.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            assert!(outcome.is_ok(), "child failed to acquire: {outcome:?}");
            return;
        }

        let dir = std::env::temp_dir().join(format!("hf-filelock-xproc-{}", std::process::id()));
        let path = dir.join("x.lock");
        let holds = dir.join("child-holds");
        let release = dir.join("parent-released");

        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("xproc dir");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--exact",
                "filelock::tests::cross_process_mutual_exclusion",
                "--nocapture",
            ])
            .env("HF_FILELOCK_DIR", &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn lock-holder child");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !holds.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(holds.exists(), "child never acquired the lock in time");

        // Held by another *process*: a short bounded attempt must fail.
        let attempt = with_exclusive(&path, Duration::from_millis(150), || {
            panic!("must not run while another process holds the lock");
        });
        assert!(matches!(attempt, Err(LockError::Timeout)));

        // Let the child exit; the kernel drops its lock with it.
        std::fs::write(&release, b"1").expect("release signal");
        let status = child.wait().expect("wait for child");
        assert!(status.success(), "child test run failed: {status:?}");
        assert_eq!(with_exclusive(&path, Duration::from_secs(2), || 5), Ok(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The panic path releases the lock (guard drop during unwind), so a
    /// crashing writer cannot wedge the next one.
    #[test]
    fn panic_inside_critical_section_releases() {
        let dir = temp_dir("panic");
        let path = dir.join("x.lock");
        let hold = || {
            let _ = with_exclusive(&path, Duration::from_secs(1), || {
                panic!("deliberate, contained panic");
            });
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(hold));
        assert!(result.is_err(), "the panic must actually happen");
        assert_eq!(with_exclusive(&path, Duration::from_secs(1), || 3), Ok(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sidecar file survives as an empty reusable artifact; writers
    /// must never assume it has content (it is a lock, not a ledger).
    #[test]
    fn lock_file_is_reusable_across_cycles() {
        let dir = temp_dir("reuse");
        let path = dir.join("ledger.json");
        for round in 0..3u32 {
            let wrote = with_exclusive(&lock_path(&path), Duration::from_secs(1), || {
                std::fs::write(&path, format!("round-{round}")).is_ok()
            });
            assert!(matches!(wrote, Ok(true)));
            let contents = std::fs::read_to_string(&path).expect("data file");
            assert_eq!(contents, format!("round-{round}"));
        }
        // And a writer may still choose to write through the guard itself.
        with_exclusive(&lock_path(&path), Duration::from_secs(1), || {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append");
            f.write_all(b"!").expect("append write");
        })
        .expect("append under lock");
        assert_eq!(
            std::fs::read_to_string(&path).expect("data file"),
            "round-2!"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
