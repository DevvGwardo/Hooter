//! Opt-in recovery of the current protected native task, not queue advancement
//! or permission to turn a read-only review into implementation.

use flint_agent::{
    AgentKind, AutomaticReviewVerdict, TaskMode, TaskOutcome, TaskRecord, TaskSource, TurnEndReason,
};
use gpui_kit::Context;

use crate::app::FlintApp;

pub const MAX_AUTO_CONTINUATIONS: u32 = 3;
pub const MAX_AUTO_REVIEWS: u32 = MAX_AUTO_CONTINUATIONS + 1;

#[derive(Default)]
pub struct AutoContinueState {
    pub attempts: u32,
    pub review_attempts: u32,
    pub paused_reason: Option<&'static str>,
    transient_error_pause: bool,
    previous: Option<TaskRecord>,
    previous_review: Option<TaskRecord>,
}

pub(crate) fn is_recoverable_error_notice(message: &str) -> bool {
    message.ends_with("Retrying this step.")
        || message.contains("doesn't accept reasoning_effort; continuing without it.")
}

fn same_task_snapshot(previous: &TaskRecord, task: &TaskRecord) -> bool {
    previous.contract.id == task.contract.id
        && previous.contract.revision == task.contract.revision
        && previous.work_revision == task.work_revision
        && previous.blockers == task.blockers
        && previous.files == task.files
        && previous.plan == task.plan
        && previous
            .assessment
            .as_ref()
            .map(|a| (&a.issues, &a.requirements))
            == task
                .assessment
                .as_ref()
                .map(|a| (&a.issues, &a.requirements))
}

impl AutoContinueState {
    pub(crate) fn pause(&mut self, reason: &'static str) {
        self.paused_reason = Some(reason);
        self.transient_error_pause = false;
    }

    pub(crate) fn pause_for_recoverable_error(&mut self, reason: &'static str) {
        if self.paused_reason.is_none() {
            self.paused_reason = Some(reason);
            self.transient_error_pause = true;
        }
    }

    pub(crate) fn clear_transient_error_pause(&mut self) {
        if self.transient_error_pause {
            self.paused_reason = None;
            self.transient_error_pause = false;
        }
    }

    pub(crate) fn lock_transient_error_pause(&mut self) {
        self.transient_error_pause = false;
    }

    fn prepare(&mut self, task: &TaskRecord) -> bool {
        if self.paused_reason.is_some() {
            return false;
        }
        if self.attempts >= MAX_AUTO_CONTINUATIONS {
            self.pause(
                "The three automatic follow-ups were used. Review the task before continuing.",
            );
            return false;
        }
        if self
            .previous
            .as_ref()
            .is_some_and(|previous| same_task_snapshot(previous, task))
        {
            self.pause("No recovery progress was observed. Review the task before continuing.");
            return false;
        }
        self.previous = Some(task.clone());
        self.attempts += 1;
        true
    }

    fn prepare_review(&mut self, task: &TaskRecord) -> bool {
        if self.paused_reason.is_some() {
            return false;
        }
        if self.review_attempts >= MAX_AUTO_REVIEWS {
            self.pause(
                "The automatic review limit was reached. Review the task before continuing.",
            );
            return false;
        }
        if self.previous_review.as_ref().is_some_and(|previous| {
            same_task_snapshot(previous, task) && previous.automatic_review == task.automatic_review
        }) {
            self.pause("Automatic review made no progress. Review the task before continuing.");
            return false;
        }
        self.previous_review = Some(task.clone());
        self.review_attempts += 1;
        true
    }
}

fn needs_recovery(task: &TaskRecord) -> bool {
    task.source == TaskSource::Native
        && task.contract.mode == TaskMode::Work
        && !task.is_accepted()
        && !task.evidence_overflow
        && task.continuation_stop.is_none()
        && task.scope_denials.is_empty()
        && !task
            .blockers
            .iter()
            .any(|blocker| blocker.source == "user review")
        && task.assessment.as_ref().is_some_and(|assessment| {
            assessment.task_id == task.contract.id
                && assessment.contract_revision == task.contract.revision
                && assessment.work_revision == task.work_revision
                && assessment.reason == TurnEndReason::Completed
                && matches!(
                    assessment.outcome,
                    TaskOutcome::Partial | TaskOutcome::Blocked
                )
        })
}

