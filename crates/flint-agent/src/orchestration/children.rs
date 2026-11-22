//! Child addressability and async task waiting (T3 Contract §5).

use super::*;

/// Query the durable task status for a given task handle.
pub async fn task(client: &CommandClient, handle: &TaskHandle) -> Result<TaskStatus, EngineError> {
    let handle = handle.clone();
    client.query(move |store| store.task(&handle)).await
}

/// Query durable task statuses for multiple task handles.
pub async fn tasks(
    client: &CommandClient,
    handles: &[TaskHandle],
) -> Result<Vec<TaskStatus>, EngineError> {
    let handles = handles.to_vec();
    client.query(move |store| store.tasks(&handles)).await
}

/// Event-driven async wait across 1..32 child task handles using notification channels
/// (tokio watch/Notify) with timeout_ms and cancellation, supporting WaitMode::Any
/// and WaitMode::All without polling loops.
pub async fn wait(client: &CommandClient, request: WaitRequest) -> Result<WaitResult, EngineError> {
    // 1. Validation
    if request.handles.is_empty() || request.handles.len() > 32 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Wait handles count must be 1..=32.",
        ));
    }
    if request.timeout_ms > 600_000 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Wait timeout exceeds maximum of 600,000 ms.",
        ));
    }

    let mut seen = Vec::with_capacity(request.handles.len());
    for handle in &request.handles {
        if seen.contains(&handle) {
            return Err(EngineError::new(
                EngineErrorCode::InvalidCommand,
                "Duplicate task handle in wait request.",
            ));
        }
        seen.push(handle);
        if handle.conversation.root != *client.root() {
            return Err(EngineError::new(
                EngineErrorCode::NotOwned,
                "Task handle is not owned by this session.",
            ));
        }
    }

    // 2. Subscribe to notification channels BEFORE reading completion state
    // to prevent lost wakeups.
    let mut watch_rx = client.revision_watch();
    watch_rx.borrow_and_update();
    let notify = client.task_notify().clone();

    let deadline = if request.timeout_ms == 0 {
        None
    } else {
        Some(tokio::time::Instant::now() + std::time::Duration::from_millis(request.timeout_ms))
    };

    loop {
        if client.is_stopping() {
            return Err(EngineError::new(
                EngineErrorCode::Cancelled,
                "Wait operation cancelled: engine is stopping.",
            ));
        }

        let statuses = client.tasks(&request.handles).await?;

        let satisfied = match request.mode {
            WaitMode::Any => statuses
                .iter()
                .any(|s| matches!(s.state, TaskRunState::Finished { .. })),
            WaitMode::All => statuses
                .iter()
                .all(|s| matches!(s.state, TaskRunState::Finished { .. })),
        };

        if satisfied {
            return Ok(WaitResult {
                statuses,
                timed_out: false,
            });
        }

        if let Some(dl) = deadline {
            let now = tokio::time::Instant::now();
            if now >= dl {
                return Ok(WaitResult {
                    statuses,
                    timed_out: true,
                });
            }
            let sleep_dur = dl - now;
            tokio::select! {
                res = watch_rx.changed() => {
                    if res.is_err() {
                        return Err(EngineError::new(
                            EngineErrorCode::Cancelled,
                            "Wait operation cancelled: watch channel disconnected.",
                        ));
                    }
                }
                _ = notify.notified() => {}
                _ = tokio::time::sleep(sleep_dur) => {}
            }
        } else {
            // Immediate snapshot (timeout_ms == 0)
            return Ok(WaitResult {
                statuses,
                timed_out: true,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskOutcome, TurnEndReason};
    use tempfile::TempDir;

    fn test_options() -> OrchestrationOptions {
        OrchestrationOptions {
            root_id: SessionId::parse("saved-root").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "local-profile".into(),
                    revision: 1,
                },
                model: "offline-test-model".into(),
                effort: None,
            },
            limits: EngineLimits::default(),
        }
    }

    fn root_conv() -> ConversationId {
        ConversationId {
            root: test_options().root_id,
            child: None,
        }
    }

    fn child_spec(
        label: &str,
        child: Option<ChildId>,
        target: Option<ProviderTarget>,
    ) -> ChildSpec {
        ChildSpec {
            label: label.into(),
            child,
            target,
            prompt: PromptSnapshot {
                request: format!("Execute task {label}"),
                effective_text: format!("Context for {label}"),
                images: Vec::new(),
                citations: Vec::new(),
            },
            delivery: Delivery::NextTurn,
        }
    }

    fn start_child_cmd(
        label: &str,
        child: Option<ChildId>,
        target: Option<ProviderTarget>,
    ) -> EngineCommand {
        EngineCommand {
            id: CommandId::fresh(),
            conversation: root_conv(),
            payload: CommandPayload::StartChild(child_spec(label, child, target)),
        }
    }

    #[tokio::test]
    async fn start_child_intake_allocation_and_idempotency() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        // Allocate first child -> agent-1
        let cmd1 = start_child_cmd("subagent-alpha", None, None);
        let receipt1 = client.submit(cmd1.clone()).await.unwrap();

        assert_eq!(receipt1.id, cmd1.id);
        assert_eq!(receipt1.conversation, root_conv());
        let task_handle1 = receipt1.task.clone().expect("task handle must be reserved");
        assert_eq!(task_handle1.conversation.root, test_options().root_id);
        assert_eq!(
            task_handle1.conversation.child,
            Some(ChildId::parse("agent-1").unwrap())
        );
        assert_eq!(task_handle1.command_id, cmd1.id);

        // Query durable task
        let status1 = client.task(&task_handle1).await.unwrap();
        assert_eq!(status1.handle, task_handle1);
        assert_eq!(status1.label, "subagent-alpha");
        assert_eq!(status1.target, test_options().target);
        assert_eq!(status1.state, TaskRunState::Queued);

        // Idempotent retry of exact same command
        let retry_receipt = client.submit(cmd1).await.unwrap();
        assert_eq!(retry_receipt, receipt1);
        assert_eq!(retry_receipt.task, Some(task_handle1));

        // Allocate second child -> agent-2
        let cmd2 = start_child_cmd("subagent-beta", None, None);
        let receipt2 = client.submit(cmd2.clone()).await.unwrap();
        let task_handle2 = receipt2.task.expect("task handle must be reserved");
        assert_eq!(
            task_handle2.conversation.child,
            Some(ChildId::parse("agent-2").unwrap())
        );

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn start_child_resumption_and_target_preservation() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        // Custom target for new child
        let custom_target = ProviderTarget::Native {
            profile: ProfileRef {
                id: "local-profile".into(),
                revision: 1,
            },
            model: "custom-model-x".into(),
            effort: None,
        };

        // Start child with custom target -> allocates agent-1
        let cmd1 = start_child_cmd("worker", None, Some(custom_target.clone()));
        let receipt1 = client.submit(cmd1).await.unwrap();
        let task_handle1 = receipt1.task.unwrap();

        // Finish task 1
        client
            .update_task_status(
                &task_handle1,
                TaskRunState::Finished {
                    reason: TurnEndReason::Completed,
                    outcome: TaskOutcome::Accepted,
                    result_row: None,
                },
            )
            .await
            .unwrap();

        let status_finished = client.task(&task_handle1).await.unwrap();
        assert!(matches!(
            status_finished.state,
            TaskRunState::Finished {
                outcome: TaskOutcome::Accepted,
                ..
            }
        ));

        // Resume agent-1 without target override -> succeeds and preserves custom target
        let agent_1 = ChildId::parse("agent-1").unwrap();
        let cmd_resume = start_child_cmd("worker-step-2", Some(agent_1.clone()), None);
        let receipt_resume = client.submit(cmd_resume.clone()).await.unwrap();
        let task_handle_resume = receipt_resume.task.unwrap();
        assert_eq!(task_handle_resume.conversation.child, Some(agent_1.clone()));
        assert_eq!(task_handle_resume.command_id, cmd_resume.id);

        let status_resumed = client.task(&task_handle_resume).await.unwrap();
        assert_eq!(status_resumed.target, custom_target); // preserved!
        assert_eq!(status_resumed.state, TaskRunState::Queued);

        // Resuming with a target override is rejected with InvalidCommand
        let cmd_invalid_override = start_child_cmd(
            "worker-step-3",
            Some(agent_1.clone()),
            Some(test_options().target),
        );
        let err = client.submit(cmd_invalid_override).await.unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resume_nonexistent_and_already_active_child_rejected() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        // Resuming nonexistent child -> NotFound
        let cmd_nonexistent =
            start_child_cmd("unknown", Some(ChildId::parse("agent-99").unwrap()), None);
        let err = client.submit(cmd_nonexistent).await.unwrap_err();
        assert_eq!(err.code, EngineErrorCode::NotFound);

        // Start child agent-1
        let cmd1 = start_child_cmd("active-agent", None, None);
        let receipt1 = client.submit(cmd1).await.unwrap();
        let handle1 = receipt1.task.unwrap();

        // While agent-1 is active (Queued), trying to resume it fails with Busy
        let cmd_busy = start_child_cmd("active-agent-2", handle1.conversation.child, None);
        let err_busy = client.submit(cmd_busy).await.unwrap_err();
        assert_eq!(err_busy.code, EngineErrorCode::Busy);

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn active_children_limit_enforced() {
        let dir = TempDir::new().unwrap();
        let mut opts = test_options();
        opts.limits.active_children = 2; // set to 2 for testing
        let handle = open_acceptance_session(dir.path(), opts).unwrap();
        let client = &handle.commands;

        let cmd1 = start_child_cmd("c1", None, None);
        let r1 = client.submit(cmd1).await.unwrap();
        let h1 = r1.task.unwrap();

        let cmd2 = start_child_cmd("c2", None, None);
        let client_r2 = client.submit(cmd2).await.unwrap();
        let _h2 = client_r2.task.unwrap();

        // 3rd active child exceeds limit of 2 -> QueueFull
        let cmd3 = start_child_cmd("c3", None, None);
        let err = client.submit(cmd3.clone()).await.unwrap_err();
        assert_eq!(err.code, EngineErrorCode::QueueFull);

        // Finish h1 -> frees up 1 active child slot
        client
            .update_task_status(
                &h1,
                TaskRunState::Finished {
                    reason: TurnEndReason::Completed,
                    outcome: TaskOutcome::Accepted,
                    result_row: None,
                },
            )
            .await
            .unwrap();

        // Now starting cmd3 succeeds
        let r3 = client.submit(cmd3).await.unwrap();
        assert!(r3.task.is_some());

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retained_children_limit_enforced() {
        let dir = TempDir::new().unwrap();
        let mut opts = test_options();
        opts.limits.retained_children = 2;
        opts.limits.active_children = 2;
        let handle = open_acceptance_session(dir.path(), opts).unwrap();
        let client = &handle.commands;

        let cmd1 = start_child_cmd("c1", None, None);
        let h1 = client.submit(cmd1).await.unwrap().task.unwrap();
        client
            .update_task_status(
                &h1,
                TaskRunState::Finished {
                    reason: TurnEndReason::Completed,
                    outcome: TaskOutcome::Accepted,
                    result_row: None,
                },
            )
            .await
            .unwrap();

        let cmd2 = start_child_cmd("c2", None, None);
        let h2 = client.submit(cmd2).await.unwrap().task.unwrap();
        client
            .update_task_status(
                &h2,
                TaskRunState::Finished {
                    reason: TurnEndReason::Completed,
                    outcome: TaskOutcome::Accepted,
                    result_row: None,
                },
            )
            .await
            .unwrap();

        // 3rd distinct child exceeds retained_children limit (2) -> QueueFull
        let cmd3 = start_child_cmd("c3", None, None);
        let err = client.submit(cmd3).await.unwrap_err();
        assert_eq!(err.code, EngineErrorCode::QueueFull);

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn single_delegation_level_and_ownership() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        // Child starting a child is rejected
        let mut child_conv = root_conv();
        child_conv.child = Some(ChildId::parse("agent-1").unwrap());
        let child_starts_child = EngineCommand {
            id: CommandId::fresh(),
            conversation: child_conv,
            payload: CommandPayload::StartChild(child_spec("nested", None, None)),
        };
        let err = client.submit(child_starts_child).await.unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);

        // Unowned root is rejected with NotOwned
        let mut unowned_conv = root_conv();
        unowned_conv.root = SessionId::parse("other-root").unwrap();
        let unowned_cmd = EngineCommand {
            id: CommandId::fresh(),
            conversation: unowned_conv,
            payload: CommandPayload::StartChild(child_spec("unowned", None, None)),
        };
        let err_unowned = client.submit(unowned_cmd).await.unwrap_err();
        assert_eq!(err_unowned.code, EngineErrorCode::NotOwned);

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_status_query_and_state_transitions() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        let cmd = start_child_cmd("state-machine", None, None);
        let receipt = client.submit(cmd.clone()).await.unwrap();
        let task_handle = receipt.task.unwrap();

        // 1. Initially Queued
        let s1 = client.task(&task_handle).await.unwrap();
        assert_eq!(s1.state, TaskRunState::Queued);

        // 2. Transition to Running
        let run_id = RunId::fresh();
        client
            .update_task_status(
                &task_handle,
                TaskRunState::Running {
                    run_id: run_id.clone(),
                    turn_id: 1,
                },
            )
            .await
            .unwrap();
        let s2 = client.task(&task_handle).await.unwrap();
        assert_eq!(
            s2.state,
            TaskRunState::Running {
                run_id: run_id.clone(),
                turn_id: 1
            }
        );

        // 3. Transition to Held
        client
            .update_task_status(&task_handle, TaskRunState::Held(HoldReason::Paused))
            .await
            .unwrap();
        let s3 = client.task(&task_handle).await.unwrap();
        assert_eq!(s3.state, TaskRunState::Held(HoldReason::Paused));

        // 4. Transition to Finished
        client
            .update_task_status(
                &task_handle,
                TaskRunState::Finished {
                    reason: TurnEndReason::Completed,
                    outcome: TaskOutcome::Accepted,
                    result_row: None,
                },
            )
            .await
            .unwrap();
        let s4 = client.task(&task_handle).await.unwrap();
        assert!(matches!(s4.state, TaskRunState::Finished { .. }));

        // 5. Updating an already finished task is rejected
        let err = client
            .update_task_status(&task_handle, TaskRunState::Queued)
            .await
            .unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);

        // 6. Querying unowned task
        let mut unowned_handle = task_handle.clone();
        unowned_handle.conversation.root = SessionId::parse("unowned-root").unwrap();
        let err_unowned = client.task(&unowned_handle).await.unwrap_err();
        assert_eq!(err_unowned.code, EngineErrorCode::NotOwned);

        // 7. Querying nonexistent task
        let mut nonexistent_handle = task_handle.clone();
        nonexistent_handle.command_id = CommandId::fresh();
        let err_not_found = client.task(&nonexistent_handle).await.unwrap_err();
        assert_eq!(err_not_found.code, EngineErrorCode::NotFound);

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wait_request_validations() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        let cmd = start_child_cmd("val-test", None, None);
        let task_handle = client.submit(cmd).await.unwrap().task.unwrap();

        // Empty handles
        let err = client
            .wait(WaitRequest {
                handles: Vec::new(),
                mode: WaitMode::Any,
                timeout_ms: 1000,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);

        // Duplicate handles
        let err_dup = client
            .wait(WaitRequest {
                handles: vec![task_handle.clone(), task_handle.clone()],
                mode: WaitMode::Any,
                timeout_ms: 1000,
            })
            .await
            .unwrap_err();
        assert_eq!(err_dup.code, EngineErrorCode::InvalidCommand);

        // Timeout > 600,000
        let err_to = client
            .wait(WaitRequest {
                handles: vec![task_handle.clone()],
                mode: WaitMode::Any,
                timeout_ms: 600_001,
            })
            .await
            .unwrap_err();
        assert_eq!(err_to.code, EngineErrorCode::InvalidCommand);

        // Unowned handle
        let mut unowned = task_handle.clone();
        unowned.conversation.root = SessionId::parse("other").unwrap();
        let err_unowned = client
            .wait(WaitRequest {
                handles: vec![unowned],
                mode: WaitMode::Any,
                timeout_ms: 1000,
            })
            .await
            .unwrap_err();
        assert_eq!(err_unowned.code, EngineErrorCode::NotOwned);

        // Nonexistent handle
        let mut nonexistent = task_handle.clone();
        nonexistent.command_id = CommandId::fresh();
        let err_nf = client
            .wait(WaitRequest {
                handles: vec![nonexistent],
                mode: WaitMode::Any,
                timeout_ms: 1000,
            })
            .await
            .unwrap_err();
        assert_eq!(err_nf.code, EngineErrorCode::NotFound);

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wait_immediate_snapshot_zero_timeout() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        let cmd = start_child_cmd("zero-to", None, None);
        let task_handle = client.submit(cmd).await.unwrap().task.unwrap();

        // While queued, snapshot returns immediately with timed_out: true
        let res_queued = client
            .wait(WaitRequest {
                handles: vec![task_handle.clone()],
                mode: WaitMode::Any,
                timeout_ms: 0,
            })
            .await
            .unwrap();
        assert!(res_queued.timed_out);
        assert_eq!(res_queued.statuses.len(), 1);
        assert_eq!(res_queued.statuses[0].state, TaskRunState::Queued);

        // Finish task
        client
            .update_task_status(
                &task_handle,
                TaskRunState::Finished {
                    reason: TurnEndReason::Completed,
                    outcome: TaskOutcome::Accepted,
                    result_row: None,
                },
            )
            .await
            .unwrap();

        // Now snapshot returns immediately with timed_out: false
        let res_finished = client
            .wait(WaitRequest {
                handles: vec![task_handle.clone()],
                mode: WaitMode::Any,
                timeout_ms: 0,
            })
            .await
            .unwrap();
        assert!(!res_finished.timed_out);
        assert!(matches!(
            res_finished.statuses[0].state,
            TaskRunState::Finished { .. }
        ));

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wait_any_event_driven() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = handle.commands.clone();

        let h1 = client
            .submit(start_child_cmd("task-1", None, None))
            .await
            .unwrap()
            .task
            .unwrap();
        let h2 = client
            .submit(start_child_cmd("task-2", None, None))
            .await
            .unwrap()
            .task
            .unwrap();

        // Spawn a background task to finish h2 after 60ms
        let client_bg = client.clone();
        let h2_bg = h2.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            client_bg
                .update_task_status(
                    &h2_bg,
                    TaskRunState::Finished {
                        reason: TurnEndReason::Completed,
                        outcome: TaskOutcome::Accepted,
                        result_row: None,
                    },
                )
                .await
                .unwrap();
        });

        // Wait with Any mode
        let start = std::time::Instant::now();
        let res = client
            .wait(WaitRequest {
                handles: vec![h1.clone(), h2.clone()],
                mode: WaitMode::Any,
                timeout_ms: 5000,
            })
            .await
            .unwrap();

        let elapsed = start.elapsed();
        assert!(!res.timed_out);
        assert!(elapsed >= std::time::Duration::from_millis(50));
        assert!(elapsed < std::time::Duration::from_millis(3000));

        let status_h2 = res.statuses.iter().find(|s| s.handle == h2).unwrap();
        assert!(matches!(status_h2.state, TaskRunState::Finished { .. }));

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wait_all_event_driven() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = handle.commands.clone();

        let h1 = client
            .submit(start_child_cmd("task-1", None, None))
            .await
            .unwrap()
            .task
            .unwrap();
        let h2 = client
            .submit(start_child_cmd("task-2", None, None))
            .await
            .unwrap()
            .task
            .unwrap();

        // Background task finishes h1 at 40ms, then h2 at 90ms
        let client_bg = client.clone();
        let h1_bg = h1.clone();
        let h2_bg = h2.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            client_bg
                .update_task_status(
                    &h1_bg,
                    TaskRunState::Finished {
                        reason: TurnEndReason::Completed,
                        outcome: TaskOutcome::Accepted,
                        result_row: None,
                    },
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            client_bg
                .update_task_status(
                    &h2_bg,
                    TaskRunState::Finished {
                        reason: TurnEndReason::Completed,
                        outcome: TaskOutcome::Accepted,
                        result_row: None,
                    },
                )
                .await
                .unwrap();
        });

        // Wait with All mode
        let start = std::time::Instant::now();
        let res = client
            .wait(WaitRequest {
                handles: vec![h1.clone(), h2.clone()],
                mode: WaitMode::All,
                timeout_ms: 5000,
            })
            .await
            .unwrap();

        let elapsed = start.elapsed();
        assert!(!res.timed_out);
        assert!(elapsed >= std::time::Duration::from_millis(80));
        assert!(elapsed < std::time::Duration::from_millis(3000));
        assert!(
            res.statuses
                .iter()
                .all(|s| matches!(s.state, TaskRunState::Finished { .. }))
        );

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wait_finished_before_wait_returns_immediately() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        let h1 = client
            .submit(start_child_cmd("fast-1", None, None))
            .await
            .unwrap()
            .task
            .unwrap();
        client
            .update_task_status(
                &h1,
                TaskRunState::Finished {
                    reason: TurnEndReason::Completed,
                    outcome: TaskOutcome::Accepted,
                    result_row: None,
                },
            )
            .await
            .unwrap();

        let start = std::time::Instant::now();
        let res = client
            .wait(WaitRequest {
                handles: vec![h1],
                mode: WaitMode::All,
                timeout_ms: 5000,
            })
            .await
            .unwrap();
        assert!(!res.timed_out);
        assert!(start.elapsed() < std::time::Duration::from_millis(200));

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wait_timeout_does_not_cancel_task() {
        let dir = TempDir::new().unwrap();
        let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let client = &handle.commands;

        let h1 = client
            .submit(start_child_cmd("long-runner", None, None))
            .await
            .unwrap()
            .task
            .unwrap();

        // Put in Running state
        client
            .update_task_status(
                &h1,
                TaskRunState::Running {
                    run_id: RunId::fresh(),
                    turn_id: 1,
                },
            )
            .await
            .unwrap();

        // Wait with short timeout
        let res = client
            .wait(WaitRequest {
                handles: vec![h1.clone()],
                mode: WaitMode::Any,
                timeout_ms: 50,
            })
            .await
            .unwrap();

        assert!(res.timed_out);
        // Verify task was NOT cancelled by timeout
        let status = client.task(&h1).await.unwrap();
        assert!(matches!(status.state, TaskRunState::Running { .. }));

        handle.controls.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn durable_recovery_across_session_reopen() {
        let dir = TempDir::new().unwrap();

        let h1 = {
            let handle = open_acceptance_session(dir.path(), test_options()).unwrap();
            let client = &handle.commands;

            let h = client
                .submit(start_child_cmd("persist-child", None, None))
                .await
                .unwrap()
                .task
                .unwrap();

            client
                .update_task_status(
                    &h,
                    TaskRunState::Running {
                        run_id: RunId::fresh(),
                        turn_id: 1,
                    },
                )
                .await
                .unwrap();

            handle.controls.shutdown().await.unwrap();
            h
        };

        // Reopen session from same directory
        let reopened_handle = open_acceptance_session(dir.path(), test_options()).unwrap();
        let reopened_client = &reopened_handle.commands;

        // Query the persisted task
        let status = reopened_client.task(&h1).await.unwrap();
        assert_eq!(status.handle, h1);
        assert_eq!(status.label, "persist-child");
        // Reopen reconciles running/queued state into Held(Restart)
        assert!(matches!(
            status.state,
            TaskRunState::Held(HoldReason::Restart { .. }) | TaskRunState::Running { .. }
        ));

        reopened_handle.controls.shutdown().await.unwrap();
    }
}
