use super::fs::{Directory, WriterLease};
use super::*;
use crate::{TaskOutcome, TaskRecord, TurnEndReason};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub(crate) const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_COMMAND_BYTES: usize = 48 * 1024 * 1024;
const MAX_NODE_BYTES: usize = 64 * 1024;
const DIGEST_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub receipt: CommandReceipt,
    pub object: String,
    pub acceptance: String,
    pub bytes: usize,
    pub preview: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct State {
    pub version: u32,
    pub root: SessionId,
    pub target: ProviderTarget,
    pub revision: u64,
    pub generation: u64,
    pub next_sequence: u64,
    pub receipt_head: Option<String>,
    pub receipt_count: u64,
    pub pending: Vec<Pending>,
    #[serde(default)]
    pub queue_revision: Option<u64>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub intents: Vec<SideEffectIntent>,
    #[serde(default)]
    pub terminal_receipts: HashMap<CommandId, CommandReceipt>,
    #[serde(default)]
    pub tasks: Vec<TaskStatus>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl State {
    pub fn queue_revision(&self) -> u64 {
        self.queue_revision.unwrap_or(self.revision)
    }

    pub fn advance_queue_revision(&mut self) -> Result<u64, EngineError> {
        let current = self.queue_revision();
        let next = current.checked_add(1).ok_or_else(corrupt)?;
        self.queue_revision = Some(next);
        Ok(next)
    }
}
#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u32,
    command: EngineCommand,
}
#[derive(Serialize, Deserialize)]
struct Node {
    version: u32,
    receipt: CommandReceipt,
    command_object: String,
    previous: Option<String>,
    ordinal: u64,
}

pub(crate) struct Store {
    pub lease: WriterLease,
    objects: Directory,
    pub document: Value,
    pub state: State,
    pub limits: EngineLimits,
    retained_index: HashMap<CommandId, CommandReceipt>,
    poisoned: bool,
    pub(crate) revision_tx: Option<tokio::sync::watch::Sender<u64>>,
    pub(crate) task_notify: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    pub fault: Option<&'static str>,
}

fn corrupt() -> EngineError {
    EngineError::new(
        EngineErrorCode::CorruptState,
        "Invalid saved orchestration. Existing history has been kept.",
    )
}
pub(crate) fn storage(_: std::io::Error) -> EngineError {
    EngineError::new(
        EngineErrorCode::Storage,
        "Couldn't commit private engine storage. Existing history has been kept.",
    )
}
pub(crate) fn digest(bytes: &[u8]) -> Digest {
    Digest(Sha256::digest(bytes).into())
}
fn object_name(bytes: &[u8]) -> String {
    digest_name(&digest(bytes))
}
fn digest_name(digest: &Digest) -> String {
    digest.to_hex()
}
fn valid_object(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}
pub(crate) fn canonical_command(command: &EngineCommand) -> Result<(Vec<u8>, Digest), EngineError> {
    validate_intake(command)?;
    let mut value = serde_json::to_value(Envelope {
        version: DIGEST_VERSION,
        command: command.clone(),
    })
    .map_err(|_| corrupt())?;
    // Keep version-1 bytes stable even when dependency feature unification
    // enables preserve_order, including objects nested inside arrays.
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).map_err(|_| corrupt())?;
    if bytes.len() > MAX_COMMAND_BYTES {
        return Err(EngineError::new(
            EngineErrorCode::QueueFull,
            "Command snapshot exceeds its byte limit.",
        ));
    }
    let digest = digest(&bytes);
    Ok((bytes, digest))
}

pub(crate) fn validate_intake(command: &EngineCommand) -> Result<(), EngineError> {
    match &command.payload {
        CommandPayload::Send {
            prompt,
            delivery: Delivery::NextTurn,
        } => validate_prompt(prompt),
        CommandPayload::StartChild(spec) => {
            if command.conversation.child.is_some() {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Child conversations cannot start children (single delegation level).",
                ));
            }
            validate_prompt(&spec.prompt)?;
            if spec.label.len() > 512 {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Child label exceeds 512 bytes.",
                ));
            }
            if spec.child.is_some() && spec.target.is_some() {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Target override is not allowed when resuming an existing child.",
                ));
            }
            if let Some(target) = &spec.target {
                validate_target(target)?;
            }
            Ok(())
        }
        CommandPayload::CancelQueued {
            expected_queue_revision,
            ..
        } => {
            if *expected_queue_revision == 0 {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Expected queue revision must be greater than zero.",
                ));
            }
            Ok(())
        }
        CommandPayload::ReplaceQueued {
            command_id,
            expected_queue_revision,
            replacement,
        } => {
            if *expected_queue_revision == 0 {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Expected queue revision must be greater than zero.",
                ));
            }
            if replacement.id == *command_id || replacement.id == command.id {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Replacement command must have a fresh identity.",
                ));
            }
            if replacement.conversation != command.conversation {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Replacement command conversation mismatch.",
                ));
            }
            match &replacement.payload {
                CommandPayload::Send {
                    prompt,
                    delivery: Delivery::NextTurn,
                } => validate_prompt(prompt),
                _ => Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Replacement payload must be a Send command with NextTurn delivery.",
                )),
            }
        }
        CommandPayload::PauseQueue {
            expected_queue_revision,
        } => {
            if *expected_queue_revision == 0 {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Expected queue revision must be greater than zero.",
                ));
            }
            Ok(())
        }
        CommandPayload::ResumeQueue {
            expected_queue_revision,
            restart_generation,
        } => {
            if *expected_queue_revision == 0 {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Expected queue revision must be greater than zero.",
                ));
            }
            if *restart_generation == 0 {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Restart generation must be greater than zero.",
                ));
            }
            Ok(())
        }
        _ => Err(EngineError::unsupported()),
    }
}

pub(crate) fn read_document(directory: &Directory) -> Result<Option<Value>, EngineError> {
    let Some(bytes) = directory
        .read_optional("history.json", MAX_SNAPSHOT_BYTES)
        .map_err(storage)?
    else {
        return Ok(None);
    };
    let document: Value = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
    let version = document["version"].as_u64().ok_or_else(corrupt)?;
    if !matches!(version, 1..=3) {
        return Err(EngineError::new(
            EngineErrorCode::CorruptState,
            "Unsupported history version. Existing history has been kept.",
        ));
    }
    if document["turn_id"].as_u64().is_none() || !document["messages"].is_array() {
        return Err(corrupt());
    }
    // Validate legacy history/task without repairing or rewriting its source.
    serde_json::from_value::<Vec<crate::provider::Message>>(document["messages"].clone())
        .map_err(|_| corrupt())?;
    if let Some(task) = document.get("task").filter(|task| !task.is_null()) {
        serde_json::from_value::<TaskRecord>(task.clone())
            .map_err(|_| corrupt())?
            .validate()
            .map_err(|_| corrupt())?;
    }
    if version == 3 {
        validate_lineage(
            directory,
            &serde_json::from_value(document["orchestration"].clone()).map_err(|_| corrupt())?,
        )?;
    }
    Ok(Some(document))
}

