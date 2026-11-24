//! The harness: guardrails that make a cheap model finish tasks.
//!
//! - Between steps, [`Harness::before_step`] returns a stuck nudge when the
//!   turn is looping.
//! - When the model stops, [`Harness::finish_decision`] returns a nudge for one
//!   more pass (leaked call, unverified edits, a failed last check, nothing
//!   done on a change request, open plan steps), at most
//!   [`MAX_CONTINUATIONS`] times per turn.
//! - Tool-call arguments and names are repaired in [`args`].

pub mod args;
pub mod guard;
pub mod leaked;

use serde_json::Value;

use crate::protocol::NudgeReason;
use crate::protocol::ToolKind;
use crate::task::{ContinuationStop, ContinuationStopCause};
use guard::TurnGuard;

/// Extra passes the harness may request per turn: room for each finish rule
/// to fire once (leaked call, verify, failed check, watchdog or open plan).
pub const MAX_CONTINUATIONS: u32 = 4;

/// Harness state for one user turn.
type FinishNudge = (NudgeReason, guard::FinishProgress);

pub struct Harness {
    guard: TurnGuard,
    continuations: u32,
    last_finish_nudge: Option<FinishNudge>,
    continuation_stop: Option<ContinuationStop>,
    blocker_recovery_nudged: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskRecord;

    #[test]
    fn repeated_finish_nudges_without_real_progress_stop_as_incomplete() {
        let mut task = TaskRecord::new(1, "Fix the code".into(), 0);
        task.observed_edits = 1;
        let mut harness = Harness::new("Fix the code");
        harness.set_task_record(&task);
        assert!(matches!(
            harness.finish_decision("done", &[]),
            FinishDecision::Continue { .. }
        ));
        // Possible shell effects may bump the revision, but no actual edit
        // or accepted check was observed.
        task.set_work_revision(1);
        harness.set_task_record(&task);
        assert_eq!(
            harness.finish_decision("done", &[]),
            FinishDecision::NoProgress
        );
    }

    #[test]
    fn actual_edits_allow_a_new_bounded_verification_followup() {
        let mut task = TaskRecord::new(1, "Fix the code".into(), 0);
        task.observed_edits = 1;
        let mut harness = Harness::new("Fix the code");
        harness.set_task_record(&task);
        assert!(matches!(
            harness.finish_decision("done", &[]),
            FinishDecision::Continue { .. }
        ));
        task.set_work_revision(1);
        task.observed_edits = 2;
        harness.set_task_record(&task);
        assert!(matches!(
            harness.finish_decision("done", &[]),
            FinishDecision::Continue { .. }
        ));
    }

    fn command(task: &mut TaskRecord, command: &str, order: u64, revision: u64) {
        task.observe_command(
            command,
            command,
            crate::task::CommandTiming {
                started: revision,
                finished: revision,
                order,
            },
            &crate::tools::ToolOutcome {
                output: "ok".into(),
                exit_code: Some(0),
                success: true,
                diff: None,
            },
            crate::task::is_verification(command),
        );
    }

    #[test]
    fn passing_bun_checks_then_install_allow_recovery_but_never_certify_stale_work() {
        let mut task = TaskRecord::new(1, "Fix the code".into(), 0);
        task.observed_edits = 1;
        let mut harness = Harness::new("Fix the code");
        command(&mut task, "bun test | tail -40", 1, 0);
        harness.set_task_record(&task);
        assert!(matches!(
            harness.finish_decision("done", &[]),
            FinishDecision::Continue { .. }
        ));
        command(&mut task, "bun run typecheck", 2, 0);
        command(&mut task, "bun test", 3, 0);
        harness.set_task_record(&task);
        command(&mut task, "bun install --frozen-lockfile", 4, 2);
        harness.set_task_record(&task);
        assert!(!task.has_current_verification());
        task.assess(crate::TurnEndReason::Completed, true);
        assert!(!task.verified_for_queue());
        assert!(matches!(harness.finish_decision("done", &[]),
            FinishDecision::Continue { message, .. } if message.contains("bun install --frozen-lockfile")
                && !message.contains("The shell wrapper")));
        command(&mut task, "bun run typecheck", 5, 2);
        command(&mut task, "bun test", 6, 2);
        harness.set_task_record(&task);
        assert_eq!(
            harness.finish_decision("checked", &[]),
            FinishDecision::Stop
        );
        assert!(task.has_current_verification());
        assert!(harness.continuation_stop().is_none());
    }

