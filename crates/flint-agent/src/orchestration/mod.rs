//! B1 durable acceptance foundation. No provider execution is enabled.
pub(crate) mod children;
mod contracts;
#[cfg_attr(windows, path = "fs_windows.rs")]
pub(crate) mod fs;
mod identity;
pub(crate) mod records;
pub(crate) mod store;
#[cfg(test)]
mod tests;
pub(crate) mod transfer;

use crate::watch::PrWatchSource;
use crate::{AgentConfig, ApprovalDecision, Op};
pub use contracts::*;
pub use identity::*;
use std::io::Read;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    pub code: EngineErrorCode,
    pub message: String,
}
impl EngineError {
    pub(crate) fn new(code: EngineErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub(crate) fn unsupported() -> Self {
        Self::new(
            EngineErrorCode::Unsupported,
            "This operation is not enabled by the B1 acceptance slice.",
        )
    }
}
impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for EngineError {}

pub struct ResolvedNative {
    pub target: ProviderTarget,
    pub base_url: String,
    pub credential: crate::secrets::SecretBytes,
    pub capabilities: ProviderCapabilities,
}
pub enum ResolvedTarget {
    Native(Box<ResolvedNative>),
    External,
}
pub struct ExternalStart {
    pub conversation: ConversationId,
    pub target: ProviderTarget,
    pub workspace: std::path::PathBuf,
    pub session_dir: std::path::PathBuf,
    pub policy: InheritedPolicy,
    pub events: ExternalEventSink,
}
#[derive(Clone)]
pub struct ExternalEventSink {
    _private: (),
}
impl ExternalEventSink {
    pub async fn record(&self, _: crate::AgentEvent) -> Result<(), EngineError> {
        Err(EngineError::unsupported())
    }
    pub fn progress(&self, _: ProgressUpdate) -> Result<(), EngineError> {
        Err(EngineError::unsupported())
    }
    pub async fn rich(&self, _: crate::rich::RichOutput) -> Result<(), EngineError> {
        Err(EngineError::unsupported())
    }
    pub async fn intent(&self, _: SideEffectIntent) -> Result<(), EngineError> {
        Err(EngineError::unsupported())
    }
}
pub struct ExternalHandle {
    pub conversation: Arc<dyn ExternalConversation>,
    pub capabilities: ProviderCapabilities,
}
pub struct HostServices {
    pub profiles: Arc<dyn TargetResolver>,
    pub external: Option<Arc<dyn ExternalAgentBridge>>,
    pub pr_watch: Option<Arc<dyn PrWatchSource>>,
    pub destinations: Option<Arc<dyn DestinationRegistry>>,
}

type Request = Box<dyn FnOnce(&mut store::Store) + Send>;
#[derive(Clone)]
pub struct CommandClient {
    tx: async_channel::Sender<Request>,
    pub(crate) acceptance_only: bool,
    root: SessionId,
    revision_rx: tokio::sync::watch::Receiver<u64>,
    task_notify: Arc<tokio::sync::Notify>,
    stopping: Arc<AtomicBool>,
}

type ActiveRunEntry = (ConversationId, Option<TaskHandle>, Arc<AtomicBool>);

#[derive(Default)]
pub(crate) struct LiveControls {
    pub(crate) active_runs: std::sync::Mutex<std::collections::HashMap<RunId, ActiveRunEntry>>,
    pub(crate) pending_approvals: std::sync::Mutex<
        std::collections::HashMap<(RunId, String), tokio::sync::oneshot::Sender<ApprovalDecision>>,
    >,
}

#[derive(Clone)]
pub struct ControlClient {
    commands: async_channel::Sender<Request>,
    stopping: Arc<AtomicBool>,
    done: tokio::sync::watch::Receiver<bool>,
    live: Arc<LiveControls>,
    acceptance_only: bool,
}
pub struct OrchestrationHandle {
    pub ops: async_channel::Sender<Op>,
    pub commands: CommandClient,
    pub controls: ControlClient,
    pub events: async_channel::Receiver<EngineEvent>,
    pub secrets: crate::secrets::SecretInbox,
}

impl CommandClient {
    pub(crate) async fn query<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut store::Store) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.tx
            .send(Box::new(move |store| {
                let _ = reply.send(action(store));
            }))
            .await
            .map_err(|_| {
                EngineError::new(
                    EngineErrorCode::Cancelled,
                    "Engine acceptance client is closed.",
                )
            })?;
        result.await.map_err(|_| {
            EngineError::new(
                EngineErrorCode::Cancelled,
                "Engine acceptance reply was disconnected.",
            )
        })?
    }
    pub async fn submit(&self, command: EngineCommand) -> Result<CommandReceipt, EngineError> {
        // Bound captured inputs before placing them in the bounded actor inbox,
        // rather than allocating an arbitrarily large canonical envelope.
        if let Err(error) = store::validate_intake(&command) {
            // Even unsupported/oversized replacements cannot reuse an
            // already accepted identity under a different payload.
            if self.receipt(&command.id).await?.is_some() {
                return Err(EngineError::new(
                    EngineErrorCode::IdentityConflict,
                    "Accepted command identity is immutable.",
                ));
            }
            return Err(error);
        }
        self.query(move |store| store.submit(command)).await
    }
    pub async fn receipt(&self, id: &CommandId) -> Result<Option<CommandReceipt>, EngineError> {
        let id = id.clone();
        self.query(move |store| store.receipt(&id)).await
    }
    pub async fn queue(
        &self,
        conversation: &ConversationId,
    ) -> Result<QueueProjection, EngineError> {
        let conversation = conversation.clone();
        self.query(move |store| store.queue(&conversation)).await
    }
    pub async fn queued_prompt(&self, id: &CommandId) -> Result<PromptSnapshot, EngineError> {
        let id = id.clone();
        self.query(move |store| store.queued_prompt(&id)).await
    }
    pub async fn recent_receipts(
        &self,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<ReceiptPage, EngineError> {
        self.query(move |store| store.recent(before_sequence, limit))
            .await
    }
    pub async fn summary(
        &self,
        conversation: &ConversationId,
    ) -> Result<ConversationSummary, EngineError> {
        let conversation = conversation.clone();
        self.query(move |store| store.summary(&conversation)).await
    }
    pub async fn records(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<RecordPage, EngineError> {
        if self.acceptance_only {
            return Err(EngineError::unsupported());
        }
        self.query(move |store| store.records(after_sequence, limit))
            .await
    }
    pub async fn records_before(
        &self,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<RecordPage, EngineError> {
        if self.acceptance_only {
            return Err(EngineError::unsupported());
        }
        self.query(move |store| store.records_before(before_sequence, limit))
            .await
    }
    pub async fn acknowledge_records(&self, through_sequence: u64) -> Result<(), EngineError> {
        if self.acceptance_only {
            return Err(EngineError::unsupported());
        }
        self.query(move |store| store.acknowledge_records(through_sequence))
            .await
    }
    pub async fn history(&self, request: HistoryPageRequest) -> Result<HistoryPage, EngineError> {
        if request.limit == 0
            || request.limit > 100
            || request.max_bytes == 0
            || request.max_bytes > 256 * 1024
        {
            return Err(EngineError::new(
                EngineErrorCode::InvalidCommand,
                "History page exceeds its bounds.",
            ));
        }
        if self.acceptance_only {
            return Err(EngineError::unsupported());
        }
        self.query(move |store| store.history(request)).await
    }
    pub async fn resolve_citation(
        &self,
        citation: Citation,
    ) -> Result<HistoryExcerpt, EngineError> {
        if self.acceptance_only {
            return Err(EngineError::unsupported());
        }
        self.query(move |store| store.resolve_citation(citation))
            .await
    }
    pub async fn task(&self, handle: &TaskHandle) -> Result<TaskStatus, EngineError> {
        children::task(self, handle).await
    }
    pub async fn tasks(&self, handles: &[TaskHandle]) -> Result<Vec<TaskStatus>, EngineError> {
        children::tasks(self, handles).await
    }
    pub async fn wait(&self, request: WaitRequest) -> Result<WaitResult, EngineError> {
        children::wait(self, request).await
    }
    pub async fn update_task_status(
        &self,
        handle: &TaskHandle,
        state: TaskRunState,
    ) -> Result<TaskStatus, EngineError> {
        let handle = handle.clone();
        self.query(move |store| store.update_task_status(&handle, state)).await
    }
    pub(crate) fn root(&self) -> &SessionId {
        &self.root
    }
    pub(crate) fn revision_watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revision_rx.clone()
    }
    pub(crate) fn task_notify(&self) -> &Arc<tokio::sync::Notify> {
        &self.task_notify
    }
    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
    pub async fn confirm_queue_continuation(
        &self,
        conversation: ConversationId,
        revision: TaskRevision,
        command_id: CommandId,
        restart_generation: u64,
    ) -> Result<QueueContinuationPermit, EngineError> {
        self.query(move |store| {
            store.confirm_queue_continuation(conversation, revision, command_id, restart_generation)
        })
        .await
    }
    pub async fn resume_one(
        &self,
        permit: QueueContinuationPermit,
    ) -> Result<CommandReceipt, EngineError> {
        self.query(move |store| store.resume_one(permit)).await
    }
    pub async fn record_intent(
        &self,
        intent: SideEffectIntent,
    ) -> Result<SideEffectIntent, EngineError> {
        self.query(move |store| store.record_intent(intent)).await
    }
    pub async fn complete_intent(
        &self,
        intent_id: u64,
        success: bool,
    ) -> Result<SideEffectIntent, EngineError> {
        self.query(move |store| store.complete_intent(intent_id, success))
            .await
    }
    pub async fn prepare_transfer(
        &self,
        spec: &TransferPreviewSpec,
    ) -> Result<TransferPreview, EngineError> {
        let spec = spec.clone();
        self.query(move |store| store.prepare_transfer(&spec)).await
    }
}
impl ControlClient {
    pub fn register_run(
        &self,
        run_id: RunId,
        conversation: ConversationId,
        task: Option<TaskHandle>,
    ) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.live
            .active_runs
            .lock()
            .unwrap()
            .insert(run_id, (conversation, task, flag.clone()));
        flag
    }

    pub fn unregister_run(&self, run_id: &RunId) {
        self.live.active_runs.lock().unwrap().remove(run_id);
    }

    pub fn register_approval(
        &self,
        run_id: RunId,
        call_id: String,
    ) -> tokio::sync::oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.live
            .pending_approvals
            .lock()
            .unwrap()
            .insert((run_id, call_id), tx);
        rx
    }

    pub fn unregister_approval(&self, run_id: &RunId, call_id: &str) {
        self.live
            .pending_approvals
            .lock()
            .unwrap()
            .remove(&(run_id.clone(), call_id.to_string()));
    }

    pub async fn stop(&self, scope: StopScope) -> Result<StopAck, EngineError> {
        if self.acceptance_only && self.live.active_runs.lock().unwrap().is_empty() {
            return Err(EngineError::unsupported());
        }
        let mut affected_runs = Vec::new();
        let runs = self.live.active_runs.lock().unwrap();
        for (run_id, (conv, task, flag)) in runs.iter() {
            let matches = match &scope {
                StopScope::Root => true,
                StopScope::Conversation(target_conv) => conv == target_conv,
                StopScope::Task(target_task) => task.as_ref() == Some(target_task),
            };
            if matches {
                flag.store(true, Ordering::SeqCst);
                affected_runs.push(run_id.clone());
            }
        }
        Ok(StopAck {
            scope,
            affected_runs,
            cancellation_requested: true,
        })
    }
    pub async fn approve(
        &self,
        address: ApprovalAddress,
        decision: ApprovalDecision,
    ) -> Result<(), EngineError> {
        if self.acceptance_only && self.live.pending_approvals.lock().unwrap().is_empty() {
            return Err(EngineError::unsupported());
        }
        let sender = self
            .live
            .pending_approvals
            .lock()
            .unwrap()
            .remove(&(address.run_id, address.call_id));
        match sender {
            Some(tx) => {
                let _ = tx.send(decision);
                Ok(())
            }
            None => Err(EngineError::new(
                EngineErrorCode::NotFound,
                "No pending approval for address.",
            )),
        }
    }
    pub async fn shutdown(&self) -> Result<(), EngineError> {
        self.stopping.store(true, Ordering::SeqCst);
        self.commands.close();
        let mut done = self.done.clone();
        done.wait_for(|done| *done).await.map_err(|_| {
            EngineError::new(
                EngineErrorCode::Storage,
                "Acceptance worker stopped without a shutdown acknowledgement.",
            )
        })?;
        Ok(())
    }
}