pub(crate) fn validate_state(state: &State) -> Result<(), EngineError> {
    validate_target(&state.target).map_err(|_| corrupt())?;
    if state.version != 1
        || state.revision == 0
        || state.next_sequence == 0
        || state.pending.len() > 20
        || state.pending.len() as u64 > state.receipt_count
        || state.receipt_count >= state.next_sequence
        || state
            .receipt_head
            .as_ref()
            .is_some_and(|name| !valid_object(name))
        || (state.receipt_head.is_none() != (state.receipt_count == 0))
        || state.intents.len() > 32
        || state.queue_revision.is_some_and(|rev| rev == 0)
    {
        return Err(corrupt());
    }
    for intent in &state.intents {
        if intent.conversation.root != state.root {
            return Err(corrupt());
        }
    }
    if state.tasks.len() > 32 {
        return Err(corrupt());
    }
    let active_tasks = state
        .tasks
        .iter()
        .filter(|t| !matches!(t.state, TaskRunState::Finished { .. }))
        .count();
    if active_tasks > 4 {
        return Err(corrupt());
    }
    let mut distinct_children = std::collections::HashSet::new();
    let mut distinct_handles = Vec::with_capacity(state.tasks.len());
    for task in &state.tasks {
        if distinct_handles.contains(&task.handle) {
            return Err(corrupt());
        }
        distinct_handles.push(task.handle.clone());
        if task.handle.conversation.root != state.root {
            return Err(corrupt());
        }
        let Some(child_id) = &task.handle.conversation.child else {
            return Err(corrupt());
        };
        distinct_children.insert(child_id.clone());
        if task.parent.root != state.root {
            return Err(corrupt());
        }
        if task.revision == 0 || task.revision > state.revision {
            return Err(corrupt());
        }
        if task.label.len() > 512 {
            return Err(corrupt());
        }
        validate_target(&task.target).map_err(|_| corrupt())?;
    }
    if distinct_children.len() > 32 {
        return Err(corrupt());
    }
    let mut bytes = 0usize;
    for (index, entry) in state.pending.iter().enumerate() {
        bytes = bytes.checked_add(entry.bytes).ok_or_else(corrupt)?;
        if bytes > 32 * 1024 * 1024
            || !valid_object(&entry.object)
            || entry.receipt.conversation.root != state.root
            || entry.receipt.conversation.child.is_some()
            || entry.receipt.accepted_sequence == 0
            || entry.receipt.accepted_sequence >= state.next_sequence
            || !valid_object(&entry.acceptance)
            || entry.preview.len() > 512
            || entry.receipt.accepted_revision == 0
            || entry.receipt.accepted_revision > state.revision
            || entry.receipt.status_revision == 0
            || entry.receipt.status_revision > state.revision
            || entry.receipt.status_revision < entry.receipt.accepted_revision
            || state.pending[..index]
                .iter()
                .any(|other| other.receipt.id == entry.receipt.id)
        {
            return Err(corrupt());
        }
    }
    Ok(())
}

fn read_object(objects: &Directory, name: &str, limit: usize) -> Result<Vec<u8>, EngineError> {
    if !valid_object(name) {
        return Err(corrupt());
    }
    let bytes = objects.read(name, limit).map_err(|_| corrupt())?;
    if object_name(&bytes) != name {
        return Err(corrupt());
    }
    Ok(bytes)
}

fn read_node(
    objects: &Directory,
    state: &State,
    name: &str,
    ordinal: u64,
) -> Result<Node, EngineError> {
    let node: Node = serde_json::from_slice(&read_object(objects, name, MAX_NODE_BYTES)?)
        .map_err(|_| corrupt())?;
    if node.version != 1
        || node.ordinal != ordinal
        || ordinal == 0
        || !valid_object(&node.command_object)
        || node.command_object != digest_name(&node.receipt.payload_digest)
        || node
            .previous
            .as_ref()
            .is_some_and(|name| !valid_object(name))
        || (node.previous.is_none() != (ordinal == 1))
        || node.receipt.conversation.root != state.root
    {
        return Err(corrupt());
    }
    Ok(node)
}

/// Read-only preflight shared by recovery, stopped v3 task mutation and
/// projections. Scan O(history nodes * <=20 active entries), one bounded node
/// at a time. No historical payload reads, unbounded receipt map or writes.
pub(crate) fn validate_lineage(directory: &Directory, state: &State) -> Result<(), EngineError> {
    validate_state(state)?;
    if state.receipt_head.is_none() {
        return Ok(());
    }
    let objects = directory
        .open_child("engine-objects")
        .map_err(|_| corrupt())?;
    // Sized by the entries actually indexed below, so this cannot silently
    // overflow if the queued-command limit is ever raised.
    let mut matched = vec![false; state.pending.len()];
    let mut next = state.receipt_head.clone();
    let mut ordinal = state.receipt_count;
    let mut before_sequence = state.next_sequence;
    while let Some(name) = next {
        let node = read_node(&objects, state, &name, ordinal)?;
        if node.receipt.accepted_sequence == 0
            || node.receipt.accepted_sequence >= before_sequence
            || node.receipt.accepted_revision == 0
            || node.receipt.accepted_revision > state.revision
        {
            return Err(corrupt());
        }
        for (index, entry) in state.pending.iter().enumerate() {
            if name == entry.acceptance {
                if node.command_object != entry.object
                    || node.receipt.id != entry.receipt.id
                    || node.receipt.conversation != entry.receipt.conversation
                    || node.receipt.payload_digest != entry.receipt.payload_digest
                    || node.receipt.accepted_sequence != entry.receipt.accepted_sequence
                    || node.receipt.accepted_revision != entry.receipt.accepted_revision
                {
                    return Err(corrupt());
                }
                matched[index] = true;
            }
        }
        before_sequence = node.receipt.accepted_sequence;
        next = node.previous;
        ordinal -= 1;
    }
    if ordinal != 0 || matched.iter().any(|found| !found) {
        return Err(corrupt());
    }
    Ok(())
}