    #[test]
    fn checking_then_invalidating_forever_still_exhausts_the_fixed_budget() {
        let mut task = TaskRecord::new(1, "Fix the code".into(), 0);
        task.observed_edits = 1;
        let mut harness = Harness::new("Fix the code");
        for pass in 0..=MAX_CONTINUATIONS {
            command(
                &mut task,
                "bun test",
                u64::from(pass) * 2 + 1,
                u64::from(pass) * 2,
            );
            command(
                &mut task,
                "bun install --frozen-lockfile",
                u64::from(pass) * 2 + 2,
                u64::from(pass) * 2 + 2,
            );
            harness.set_task_record(&task);
            let decision = harness.finish_decision("done", &[]);
            if pass < MAX_CONTINUATIONS {
                assert!(matches!(decision, FinishDecision::Continue { .. }));
            } else {
                assert_eq!(decision, FinishDecision::BudgetExhausted);
            }
        }
        let stop = harness.continuation_stop().unwrap();
        assert_eq!(stop.cause, ContinuationStopCause::BudgetExhausted);
        assert_eq!(stop.followups, MAX_CONTINUATIONS);
        assert!(!task.has_current_verification());
    }

    #[test]
    fn overlapping_check_epochs_are_not_recovery_progress() {
        let mut task = TaskRecord::new(1, "Fix the code".into(), 0);
        task.observed_edits = 1;
        let mut harness = Harness::new("Fix the code");
        harness.set_task_record(&task);
        assert!(matches!(
            harness.finish_decision("done", &[]),
            FinishDecision::Continue { .. }
        ));
        command(&mut task, "bun test", 1, 1);
        task.checks[0].started_revision = 0;
        harness.set_task_record(&task);
        assert_eq!(
            harness.finish_decision("done", &[]),
            FinishDecision::NoProgress
        );
        let stop = harness.continuation_stop().unwrap();
        assert_eq!(stop.cause, ContinuationStopCause::NoProgress);
        assert_eq!(stop.followups, 1);
    }

    #[test]
    fn protected_blockers_never_reach_the_recovery_nudge() {
        for source in ["user review", "child dependency", "agent report"] {
            let mut task = TaskRecord::new(1, "Analyze the source.".into(), 0);
            task.add_blocker("Input is unavailable", source);
            if source == "agent report" {
                task.note_scope_denial("run_command", &serde_json::json!({"command":"cargo test"}));
            }
            let mut harness = Harness::new("Analyze the source.");
            harness.set_task_record(&task);
            assert_eq!(
                harness.finish_decision("Cannot proceed.", &[]),
                FinishDecision::Stop
            );
            assert!(!harness.blocker_recovery_nudged);
        }
    }

    #[test]
    fn blocker_recovery_obeys_the_existing_total_followup_budget() {
        let mut task = TaskRecord::new(1, "What changed?".into(), 0);
        task.add_blocker("No activity in scratch folder", "agent report");
        let mut harness = Harness::new("What changed?");
        harness.set_task_record(&task);
        harness.continuations = MAX_CONTINUATIONS;
        assert_eq!(
            harness.finish_decision("Cannot tell.", &[]),
            FinishDecision::BudgetExhausted
        );
        assert_eq!(
            harness.continuation_stop().unwrap().followups,
            MAX_CONTINUATIONS
        );
        assert_eq!(task.blockers.len(), 1);
    }
}

/// Bounded execution control. `Stop` is not a judgment of task acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishDecision {
    Continue {
        reason: NudgeReason,
        message: String,
    },
    Stop,
    BudgetExhausted,
    NoProgress,
}

