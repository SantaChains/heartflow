//! Loop-mode auto-continue driver (roadmap C-1 末相): after an operator's
//! kickoff turn, keep running turns autonomously until the single active
//! mission completes, blocks (three consecutive failed turns), the operator
//! interrupts, or a hard safety cap is reached.
//!
//! Each iteration re-reads the mission ledger from disk — the same discipline
//! as `auto_record_turn`/`turn_reminder`, where disk is the mission source of
//! truth. Completion is checked *before* each turn (mechanical criteria
//! re-verify at complete time), so the turn that satisfies the last criterion
//! is followed by a stop, not another turn.

use std::io;
use std::path::Path;

use runtime::{MissionError, MissionLedger, MissionStatus};

use crate::{run_turn_interactive, AgentRuntime, CliPermissionPrompter, SessionShared};

/// REPL-side gate for the loop-mode auto-continue: after ANY turn-driving
/// path (direct input, queued delivery, `/guide`), a loop-mode session keeps
/// driving the single active mission until it completes, blocks, is
/// interrupted, or hits the hard cap. Extracted so every turn path shares
/// one gate instead of the main input path being special.
pub(crate) async fn maybe_autopilot(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
    mode: &str,
) -> io::Result<()> {
    if mode != "loop" {
        return Ok(());
    }
    let missions_root = crate::storage::sessions_dir().join("missions");
    run_loop_autopilot(state, runtime, prompter, &missions_root).await
}

/// Hard safety cap on consecutive auto-continue turns per operator kickoff.
/// The mission budget's `turns` axis is the canonical limit; this exists so a
/// criteria-less or manual-criteria mission cannot loop forever.
const MAX_AUTOPILOT_TURNS: usize = 64;

/// S2 (roadmap §16 R2): transport-class turn failures (connection drops,
/// timeouts, rate-limit bursts, stream deaths) are transient — they no longer
/// count as blocked claims. Instead the autopilot retries the same nudge with
/// exponential backoff up to this many times; only after exhausting the
/// retries does one blocked claim get recorded. Auth/overflow/model failures
/// still count immediately (a blocked mission is a *real* block). The TUI's
/// loop autopilot (`tui/stream.rs`) shares this budget and schedule.
pub(crate) const MAX_TRANSPORT_RETRIES: usize = 3;

/// Backoff schedule for transport retries: 30s -> 2m -> 8m (capped).
#[must_use]
pub(crate) fn transport_backoff(retry_index: usize) -> std::time::Duration {
    match retry_index {
        0 => std::time::Duration::from_secs(30),
        1 => std::time::Duration::from_secs(120),
        _ => std::time::Duration::from_secs(480),
    }
}

/// The single active mission's id, or `None`. With several active missions the
/// attribution of a turn's budget is a guess, so the autopilot refuses rather
/// than pick one (mirrors `auto_record_turn`'s single-active rule).
pub(crate) fn single_active_mission_id(ledger: &MissionLedger) -> Option<u64> {
    let active: Vec<u64> = ledger
        .list()
        .iter()
        .filter(|mission| mission.status == MissionStatus::Active)
        .map(|mission| mission.id)
        .collect();
    (active.len() == 1).then(|| active[0])
}

/// The driving half of D1.2: an exhausted budget keeps the mission `Active`
/// (the reminder goes quiet, the mission survives for `/mission resume` or a
/// raised budget) — but the autopilot must stop nudging it. A spent budget is
/// the operator's cap, not a suggestion; driving on burns tokens for work
/// that was capped.
pub(crate) fn budget_stop(ledger: &MissionLedger, mission_id: u64) -> bool {
    ledger
        .get(mission_id)
        .is_some_and(|mission| mission.budget_exhausted)
}

