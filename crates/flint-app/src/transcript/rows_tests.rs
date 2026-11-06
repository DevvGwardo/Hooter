use super::{collapsed_detail, is_exit_code_line};
use crate::ui;
use flint_agent::ToolKind;

#[test]
#[ignore = "manual release-mode performance probe"]
fn collapsed_detail_perf_probe() {
    let output = "x".repeat(8 * 1024 * 1024);
    for (name, kind, failed) in [
        ("command", ToolKind::Command, false),
        ("search", ToolKind::Search, false),
        ("failed_edit", ToolKind::Edit, true),
    ] {
        let start = std::time::Instant::now();
        let mut capacity = 0;
        for _ in 0..100 {
            let preview = collapsed_detail(kind, std::hint::black_box(&output), failed).unwrap();
            capacity = capacity.max(preview.capacity());
            std::hint::black_box(preview);
        }
        eprintln!(
            "collapsed_detail_{name}_100_runs_ms={:.3}",
            start.elapsed().as_secs_f64() * 1000.
        );
        eprintln!("collapsed_detail_{name}_capacity_bytes={capacity}");
    }
}

#[test]
fn collapsed_output_details_are_bounded_before_text_layout() {
    let output = format!("  {} UNIQUE_TAIL  ", "界🙂".repeat(512));
    for (kind, failed) in [
        (ToolKind::Command, false),
        (ToolKind::Search, false),
        (ToolKind::Edit, true),
    ] {
        let preview = collapsed_detail(kind, &output, failed).unwrap();
        assert!(preview.chars().count() <= ui::MAX_HEADER_PREVIEW_CHARS + 2);
        assert!(preview.ends_with(" …"));
        assert!(!preview.contains("UNIQUE_TAIL"));
        assert!(preview.capacity() <= ui::MAX_HEADER_PREVIEW_CHARS * 8);
    }
}

#[test]
fn collapsed_details_preserve_head_tail_blank_marker_and_read_count_selection() {
    for (kind, output, failed, expected) in [
        (
            ToolKind::Command,
            " first \n second 界 \r\n [ExIt CoDe: 2]\n \n",
            false,
            Some("second 界"),
        ),
        (ToolKind::Command, " \n[exit code: 0]\n", false, None),
        (ToolKind::Command, "", false, None),
        (
            ToolKind::Search,
            " first 界 \r\nsecond",
            false,
            Some("first 界"),
        ),
        (ToolKind::Edit, " error é \nsecond", true, Some("error é")),
        (ToolKind::Edit, "ok", false, None),
        (ToolKind::Read, "", false, Some("0 lines")),
        (ToolKind::Read, "one\n", false, Some("1 line")),
        (ToolKind::Read, "one\r\ntwo\n", false, Some("2 lines")),
        (ToolKind::Other, "not a detail", false, None),
        (ToolKind::Search, "\nsecond", false, Some("")),
    ] {
        assert_eq!(collapsed_detail(kind, output, failed).as_deref(), expected);
    }
}

#[test]
fn exit_marker_matches_the_previous_rule_without_full_line_normalization() {
    let old_rule = |line: &str| {
        let lower = line.trim().to_ascii_lowercase();
        lower.starts_with("[exit code:") && lower.ends_with(']')
    };
    for line in [
        "",
        "é",
        "[exit code:",
        "[exit code: junk]",
        "[EXIT CODE: 2] suffix",
        "☃[exit code: 0]",
        "［exit code: 0］",
        "[exit codÉ: 0]",
    ] {
        assert_eq!(is_exit_code_line(line), old_rule(line), "{line:?}");
    }
    for mask in 0..1024 {
        let mut prefix = b"[exit code:".to_vec();
        for (n, byte) in prefix.iter_mut().enumerate() {
            if mask & (1 << n) != 0 {
                byte.make_ascii_uppercase();
            }
        }
        let prefix = std::str::from_utf8(&prefix).unwrap();
        for ending in ["]", " 12]", "🙂]", "1] suffix", "\n"] {
            let line = format!("\u{2003}{prefix}{ending}\r\n");
            assert_eq!(is_exit_code_line(&line), old_rule(&line));
        }
    }
    let oversized = "界🙂".repeat(1024 * 1024);
    assert!(!is_exit_code_line(&oversized));
}

#[test]
fn an_unanswered_request_does_not_read_as_a_denial_the_user_made() {
    use crate::transcript::approval::label;
    use flint_agent::ApprovalDecision;

    let (_, waiting) = label(ToolKind::Command, None, false);
    assert!(waiting.contains("waiting for you below"), "{waiting:?}");

    // The turn ended first: stored as a denial so it cannot be acted on.
    let (icon, ended) = label(ToolKind::Command, Some(ApprovalDecision::Deny), true);
    assert_eq!(ended, "Not answered — the turn ended");
    assert_eq!(icon, gpui_kit::assets::IconName::ShieldAlert);

    // The user's own denial keeps its wording.
    let (icon, denied) = label(ToolKind::Command, Some(ApprovalDecision::Deny), false);
    assert_eq!(denied, "Denied");
    assert_eq!(icon, gpui_kit::assets::IconName::ShieldX);

    for decision in [ApprovalDecision::Approve, ApprovalDecision::ApproveAlways] {
        let (_, text) = label(ToolKind::Command, Some(decision), false);
        assert!(text.starts_with("Approved"), "{text:?}");
    }
}

#[test]
fn the_turn_sweep_marks_an_unanswered_request_as_ended() {
    use crate::view_model::SessionView;
    use flint_agent::{AgentEvent, ApprovalDecision, TurnEndReason};
    use std::time::Duration;

    let mut view = SessionView::default();
    view.fold(AgentEvent::TurnStarted { turn_id: 1 }, Duration::ZERO);
    view.fold(
        AgentEvent::ApprovalRequested {
            call_id: "c1".into(),
            kind: ToolKind::Command,
            summary: "cargo test".into(),
        },
        Duration::from_secs(1),
    );
    view.fold(
        AgentEvent::TurnFinished {
            turn_id: 1,
            reason: TurnEndReason::Interrupted,
        },
        Duration::from_secs(2),
    );
    assert!(matches!(
        view.items.first(),
        Some(crate::view_model::Item::Approval {
            decision: Some(ApprovalDecision::Deny),
            ended: true,
            ..
        })
    ));

    // A denial the user gave is not marked ended.
    let mut answered = SessionView::default();
    answered.fold(AgentEvent::TurnStarted { turn_id: 1 }, Duration::ZERO);
    answered.fold(
        AgentEvent::ApprovalRequested {
            call_id: "c2".into(),
            kind: ToolKind::Command,
            summary: "cargo test".into(),
        },
        Duration::from_secs(1),
    );
    answered.resolve_approval("c2", ApprovalDecision::Deny);
    answered.fold(
        AgentEvent::TurnFinished {
            turn_id: 1,
            reason: TurnEndReason::Completed,
        },
        Duration::from_secs(2),
    );
    assert!(matches!(
        answered.items.first(),
        Some(crate::view_model::Item::Approval {
            decision: Some(ApprovalDecision::Deny),
            ended: false,
            ..
        })
    ));
}