impl Harness {
    pub fn new(user_message: &str) -> Self {
        Self {
            guard: TurnGuard::new(user_message),
            continuations: 0,
            last_finish_nudge: None,
            continuation_stop: None,
            blocker_recovery_nudged: false,
        }
    }

    pub fn record_tool_call(
        &mut self,
        name: &str,
        kind: ToolKind,
        args: &Value,
        path: Option<&str>,
    ) {
        self.guard.record_tool_call(name, kind, args, path);
    }

    pub fn record_tool_result(
        &mut self,
        name: &str,
        kind: ToolKind,
        args: &Value,
        output: &str,
        exit_code: Option<i32>,
        success: bool,
    ) {
        self.guard
            .record_tool_result(name, kind, args, output, exit_code, success);
    }

    /// A message the user sent while the turn was running.
    pub fn add_user_message(&mut self, text: &str) {
        self.guard.add_user_message(text);
    }

    pub fn set_task_contract(&mut self, contract: &crate::task::TaskContract) {
        self.guard.set_task_contract(contract);
    }

    pub fn set_task_record(&mut self, task: &crate::task::TaskRecord) {
        self.guard.set_task_record(task);
    }

    /// The model's latest `update_plan`: the steps still open.
    pub fn record_plan(&mut self, open: Vec<String>) {
        self.guard.record_plan(open);
    }

    /// A stuck nudge to append before the next model call, if any.
    pub fn before_step(&mut self) -> Option<String> {
        self.guard.take_stuck_nudge()
    }

    pub fn finish_decision(&mut self, final_text: &str, tool_names: &[&str]) -> FinishDecision {
        self.continuation_stop = None;
        let nudge = if self.guard.has_blockers() {
            let recovery = if !self.blocker_recovery_nudged && self.guard.can_recover_blocker() {
                self.blocker_recovery_nudged = true;
                Some((
                    NudgeReason::BlockerRecovery,
                    guard::BLOCKER_RECOVERY_NUDGE.into(),
                ))
            } else {
                None
            };
            recovery
                .or_else(|| {
                    leaked::detect_leaked_tool_call(final_text, tool_names)
                        .map(|call| (NudgeReason::LeakedCall, leaked::leaked_call_nudge(&call)))
                })
                .or_else(|| self.guard.factual_finish())
        } else if let Some(message) = self.guard.analysis_scope_nudge(final_text) {
            Some((NudgeReason::Watchdog, message))
        } else {
            match leaked::detect_leaked_tool_call(final_text, tool_names) {
                Some(call) => Some((NudgeReason::LeakedCall, leaked::leaked_call_nudge(&call))),
                None => self.guard.before_finish(),
            }
        };
        match nudge {
            Some((_, message)) if self.continuations >= MAX_CONTINUATIONS => {
                self.continuation_stop = Some(ContinuationStop {
                    cause: ContinuationStopCause::BudgetExhausted,
                    followups: self.continuations,
                    condition: crate::tools::head_tail(&message, 3_800),
                });
                FinishDecision::BudgetExhausted
            }
            Some((reason, message)) => {
                if let Some(progress) = self.guard.finish_progress() {
                    if self.last_finish_nudge.as_ref().is_some_and(
                        |(previous_reason, previous_progress)| {
                            previous_reason == &reason && previous_progress == &progress
                        },
                    ) {
                        self.continuation_stop = Some(ContinuationStop {
                            cause: ContinuationStopCause::NoProgress,
                            followups: self.continuations,
                            condition: crate::tools::head_tail(&message, 3_800),
                        });
                        return FinishDecision::NoProgress;
                    }
                    self.last_finish_nudge = Some((reason, progress));
                }
                self.continuations += 1;
                FinishDecision::Continue { reason, message }
            }
            None => FinishDecision::Stop,
        }
    }

    pub fn continuation_stop(&self) -> Option<ContinuationStop> {
        self.continuation_stop.clone()
    }
}