/// Counts serialized bytes without allocating a buffer.
struct ByteCounter(usize);
impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Store {
    fn healthy(&self) -> Result<(), EngineError> {
        if self.poisoned {
            return Err(EngineError::new(
                EngineErrorCode::Storage,
                "Canonical commit is uncertain. Shut down and reopen before querying or accepting work.",
            ));
        }
        Ok(())
    }
    pub fn open(path: &Path, options: OrchestrationOptions) -> Result<Self, EngineError> {
        validate_limits(&options.limits)?;
        validate_target(&options.target)?;
        let lease = WriterLease::acquire(path).map_err(|error| {
            let code = if error.to_string().contains("Another writer") {
                EngineErrorCode::Busy
            } else {
                EngineErrorCode::Storage
            };
            EngineError::new(
                code,
                "Cannot acquire the exclusive engine writer. Existing history has been kept.",
            )
        })?;
        let document = read_document(&lease.directory)?.unwrap_or_else(|| {
            json!({
                "version": 2, "turn_id": 0, "messages": []
            })
        });
        let existing = document["version"] == 3;
        let mut state: State = if existing {
            serde_json::from_value(document["orchestration"].clone()).map_err(|_| corrupt())?
        } else {
            State {
                version: 1,
                root: options.root_id.clone(),
                target: options.target.clone(),
                revision: 1,
                generation: 0,
                next_sequence: 1,
                receipt_head: None,
                receipt_count: 0,
                pending: Vec::new(),
                queue_revision: Some(1),
                paused: true,
                intents: Vec::new(),
                terminal_receipts: HashMap::new(),
                tasks: Vec::new(),
                extra: BTreeMap::new(),
            }
        };
        if state.root != options.root_id || state.target != options.target {
            return Err(EngineError::new(
                EngineErrorCode::IdentityConflict,
                "Saved session/provider identity does not match.",
            ));
        }
        if state.pending.len() > options.limits.queued_commands
            || state.pending.iter().map(|entry| entry.bytes).sum::<usize>()
                > options.limits.queued_bytes
        {
            return Err(EngineError::new(
                EngineErrorCode::QueueFull,
                "Saved queue exceeds the configured limits.",
            ));
        }
        if state.tasks.len() > options.limits.retained_children {
            return Err(EngineError::new(
                EngineErrorCode::QueueFull,
                "Saved retained children exceed configured limits.",
            ));
        }
        let active_children = state
            .tasks
            .iter()
            .filter(|t| !matches!(t.state, TaskRunState::Finished { .. }))
            .count();
        if active_children > options.limits.active_children {
            return Err(EngineError::new(
                EngineErrorCode::QueueFull,
                "Saved active children exceed configured limits.",
            ));
        }
        let objects = lease.directory.child("engine-objects").map_err(storage)?;
        let mut store = Self {
            lease,
            objects,
            document,
            state: state.clone(),
            limits: options.limits,
            retained_index: HashMap::new(),
            poisoned: false,
            revision_tx: None,
            task_notify: None,
            #[cfg(test)]
            fault: None,
        };
        if existing {
            let mut next = state.receipt_head.clone();
            let mut ordinal = state.receipt_count;
            while let Some(name) = next {
                let node = store.node(&name, ordinal)?;
                store
                    .retained_index
                    .entry(node.receipt.id.clone())
                    .or_insert(node.receipt);
                next = node.previous;
                ordinal -= 1;
            }
            for (id, receipt) in &state.terminal_receipts {
                store.retained_index.insert(id.clone(), receipt.clone());
            }
        }
        // read_document already proved membership in the canonical lineage
        // before this owner can create objects or commit recovery. Only active
        // payloads need to be opened here; historical payloads stay on disk.
        for entry in &state.pending {
            store.prompt(entry)?;
        }
        state.generation = state.generation.checked_add(1).ok_or_else(corrupt)?;
        if existing {
            state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
            state.paused = true;
        }
        let mut ambiguous_intents = Vec::new();
        for intent in &mut state.intents {
            if matches!(intent.state, IntentState::Dispatching) {
                intent.state = IntentState::Ambiguous;
                ambiguous_intents.push(intent.id);
            }
        }
        for pending in &mut state.pending {
            match &pending.receipt.state {
                CommandState::Started { run_id, .. } => {
                    pending.receipt.status_revision = state.revision;
                    let matching: Vec<u64> = state
                        .intents
                        .iter()
                        .filter(|i| &i.run_id == run_id && i.state == IntentState::Ambiguous)
                        .map(|i| i.id)
                        .collect();
                    let intent_ids = if matching.is_empty() {
                        ambiguous_intents.clone()
                    } else {
                        matching
                    };
                    pending.receipt.state = CommandState::Held(HoldReason::AmbiguousEffects {
                        run_id: run_id.clone(),
                        intent_ids,
                    });
                }
                CommandState::Accepted | CommandState::Held(HoldReason::Restart { .. }) => {
                    pending.receipt.status_revision = state.revision;
                    pending.receipt.state = CommandState::Held(HoldReason::Restart {
                        generation: state.generation,
                    });
                }
                CommandState::Held(_) | CommandState::Finished { .. } => {}
            }
        }
        for task in &mut state.tasks {
            if matches!(task.state, TaskRunState::Queued) {
                task.revision = state.revision;
                task.state = TaskRunState::Held(HoldReason::Restart {
                    generation: state.generation,
                });
            }
        }
        store.commit(state)?;
        Ok(store)
    }

    fn object(&self, name: &str, limit: usize) -> Result<Vec<u8>, EngineError> {
        read_object(&self.objects, name, limit)
    }
    fn node(&self, name: &str, ordinal: u64) -> Result<Node, EngineError> {
        read_node(&self.objects, &self.state, name, ordinal)
    }
    fn retained(&self, id: &CommandId) -> Result<Option<CommandReceipt>, EngineError> {
        self.healthy()?;
        // Fast path: active pending entries hold the latest runtime state.
        if let Some(entry) = self
            .state
            .pending
            .iter()
            .find(|entry| &entry.receipt.id == id)
        {
            return Ok(Some(entry.receipt.clone()));
        }
        if let Some(receipt) = self.state.terminal_receipts.get(id) {
            return Ok(Some(receipt.clone()));
        }
        Ok(self.retained_index.get(id).cloned())
    }

    pub fn receipt(&self, id: &CommandId) -> Result<Option<CommandReceipt>, EngineError> {
        self.retained(id)
    }
    pub fn submit(&mut self, command: EngineCommand) -> Result<CommandReceipt, EngineError> {
        self.healthy()?;
        let retained = self.retained(&command.id)?;
        let (bytes, payload_digest) = canonical_command(&command).map_err(|error| {
            if retained.is_some() {
                EngineError::new(
                    EngineErrorCode::IdentityConflict,
                    "Accepted command identity is immutable.",
                )
            } else {
                error
            }
        })?;
        if let Some(receipt) = retained {
            if receipt.conversation != command.conversation
                || receipt.payload_digest != payload_digest
            {
                return Err(EngineError::new(
                    EngineErrorCode::IdentityConflict,
                    "Accepted command identity is immutable.",
                ));
            }
            return Ok(receipt);
        }
        self.owned(&command.conversation)?;
        match &command.payload {
            CommandPayload::Send {
                prompt,
                delivery: Delivery::NextTurn,
            } => {
                validate_prompt(prompt)?;
                if self.state.pending.len() >= self.limits.queued_commands
                    || bytes.len().saturating_add(
                        self.state
                            .pending
                            .iter()
                            .map(|entry| entry.bytes)
                            .sum::<usize>(),
                    ) > self.limits.queued_bytes
                {
                    return Err(EngineError::new(
                        EngineErrorCode::QueueFull,
                        "Engine queue capacity reached.",
                    ));
                }
                let mut state = self.state.clone();
                state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
                state.advance_queue_revision()?;
                let receipt = CommandReceipt {
                    id: command.id.clone(),
                    conversation: command.conversation.clone(),
                    task: None,
                    payload_digest,
                    accepted_sequence: state.next_sequence,
                    accepted_revision: state.revision,
                    status_revision: state.revision,
                    state: CommandState::Accepted,
                };
                state.next_sequence = state.next_sequence.checked_add(1).ok_or_else(corrupt)?;
                state.receipt_count = state.receipt_count.checked_add(1).ok_or_else(corrupt)?;
                let object = object_name(&bytes);
                self.fail("object")?;
                self.objects.immutable(&object, &bytes).map_err(storage)?;
                let node = Node {
                    version: 1,
                    receipt: receipt.clone(),
                    command_object: object.clone(),
                    previous: state.receipt_head.clone(),
                    ordinal: state.receipt_count,
                };
                let node_bytes = serde_json::to_vec(&node).map_err(|_| corrupt())?;
                let node_name = object_name(&node_bytes);
                self.fail("receipt")?;
                self.objects
                    .immutable(&node_name, &node_bytes)
                    .map_err(storage)?;
                state.receipt_head = Some(node_name.clone());
                let preview: String = prompt
                    .request
                    .chars()
                    .scan(0usize, |size, ch| {
                        *size += ch.len_utf8();
                        (*size <= 512).then_some(ch)
                    })
                    .collect();
                state.pending.push(Pending {
                    receipt: receipt.clone(),
                    object,
                    acceptance: node_name,
                    bytes: bytes.len(),
                    preview,
                });
                self.commit(state)?;
                Ok(receipt)
            }
            CommandPayload::CancelQueued {
                command_id,
                expected_queue_revision,
            } => {
                if *expected_queue_revision != self.state.queue_revision() {
                    return Err(EngineError::new(
                        EngineErrorCode::StaleRevision,
                        "Queue revision conflict.",
                    ));
                }
                let target_index = self
                    .state
                    .pending
                    .iter()
                    .position(|e| &e.receipt.id == command_id)
                    .ok_or_else(|| {
                        EngineError::new(EngineErrorCode::NotFound, "Queued command not found.")
                    })?;
                let target = &self.state.pending[target_index];
                if matches!(target.receipt.state, CommandState::Started { .. }) {
                    return Err(EngineError::new(
                        EngineErrorCode::InvalidCommand,
                        "Cannot cancel a started command.",
                    ));
                }
                let mut state = self.state.clone();
                state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
                state.advance_queue_revision()?;
                let removed = state.pending.remove(target_index);
                let mut cancelled_receipt = removed.receipt;
                cancelled_receipt.status_revision = state.revision;
                cancelled_receipt.state = CommandState::Finished {
                    run_id: None,
                    turn_id: None,
                    reason: CommandFinish::CancelledBeforeStart,
                };
                state
                    .terminal_receipts
                    .insert(command_id.clone(), cancelled_receipt.clone());
                self.retained_index
                    .insert(command_id.clone(), cancelled_receipt);
                if let Some(task) = state
                    .tasks
                    .iter_mut()
                    .find(|t| &t.handle.command_id == command_id)
                {
                    task.revision = state.revision;
                    task.state = TaskRunState::Finished {
                        reason: TurnEndReason::Interrupted,
                        outcome: TaskOutcome::Unknown,
                        result_row: None,
                    };
                }

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
                let object = object_name(&bytes);
                self.fail("object")?;
                self.objects.immutable(&object, &bytes).map_err(storage)?;
                let node = Node {
                    version: 1,
                    receipt: control_receipt.clone(),
                    command_object: object,
                    previous: state.receipt_head.clone(),
                    ordinal: state.receipt_count,
                };
                let node_bytes = serde_json::to_vec(&node).map_err(|_| corrupt())?;
                let node_name = object_name(&node_bytes);
                self.fail("receipt")?;
                self.objects
                    .immutable(&node_name, &node_bytes)
                    .map_err(storage)?;
                state.receipt_head = Some(node_name);
                self.retained_index
                    .insert(command.id.clone(), control_receipt.clone());
                self.commit(state)?;
                Ok(control_receipt)
            }
            CommandPayload::ReplaceQueued {
                command_id,
                expected_queue_revision,
                replacement,
            } => {
                if *expected_queue_revision != self.state.queue_revision() {
                    return Err(EngineError::new(
                        EngineErrorCode::StaleRevision,
                        "Queue revision conflict.",
                    ));
                }
                let target_index = self
                    .state
                    .pending
                    .iter()
                    .position(|e| &e.receipt.id == command_id)
                    .ok_or_else(|| {
                        EngineError::new(EngineErrorCode::NotFound, "Queued command not found.")
                    })?;
                let target = &self.state.pending[target_index];
                if matches!(target.receipt.state, CommandState::Started { .. }) {
                    return Err(EngineError::new(
                        EngineErrorCode::InvalidCommand,
                        "Cannot replace a started command.",
                    ));
                }
                if self.retained(&replacement.id)?.is_some() {
                    return Err(EngineError::new(
                        EngineErrorCode::IdentityConflict,
                        "Replacement command identity already exists.",
                    ));
                }
                let CommandPayload::Send { prompt, .. } = &replacement.payload else {
                    return Err(EngineError::new(
                        EngineErrorCode::InvalidCommand,
                        "Replacement payload must be Send.",
                    ));
                };
                let (replacement_bytes, replacement_digest) = canonical_command(replacement)?;
                let old_pending_bytes = self.state.pending[target_index].bytes;
                let current_bytes: usize = self.state.pending.iter().map(|e| e.bytes).sum();
                let new_bytes = current_bytes
                    .saturating_sub(old_pending_bytes)
                    .saturating_add(replacement_bytes.len());
                if new_bytes > self.limits.queued_bytes {
                    return Err(EngineError::new(
                        EngineErrorCode::QueueFull,
                        "Engine queue capacity reached.",
                    ));
                }
                let mut state = self.state.clone();
                state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
                state.advance_queue_revision()?;

                let replacement_object = object_name(&replacement_bytes);
                self.objects
                    .immutable(&replacement_object, &replacement_bytes)
                    .map_err(storage)?;

                let replacement_receipt = CommandReceipt {
                    id: replacement.id.clone(),
                    conversation: replacement.conversation.clone(),
                    task: None,
                    payload_digest: replacement_digest,
                    accepted_sequence: state.next_sequence,
                    accepted_revision: state.revision,
                    status_revision: state.revision,
                    state: CommandState::Accepted,
                };
                state.next_sequence = state.next_sequence.checked_add(1).ok_or_else(corrupt)?;
                state.receipt_count = state.receipt_count.checked_add(1).ok_or_else(corrupt)?;
                let replacement_node = Node {
                    version: 1,
                    receipt: replacement_receipt.clone(),
                    command_object: replacement_object.clone(),
                    previous: state.receipt_head.clone(),
                    ordinal: state.receipt_count,
                };
                let rnode_bytes = serde_json::to_vec(&replacement_node).map_err(|_| corrupt())?;
                let rnode_name = object_name(&rnode_bytes);
                self.objects
                    .immutable(&rnode_name, &rnode_bytes)
                    .map_err(storage)?;
                state.receipt_head = Some(rnode_name.clone());

                let preview: String = prompt
                    .request
                    .chars()
                    .scan(0usize, |size, ch| {
                        *size += ch.len_utf8();
                        (*size <= 512).then_some(ch)
                    })
                    .collect();
                state.pending[target_index] = Pending {
                    receipt: replacement_receipt.clone(),
                    object: replacement_object,
                    acceptance: rnode_name,
                    bytes: replacement_bytes.len(),
                    preview,
                };

                let mut superseded_receipt = target.receipt.clone();
                superseded_receipt.status_revision = state.revision;
                superseded_receipt.state = CommandState::Finished {
                    run_id: None,
                    turn_id: None,
                    reason: CommandFinish::Superseded {
                        replacement: replacement.id.clone(),
                    },
                };
                state
                    .terminal_receipts
                    .insert(command_id.clone(), superseded_receipt.clone());
                self.retained_index
                    .insert(command_id.clone(), superseded_receipt);
                self.retained_index
                    .insert(replacement.id.clone(), replacement_receipt);

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
                let control_object = object_name(&bytes);
                self.objects
                    .immutable(&control_object, &bytes)
                    .map_err(storage)?;
                let control_node = Node {
                    version: 1,
                    receipt: control_receipt.clone(),
                    command_object: control_object,
                    previous: state.receipt_head.clone(),
                    ordinal: state.receipt_count,
                };
                let cnode_bytes = serde_json::to_vec(&control_node).map_err(|_| corrupt())?;
                let cnode_name = object_name(&cnode_bytes);
                self.objects
                    .immutable(&cnode_name, &cnode_bytes)
                    .map_err(storage)?;
                state.receipt_head = Some(cnode_name);
                self.retained_index
                    .insert(command.id.clone(), control_receipt.clone());

                self.commit(state)?;
                Ok(control_receipt)
            }
            CommandPayload::PauseQueue {
                expected_queue_revision,
            } => {
                if *expected_queue_revision != self.state.queue_revision() {
                    return Err(EngineError::new(
                        EngineErrorCode::StaleRevision,
                        "Queue revision conflict.",
                    ));
                }
                let mut state = self.state.clone();
                state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
                state.advance_queue_revision()?;
                state.paused = true;
                for task in &mut state.tasks {
                    if matches!(task.state, TaskRunState::Queued) {
                        task.revision = state.revision;
                        task.state = TaskRunState::Held(HoldReason::Paused);
                    }
                }

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
                let object = object_name(&bytes);
                self.fail("object")?;
                self.objects.immutable(&object, &bytes).map_err(storage)?;
                let node = Node {
                    version: 1,
                    receipt: control_receipt.clone(),
                    command_object: object,
                    previous: state.receipt_head.clone(),
                    ordinal: state.receipt_count,
                };
                let node_bytes = serde_json::to_vec(&node).map_err(|_| corrupt())?;
                let node_name = object_name(&node_bytes);
                self.fail("receipt")?;
                self.objects
                    .immutable(&node_name, &node_bytes)
                    .map_err(storage)?;
                state.receipt_head = Some(node_name);
                self.retained_index
                    .insert(command.id.clone(), control_receipt.clone());
                self.commit(state)?;
                Ok(control_receipt)
            }
            CommandPayload::ResumeQueue {
                expected_queue_revision,
                restart_generation,
            } => {
                if *expected_queue_revision != self.state.queue_revision() {
                    return Err(EngineError::new(
                        EngineErrorCode::StaleRevision,
                        "Queue revision conflict.",
                    ));
                }
                if *restart_generation != self.state.generation {
                    return Err(EngineError::new(
                        EngineErrorCode::StaleRevision,
                        "Restart generation conflict.",
                    ));
                }
                let mut state = self.state.clone();
                state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
                state.advance_queue_revision()?;
                state.paused = false;
                for pending in &mut state.pending {
                    if matches!(
                        pending.receipt.state,
                        CommandState::Held(HoldReason::Paused)
                            | CommandState::Held(HoldReason::Restart { .. })
                    ) {
                        pending.receipt.status_revision = state.revision;
                        pending.receipt.state = CommandState::Accepted;
                    }
                }
                for task in &mut state.tasks {
                    if matches!(
                        task.state,
                        TaskRunState::Held(HoldReason::Paused)
                            | TaskRunState::Held(HoldReason::Restart { .. })
                    ) {
                        task.revision = state.revision;
                        task.state = TaskRunState::Queued;
                    }
                }

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
                let object = object_name(&bytes);
                self.fail("object")?;
                self.objects.immutable(&object, &bytes).map_err(storage)?;
                let node = Node {
                    version: 1,
                    receipt: control_receipt.clone(),
                    command_object: object,
                    previous: state.receipt_head.clone(),
                    ordinal: state.receipt_count,
                };
                let node_bytes = serde_json::to_vec(&node).map_err(|_| corrupt())?;
                let node_name = object_name(&node_bytes);
                self.fail("receipt")?;
                self.objects
                    .immutable(&node_name, &node_bytes)
                    .map_err(storage)?;
                state.receipt_head = Some(node_name);
                self.retained_index
                    .insert(command.id.clone(), control_receipt.clone());
                self.commit(state)?;
                Ok(control_receipt)
            }
            CommandPayload::StartChild(spec) => {
                let active_children = self
                    .state
                    .tasks
                    .iter()
                    .filter(|t| !matches!(t.state, TaskRunState::Finished { .. }))
                    .count();
                let distinct_children: std::collections::HashSet<ChildId> = self
                    .state
                    .tasks
                    .iter()
                    .filter_map(|t| t.handle.conversation.child.clone())
                    .collect();

                let (child_id, target) = if let Some(child_id) = &spec.child {
                    if !distinct_children.contains(child_id) {
                        return Err(EngineError::new(
                            EngineErrorCode::NotFound,
                            "Child not found.",
                        ));
                    }
                    let is_active = self.state.tasks.iter().any(|t| {
                        t.handle.conversation.child.as_ref() == Some(child_id)
                            && !matches!(t.state, TaskRunState::Finished { .. })
                    });
                    if is_active {
                        return Err(EngineError::new(
                            EngineErrorCode::Busy,
                            "Child is already active.",
                        ));
                    }
                    if active_children >= self.limits.active_children {
                        return Err(EngineError::new(
                            EngineErrorCode::QueueFull,
                            "Active children limit reached.",
                        ));
                    }
                    if self.state.tasks.len() >= self.limits.retained_children {
                        return Err(EngineError::new(
                            EngineErrorCode::QueueFull,
                            "Retained children limit reached.",
                        ));
                    }
                    let target = self
                        .state
                        .tasks
                        .iter()
                        .find(|t| t.handle.conversation.child.as_ref() == Some(child_id))
                        .unwrap()
                        .target
                        .clone();
                    (child_id.clone(), target)
                } else {
                    if distinct_children.len() >= self.limits.retained_children {
                        return Err(EngineError::new(
                            EngineErrorCode::QueueFull,
                            "Retained children limit reached.",
                        ));
                    }
                    if active_children >= self.limits.active_children {
                        return Err(EngineError::new(
                            EngineErrorCode::QueueFull,
                            "Active children limit reached.",
                        ));
                    }
                    if self.state.tasks.len() >= self.limits.retained_children {
                        return Err(EngineError::new(
                            EngineErrorCode::QueueFull,
                            "Retained children limit reached.",
                        ));
                    }
                    let mut n = 1u64;
                    let allocated = loop {
                        let candidate = ChildId::parse(&format!("agent-{n}")).unwrap();
                        if !distinct_children.contains(&candidate) {
                            break candidate;
                        }
                        n = n.checked_add(1).ok_or_else(corrupt)?;
                    };
                    let target = spec
                        .target
                        .clone()
                        .unwrap_or_else(|| self.state.target.clone());
                    (allocated, target)
                };

                let child_conversation = ConversationId {
                    root: self.state.root.clone(),
                    child: Some(child_id),
                };
                let task_handle = TaskHandle {
                    conversation: child_conversation,
                    command_id: command.id.clone(),
                };

                if self.state.pending.len() >= self.limits.queued_commands
                    || bytes.len().saturating_add(
                        self.state
                            .pending
                            .iter()
                            .map(|entry| entry.bytes)
                            .sum::<usize>(),
                    ) > self.limits.queued_bytes
                {
                    return Err(EngineError::new(
                        EngineErrorCode::QueueFull,
                        "Engine queue capacity reached.",
                    ));
                }

                let mut state = self.state.clone();
                state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
                state.advance_queue_revision()?;

                let receipt = CommandReceipt {
                    id: command.id.clone(),
                    conversation: command.conversation.clone(),
                    task: Some(task_handle.clone()),
                    payload_digest,
                    accepted_sequence: state.next_sequence,
                    accepted_revision: state.revision,
                    status_revision: state.revision,
                    state: CommandState::Accepted,
                };
                state.next_sequence = state.next_sequence.checked_add(1).ok_or_else(corrupt)?;
                state.receipt_count = state.receipt_count.checked_add(1).ok_or_else(corrupt)?;

                let object = object_name(&bytes);
                self.fail("object")?;
                self.objects.immutable(&object, &bytes).map_err(storage)?;

                let node = Node {
                    version: 1,
                    receipt: receipt.clone(),
                    command_object: object.clone(),
                    previous: state.receipt_head.clone(),
                    ordinal: state.receipt_count,
                };
                let node_bytes = serde_json::to_vec(&node).map_err(|_| corrupt())?;
                let node_name = object_name(&node_bytes);
                self.fail("receipt")?;
                self.objects
                    .immutable(&node_name, &node_bytes)
                    .map_err(storage)?;
                state.receipt_head = Some(node_name.clone());

                let preview: String = spec
                    .prompt
                    .request
                    .chars()
                    .scan(0usize, |size, ch| {
                        *size += ch.len_utf8();
                        (*size <= 512).then_some(ch)
                    })
                    .collect();
                state.pending.push(Pending {
                    receipt: receipt.clone(),
                    object,
                    acceptance: node_name,
                    bytes: bytes.len(),
                    preview,
                });

                let task_status = TaskStatus {
                    handle: task_handle,
                    target,
                    parent: command.conversation.clone(),
                    label: spec.label.clone(),
                    revision: state.revision,
                    state: TaskRunState::Queued,
                };
                state.tasks.push(task_status);

                self.commit(state)?;
                Ok(receipt)
            }
            _ => Err(EngineError::unsupported()),
        }
    }

    pub fn owned(&self, conversation: &ConversationId) -> Result<(), EngineError> {
        self.healthy()?;
        if conversation.root != self.state.root {
            return Err(EngineError::new(
                EngineErrorCode::NotOwned,
                "Conversation is not owned by this B1 ledger.",
            ));
        }
        if let Some(child_id) = &conversation.child {
            let is_known = self
                .state
                .tasks
                .iter()
                .any(|t| t.handle.conversation.child.as_ref() == Some(child_id));
            if !is_known {
                return Err(EngineError::new(
                    EngineErrorCode::NotOwned,
                    "Conversation is not owned by this B1 ledger.",
                ));
            }
        }
        Ok(())
    }
    pub fn queue(&self, conversation: &ConversationId) -> Result<QueueProjection, EngineError> {
        self.owned(conversation)?;
        let hold = self
            .state
            .pending
            .iter()
            .find_map(|entry| match &entry.receipt.state {
                CommandState::Held(hold) => Some(hold.clone()),
                _ => None,
            })
            .or(self.state.paused.then_some(HoldReason::Paused));
        Ok(QueueProjection {
            revision: self.state.queue_revision(),
            restart_generation: self.state.generation,
            paused: self.state.paused,
            hold,
            items: self
                .state
                .pending
                .iter()
                .map(|entry| QueuedCommand {
                    receipt: entry.receipt.clone(),
                    preview: entry.preview.clone(),
                    captured_bytes: entry.bytes as u64,
                })
                .collect(),
        })
    }
    pub fn prompt(&self, entry: &Pending) -> Result<PromptSnapshot, EngineError> {
        self.healthy()?;
        let bytes = self.object(&entry.object, MAX_COMMAND_BYTES)?;
        let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
        if envelope.version != DIGEST_VERSION
            || bytes.len() != entry.bytes
            || digest(&bytes) != entry.receipt.payload_digest
            || envelope.command.id != entry.receipt.id
            || envelope.command.conversation != entry.receipt.conversation
        {
            return Err(corrupt());
        }
        let prompt = match envelope.command.payload {
            CommandPayload::Send { prompt, .. } => prompt,
            CommandPayload::StartChild(spec) => spec.prompt,
            _ => return Err(corrupt()),
        };
        validate_prompt(&prompt).map_err(|_| corrupt())?;
        Ok(prompt)
    }
    pub fn queued_prompt(&self, id: &CommandId) -> Result<PromptSnapshot, EngineError> {
        self.healthy()?;
        self.prompt(
            self.state
                .pending
                .iter()
                .find(|entry| &entry.receipt.id == id)
                .ok_or_else(|| {
                    EngineError::new(EngineErrorCode::NotFound, "Command is not queued.")
                })?,
        )
    }
    pub fn recent(&self, before: Option<u64>, limit: usize) -> Result<ReceiptPage, EngineError> {
        self.healthy()?;
        if limit == 0 || limit > self.limits.history_page_rows {
            return Err(EngineError::new(
                EngineErrorCode::InvalidCommand,
                "Receipt page limit must be 1..=100.",
            ));
        }
        let mut next = self.state.receipt_head.clone();
        let mut ordinal = self.state.receipt_count;
        let mut receipts = Vec::new();
        // Reserve the complete JSON page envelope and worst-case continuation,
        // not merely the sum of individual receipt bodies.
        let mut counter = ByteCounter(0);
        serde_json::to_writer(
            &mut counter,
            &ReceiptPage {
                receipts: Vec::new(),
                next_before_sequence: Some(u64::MAX),
            },
        )
        .map_err(|_| corrupt())?;
        let mut bytes = counter.0;
        if bytes > self.limits.history_page_bytes {
            return Err(EngineError::new(
                EngineErrorCode::InvalidCommand,
                "Receipt page envelope exceeds its byte limit.",
            ));
        }
        let mut more = false;
        while let Some(name) = next {
            let node = self.node(&name, ordinal)?;
            next = node.previous;
            ordinal -= 1;
            if before.is_some_and(|before| node.receipt.accepted_sequence >= before) {
                continue;
            }
            let receipt = self
                .state
                .pending
                .iter()
                .find(|entry| entry.receipt.id == node.receipt.id)
                .map_or(node.receipt, |entry| entry.receipt.clone());
            let mut counter = ByteCounter(0);
            serde_json::to_writer(&mut counter, &receipt).map_err(|_| corrupt())?;
            bytes += counter.0 + usize::from(!receipts.is_empty());
            if bytes > self.limits.history_page_bytes {
                if receipts.is_empty() {
                    return Err(EngineError::new(
                        EngineErrorCode::InvalidCommand,
                        "Receipt exceeds page byte limit.",
                    ));
                }
                more = true;
                break;
            }
            receipts.push(receipt);
            if receipts.len() == limit {
                break;
            }
        }
        let next_before_sequence = if next.is_some() || more {
            receipts.last().map(|r| r.accepted_sequence)
        } else {
            None
        };
        Ok(ReceiptPage {
            receipts,
            next_before_sequence,
        })
    }
    pub fn summary(
        &self,
        conversation: &ConversationId,
    ) -> Result<ConversationSummary, EngineError> {
        self.owned(conversation)?;
        summary_from(&self.document, &self.state)
    }

    pub fn tasks(&self, handles: &[TaskHandle]) -> Result<Vec<TaskStatus>, EngineError> {
        self.healthy()?;
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            if handle.conversation.root != self.state.root {
                return Err(EngineError::new(
                    EngineErrorCode::NotOwned,
                    "Task handle is not owned by this session.",
                ));
            }
            let status = self
                .state
                .tasks
                .iter()
                .find(|t| &t.handle == handle)
                .ok_or_else(|| {
                    EngineError::new(EngineErrorCode::NotFound, "Task handle not found.")
                })?;
            results.push(status.clone());
        }
        Ok(results)
    }

    pub fn task(&self, handle: &TaskHandle) -> Result<TaskStatus, EngineError> {
        let mut list = self.tasks(std::slice::from_ref(handle))?;
        Ok(list.remove(0))
    }

    pub fn update_task_status(
        &mut self,
        handle: &TaskHandle,
        new_state: TaskRunState,
    ) -> Result<TaskStatus, EngineError> {
        self.healthy()?;
        if handle.conversation.root != self.state.root {
            return Err(EngineError::new(
                EngineErrorCode::NotOwned,
                "Task handle is not owned by this session.",
            ));
        }
        let pos = self
            .state
            .tasks
            .iter()
            .position(|t| &t.handle == handle)
            .ok_or_else(|| {
                EngineError::new(EngineErrorCode::NotFound, "Task handle not found.")
            })?;
        if matches!(self.state.tasks[pos].state, TaskRunState::Finished { .. }) {
            return Err(EngineError::new(
                EngineErrorCode::InvalidCommand,
                "Cannot update a finished task.",
            ));
        }
        let mut state = self.state.clone();
        state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
        let task = &mut state.tasks[pos];
        task.state = new_state.clone();
        task.revision = state.revision;
        let updated_task = task.clone();

        if let Some(pending_pos) = state
            .pending
            .iter()
            .position(|e| e.receipt.id == handle.command_id)
        {
            let mut entry = state.pending.remove(pending_pos);
            entry.receipt.status_revision = state.revision;
            match &new_state {
                TaskRunState::Finished { reason, .. } => {
                    entry.receipt.state = CommandState::Finished {
                        run_id: None,
                        turn_id: None,
                        reason: CommandFinish::Executed(reason.clone()),
                    };
                    state
                        .terminal_receipts
                        .insert(handle.command_id.clone(), entry.receipt.clone());
                    self.retained_index
                        .insert(handle.command_id.clone(), entry.receipt);
                }
                TaskRunState::Running { run_id, turn_id } => {
                    entry.receipt.state = CommandState::Started {
                        run_id: run_id.clone(),
                        turn_id: *turn_id,
                        safe_boundary_step: None,
                    };
                    state.pending.insert(pending_pos, entry);
                }
                TaskRunState::Held(reason) => {
                    entry.receipt.state = CommandState::Held(reason.clone());
                    state.pending.insert(pending_pos, entry);
                }
                TaskRunState::Queued => {
                    entry.receipt.state = CommandState::Accepted;
                    state.pending.insert(pending_pos, entry);
                }
            }
        }
        self.commit(state)?;
        Ok(updated_task)
    }
    pub(crate) fn commit(&mut self, state: State) -> Result<(), EngineError> {
        if self.poisoned {
            return Err(storage(std::io::Error::other(
                "Writer must reopen after failure.",
            )));
        }
        validate_state(&state)?;
        let mut document = self.document.clone();
        document["version"] = json!(3);
        document["orchestration"] = serde_json::to_value(&state).map_err(|_| corrupt())?;
        let bytes = serde_json::to_vec(&document).map_err(|_| corrupt())?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(EngineError::new(
                EngineErrorCode::Storage,
                "Canonical snapshot exceeds its byte limit.",
            ));
        }
        self.fail("snapshot")?;
        #[cfg(test)]
        let fault = self.fault;
        #[cfg(not(test))]
        let fault: Option<&str> = None;
        let result = self.lease.directory.replace_with(
            "history.json",
            &bytes,
            || {
                if fault == Some("before-rename") {
                    Err(std::io::Error::other("injected"))
                } else {
                    Ok(())
                }
            },
            || {
                if fault == Some("after-rename") {
                    Err(std::io::Error::other("injected"))
                } else {
                    Ok(())
                }
            },
        );
        if let Err(error) = result {
            // Rename may have succeeded. No receipt is reported and no further
            // acceptance is allowed until reopening reconciles canonical disk.
            self.poisoned = true;
            return Err(storage(error));
        }
        for entry in &state.pending {
            self.retained_index
                .entry(entry.receipt.id.clone())
                .or_insert_with(|| entry.receipt.clone());
        }
        for (id, receipt) in &state.terminal_receipts {
            self.retained_index.insert(id.clone(), receipt.clone());
        }
        self.document = document;
        self.state = state;
        if let Some(tx) = &self.revision_tx {
            tx.send_replace(self.state.revision);
        }
        if let Some(notify) = &self.task_notify {
            notify.notify_waiters();
        }
        Ok(())
    }

    pub fn confirm_queue_continuation(
        &self,
        conversation: ConversationId,
        revision: TaskRevision,
        command_id: CommandId,
        restart_generation: u64,
    ) -> Result<QueueContinuationPermit, EngineError> {
        self.owned(&conversation)?;
        if restart_generation != self.state.generation {
            return Err(EngineError::new(
                EngineErrorCode::StaleRevision,
                "Restart generation conflict.",
            ));
        }
        let pending = self
            .state
            .pending
            .iter()
            .find(|entry| entry.receipt.id == command_id)
            .ok_or_else(|| {
                EngineError::new(EngineErrorCode::NotFound, "Queued command not found.")
            })?;
        match &pending.receipt.state {
            CommandState::Held(HoldReason::TaskReview {
                revision: held_rev, ..
            }) => {
                if held_rev != &revision {
                    return Err(EngineError::new(
                        EngineErrorCode::StaleRevision,
                        "Task revision conflict.",
                    ));
                }
            }
            CommandState::Held(_) => {}
            _ => {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Command is not in a held state.",
                ));
            }
        }
        Ok(QueueContinuationPermit {
            conversation,
            revision,
            command_id,
            restart_generation,
        })
    }

    pub fn resume_one(
        &mut self,
        permit: QueueContinuationPermit,
    ) -> Result<CommandReceipt, EngineError> {
        self.healthy()?;
        self.owned(&permit.conversation)?;
        if permit.restart_generation != self.state.generation {
            return Err(EngineError::new(
                EngineErrorCode::StaleRevision,
                "Restart generation conflict.",
            ));
        }
        let target_index = self
            .state
            .pending
            .iter()
            .position(|entry| entry.receipt.id == permit.command_id)
            .ok_or_else(|| {
                EngineError::new(EngineErrorCode::NotFound, "Queued command not found.")
            })?;
        let entry = &self.state.pending[target_index];
        match &entry.receipt.state {
            CommandState::Held(HoldReason::TaskReview { revision, .. }) => {
                if revision != &permit.revision {
                    return Err(EngineError::new(
                        EngineErrorCode::StaleRevision,
                        "Task revision conflict.",
                    ));
                }
            }
            CommandState::Held(_) => {}
            _ => {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Command is no longer held.",
                ));
            }
        }
        let mut state = self.state.clone();
        state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
        state.advance_queue_revision()?;
        let entry = &mut state.pending[target_index];
        entry.receipt.status_revision = state.revision;
        entry.receipt.state = CommandState::Accepted;
        let updated_receipt = entry.receipt.clone();
        self.commit(state)?;
        Ok(updated_receipt)
    }

    pub fn record_intent(
        &mut self,
        intent: SideEffectIntent,
    ) -> Result<SideEffectIntent, EngineError> {
        self.healthy()?;
        self.owned(&intent.conversation)?;
        if self.state.intents.len() >= 32 {
            return Err(EngineError::new(
                EngineErrorCode::QueueFull,
                "Maximum active side-effect intents reached.",
            ));
        }
        if self.state.intents.iter().any(|i| i.id == intent.id) {
            return Err(EngineError::new(
                EngineErrorCode::IdentityConflict,
                "Side-effect intent ID already exists.",
            ));
        }
        let mut state = self.state.clone();
        state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
        state.intents.push(intent.clone());
        self.commit(state)?;
        Ok(intent)
    }

    pub fn complete_intent(
        &mut self,
        intent_id: u64,
        success: bool,
    ) -> Result<SideEffectIntent, EngineError> {
        self.healthy()?;
        let pos = self
            .state
            .intents
            .iter()
            .position(|i| i.id == intent_id)
            .ok_or_else(|| {
                EngineError::new(EngineErrorCode::NotFound, "Side-effect intent not found.")
            })?;
        let mut state = self.state.clone();
        state.revision = state.revision.checked_add(1).ok_or_else(corrupt)?;
        state.intents[pos].state = IntentState::Completed { success };
        let updated = state.intents[pos].clone();
        self.commit(state)?;
        Ok(updated)
    }
    fn fail(&self, point: &str) -> Result<(), EngineError> {
        #[cfg(test)]
        if self.fault == Some(point) {
            return Err(storage(std::io::Error::other("injected")));
        }
        let _ = point;
        Ok(())
    }
}