impl FlintApp {
    pub(crate) fn auto_continue_task(&mut self, ix: usize, cx: &mut Context<Self>) -> bool {
        let session = &self.sessions[ix];
        if !self.settings.auto_continue_fixes
            || session.agent != AgentKind::Flint
            || self.prompt_busy(ix)
            || session.stopping
            || session.history_save_failed
            || session.storage_failed()
            || session.native_history_conflict
            || session.prompt_queue.save_failed
            || session.prompt_queue.unreadable
            || session.steering_pending.is_some()
            || session.view.pending_approvals > 0
            || session.view.has_pending_approval()
            || !matches!(session.view.last_reason, Some(TurnEndReason::Completed))
            || session.ops.as_ref().is_none_or(|ops| ops.is_closed())
        {
            return false;
        }
        let Some(task) = session.view.task.as_ref().cloned() else {
            return false;
        };
        if self.sessions[ix].auto_continue.paused_reason.is_some() {
            return false;
        }
        if task.can_automatically_review() {
            self.hold_queue_for_review(ix, cx);
            if self.sessions[ix].prompt_queue.save_failed {
                return false;
            }
            if !self.sessions[ix].auto_continue.prepare_review(&task) {
                return false;
            }
            return self.dispatch_automatic_review(
                ix,
                task.contract.id,
                task.contract.revision,
                task.work_revision,
                cx,
            );
        }
        if task.automatic_review.as_ref().is_some_and(|review| {
            review.verdict == AutomaticReviewVerdict::Blocked
                && review.work_revision == task.work_revision
                && review.contract_revision == task.contract.revision
        }) {
            self.sessions[ix].auto_continue.pause(
                "Independent review is blocked on evidence or access. Review its findings before continuing.",
            );
            return false;
        }
        if !needs_recovery(&task) {
            return false;
        }
        self.hold_queue_for_review(ix, cx);
        if self.sessions[ix].prompt_queue.save_failed {
            return false;
        }
        if !self.sessions[ix].auto_continue.prepare(&task) {
            return false;
        }
        self.dispatch_automatic_task_continuation(ix, task.contract.id, task.contract.revision, cx)
    }