/// Opens acceptance storage only. Neither config nor host services are used
/// to resolve credentials, start processes, contact providers, or schedule work.
pub fn spawn_orchestrated_session(
    config: AgentConfig,
    options: OrchestrationOptions,
    _services: HostServices,
) -> Result<OrchestrationHandle, EngineError> {
    let dir = config.session_dir.ok_or_else(|| {
        EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Durable acceptance requires a session directory.",
        )
    })?;
    open_client(&dir, options, false)
}

/// B1 storage entry point for hosts that intentionally do not start an executor.
pub fn open_acceptance_session(
    dir: &Path,
    options: OrchestrationOptions,
) -> Result<OrchestrationHandle, EngineError> {
    open_client(dir, options, true)
}
fn open_client(
    dir: &Path,
    options: OrchestrationOptions,
    acceptance_only: bool,
) -> Result<OrchestrationHandle, EngineError> {
    let inbox = options.limits.command_inbox;
    let root_id = options.root_id.clone();
    let mut store = store::Store::open(dir, options)?;
    let (revision_tx, revision_rx) = tokio::sync::watch::channel(store.state.revision);
    let task_notify = Arc::new(tokio::sync::Notify::new());
    store.revision_tx = Some(revision_tx);
    store.task_notify = Some(task_notify.clone());
    let (tx, rx) = async_channel::bounded::<Request>(inbox);
    let (ops, incoming_ops) = async_channel::bounded::<Op>(1);
    let (events_tx, events) = async_channel::bounded::<EngineEvent>(1);
    // There are no B1 execution events. Receipts are authoritative replies and
    // query projections; no background lossy receipt broadcast is invented.
    drop(events_tx);
    // Legacy ops fail visibly as a closed channel in this nonexecuting surface.
    incoming_ops.close();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let (finished, done) = tokio::sync::watch::channel(false);
    let live = Arc::new(LiveControls::default());
    let controls = ControlClient {
        commands: tx.clone(),
        stopping: stopping.clone(),
        done,
        live,
        acceptance_only,
    };
    let commands = CommandClient {
        tx,
        acceptance_only,
        root: root_id,
        revision_rx,
        task_notify,
        stopping,
    };
    std::thread::Builder::new()
        .name("flint-acceptance".into())
        .spawn(move || {
            let mut store = store;
            while let Ok(action) = rx.recv_blocking() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                action(&mut store);
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
            // Closing async-channel alone retains queued callbacks while a
            // sender exists. Drop them explicitly so cancelled replies settle.
            rx.close();
            while rx.try_recv().is_ok() {}
            drop(store);
            let _ = finished.send(true);
        })
        .map_err(|_| {
            EngineError::new(
                EngineErrorCode::Storage,
                "Couldn't start the acceptance worker.",
            )
        })?;
    Ok(OrchestrationHandle {
        ops,
        commands,
        controls,
        events,
        secrets: crate::secrets::SecretInbox { _private: () },
    })
}

