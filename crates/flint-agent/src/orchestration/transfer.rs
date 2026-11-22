//! T3 Contract §7 & §9 context transfer, handoff and fork preview.

use super::store::{Store, corrupt, object_name, storage, validate_prompt, validate_target};
use super::*;
use crate::rich::ContentRef;
use crate::{TaskContract, TaskMode, TaskRecord};
use sha2::{Digest as _, Sha256};

/// Deterministic preimage for SHA-256 `transfer_digest` computation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransferDigestPreimage {
    pub version: u32,
    pub mode: TransferMode,
    pub source: ConversationId,
    pub source_revision: u64,
    pub source_task: TaskRevision,
    pub through_row: RowId,
    pub destination_root: SessionId,
    pub target: ProviderTarget,
    pub current_request: String,
    pub current_effective_text: String,
    pub included_rows: Vec<RowId>,
    pub omitted: Vec<HistoryOmission>,
}

/// Computes the cryptographic SHA-256 transfer_digest over included rows,
/// source task revision, destination root, target, mode, through_row, and current request.
pub fn compute_transfer_digest(
    spec: &TransferPreviewSpec,
    included_rows: &[RowId],
    omitted: &[HistoryOmission],
) -> Digest {
    let preimage = TransferDigestPreimage {
        version: 1,
        mode: spec.mode.clone(),
        source: spec.source.clone(),
        source_revision: spec.source_revision,
        source_task: spec.source_task.clone(),
        through_row: spec.through_row.clone(),
        destination_root: spec.destination_root.clone(),
        target: spec.target.clone(),
        current_request: spec.current.request.clone(),
        current_effective_text: spec.current.effective_text.clone(),
        included_rows: included_rows.to_vec(),
        omitted: omitted.to_vec(),
    };
    let bytes =
        serde_json::to_vec(&preimage).expect("TransferDigestPreimage serialization cannot fail");
    Digest(Sha256::digest(&bytes).into())
}

/// Token estimation heuristic: ~4 characters per token with integer rounding.
pub fn estimate_tokens(chars: usize) -> u64 {
    (chars.saturating_add(3) / 4) as u64
}

/// Estimate tokens for a PromptSnapshot.
pub fn estimate_prompt_tokens(prompt: &PromptSnapshot) -> u64 {
    let text_tokens = estimate_tokens(
        prompt
            .request
            .len()
            .saturating_add(prompt.effective_text.len()),
    );
    // Budget conservatively ~2,000 tokens per image attachment (8,000 char equiv).
    let image_tokens = (prompt.images.len() as u64).saturating_mul(2000);
    let citation_tokens = (prompt.citations.len() as u64).saturating_mul(10);
    text_tokens
        .saturating_add(image_tokens)
        .saturating_add(citation_tokens)
}

/// Estimate UTF-8 byte size for a PromptSnapshot.
pub fn estimate_prompt_bytes(prompt: &PromptSnapshot) -> usize {
    let text_bytes = prompt
        .request
        .len()
        .saturating_add(prompt.effective_text.len());
    let images_bytes: usize = prompt
        .images
        .iter()
        .map(|img| {
            img.data
                .len()
                .saturating_add(img.name.len())
                .saturating_add(img.mime_type.len())
        })
        .sum();
    text_bytes.saturating_add(images_bytes)
}

/// Estimate tokens for host-owned task scope.
pub fn estimate_scope_tokens(scope: Option<&TaskContract>) -> u64 {
    estimate_tokens(estimate_scope_bytes(scope))
}

/// Estimate UTF-8 byte size for host-owned task scope.
pub fn estimate_scope_bytes(scope: Option<&TaskContract>) -> usize {
    let Some(contract) = scope else { return 0 };
    let mut bytes = contract.objective.len();
    for req in &contract.requirements {
        bytes = bytes.saturating_add(req.spec.text().len());
    }
    for res in &contract.restrictions {
        bytes = bytes.saturating_add(res.len());
    }
    for ins in &contract.instructions {
        bytes = bytes.saturating_add(ins.len());
    }
    bytes
}

/// Calculate byte size for a history row's content and attribution formatting.
pub fn row_bytes(row: &HistoryRow) -> usize {
    let content_len = match &row.content {
        ContentRef::Inline(text) => text.len(),
        ContentRef::Asset(asset) => asset.bytes as usize,
    };
    let attribution_len = format_attributed_row(row).len();
    content_len.saturating_add(attribution_len)
}

/// Estimate tokens for a history row.
pub fn row_tokens(row: &HistoryRow) -> u64 {
    estimate_tokens(row_bytes(row))
}