pub(crate) fn validate_target(target: &ProviderTarget) -> Result<(), EngineError> {
    let (profile, text) = match target {
        ProviderTarget::Native { profile, model, .. } => (profile, model),
        ProviderTarget::Acp {
            profile,
            agent_id,
            options,
        } => {
            if options.len() > 32
                || options
                    .iter()
                    .any(|(key, value)| key.len() > 128 || value.len() > 512)
            {
                return Err(corrupt());
            }
            (profile, agent_id)
        }
    };
    SessionId::parse(&profile.id)?;
    if profile.revision == 0 || text.trim().is_empty() || text.len() > 512 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Invalid provider target.",
        ));
    }
    Ok(())
}
fn validate_limits(limits: &EngineLimits) -> Result<(), EngineError> {
    let defaults = EngineLimits::default();
    if limits.queued_commands == 0
        || limits.queued_commands > defaults.queued_commands
        || limits.queued_bytes == 0
        || limits.queued_bytes > defaults.queued_bytes
        || limits.command_inbox == 0
        || limits.command_inbox > defaults.command_inbox
        || limits.record_delivery == 0
        || limits.record_delivery > defaults.record_delivery
        || limits.progress_keys == 0
        || limits.progress_keys > defaults.progress_keys
        || limits.progress_bytes == 0
        || limits.progress_bytes > defaults.progress_bytes
        || limits.outbox_bytes == 0
        || limits.outbox_bytes > defaults.outbox_bytes
        || limits.active_children == 0
        || limits.active_children > defaults.active_children
        || limits.retained_children == 0
        || limits.retained_children > defaults.retained_children
        || limits.history_page_rows == 0
        || limits.history_page_rows > defaults.history_page_rows
        || limits.history_page_bytes == 0
        || limits.history_page_bytes > defaults.history_page_bytes
    {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Engine limits exceed B1 bounds.",
        ));
    }
    Ok(())
}
fn validate_prompt(prompt: &PromptSnapshot) -> Result<(), EngineError> {
    let bytes = prompt.images.iter().fold(
        prompt
            .request
            .len()
            .saturating_add(prompt.effective_text.len()),
        |sum, image| {
            sum.saturating_add(image.data.len())
                .saturating_add(image.name.len())
        },
    );
    if bytes > 32 * 1024 * 1024 {
        return Err(EngineError::new(
            EngineErrorCode::QueueFull,
            "Frozen prompt exceeds its input byte limit.",
        ));
    }
    if (prompt.request.trim().is_empty() && prompt.images.is_empty())
        || prompt.images.len() > 4
        || prompt.citations.len() > 32
        || prompt.images.iter().any(|image| {
            image.name.is_empty()
                || image.name.len() > 512
                || !matches!(
                    image.mime_type.as_str(),
                    "image/png" | "image/jpeg" | "image/webp"
                )
                || image.data.is_empty()
        })
        || prompt
            .citations
            .iter()
            .any(|citation| citation.utf8_start > citation.utf8_end || citation.row.ordinal == 0)
    {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Invalid frozen prompt snapshot.",
        ));
    }
    Ok(())
}
pub(crate) fn summary_from(
    document: &Value,
    state: &State,
) -> Result<ConversationSummary, EngineError> {
    let outcome = document
        .get("task")
        .and_then(|task| task.get("assessment"))
        .and_then(|assessment| assessment.get("outcome"))
        .map(|outcome| serde_json::from_value(outcome.clone()))
        .transpose()
        .map_err(|_| corrupt())?
        .unwrap_or(TaskOutcome::Unknown);
    let hold = state
        .pending
        .iter()
        .find_map(|entry| match &entry.receipt.state {
            CommandState::Held(hold) => Some(hold.clone()),
            _ => None,
        })
        .or(state.paused.then_some(HoldReason::Paused));
    Ok(ConversationSummary {
        conversation: ConversationId {
            root: state.root.clone(),
            child: None,
        },
        title: None,
        target: state.target.clone(),
        // B1 creates acceptance receipts, not engine event records.
        last_record_sequence: 0,
        snapshot_revision: state.revision,
        last_turn_id: document["turn_id"].as_u64().ok_or_else(corrupt)?,
        running: false,
        queued: state.pending.len(),
        hold,
        outcome,
        unread: false,
        preview: state
            .pending
            .last()
            .map_or_else(String::new, |entry| entry.preview.clone()),
    })
}