/// The continuation nudge for an auto-continue turn. The per-turn mission
/// reminder already carries the `goal_continue` contract in the system prompt;
/// this user message names the concrete next step so the model does not stop.
pub(crate) fn continue_nudge(mission_id: u64) -> String {
    format!(
        "AUTO-CONTINUE: mission m{mission_id} is not complete. Continue working toward it autonomously: take the next concrete step, verify the remaining criteria, and persist durable findings. Do not stop until every criterion is verified or you are genuinely blocked - if blocked, say exactly what is blocked and why."
    )
}

/// Run the loop-mode auto-continue cycle after an operator's kickoff turn.
pub(crate) async fn run_loop_autopilot(
    state: &SessionShared,
    runtime: &mut AgentRuntime,
    prompter: &mut CliPermissionPrompter,
    missions_root: &Path,
) -> io::Result<()> {
    // S2: transport faults retry with backoff instead of poisoning the
    // blocked streak; the counter resets on any non-transport outcome.
    let mut transport_retries = 0usize;

    for _ in 0..MAX_AUTOPILOT_TURNS {
        let ledger = MissionLedger::load(missions_root);
        let Some(mission_id) = single_active_mission_id(&ledger) else {
            // No active mission (none, or several: ambiguous), or it already
            // reached a terminal state — nothing to drive autonomously.
            return Ok(());
        };

        // Completion gate first: mechanical criteria re-verify at complete
        // time; a satisfied mission stops the loop immediately.
        match ledger.complete(mission_id) {
            Ok(_) => {
                println!("loop: mission m{mission_id} complete - stopping.");
                return Ok(());
            }
            Err(MissionError::Unsatisfied { .. }) => {
                // D1.2 driving half: the criteria are not met, but a spent
                // budget stops the driver — the mission stays open for the
                // operator; it just stops spending on its own.
                if budget_stop(&ledger, mission_id) {
                    println!(
                        "loop: mission m{mission_id} budget exhausted - stopping (raise the budget with /mission or finish the remaining criteria manually)."
                    );
                    return Ok(());
                }
            }
            Err(MissionError::WeakOnly { .. }) => {
                println!(
                    "loop: mission m{mission_id} has only weak (claimed) evidence - add a mechanical criterion or approve it manually; stopping."
                );
                return Ok(());
            }
            // NotActive/NotFound: the mission transitioned (paused, blocked,
            // complete) or vanished between iterations — stop either way.
            Err(_) => return Ok(()),
        }

        let outcome =
            run_turn_interactive(state, runtime, &continue_nudge(mission_id), Some(prompter))
                .await?;

        if outcome.interrupted {
            println!("loop: interrupted by the operator - auto-continue paused.");
            return Ok(());
        }
        if outcome.ok {
            transport_retries = 0;
            continue;
        }
        // S2: a transport-class failure is transient. Back off and retry the
        // same nudge without touching the blocked streak; after exhausting
        // the retries, fall through and count one real blocked claim.
        if outcome.error_class == Some(runtime::ErrorKind::Transport)
            && transport_retries < MAX_TRANSPORT_RETRIES
        {
            let delay = transport_backoff(transport_retries);
            transport_retries += 1;
            println!(
                "loop: transport failure ({}); retrying in {}s (transport retry {transport_retries}/{MAX_TRANSPORT_RETRIES}). Ctrl+C pauses auto-continue.",
                runtime::ErrorKind::Transport.label(),
                delay.as_secs()
            );
            // A1 (semantic review): the backoff must stay interruptible —
            // racing ctrl_c means the operator can always stop the loop, no
            // matter how long the backoff window is.
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                result = tokio::signal::ctrl_c() => {
                    let _ = result;
                    println!("loop: interrupted by the operator during backoff - auto-continue paused.");
                    return Ok(());
                }
            }
            continue;
        }
        transport_retries = 0;
        // A failed turn is a blocked claim; the ledger flips to Blocked on
        // the third consecutive claim (anti-early-give-up). A successful
        // turn already reset the streak via `auto_record_turn`.
        match ledger.mark_blocked(mission_id, "auto-continue turn failed") {
            Ok((mission, true)) => {
                println!(
                    "loop: mission m{mission_id} blocked after {} consecutive failed turns - stopping.",
                    mission.blocked_streak
                );
                return Ok(());
            }
            Ok((mission, false)) => {
                println!(
                    "loop: blocked claim {}/{} recorded; continuing.",
                    mission.blocked_streak,
                    runtime::BLOCKED_STREAK_NEEDED
                );
            }
            Err(_) => return Ok(()),
        }
    }
    println!("loop: auto-continue cap ({MAX_AUTOPILOT_TURNS}) reached - stopping.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{budget_stop, continue_nudge, single_active_mission_id};
    use runtime::{CompletionCriterion, MissionBudget, MissionLedger, MissionStatus};

    #[test]
    fn single_active_mission_picks_the_only_active_and_ignores_terminal() {
        let ledger = MissionLedger::new();
        // Empty: nothing active.
        assert_eq!(single_active_mission_id(&ledger), None);

        let m1 = ledger.create(
            "ship the thing",
            vec![CompletionCriterion::FreeText {
                check: "claimed".to_string(),
            }],
            MissionBudget::default(),
        );
        assert_eq!(single_active_mission_id(&ledger), Some(m1.id));

        // A second active mission makes attribution ambiguous -> None.
        let m2 = ledger.create(
            "other thing",
            vec![CompletionCriterion::FreeText {
                check: "claimed".to_string(),
            }],
            MissionBudget::default(),
        );
        assert_eq!(single_active_mission_id(&ledger), None);

        // Pausing one leaves a single active mission again.
        ledger.pause(m2.id).expect("pause");
        assert_eq!(single_active_mission_id(&ledger), Some(m1.id));

        // A completed mission is no longer active.
        let m3 = ledger.create(
            "done thing",
            vec![CompletionCriterion::CommandSucceeds {
                cmd: "exit 0".to_string(),
            }],
            MissionBudget::default(),
        );
        let m3 = ledger
            .complete(m3.id)
            .expect("mechanical criterion verifies");
        assert_eq!(m3.status, MissionStatus::Complete);
        // m1 active + m3 complete -> single active is still m1.
        assert_eq!(single_active_mission_id(&ledger), Some(m1.id));
    }

    #[test]
    fn continue_nudge_names_the_mission() {
        let nudge = continue_nudge(7);
        assert!(nudge.contains("m7"));
        assert!(nudge.contains("AUTO-CONTINUE"));
    }

    #[test]
    fn budget_stop_trips_only_when_the_budget_is_actually_spent() {
        let ledger = MissionLedger::new();
        let mission = ledger.create(
            "capped work",
            vec![CompletionCriterion::FreeText {
                check: "claimed".to_string(),
            }],
            MissionBudget {
                tokens: Some(100),
                turns: None,
                wall_ms: None,
            },
        );
        // Fresh budget: keep driving.
        assert!(!budget_stop(&ledger, mission.id));
        // Partial spend (below the attention fraction): keep driving.
        let _ = ledger.record_turn(mission.id, 10);
        assert!(!budget_stop(&ledger, mission.id));
        // Fully spent: the mission stays Active (D1.2) but the driver stops.
        let (spent, _) = ledger
            .record_turn(mission.id, 100)
            .expect("second turn records");
        assert_eq!(spent.status, MissionStatus::Active);
        assert!(budget_stop(&ledger, mission.id));
    }

    #[test]
    fn transport_backoff_is_exponential_and_capped() {
        use super::transport_backoff;
        assert_eq!(transport_backoff(0).as_secs(), 30);
        assert_eq!(transport_backoff(1).as_secs(), 120);
        assert_eq!(transport_backoff(2).as_secs(), 480);
        // Out-of-range indices clamp to the cap, never panic.
        assert_eq!(transport_backoff(9).as_secs(), 480);
        assert!(transport_backoff(0) < transport_backoff(1));
        assert!(transport_backoff(1) < transport_backoff(2));
    }
}