/// Format a single history row into an attributed inert excerpt.
/// Never promotes historical output into authoritative instructions.
pub fn format_attributed_row(row: &HistoryRow) -> String {
    let role_str = match row.role {
        RowRole::User => "user",
        RowRole::Assistant => "assistant",
        RowRole::Tool => "tool report",
        RowRole::SystemNotice => "system notice",
    };
    let target_str = match &row.source_target {
        ProviderTarget::Native { model, .. } => format!("native target ({model})"),
        ProviderTarget::Acp { agent_id, .. } => format!("acp target ({agent_id})"),
    };
    let text = match &row.content {
        ContentRef::Inline(text) => text.as_str(),
        ContentRef::Asset(_asset) => "[stored asset ref]",
    };
    format!(
        "[Historical {role_str} from {target_str} (ordinal {})]\n{text}\n",
        row.id.ordinal
    )
}

/// Formats the full attributed history string from the included rows.
pub fn format_attributed_history(rows: &[HistoryRow]) -> String {
    let mut out = String::new();
    for row in rows {
        out.push_str(&format_attributed_row(row));
    }
    out
}

/// Prepares a context transfer preview under T3 Contract §7 & §9.
///
/// 1. Validates source conversation ownership and current revisions.
/// 2. Verifies TransferBudget limits (max_input_tokens, max_utf8_bytes, max_images).
/// 3. Bounded row selection: selects candidate rows up to `through_row.ordinal`,
///    trimming the oldest rows if needed to stay within budget.
/// 4. Computes cryptographic SHA-256 `transfer_digest` over included rows, source task revision,
///    destination root, and target.
/// 5. Returns `TransferPreview`.
pub fn prepare_transfer(
    store: &Store,
    spec: &TransferPreviewSpec,
) -> Result<TransferPreview, EngineError> {
    store.owned(&spec.source)?;

    if spec.source_revision != store.state.revision {
        return Err(EngineError::new(
            EngineErrorCode::StaleRevision,
            "Transfer source revision is stale.",
        ));
    }

    let current_task = store.current_task_revision()?;
    if spec.source_task != current_task {
        return Err(EngineError::new(
            EngineErrorCode::StaleRevision,
            "Transfer source task revision is stale.",
        ));
    }

    if spec.destination_root == spec.source.root {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Destination root must be distinct from source conversation root.",
        ));
    }

    if spec.through_row.conversation != spec.source {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "through_row conversation does not match source conversation.",
        ));
    }

    // 1. Check non-negotiable base budget (current prompt snapshot + host scope)
    let base_images = spec.current.images.len();
    if base_images > spec.budget.max_images {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Prompt images exceed transfer budget max_images.",
        ));
    }

    let task_opt = store.current_task_record()?;
    let scope_ref = task_opt.as_ref().map(|t| &t.contract);
    let base_bytes =
        estimate_prompt_bytes(&spec.current).saturating_add(estimate_scope_bytes(scope_ref));
    if base_bytes > spec.budget.max_utf8_bytes {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Current request and host scope exceed transfer budget byte limit.",
        ));
    }

    let base_tokens =
        estimate_prompt_tokens(&spec.current).saturating_add(estimate_scope_tokens(scope_ref));
    let available_input_tokens = if spec.budget.reserve_output_tokens > 0 {
        spec.budget
            .max_input_tokens
            .saturating_sub(spec.budget.reserve_output_tokens)
    } else {
        spec.budget.max_input_tokens
    };
    if available_input_tokens > 0 && base_tokens > available_input_tokens {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Current request and host scope exceed available input token budget.",
        ));
    }

    // 2. Load candidate history rows up to through_row.ordinal
    let all_rows = store.load_history_rows(&spec.source)?;
    let mut candidate_rows: Vec<HistoryRow> = all_rows
        .into_iter()
        .filter(|row| row.id.ordinal <= spec.through_row.ordinal)
        .collect();
    candidate_rows.sort_by_key(|row| row.id.ordinal);

    if spec.through_row.ordinal > 0
        && !candidate_rows
            .iter()
            .any(|r| r.id.ordinal == spec.through_row.ordinal)
    {
        return Err(EngineError::new(
            EngineErrorCode::NotFound,
            format!(
                "through_row ordinal {} not found in source history.",
                spec.through_row.ordinal
            ),
        ));
    }

    // 3. Trim oldest rows atomically until candidate slice fits within budget
    let mut start_idx = 0;
    while start_idx <= candidate_rows.len() {
        let slice = &candidate_rows[start_idx..];
        let slice_bytes: usize = slice.iter().map(row_bytes).sum();
        let slice_tokens: u64 = slice.iter().map(row_tokens).sum();
        let total_bytes = base_bytes.saturating_add(slice_bytes);
        let total_tokens = base_tokens.saturating_add(slice_tokens);

        let bytes_fit = total_bytes <= spec.budget.max_utf8_bytes;
        let tokens_fit = available_input_tokens == 0 || total_tokens <= available_input_tokens;

        if bytes_fit && tokens_fit {
            break;
        }
        start_idx += 1;
    }

    let included_slice = &candidate_rows[start_idx..];
    let included_row_ids: Vec<RowId> = included_slice.iter().map(|r| r.id.clone()).collect();
    let final_bytes =
        base_bytes.saturating_add(included_slice.iter().map(row_bytes).sum::<usize>());
    let final_tokens =
        base_tokens.saturating_add(included_slice.iter().map(row_tokens).sum::<u64>());

    let mut omitted = Vec::new();
    if start_idx > 0 && !candidate_rows.is_empty() {
        omitted.push(HistoryOmission {
            conversation: spec.source.clone(),
            first_ordinal: candidate_rows[0].id.ordinal,
            last_ordinal: candidate_rows[start_idx - 1].id.ordinal,
            reason: "Excluded due to transfer budget constraints.".into(),
        });
    }

    // 4. Capability warnings
    let mut capability_warnings = Vec::new();
    if matches!(&spec.target, ProviderTarget::Acp { .. }) {
        capability_warnings.push(
            "Destination target Acp has no guaranteed native retrieval tool; use app lookup/reattach for omitted history."
                .into(),
        );
    }

    // 5. Compute cryptographic SHA-256 transfer_digest
    let transfer_digest = compute_transfer_digest(spec, &included_row_ids, &omitted);

    Ok(TransferPreview {
        source_revision: spec.source_revision,
        transfer_digest,
        estimated_input_tokens: final_tokens,
        bytes: final_bytes,
        included_rows: included_row_ids,
        omitted,
        capability_warnings,
    })
}

