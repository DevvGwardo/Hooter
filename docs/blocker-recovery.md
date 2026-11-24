# Informational blocker recovery

A native turn ending normally is not evidence that its task succeeded. Agent
blocker reports remain recorded until explicitly resolved.

## Before stopping

Questions and read-only analysis with only review requirements can receive one
bounded recovery follow-up when an agent claims a blocker without an observed
tool failure. This follow-up also counts toward the existing per-turn finish
budget. Repeating or omitting the claim does not restart recovery.

The recovery check does not run for work tasks, observed edits, explicit required
files/checks, tool failures, scope denials, user-review/child blockers, incomplete
evidence, or independent-review feedback. Those obstacles remain protected.

The deterministic `blocker_recovery` rule decides whether another permitted
inspection could make progress. It applies only to informational tasks whose
only blockers are agent reports, with no denied scope, failed tool, child
report, evidence overflow or independent review; otherwise the blocker stays in
place. The follow-up fires at most once per turn and shares the four-follow-up
budget. Existing approvals, tool scope and sandboxing are unchanged. Recovery
never clears a blocker or turns missing verification into success.

## Resolving a claim

For informational tasks without observed edits or explicit file/check
requirements, a new successful known inspection after the blocker was reported
can support `report_task.resolved_blockers`. The exact prior text must be named,
and the summary should explain relevant evidence. Old, stale, failed, or
old-scope inspections do not qualify. User-review blockers cannot be cleared by
the agent. Work tasks retain the current-passing-check requirement.

Recovery receipts are recorded only while agent blockers exist, so ordinary
reads do not churn task projections. Existing snapshots without these receipts
remain readable. Inspection does not certify file changes, satisfy required
checks, or claim user acceptance. Uncertain shell effects still prevent an
automatic `Answered` result; they do not force code verification for a question
without code deliverables.

## Recent activity questions

The general agent's private working folder is an output scratch folder, not the
user's project inventory. The prompt and recovery guidance direct bounded,
read-only discovery of known project roots and relevant activity/session
records within existing permissions. They prohibit scanning unrelated private
data or bypassing denials. No user-specific paths or credentials are embedded.

An empty scratch-folder search is not global evidence of inactivity. Commits,
clone times, filesystem timestamps, and repository creation are distinct;
answers must state what the inspected sources actually establish.

## Offline regression checks

```sh
FLINT_LIVE=0 FLINT_EVAL=0 FLINT_BENCH=0 cargo test --offline --locked -p flint-agent
```

The agent regressions use scripted loopback inference and disposable files,
including read-only discovery outside the scratch folder under the existing
sandbox. They do not call live providers or use real credentials.
