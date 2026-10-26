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
            Err(MissionError::Unsatisfied { .. }) => {}
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
        if !outcome.ok {
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
    }
    println!("loop: auto-continue cap ({MAX_AUTOPILOT_TURNS}) reached - stopping.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{continue_nudge, single_active_mission_id};
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
}