/// Validates intake for `CommandPayload::Transfer(TransferSpec)`.
pub fn validate_transfer_intake(
    command: &EngineCommand,
    spec: &TransferSpec,
) -> Result<(), EngineError> {
    if spec.expected_transfer_digest == Digest([0; 32]) {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Zero-digest sentinel is not authorized for transfer execution.",
        ));
    }

    validate_prompt(&spec.preview.current)?;
    validate_target(&spec.preview.target)?;

    if spec.preview.source != command.conversation {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Transfer source conversation must match command conversation.",
        ));
    }

    if spec.preview.through_row.conversation != command.conversation {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Transfer through_row must belong to source conversation.",
        ));
    }

    if spec.preview.destination_root == command.conversation.root {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Destination root must be distinct from source root.",
        ));
    }

    if spec.preview.budget.max_input_tokens == 0 || spec.preview.budget.max_utf8_bytes == 0 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Transfer budget must specify positive max_input_tokens and max_utf8_bytes.",
        ));
    }

    Ok(())
}

/// Executes `CommandPayload::Transfer(TransferSpec)` within the store.
///
/// - Re-verifies source revisions and freshly re-runs `prepare_transfer`.
/// - Validates equality of `expected_transfer_digest` with freshly verified digest.
/// - In Handoff mode: pauses source queue.
/// - In Fork mode: leaves source checkpoint intact.
/// - Maintains immutable `Lineage` and `TransferPackage` contracts.
/// - Commits command receipt to durable store.
pub fn execute_transfer(
    store: &mut Store,
    command: &EngineCommand,
    bytes: &[u8],
    payload_digest: Digest,
    spec: &TransferSpec,
) -> Result<CommandReceipt, EngineError> {
    store.owned(&command.conversation)?;

    // Source must be idle: no started command, no in-flight side-effect dispatch
    if store
        .state
        .pending
        .iter()
        .any(|entry| matches!(entry.receipt.state, CommandState::Started { .. }))
    {
        return Err(EngineError::new(
            EngineErrorCode::Busy,
            "Cannot transfer while a command is actively running.",
        ));
    }
    if store
        .state
        .intents
        .iter()
        .any(|intent| matches!(intent.state, IntentState::Dispatching))
    {
        return Err(EngineError::new(
            EngineErrorCode::Busy,
            "Cannot transfer with ambiguous side-effects in dispatch.",
        ));
    }

    // Validate freshness of source revisions
    if spec.preview.source_revision != store.state.revision {
        return Err(EngineError::new(
            EngineErrorCode::StaleRevision,
            "Transfer source revision is stale.",
        ));
    }

    let current_task = store.current_task_revision()?;
    if spec.preview.source_task != current_task {
        return Err(EngineError::new(
            EngineErrorCode::StaleRevision,
            "Transfer source task revision is stale.",
        ));
    }

    // Re-verify transfer preview
    let fresh_preview = prepare_transfer(store, &spec.preview)?;
    if spec.expected_transfer_digest != fresh_preview.transfer_digest {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Transfer digest mismatch with freshly verified digest.",
        ));
    }

    // Build immutable Lineage
    let lineage = Lineage {
        source: spec.preview.source.clone(),
        through_row: spec.preview.through_row.clone(),
        destination: ConversationId {
            root: spec.preview.destination_root.clone(),
            child: None,
        },
        target: spec.preview.target.clone(),
        transfer_digest: spec.expected_transfer_digest.clone(),
        mode: spec.preview.mode.clone(),
    };

    // Load included row content for attributed history formatting
    let all_rows = store.load_history_rows(&spec.preview.source)?;
    let included_rows: Vec<HistoryRow> = all_rows
        .into_iter()
        .filter(|row| fresh_preview.included_rows.contains(&row.id))
        .collect();
    let attributed_history = format_attributed_history(&included_rows);

    // Host-owned inherited scope (constraints/objective only, excluding certs/passes)
    let inherited_scope = if let Some(task) = store.current_task_record()? {
        task.contract
    } else {
        TaskContract {
            id: 1,
            revision: 1,
            original_request: spec.preview.current.request.clone(),
            objective: spec.preview.current.request.clone(),
            mode: TaskMode::default(),
            requirements: Vec::new(),
            restrictions: Vec::new(),
            instructions: Vec::new(),
        }
    };

    let inherited_hold = store.current_hold().or_else(|| {
        if spec.preview.mode == TransferMode::Handoff {
            Some(HoldReason::Paused)
        } else {
            None
        }
    });

    // Build TransferPackage
    let package = TransferPackage {
        lineage: lineage.clone(),
        current: spec.preview.current.clone(),
        attributed_history,
        inherited_scope,
        inherited_hold,
    };

    // Persist TransferPackage as immutable object in store.objects
    let pkg_bytes = serde_json::to_vec(&package).map_err(|_| corrupt())?;
    let pkg_name = object_name(&pkg_bytes);
    store.fail("package")?;
    store
        .objects
        .immutable(&pkg_name, &pkg_bytes)
        .map_err(storage)?;

    // Commit changes to state
    let mut state = store.state.clone();
    state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
    let _ = state.advance_queue_revision();

    // Handoff pauses the source; fork leaves source checkpoint intact
    if spec.preview.mode == TransferMode::Handoff {
        state.paused = true;
        for entry in &mut state.pending {
            if matches!(entry.receipt.state, CommandState::Accepted) {
                entry.receipt.state = CommandState::Held(HoldReason::Paused);
                entry.receipt.status_revision = state.revision;
            }
        }
    }

    state.extra.insert(
        "last_lineage".into(),
        serde_json::to_value(&lineage).map_err(|_| corrupt())?,
    );
    state.extra.insert(
        "last_transfer_package".into(),
        serde_json::to_value(&pkg_name).map_err(|_| corrupt())?,
    );

    let control_receipt = CommandReceipt {
        id: command.id.clone(),
        conversation: command.conversation.clone(),
        task: None,
        payload_digest,
        accepted_sequence: state.next_sequence,
        accepted_revision: state.revision,
        status_revision: state.revision,
        state: CommandState::Finished {
            run_id: None,
            turn_id: None,
            reason: CommandFinish::Applied,
        },
    };

    state.next_sequence = state.next_sequence.checked_add(1).ok_or_else(corrupt)?;
    state.receipt_count = state.receipt_count.checked_add(1).ok_or_else(corrupt)?;

    let object = object_name(bytes);
    store.fail("object")?;
    store.objects.immutable(&object, bytes).map_err(storage)?;

    let node = super::store::Node {
        version: 1,
        receipt: control_receipt.clone(),
        command_object: object,
        previous: state.receipt_head.clone(),
        ordinal: state.receipt_count,
    };
    let node_bytes = serde_json::to_vec(&node).map_err(|_| corrupt())?;
    let node_name = object_name(&node_bytes);
    store.fail("receipt")?;
    store
        .objects
        .immutable(&node_name, &node_bytes)
        .map_err(storage)?;
    state.receipt_head = Some(node_name);

    store
        .retained_index
        .insert(command.id.clone(), control_receipt.clone());
    store.commit(state)?;

    Ok(control_receipt)
}

