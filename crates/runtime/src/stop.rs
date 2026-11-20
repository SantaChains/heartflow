//! Behavior-level stop policy (roadmap §15 A2 / §16 R2, S6): loop detection
//! and runaway-tool fuses for the turn loop. Pure and clock-free so the fuse
//! logic is unit-testable without a runtime.
//!
//! This module covers loop detection only (two arms, below). The companion
//! fuse — N consecutive all-error tool batches — lives in `conversation.rs`
//! next to the tool-execution path it guards, because it needs the live
//! tool results rather than a batch history.
//!
//! Two arms, mirroring zaly's stop gates (`packages/agent/src/stop.ts`):
//! consecutive identical tool batches, and repetition within a sliding
//! window. Both hash `name\0input-json` per batch — not per call — so a
//! batch of three distinct reads is never mistaken for a loop.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

/// Default consecutive-identical-batch threshold: the same tool batch three
/// times in a row is a loop (zaly: `MAX_CONSECUTIVE_REPEATS`).
pub const MAX_REPEAT_STREAK: usize = 3;

/// Sliding-window arm: the same batch repeating this many times within the
/// last `WINDOW_SIZE` iterations is a loop, even with interruptions between.
pub const WINDOW_MAX_REPEATS: usize = 4;

/// Sliding-window size in iterations.
pub const WINDOW_SIZE: usize = 10;

/// Tool batches (name + serialized input) seen so far are hashed into one
/// `u64` per iteration via `DefaultHasher` (`SipHash`, stable within a process —
/// the policy never persists hashes, so cross-run stability is irrelevant).
#[must_use]
fn batch_key(calls: &[(String, String)]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Sort so one batch's internal ordering does not change the key.
    let mut sorted: Vec<&(String, String)> = calls.iter().collect();
    sorted.sort();
    for (name, input) in &sorted {
        name.hash(&mut hasher);
        0u8.hash(&mut hasher);
        input.hash(&mut hasher);
    }
    hasher.finish()
}

/// Behavior-level loop detector for one turn. `register` is called once per
/// iteration with the pending tool calls (empty when the model produced no
/// tool calls — that ends the turn anyway, and registering an empty batch
/// with a distinct key keeps semantics simple).
#[derive(Debug)]
pub struct StopPolicy {
    window: VecDeque<u64>,
    last: Option<u64>,
    streak: usize,
}

impl StopPolicy {
    #[must_use]
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW_SIZE),
            last: None,
            streak: 0,
        }
    }

    /// Register one iteration's tool batch. Returns a human-readable stop
    /// reason when either loop arm trips; `None` means the turn may continue.
    pub fn register(&mut self, calls: &[(String, String)]) -> Option<String> {
        let key = batch_key(calls);

        // Arm 1: consecutive identical batches.
        if self.last == Some(key) {
            self.streak += 1;
        } else {
            self.streak = 1;
            self.last = Some(key);
        }
        if self.streak >= MAX_REPEAT_STREAK {
            return Some(format!(
                "the model repeated the identical tool batch {} times in a row",
                self.streak
            ));
        }

        // Arm 2: repetition within the sliding window.
        self.window.push_back(key);
        if self.window.len() > WINDOW_SIZE {
            self.window.pop_front();
        }
        let repeats = self.window.iter().filter(|k| **k == key).count();
        if repeats >= WINDOW_MAX_REPEATS {
            return Some(format!(
                "the model repeated the identical tool batch {repeats} times within the last {} iterations",
                self.window.len()
            ));
        }
        None
    }
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{StopPolicy, MAX_REPEAT_STREAK, WINDOW_MAX_REPEATS, WINDOW_SIZE};

    fn call(name: &str, input: &str) -> (String, String) {
        (name.to_string(), input.to_string())
    }

    #[test]
    fn consecutive_identical_batches_trip_the_streak_arm() {
        let mut policy = StopPolicy::new();
        let batch = vec![call("read_file", r#"{"path":"a.rs"}"#)];
        assert!(policy.register(&batch).is_none());
        assert!(policy.register(&batch).is_none());
        // Third identical batch trips (MAX_REPEAT_STREAK = 3).
        let reason = policy.register(&batch).expect("streak arm trips");
        assert!(reason.contains(&MAX_REPEAT_STREAK.to_string()), "{reason}");
    }

    #[test]
    fn a_different_call_resets_the_streak() {
        let mut policy = StopPolicy::new();
        let a = vec![call("read_file", r#"{"path":"a.rs"}"#)];
        let b = vec![call("read_file", r#"{"path":"b.rs"}"#)];
        assert!(policy.register(&a).is_none());
        assert!(policy.register(&a).is_none());
        assert!(policy.register(&b).is_none());
        // Streak broken; b alone starts from 1.
        assert!(policy.register(&a).is_none());
    }

    #[test]
    fn ever_changing_batches_never_trip_either_arm() {
        let mut policy = StopPolicy::new();
        for i in 0..(WINDOW_SIZE * 3) {
            let batch = vec![call("read_file", &format!(r#"{{"path":"f{i}.rs"}}"#))];
            assert!(policy.register(&batch).is_none(), "tripped at {i}");
        }
    }

    #[test]
    fn window_arm_trips_without_consecutiveness() {
        let mut policy = StopPolicy::new();
        let loop_batch = vec![call("bash", r#"{"cmd":"cargo build"}"#)];
        let filler = vec![call("read_file", r#"{"path":"c.rs"}"#)];
        // Pattern: loop, filler, loop, filler, ... — never consecutive, but
        // once the window fills with loop keys every loop iteration trips.
        let mut trips = 0;
        for i in 0..20 {
            let batch = if i % 2 == 0 { &loop_batch } else { &filler };
            if policy.register(batch).is_some() {
                trips += 1;
            }
        }
        assert!(trips >= 1, "window arm never tripped");
    }

    #[test]
    fn window_arm_threshold_matches_constants() {
        let mut policy = StopPolicy::new();
        let batch = vec![call("bash", r#"{"cmd":"ls"}"#)];
        let filler = vec![call("bash", r#"{"cmd":"ls -la"}"#)];
        // WINDOW_MAX_REPEATS - 1 occurrences interleaved with fillers: no trip.
        for i in 0..(2 * (WINDOW_MAX_REPEATS - 1)) {
            let b = if i % 2 == 0 { &batch } else { &filler };
            assert!(policy.register(b).is_none(), "trip too early at {i}");
        }
        // One more occurrence within the window trips.
        assert!(policy.register(&batch).is_some());
    }

    #[test]
    fn ordering_inside_one_batch_does_not_change_the_key() {
        let a = vec![
            call("read_file", r#"{"path":"a.rs"}"#),
            call("read_file", r#"{"path":"b.rs"}"#),
        ];
        let b = vec![
            call("read_file", r#"{"path":"b.rs"}"#),
            call("read_file", r#"{"path":"a.rs"}"#),
        ];
        let mut policy = StopPolicy::new();
        assert!(policy.register(&a).is_none());
        assert!(policy.register(&b).is_none());
        // Same content, different order -> same key -> streak continues to 3.
        assert!(policy.register(&a).is_some());
    }

    #[test]
    fn empty_batches_are_registered_with_a_distinct_key() {
        let mut policy = StopPolicy::new();
        let empty: Vec<(String, String)> = Vec::new();
        assert!(policy.register(&empty).is_none());
        assert!(policy.register(&empty).is_none());
        assert!(policy.register(&empty).is_some());
    }
}