pub fn read_conversation_summary(dir: &Path) -> Result<Option<ConversationSummary>, EngineError> {
    // Ignore legacy transcript bodies when building the lightweight projection.
    #[derive(serde::Deserialize)]
    struct Projection {
        version: u64,
        turn_id: u64,
        #[serde(default)]
        task: Option<crate::TaskRecord>,
        orchestration: Option<store::State>,
    }
    let directory = fs::Directory::open(dir).map_err(store::storage)?;
    let file = match directory.reader("history.json", store::MAX_SNAPSHOT_BYTES) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(store::storage(error)),
    };
    let projection: Projection = serde_json::from_reader(std::io::BufReader::new(
        file.take(store::MAX_SNAPSHOT_BYTES as u64 + 1),
    ))
    .map_err(|_| {
        EngineError::new(
            EngineErrorCode::CorruptState,
            "Couldn't read the engine summary. History has been kept.",
        )
    })?;
    if !matches!(projection.version, 1..=3) {
        return Err(EngineError::new(
            EngineErrorCode::CorruptState,
            "Unsupported history version.",
        ));
    }
    if projection.version != 3 {
        return Ok(None);
    }
    let Some(state) = projection.orchestration else {
        return Err(EngineError::new(
            EngineErrorCode::CorruptState,
            "Missing v3 orchestration.",
        ));
    };
    store::validate_lineage(&directory, &state)?;
    if let Some(task) = &projection.task {
        task.validate().map_err(|_| {
            EngineError::new(EngineErrorCode::CorruptState, "Invalid task projection.")
        })?;
    }
    store::summary_from(
        &serde_json::json!({"turn_id":projection.turn_id,"task":projection.task}),
        &state,
    )
    .map(Some)
}

pub fn read_agent_record(
    path: &Path,
    payload: &AgentRecordPayload,
    max_bytes: usize,
) -> Result<crate::AgentEvent, EngineError> {
    records::read_agent_record(path, payload, max_bytes)
}

pub fn read_asset(
    path: &Path,
    asset: &crate::rich::AssetRef,
    max_bytes: usize,
) -> Result<Vec<u8>, EngineError> {
    records::read_asset(path, asset, max_bytes)
}

pub fn write_asset(
    path: &Path,
    mime_type: &str,
    bytes: &[u8],
) -> Result<crate::rich::AssetRef, EngineError> {
    records::write_asset(path, mime_type, bytes)
}