/// Initializes a destination session from an accepted `TransferPackage`.
/// Implements T3 Contract §7 destination acceptance:
/// - Destination root matches `package.lineage.destination.root`.
/// - Persists `TransferPackage` and `Lineage`.
/// - Submits first command with `package.current` and places it in `CommandState::Held`.
/// - Never auto-sends work without explicit user resume.
#[allow(dead_code)]
pub fn accept_transfer_into_destination(
    destination_store: &mut Store,
    package: TransferPackage,
) -> Result<DestinationAccepted, EngineError> {
    if destination_store.state.root != package.lineage.destination.root {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Destination store root mismatch with package destination.",
        ));
    }

    // Persist immutable package in destination objects
    let pkg_bytes = serde_json::to_vec(&package).map_err(|_| corrupt())?;
    let pkg_name = object_name(&pkg_bytes);
    destination_store
        .objects
        .immutable(&pkg_name, &pkg_bytes)
        .map_err(storage)?;

    let mut state = destination_store.state.clone();
    state.extra.insert(
        "lineage".into(),
        serde_json::to_value(&package.lineage).map_err(|_| corrupt())?,
    );
    state.extra.insert(
        "transfer_package".into(),
        serde_json::to_value(&pkg_name).map_err(|_| corrupt())?,
    );

    // Initialize TaskRecord with inherited scope
    let mut task_record = TaskRecord::new(
        package.inherited_scope.id,
        package.inherited_scope.original_request.clone(),
        0,
    );
    task_record.contract = package.inherited_scope.clone();
    destination_store.document["task"] =
        serde_json::to_value(&task_record).map_err(|_| corrupt())?;

    // Create held first command
    let first_command = EngineCommand {
        id: CommandId::fresh(),
        conversation: package.lineage.destination.clone(),
        payload: CommandPayload::Send {
            prompt: package.current.clone(),
            delivery: Delivery::NextTurn,
        },
    };

    let (cmd_bytes, cmd_digest) = super::store::canonical_command(&first_command)?;
    let cmd_obj = object_name(&cmd_bytes);
    destination_store
        .objects
        .immutable(&cmd_obj, &cmd_bytes)
        .map_err(storage)?;

    state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
    let hold_reason = package.inherited_hold.clone().unwrap_or(HoldReason::Paused);

    let first_receipt = CommandReceipt {
        id: first_command.id.clone(),
        conversation: first_command.conversation.clone(),
        task: None,
        payload_digest: cmd_digest,
        accepted_sequence: state.next_sequence,
        accepted_revision: state.revision,
        status_revision: state.revision,
        state: CommandState::Held(hold_reason),
    };

    state.next_sequence = state.next_sequence.checked_add(1).ok_or_else(corrupt)?;
    state.receipt_count = state.receipt_count.checked_add(1).ok_or_else(corrupt)?;

    let node = super::store::Node {
        version: 1,
        receipt: first_receipt.clone(),
        command_object: cmd_obj,
        previous: state.receipt_head.clone(),
        ordinal: state.receipt_count,
    };
    let node_bytes = serde_json::to_vec(&node).map_err(|_| corrupt())?;
    let node_name = object_name(&node_bytes);
    destination_store
        .objects
        .immutable(&node_name, &node_bytes)
        .map_err(storage)?;
    state.receipt_head = Some(node_name);

    destination_store
        .retained_index
        .insert(first_command.id.clone(), first_receipt.clone());
    destination_store.commit(state)?;

    Ok(DestinationAccepted {
        conversation: package.lineage.destination,
        first_command: first_receipt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_options(root: &str) -> OrchestrationOptions {
        OrchestrationOptions {
            root_id: SessionId::parse(root).unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "test-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            limits: EngineLimits::default(),
        }
    }

    fn open_store(dir: &TempDir, root: &str) -> Store {
        Store::open(dir.path(), test_options(root)).unwrap()
    }

    fn test_conv(root: &str) -> ConversationId {
        ConversationId {
            root: SessionId::parse(root).unwrap(),
            child: None,
        }
    }

    fn sample_prompt(text: &str) -> PromptSnapshot {
        PromptSnapshot {
            request: text.into(),
            effective_text: format!("{text}\n[attached context]"),
            images: Vec::new(),
            citations: Vec::new(),
        }
    }

    fn sample_row(conv: &ConversationId, ordinal: u64, text: &str, role: RowRole) -> HistoryRow {
        HistoryRow {
            id: RowId {
                conversation: conv.clone(),
                ordinal,
            },
            turn_id: 1,
            role,
            source_target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "test-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            content: ContentRef::Inline(text.into()),
            content_digest: super::store::digest(text.as_bytes()),
            citations: Vec::new(),
            truncated: false,
        }
    }

    fn default_budget() -> TransferBudget {
        TransferBudget {
            max_input_tokens: 10_000,
            reserve_output_tokens: 2_000,
            max_utf8_bytes: 100_000,
            max_images: 4,
        }
    }

    #[test]
    fn test_prepare_transfer_basic_preview() {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir, "root-source");
        store.state.paused = false;
        let conv = test_conv("root-source");

        let r1 = sample_row(&conv, 1, "First user turn", RowRole::User);
        let r2 = sample_row(&conv, 2, "First assistant response", RowRole::Assistant);
        store.record_history_row(r1).unwrap();
        store.record_history_row(r2).unwrap();

        let spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 2,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "claude-3-5-sonnet".into(),
                effort: None,
            },
            current: sample_prompt("Continue solving task"),
            budget: default_budget(),
        };

        let preview = prepare_transfer(&store, &spec).unwrap();
        assert_eq!(preview.source_revision, store.state.revision);
        assert_eq!(preview.included_rows.len(), 2);
        assert_eq!(preview.included_rows[0].ordinal, 1);
        assert_eq!(preview.included_rows[1].ordinal, 2);
        assert!(preview.omitted.is_empty());
        assert!(preview.bytes > 0);
        assert!(preview.estimated_input_tokens > 0);
        assert_ne!(preview.transfer_digest, Digest([0; 32]));
    }

    #[test]
    fn test_prepare_transfer_omission_when_budget_exceeded() {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir, "root-source");
        store.state.paused = false;
        let conv = test_conv("root-source");

        for i in 1..=5 {
            let row = sample_row(
                &conv,
                i,
                &format!("Historical transcript turn line {i}"),
                RowRole::User,
            );
            store.record_history_row(row).unwrap();
        }

        // Extremely tight token budget that can only fit 2 recent rows + current prompt
        let tight_budget = TransferBudget {
            max_input_tokens: 60,
            reserve_output_tokens: 0,
            max_utf8_bytes: 100_000,
            max_images: 4,
        };

        let spec = TransferPreviewSpec {
            mode: TransferMode::Fork,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 5,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "offline-model".into(),
                effort: None,
            },
            current: sample_prompt("Short request"),
            budget: tight_budget,
        };

        let preview = prepare_transfer(&store, &spec).unwrap();
        // Oldest rows were trimmed, newest rows kept
        assert!(preview.included_rows.len() < 5);
        assert!(!preview.omitted.is_empty());
        let omission = &preview.omitted[0];
        assert_eq!(omission.first_ordinal, 1);
        assert_eq!(
            omission.last_ordinal,
            5 - preview.included_rows.len() as u64
        );
        assert!(
            preview
                .included_rows
                .iter()
                .all(|r| r.ordinal > omission.last_ordinal)
        );
    }

    #[test]
    fn test_prepare_transfer_rejects_when_base_prompt_exceeds_budget() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, "root-source");
        let conv = test_conv("root-source");

        let spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: sample_prompt("A very long request that exceeds token limit"),
            budget: TransferBudget {
                max_input_tokens: 5,
                reserve_output_tokens: 0,
                max_utf8_bytes: 100_000,
                max_images: 4,
            },
        };

        let err = prepare_transfer(&store, &spec).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);
        assert!(
            err.message
                .contains("Current request and host scope exceed")
        );
    }

    #[test]
    fn test_prepare_transfer_rejects_exceeded_images() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, "root-source");
        let conv = test_conv("root-source");

        let mut prompt = sample_prompt("Analyze these images");
        prompt.images.push(crate::ImageAttachment {
            name: "img1.png".into(),
            mime_type: "image/png".into(),
            data: "data1".into(),
        });
        prompt.images.push(crate::ImageAttachment {
            name: "img2.png".into(),
            mime_type: "image/png".into(),
            data: "data2".into(),
        });

        let spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: prompt,
            budget: TransferBudget {
                max_input_tokens: 10_000,
                reserve_output_tokens: 1_000,
                max_utf8_bytes: 100_000,
                max_images: 1, // Budget allows only 1 image
            },
        };

        let err = prepare_transfer(&store, &spec).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);
        assert!(err.message.contains("max_images"));
    }

    #[test]
    fn test_prepare_transfer_rejects_stale_source_revision() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, "root-source");
        let conv = test_conv("root-source");

        let spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision + 5, // Stale!
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: sample_prompt("Hello"),
            budget: default_budget(),
        };

        let err = prepare_transfer(&store, &spec).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::StaleRevision);
    }

    #[test]
    fn test_prepare_transfer_rejects_stale_source_task() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, "root-source");
        let conv = test_conv("root-source");

        let spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: TaskRevision {
                turn_id: 99,
                task_id: 1,
                contract_revision: 1,
                work_revision: 1,
            },
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: sample_prompt("Hello"),
            budget: default_budget(),
        };

        let err = prepare_transfer(&store, &spec).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::StaleRevision);
    }

    #[test]
    fn test_prepare_transfer_rejects_same_destination_root() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, "root-source");
        let conv = test_conv("root-source");

        let spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-source").unwrap(), // Same root!
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: sample_prompt("Hello"),
            budget: default_budget(),
        };

        let err = prepare_transfer(&store, &spec).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);
    }

    #[test]
    fn test_prepare_transfer_rejects_unknown_through_row() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, "root-source");
        let conv = test_conv("root-source");

        let spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 99, // Doesn't exist!
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: sample_prompt("Hello"),
            budget: default_budget(),
        };

        let err = prepare_transfer(&store, &spec).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::NotFound);
    }

    #[test]
    fn test_transfer_digest_cryptographic_properties() {
        let conv = test_conv("root-source");
        let spec1 = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: 1,
            source_task: TaskRevision {
                turn_id: 0,
                task_id: 0,
                contract_revision: 0,
                work_revision: 0,
            },
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 1,
            },
            destination_root: SessionId::parse("root-dest-1").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "p1".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: sample_prompt("Test prompt"),
            budget: default_budget(),
        };

        let rows = vec![RowId {
            conversation: conv.clone(),
            ordinal: 1,
        }];
        let omitted = Vec::new();

        let d1 = compute_transfer_digest(&spec1, &rows, &omitted);
        let d1_again = compute_transfer_digest(&spec1, &rows, &omitted);
        assert_eq!(d1, d1_again, "Digest must be deterministic");

        // Change destination root -> changes digest
        let mut spec2 = spec1.clone();
        spec2.destination_root = SessionId::parse("root-dest-2").unwrap();
        let d2 = compute_transfer_digest(&spec2, &rows, &omitted);
        assert_ne!(d1, d2);

        // Change target -> changes digest
        let mut spec3 = spec1.clone();
        spec3.target = ProviderTarget::Native {
            profile: ProfileRef {
                id: "p1".into(),
                revision: 1,
            },
            model: "claude-3-5".into(),
            effort: None,
        };
        let d3 = compute_transfer_digest(&spec3, &rows, &omitted);
        assert_ne!(d1, d3);

        // Change source task revision -> changes digest
        let mut spec4 = spec1.clone();
        spec4.source_task.work_revision = 2;
        let d4 = compute_transfer_digest(&spec4, &rows, &omitted);
        assert_ne!(d1, d4);

        // Change mode (Handoff vs Fork) -> changes digest
        let mut spec5 = spec1.clone();
        spec5.mode = TransferMode::Fork;
        let d5 = compute_transfer_digest(&spec5, &rows, &omitted);
        assert_ne!(d1, d5);
    }

    #[test]
    fn test_execute_transfer_handoff_intake_and_execution() {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir, "root-source");
        store.state.paused = false;
        let conv = test_conv("root-source");

        let r1 = sample_row(&conv, 1, "User asked to build feature", RowRole::User);
        store.record_history_row(r1).unwrap();

        let preview_spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 1,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: sample_prompt("Resume feature build"),
            budget: default_budget(),
        };

        let preview = prepare_transfer(&store, &preview_spec).unwrap();

        let transfer_spec = TransferSpec {
            preview: preview_spec.clone(),
            expected_transfer_digest: preview.transfer_digest.clone(),
        };

        let command = EngineCommand {
            id: CommandId::fresh(),
            conversation: conv.clone(),
            payload: CommandPayload::Transfer(transfer_spec),
        };

        let receipt = store.submit(command.clone()).unwrap();
        assert_eq!(receipt.id, command.id);
        assert_eq!(
            receipt.state,
            CommandState::Finished {
                run_id: None,
                turn_id: None,
                reason: CommandFinish::Applied,
            }
        );

        // Handoff must pause the source store
        assert!(store.state.paused, "Handoff must pause source store");

        // Immutable lineage and package must be persisted
        let lineage: Lineage =
            serde_json::from_value(store.state.extra.get("last_lineage").unwrap().clone()).unwrap();
        assert_eq!(lineage.source, conv);
        assert_eq!(lineage.mode, TransferMode::Handoff);
        assert_eq!(lineage.transfer_digest, preview.transfer_digest);

        let pkg_obj_name: String = serde_json::from_value(
            store
                .state
                .extra
                .get("last_transfer_package")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let pkg_bytes = store.objects.read(&pkg_obj_name, 1024 * 1024).unwrap();
        let package: TransferPackage = serde_json::from_slice(&pkg_bytes).unwrap();
        assert_eq!(package.current.request, "Resume feature build");
        assert!(
            package
                .attributed_history
                .contains("User asked to build feature")
        );
        assert_eq!(package.inherited_hold, Some(HoldReason::Paused));

        // Submitting same command again is idempotent
        let retry = store.submit(command).unwrap();
        assert_eq!(retry, receipt);
    }

    #[test]
    fn test_execute_transfer_fork_leaves_source_unpaused() {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir, "root-source");
        store.state.paused = false;
        let conv = test_conv("root-source");

        let preview_spec = TransferPreviewSpec {
            mode: TransferMode::Fork,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-profile".into(),
                    revision: 1,
                },
                model: "gpt-4o".into(),
                effort: None,
            },
            current: sample_prompt("Fork experiment"),
            budget: default_budget(),
        };

        let preview = prepare_transfer(&store, &preview_spec).unwrap();

        let transfer_spec = TransferSpec {
            preview: preview_spec,
            expected_transfer_digest: preview.transfer_digest,
        };

        let command = EngineCommand {
            id: CommandId::fresh(),
            conversation: conv,
            payload: CommandPayload::Transfer(transfer_spec),
        };

        let _ = store.submit(command).unwrap();
        assert!(!store.state.paused, "Fork must leave source unpaused");
    }

    #[test]
    fn test_execute_transfer_rejects_zero_digest_sentinel() {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir, "root-source");
        store.state.paused = false;
        let conv = test_conv("root-source");

        let transfer_spec = TransferSpec {
            preview: TransferPreviewSpec {
                mode: TransferMode::Handoff,
                source: conv.clone(),
                source_revision: store.state.revision,
                source_task: store.current_task_revision().unwrap(),
                through_row: RowId {
                    conversation: conv.clone(),
                    ordinal: 0,
                },
                destination_root: SessionId::parse("root-dest").unwrap(),
                target: ProviderTarget::Native {
                    profile: ProfileRef {
                        id: "p".into(),
                        revision: 1,
                    },
                    model: "m".into(),
                    effort: None,
                },
                current: sample_prompt("Hello"),
                budget: default_budget(),
            },
            expected_transfer_digest: Digest([0; 32]), // Rejected sentinel!
        };

        let command = EngineCommand {
            id: CommandId::fresh(),
            conversation: conv,
            payload: CommandPayload::Transfer(transfer_spec),
        };

        let err = store.submit(command).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);
        assert!(err.message.contains("Zero-digest sentinel"));
    }

    #[test]
    fn test_execute_transfer_rejects_digest_mismatch() {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir, "root-source");
        store.state.paused = false;
        let conv = test_conv("root-source");

        let preview_spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "p".into(),
                    revision: 1,
                },
                model: "m".into(),
                effort: None,
            },
            current: sample_prompt("Hello"),
            budget: default_budget(),
        };

        let transfer_spec = TransferSpec {
            preview: preview_spec,
            expected_transfer_digest: Digest([42; 32]), // Wrong digest!
        };

        let command = EngineCommand {
            id: CommandId::fresh(),
            conversation: conv,
            payload: CommandPayload::Transfer(transfer_spec),
        };

        let err = store.submit(command).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::InvalidCommand);
        assert!(err.message.contains("digest mismatch"));
    }

    #[test]
    fn test_execute_transfer_rejects_stale_source_revision() {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir, "root-source");
        store.state.paused = false;
        let conv = test_conv("root-source");

        let preview_spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: conv.clone(),
            source_revision: store.state.revision,
            source_task: store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "p".into(),
                    revision: 1,
                },
                model: "m".into(),
                effort: None,
            },
            current: sample_prompt("Hello"),
            budget: default_budget(),
        };

        let preview = prepare_transfer(&store, &preview_spec).unwrap();

        // Mutate source store state revision
        store.state.revision += 1;

        let transfer_spec = TransferSpec {
            preview: preview_spec,
            expected_transfer_digest: preview.transfer_digest,
        };

        let command = EngineCommand {
            id: CommandId::fresh(),
            conversation: conv,
            payload: CommandPayload::Transfer(transfer_spec),
        };

        let err = store.submit(command).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::StaleRevision);
    }

    #[test]
    fn test_destination_acceptance_initialization() {
        let source_dir = TempDir::new().unwrap();
        let mut source_store = open_store(&source_dir, "root-source");
        let source_conv = test_conv("root-source");

        let preview_spec = TransferPreviewSpec {
            mode: TransferMode::Handoff,
            source: source_conv.clone(),
            source_revision: source_store.state.revision,
            source_task: source_store.current_task_revision().unwrap(),
            through_row: RowId {
                conversation: source_conv.clone(),
                ordinal: 0,
            },
            destination_root: SessionId::parse("root-dest").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "dest-p".into(),
                    revision: 1,
                },
                model: "m".into(),
                effort: None,
            },
            current: sample_prompt("First user prompt in destination"),
            budget: default_budget(),
        };

        let preview = prepare_transfer(&source_store, &preview_spec).unwrap();
        let transfer_spec = TransferSpec {
            preview: preview_spec,
            expected_transfer_digest: preview.transfer_digest,
        };

        let command = EngineCommand {
            id: CommandId::fresh(),
            conversation: source_conv,
            payload: CommandPayload::Transfer(transfer_spec),
        };

        let _ = source_store.submit(command).unwrap();
        let pkg_obj_name: String = serde_json::from_value(
            source_store
                .state
                .extra
                .get("last_transfer_package")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let pkg_bytes = source_store
            .objects
            .read(&pkg_obj_name, 1024 * 1024)
            .unwrap();
        let package: TransferPackage = serde_json::from_slice(&pkg_bytes).unwrap();

        // Now open destination store
        let dest_dir = TempDir::new().unwrap();
        let mut dest_store = open_store(&dest_dir, "root-dest");

        let accepted = accept_transfer_into_destination(&mut dest_store, package).unwrap();
        assert_eq!(accepted.conversation.root.as_str(), "root-dest");
        assert!(matches!(
            accepted.first_command.state,
            CommandState::Held(_)
        ));

        // Check task was initialized with inherited scope
        let task_val = dest_store.document.get("task").unwrap();
        let task: TaskRecord = serde_json::from_value(task_val.clone()).unwrap();
        assert_eq!(
            task.contract.original_request,
            "First user prompt in destination"
        );
    }
}