    pub(crate) fn automatic_resume_block_reason(
        &self,
        ix: usize,
        task: &TaskRecord,
    ) -> Option<&'static str> {
        let session = self.sessions.get(ix)?;
        if !self.settings.auto_continue_fixes {
            return Some("Turn Auto-fix on to resume automatic fixes and review.");
        }
        if session.agent != AgentKind::Flint || task.source != TaskSource::Native {
            return Some("Automatic fixes and review apply only to native tasks.");
        }
        if task.contract.mode != TaskMode::Work {
            return Some("Automatic fixes and review apply only to work tasks.");
        }
        let outcome = task
            .assessment
            .as_ref()
            .map_or(TaskOutcome::Unknown, |assessment| assessment.outcome);
        if task.is_accepted() || outcome == TaskOutcome::Verified {
            return Some("This task does not need automatic fixes or review.");
        }
        if session.view.pending_approvals > 0 || session.view.has_pending_approval() {
            return Some("Resolve pending tool approvals before resuming automatic fixes.");
        }
        if session.steering_pending.is_some() {
            return Some("Wait for the steered message before resuming automatic fixes.");
        }
        if session.prompt_queue.save_failed || session.prompt_queue.unreadable {
            return Some("Repair the saved prompt queue before resuming automatic fixes.");
        }
        if task.evidence_overflow {
            return Some(
                "The observation limit was reached; review the task or continue manually.",
            );
        }
        if !task.scope_denials.is_empty() {
            return Some(
                "Scope restrictions blocked part of this task; revise scope or continue manually.",
            );
        }
        if task
            .blockers
            .iter()
            .any(|blocker| blocker.source == "user review")
        {
            return Some("A user decision is required; review acceptance or revise scope.");
        }
        if task.automatic_review.as_ref().is_some_and(|review| {
            review.verdict == AutomaticReviewVerdict::Blocked
                && review.work_revision == task.work_revision
                && review.contract_revision == task.contract.revision
        }) {
            return Some(
                "Independent review is blocked on evidence or access. Review its findings before continuing.",
            );
        }
        let Some(assessment) = task.assessment.as_ref() else {
            return Some("No task assessment is available; continue manually if needed.");
        };
        if assessment.task_id != task.contract.id
            || assessment.contract_revision != task.contract.revision
            || assessment.work_revision != task.work_revision
        {
            return Some(
                "Task scope or workspace changed since the last assessment; continue manually or review the current state.",
            );
        }
        if task.can_automatically_review() {
            return None;
        }
        match assessment.outcome {
            TaskOutcome::Partial | TaskOutcome::Blocked => None,
            TaskOutcome::NeedsReview => {
                if task.unknown_effects {
                    Some(
                        "Unverified command effects block automatic review; review acceptance or continue manually.",
                    )
                } else if !task.has_current_verification() {
                    Some(
                        "Current passing verification is required before automatic review; continue manually or review acceptance.",
                    )
                } else if task.plan.iter().any(|step| step.status.is_open()) {
                    Some("Open plan steps remain; continue the task manually.")
                } else {
                    Some(
                        "Automatic review cannot run on this task state; review acceptance or continue manually.",
                    )
                }
            }
            TaskOutcome::Unknown
            | TaskOutcome::Answered
            | TaskOutcome::Verified
            | TaskOutcome::Accepted => Some(
                "No recoverable work or reviewable result was assessed; continue manually if needed.",
            ),
        }
    }

    pub(crate) fn resume_automatic_task_in(
        &mut self,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        if ix >= self.sessions.len() {
            return Err("This session is no longer available.");
        }
        if !self.settings.auto_continue_fixes {
            return Err("Turn Auto-fix on to resume automatic fixes and review.");
        }
        if self.prompt_busy(ix) {
            return Err("Wait for the current turn to stop before resuming automatic fixes.");
        }
        if self.sessions[ix].stopping {
            return Err("The engine is stopping; wait for it to finish before continuing.");
        }
        if self.sessions[ix].history_save_failed || self.sessions[ix].native_history_conflict {
            return Err("Repair or confirm saved native history before resuming automatic fixes.");
        }
        if !self.storage_ready(ix, cx) {
            return Err("Repair session storage before resuming automatic fixes.");
        }
        let Some(task) = self.sessions[ix].view.task.as_ref().cloned() else {
            return Err("No task is available to resume.");
        };
        if let Some(reason) = self.automatic_resume_block_reason(ix, &task) {
            return Err(reason);
        }
        if let Err(error) = self.ensure_engine(ix, cx) {
            self.store_error = Some(format!("{error:#}"));
            cx.notify();
            return Err("Couldn't start the engine.");
        }
        if self.sessions[ix]
            .ops
            .as_ref()
            .is_none_or(|ops| ops.is_closed())
        {
            return Err("The engine is unavailable. Continue or restart the session.");
        }
        self.sessions[ix].auto_continue = Default::default();
        self.hold_queue_for_review(ix, cx);
        if self.sessions[ix].prompt_queue.save_failed {
            return Err("Couldn't save the prompt queue.");
        }
        let dispatched = if task.can_automatically_review() {
            self.sessions[ix].auto_continue.prepare_review(&task)
                && self.dispatch_automatic_review(
                    ix,
                    task.contract.id,
                    task.contract.revision,
                    task.work_revision,
                    cx,
                )
        } else {
            self.sessions[ix].auto_continue.prepare(&task)
                && self.dispatch_automatic_task_continuation(
                    ix,
                    task.contract.id,
                    task.contract.revision,
                    cx,
                )
        };
        if !dispatched {
            return Err("Couldn't resume automatic fixes and review.");
        }
        Ok(())
    }

    pub fn resume_automatic_task(&mut self, cx: &mut Context<Self>) {
        let ix = self.active;
        if let Err(reason) = self.resume_automatic_task_in(ix, cx)
            && let Some(session) = self.sessions.get_mut(ix)
        {
            session.auto_continue.pause(reason);
            if let Some(panel) = &mut self.task_panel
                && panel.target.parent == session.uid
            {
                panel.error = Some(reason.into());
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Options;
    use crate::settings::KeySources;
    use flint_agent::{
        AgentEvent, CommandEvidence, FileEvidence, FileState, Op, SessionHandle, TaskOutcome,
        TaskRecord, TurnEndReason,
    };
    use gpui_kit::{AppContext as _, Entity, TestAppContext};

    fn test_task(outcome: TaskOutcome, reason: TurnEndReason) -> TaskRecord {
        let mut task = TaskRecord::new(
            1,
            "Fix the parser.\nRequired file: out.txt\nRequired check: cargo test".into(),
            7,
        );
        task.observed_edits = 1;
        task.files = vec![FileEvidence {
            path: "out.txt".into(),
            state: if outcome == TaskOutcome::Partial {
                FileState::Missing
            } else {
                FileState::Present {
                    bytes: 42,
                    fingerprint: 9,
                }
            },
            reported: false,
        }];
        task.checks.push(CommandEvidence {
            id: 1,
            call_id: "c1".into(),
            command: "cargo test".into(),
            contract_revision: 1,
            work_revision: 7,
            started_revision: 7,
            success: true,
            exit_code: Some(0),
            detail: "ok".into(),
            verification: true,
        });
        if outcome == TaskOutcome::Blocked {
            task.add_blocker("Missing fixture.", "agent report");
        }
        task.assess(reason, true);
        task
    }

    struct Harness {
        window: gpui_kit::AnyWindowHandle,
        app: Entity<FlintApp>,
        _temp: tempfile::TempDir,
    }

    impl Harness {
        fn new(cx: &mut TestAppContext) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let ws = temp.path().join("ws");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&ws).unwrap();
            let key = home.join("api.key");
            std::fs::write(&key, "test-key").unwrap();
            let options = Options {
                workspace: Some(ws),
                home: Some(home),
                key_path: Some(key),
                key_sources: Some(KeySources::none()),
                engine_poll: true,
                skip_permission_choice: true,
                ..Options::default()
            };
            cx.update(crate::init);
            let (window, app) = cx.update(|cx| {
                let bounds = gpui_kit::Bounds {
                    origin: gpui_kit::Point::default(),
                    size: gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(800.)),
                };
                gpui_kit::open_window(
                    gpui_kit::WindowOptions {
                        window_bounds: Some(gpui_kit::WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| FlintApp::new(options, window, cx)),
                )
                .unwrap()
            });
            cx.run_until_parked();
            Self {
                window,
                app,
                _temp: temp,
            }
        }

        fn attach(
            &self,
            ix: usize,
            cx: &mut TestAppContext,
        ) -> (
            async_channel::Receiver<Op>,
            async_channel::Sender<AgentEvent>,
        ) {
            let (ops_tx, ops_rx) = async_channel::unbounded();
            let (events_tx, events_rx) = async_channel::unbounded();
            self.app.update(cx, |app, cx| {
                app.attach_engine(
                    ix,
                    SessionHandle {
                        ops: ops_tx,
                        events: events_rx,
                    },
                    cx,
                );
            });
            (ops_rx, events_tx)
        }

        fn drain_ops(rx: &async_channel::Receiver<Op>) -> Vec<Op> {
            let mut ops = Vec::new();
            while let Ok(op) = rx.try_recv() {
                ops.push(op);
            }
            ops
        }

        fn pump(cx: &mut TestAppContext) {
            cx.run_until_parked();
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
        }
    }

    #[test]
    fn prepare_review_bounds_unproductive_and_repeated_automatic_reviews() {
        let mut state = AutoContinueState::default();
        let mut task = test_task(TaskOutcome::NeedsReview, TurnEndReason::Completed);
        assert!(task.can_automatically_review());

        assert!(state.prepare_review(&task));
        assert_eq!(state.attempts, 0);
        assert_eq!(state.review_attempts, 1);
        assert!(state.paused_reason.is_none());

        // Simulates record_automatic_review failing without advancing work_revision or recording a review.
        assert!(!state.prepare_review(&task));
        assert!(state.paused_reason.is_some());

        // Even when work_revision moves on every cycle, review attempts stay strictly bounded.
        let mut drifting = AutoContinueState::default();
        for n in 0..MAX_AUTO_REVIEWS {
            task.work_revision += 1;
            task.checks[0].work_revision = task.work_revision;
            task.checks[0].started_revision = task.work_revision;
            task.assess(TurnEndReason::Completed, true);
            assert!(drifting.prepare_review(&task), "attempt {n}");
        }
        task.work_revision += 1;
        task.checks[0].work_revision = task.work_revision;
        task.checks[0].started_revision = task.work_revision;
        task.assess(TurnEndReason::Completed, true);
        assert!(!drifting.prepare_review(&task));
        assert!(drifting.paused_reason.is_some());
    }

    #[test]
    fn recoverable_mid_turn_error_pause_clears_on_completed_but_permanent_error_stays_paused() {
        let mut state = AutoContinueState::default();
        assert!(is_recoverable_error_notice(
            "The model's reply was cut off (upstream reset). Retrying this step."
        ));
        assert!(is_recoverable_error_notice(
            "This model endpoint doesn't accept reasoning_effort; continuing without it."
        ));
        assert!(!is_recoverable_error_notice("Provider failed"));

        state.pause_for_recoverable_error("transient");
        assert_eq!(state.paused_reason, Some("transient"));
        state.clear_transient_error_pause();
        assert_eq!(state.paused_reason, None);

        // Non-Completed turn locks a transient pause; permanent pause cannot be cleared.
        state.pause_for_recoverable_error("transient");
        state.lock_transient_error_pause();
        state.clear_transient_error_pause();
        assert_eq!(state.paused_reason, Some("transient"));

        let mut permanent = AutoContinueState::default();
        permanent.pause("Provider failed");
        permanent.pause_for_recoverable_error("transient");
        permanent.clear_transient_error_pause();
        assert_eq!(permanent.paused_reason, Some("Provider failed"));
    }

    #[gpui_kit::test]
    fn auto_continue_bounds_failed_automatic_review_loop(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.app
            .update(cx, |app, _| app.settings.auto_continue_fixes = true);
        let (ops_rx, events_tx) = h.attach(0, cx);
        let task = test_task(TaskOutcome::NeedsReview, TurnEndReason::Completed);
        assert!(task.can_automatically_review());

        for turn_id in 1..=3 {
            events_tx
                .try_send(AgentEvent::TurnStarted { turn_id })
                .unwrap();
            events_tx
                .try_send(AgentEvent::TaskAssessed {
                    turn_id,
                    task: Box::new(task.clone()),
                })
                .unwrap();
            events_tx
                .try_send(AgentEvent::TurnFinished {
                    turn_id,
                    reason: TurnEndReason::Completed,
                })
                .unwrap();
            Harness::pump(cx);
        }

        let ops = Harness::drain_ops(&ops_rx);
        assert_eq!(
            ops.len(),
            1,
            "expected exactly 1 review dispatch, got {ops:?}"
        );
        assert!(matches!(ops[0], Op::AutomaticallyReviewTask { .. }));
        h.app.read_with(cx, |app, _| {
            assert_eq!(app.session().auto_continue.review_attempts, 1);
            assert!(app.session().auto_continue.paused_reason.is_some());
        });
    }

    #[gpui_kit::test]
    fn recoverable_mid_turn_error_clears_when_turn_completes_cleanly(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.app
            .update(cx, |app, _| app.settings.auto_continue_fixes = true);
        let (ops_rx, events_tx) = h.attach(0, cx);

        events_tx
            .try_send(AgentEvent::TurnStarted { turn_id: 1 })
            .unwrap();
        events_tx
            .try_send(AgentEvent::Error(
                "The model's reply was cut off (provider error: upstream reset). Retrying this step.".into(),
            ))
            .unwrap();
        Harness::pump(cx);
        h.app.read_with(cx, |app, _| {
            assert!(app.session().auto_continue.paused_reason.is_some());
        });

        let task = test_task(TaskOutcome::Partial, TurnEndReason::Completed);
        events_tx
            .try_send(AgentEvent::TaskAssessed {
                turn_id: 1,
                task: Box::new(task),
            })
            .unwrap();
        events_tx
            .try_send(AgentEvent::TurnFinished {
                turn_id: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        Harness::pump(cx);

        let ops = Harness::drain_ops(&ops_rx);
        assert!(matches!(
            ops.as_slice(),
            [Op::ContinueTask {
                task_id: 1,
                expected_revision: 1
            }]
        ));
        h.app.read_with(cx, |app, _| {
            assert!(app.session().auto_continue.paused_reason.is_none());
            assert_eq!(app.session().auto_continue.attempts, 1);
        });
    }

    #[gpui_kit::test]
    fn resume_automatic_task_targets_panel_session_handles_needs_review_and_surfaces_blocks(
        cx: &mut TestAppContext,
    ) {
        let h = Harness::new(cx);
        let (ops0_rx, events0_tx) = h.attach(0, cx);
        let uid0 = h.app.read_with(cx, |app, _| app.sessions[0].uid);

        // Ended NeedsReview on session 0 while Auto-fix was off; then user turns
        // Auto-fix on, switches to session 1, and invokes "Auto-fix and review" on session 0.
        let mut needs_review = test_task(TaskOutcome::NeedsReview, TurnEndReason::Completed);
        needs_review.assess(TurnEndReason::Completed, true);
        events0_tx
            .try_send(AgentEvent::TurnStarted { turn_id: 1 })
            .unwrap();
        events0_tx
            .try_send(AgentEvent::TaskAssessed {
                turn_id: 1,
                task: Box::new(needs_review),
            })
            .unwrap();
        events0_tx
            .try_send(AgentEvent::TurnFinished {
                turn_id: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        Harness::pump(cx);
        assert!(Harness::drain_ops(&ops0_rx).is_empty());
        h.app.update(cx, |app, _| {
            app.settings.auto_continue_fixes = true;
            app.sessions[0].view.last_reason = None;
        });

        cx.update_window(h.window, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.new_session(window, cx);
                assert_eq!(app.active, 1);
                app.open_task_details(
                    crate::subagent_ui::Conversation::parent(uid0),
                    1,
                    window,
                    cx,
                );
                assert!(app.task_panel.is_some());
                app.resume_task_panel_automatic(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();

        let ops0 = Harness::drain_ops(&ops0_rx);
        assert!(matches!(
            ops0.as_slice(),
            [Op::AutomaticallyReviewTask {
                task_id: 1,
                expected_revision: 1,
                expected_work_revision: 7
            }]
        ));
        h.app.read_with(cx, |app, _| {
            assert!(app.task_panel.is_none(), "panel should close on resume");
        });

        // Non-Completed Partial turn (e.g. Interrupted) also resumes via ContinueTask.
        let (ops1_rx, events1_tx) = h.attach(1, cx);
        let uid1 = h.app.read_with(cx, |app, _| app.sessions[1].uid);
        events1_tx
            .try_send(AgentEvent::TurnStarted { turn_id: 1 })
            .unwrap();
        let interrupted_task = test_task(TaskOutcome::Partial, TurnEndReason::Interrupted);
        events1_tx
            .try_send(AgentEvent::TaskAssessed {
                turn_id: 1,
                task: Box::new(interrupted_task),
            })
            .unwrap();
        events1_tx
            .try_send(AgentEvent::TurnFinished {
                turn_id: 1,
                reason: TurnEndReason::Interrupted,
            })
            .unwrap();
        Harness::pump(cx);
        assert!(Harness::drain_ops(&ops1_rx).is_empty());

        cx.update_window(h.window, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.open_task_details(
                    crate::subagent_ui::Conversation::parent(uid1),
                    1,
                    window,
                    cx,
                );
                app.resume_task_panel_automatic(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        let ops1 = Harness::drain_ops(&ops1_rx);
        assert!(matches!(
            ops1.as_slice(),
            [Op::ContinueTask {
                task_id: 1,
                expected_revision: 1
            }]
        ));

        // Non-resumable NeedsReview (unverified command effects) sets a clear block reason and panel error.
        let mut unverified_review = test_task(TaskOutcome::NeedsReview, TurnEndReason::Completed);
        unverified_review.unknown_effects = true;
        unverified_review.assess(TurnEndReason::Completed, true);
        assert_eq!(
            unverified_review.assessment.as_ref().unwrap().outcome,
            TaskOutcome::NeedsReview
        );
        assert!(!unverified_review.can_automatically_review());

        events1_tx
            .try_send(AgentEvent::TurnStarted { turn_id: 2 })
            .unwrap();
        events1_tx
            .try_send(AgentEvent::TaskAssessed {
                turn_id: 2,
                task: Box::new(unverified_review),
            })
            .unwrap();
        events1_tx
            .try_send(AgentEvent::TurnFinished {
                turn_id: 2,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        Harness::pump(cx);

        cx.update_window(h.window, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                let task = app.sessions[1].view.task.as_ref().unwrap();
                assert!(app.automatic_resume_block_reason(1, task).is_some());
                app.open_task_details(
                    crate::subagent_ui::Conversation::parent(uid1),
                    2,
                    window,
                    cx,
                );
                app.resume_automatic_task(cx);
                assert!(
                    app.task_panel
                        .as_ref()
                        .and_then(|p| p.error.as_ref())
                        .is_some()
                );
                assert!(app.sessions[1].auto_continue.paused_reason.is_some());
            });
        })
        .unwrap();
    }
}
