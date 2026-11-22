//! T3 Contract §4 & §7 records delivery, outbox acknowledgement, history pagination,
//! citation excerpt extraction, and agent record loading.

use super::contracts::*;
use super::store::{self, ByteCounter, Store};
use super::EngineError;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordCursor {
    After(u64),
    Before(Option<u64>),
}

impl From<u64> for RecordCursor {
    fn from(seq: u64) -> Self {
        RecordCursor::After(seq)
    }
}

impl From<Option<u64>> for RecordCursor {
    fn from(seq: Option<u64>) -> Self {
        RecordCursor::Before(seq)
    }
}

/// Paginate durable root-sequenced EngineRecord objects, strictly increasing sequence order,
/// capped by limits.record_delivery (default 256).
pub(crate) fn records(
    store: &Store,
    after_sequence: u64,
    limit: usize,
) -> Result<RecordPage, EngineError> {
    paginate_records(store, RecordCursor::After(after_sequence), limit)
}

/// Paginate durable root-sequenced EngineRecord objects before an optional upper bound sequence.
pub(crate) fn records_before(
    store: &Store,
    before_sequence: Option<u64>,
    limit: usize,
) -> Result<RecordPage, EngineError> {
    paginate_records(store, RecordCursor::Before(before_sequence), limit)
}

/// Generalized record paginator supporting either an After or Before cursor.
pub(crate) fn paginate_records(
    store: &Store,
    cursor: impl Into<RecordCursor>,
    limit: usize,
) -> Result<RecordPage, EngineError> {
    if limit == 0 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Record delivery limit must be greater than zero.",
        ));
    }
    let effective_limit = limit.min(store.limits.record_delivery);
    let cursor = cursor.into();

    let matching: Vec<&EngineRecord> = match cursor {
        RecordCursor::After(after_seq) => store
            .state
            .transcript
            .iter()
            .filter(|r| r.sequence > after_seq)
            .collect(),
        RecordCursor::Before(Some(before_seq)) => store
            .state
            .transcript
            .iter()
            .filter(|r| r.sequence < before_seq)
            .collect(),
        RecordCursor::Before(None) => store.state.transcript.iter().collect(),
    };

    let has_more = matching.len() > effective_limit;
    let page_records: Vec<EngineRecord> = matching
        .into_iter()
        .take(effective_limit)
        .cloned()
        .collect();

    let next_after_sequence = match cursor {
        RecordCursor::After(after_seq) => page_records
            .last()
            .map(|r| r.sequence)
            .unwrap_or(after_seq),
        RecordCursor::Before(_) => page_records.last().map(|r| r.sequence).unwrap_or(0),
    };

    Ok(RecordPage {
        records: page_records,
        next_after_sequence,
        has_more,
    })
}

/// Advance delivery watermark up to sequence for delivery outbox compaction while keeping
/// immutable transcript records intact.
pub(crate) fn acknowledge_records(store: &mut Store, sequence: u64) -> Result<(), EngineError> {
    if sequence >= store.state.next_sequence {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Acknowledgement sequence exceeds latest record sequence.",
        ));
    }
    // Watermark only advances monotonically; idempotent if already acknowledged.
    if sequence <= store.state.delivery_watermark {
        return Ok(());
    }

    let mut state = store.state.clone();
    state.delivery_watermark = sequence;
    state.outbox.retain(|record| record.sequence > sequence);
    state.revision = state.revision.checked_add(1).ok_or_else(|| {
        EngineError::new(EngineErrorCode::CorruptState, "Revision overflow.")
    })?;
    store.commit(state)?;
    Ok(())
}

/// Append an EngineRecord to the store's outbox and durable transcript, enforcing outbox_bytes.
pub(crate) fn append_record(
    store: &mut Store,
    mut record: EngineRecord,
) -> Result<EngineRecord, EngineError> {
    if record.sequence == 0 {
        record.sequence = store.state.next_sequence;
    } else if record.sequence < store.state.next_sequence {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Record sequence must be strictly increasing.",
        ));
    }
    let next_seq = record.sequence.checked_add(1).ok_or_else(|| {
        EngineError::new(EngineErrorCode::CorruptState, "Sequence overflow.")
    })?;

    // Check outbox byte budget
    let record_bytes = serde_json::to_vec(&record).map_err(|_| {
        EngineError::new(EngineErrorCode::CorruptState, "Cannot serialize record.")
    })?;
    let mut current_outbox_bytes: usize = 0;
    for item in &store.state.outbox {
        let b = serde_json::to_vec(item).map_err(|_| {
            EngineError::new(EngineErrorCode::CorruptState, "Cannot serialize outbox record.")
        })?;
        current_outbox_bytes = current_outbox_bytes.saturating_add(b.len());
    }
    if current_outbox_bytes.saturating_add(record_bytes.len()) > store.limits.outbox_bytes {
        return Err(EngineError::new(
            EngineErrorCode::QueueFull,
            "Delivery outbox byte limit exceeded.",
        ));
    }

    // Persist immutable RecordNode to engine-objects
    let node = serde_json::json!({
        "version": 1,
        "sequence": record.sequence,
        "record": &record,
    });
    let node_bytes = serde_json::to_vec(&node).map_err(|_| {
        EngineError::new(EngineErrorCode::CorruptState, "Cannot serialize record node.")
    })?;
    let node_obj = store::object_name(&node_bytes);
    store
        .objects_dir()
        .immutable(&node_obj, &node_bytes)
        .map_err(store::storage)?;

    let mut state = store.state.clone();
    state.next_sequence = next_seq;
    state.revision = state.revision.checked_add(1).ok_or_else(|| {
        EngineError::new(EngineErrorCode::CorruptState, "Revision overflow.")
    })?;
    record.revision = state.revision;
    state.outbox.push(record.clone());
    state.transcript.push(record.clone());
    store.commit(state)?;
    Ok(record)
}

/// Paginate HistoryRow records before ordinal, enforcing limits.history_page_rows (<=100)
/// and limits.history_page_bytes (<=256 KiB).
pub(crate) fn history(
    store: &Store,
    request: HistoryPageRequest,
) -> Result<HistoryPage, EngineError> {
    if request.conversation.root != store.state.root {
        return Err(EngineError::new(
            EngineErrorCode::NotOwned,
            "Conversation root mismatch.",
        ));
    }
    if request.limit == 0 || request.limit > 100 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "History page limit must be 1..=100.",
        ));
    }
    if request.max_bytes == 0 || request.max_bytes > 256 * 1024 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "History page max_bytes must be 1..=262144.",
        ));
    }

    let effective_limit = request.limit.min(store.limits.history_page_rows);
    let effective_max_bytes = request.max_bytes.min(store.limits.history_page_bytes);

    // Filter matching conversation and before_ordinal
    let mut candidates: Vec<&HistoryRow> = store
        .state
        .history_rows
        .iter()
        .filter(|row| {
            row.id.conversation == request.conversation
                && request
                    .before_ordinal
                    .is_none_or(|before| row.id.ordinal < before)
        })
        .collect();

    // Sort descending by ordinal (most recent rows before ordinal first)
    candidates.sort_by_key(|b| std::cmp::Reverse(b.id.ordinal));

    let mut counter = ByteCounter(0);
    serde_json::to_writer(
        &mut counter,
        &HistoryPage {
            rows: Vec::new(),
            next_before_ordinal: Some(u64::MAX),
        },
    )
    .map_err(|_| store::corrupt())?;
    let mut bytes = counter.0;
    if bytes > effective_max_bytes {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "History page envelope exceeds byte limit.",
        ));
    }

    let mut rows = Vec::new();
    let mut more = false;
    let mut remaining_iter = candidates.into_iter();

    while let Some(row) = remaining_iter.next() {
        let mut row_counter = ByteCounter(0);
        serde_json::to_writer(&mut row_counter, row).map_err(|_| store::corrupt())?;
        let row_bytes = row_counter.0 + usize::from(!rows.is_empty());

        if bytes.saturating_add(row_bytes) > effective_max_bytes {
            if rows.is_empty() {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "History row exceeds page byte limit.",
                ));
            }
            more = true;
            break;
        }

        bytes += row_bytes;
        rows.push(row.clone());
        if rows.len() == effective_limit {
            more = remaining_iter.next().is_some();
            break;
        }
    }

    let next_before_ordinal = if more {
        rows.last().map(|r| r.id.ordinal)
    } else {
        None
    };

    Ok(HistoryPage {
        rows,
        next_before_ordinal,
    })
}

/// Append a HistoryRow record into the store, verifying content digest and non-zero ordinal.
pub(crate) fn append_history_row(store: &mut Store, row: HistoryRow) -> Result<(), EngineError> {
    if row.id.ordinal == 0 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "History row ordinal must be greater than zero.",
        ));
    }
    if row.id.conversation.root != store.state.root {
        return Err(EngineError::new(
            EngineErrorCode::NotOwned,
            "History row conversation root mismatch.",
        ));
    }

    // Verify content digest matches
    match &row.content {
        crate::rich::ContentRef::Inline(s) => {
            if store::digest(s.as_bytes()) != row.content_digest {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "History row content digest mismatch.",
                ));
            }
        }
        crate::rich::ContentRef::Asset(asset) => {
            if asset.digest != row.content_digest {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "History row asset digest mismatch.",
                ));
            }
        }
    }

    let mut state = store.state.clone();
    state.history_rows.push(row);
    store.commit(state)?;
    Ok(())
}

/// Extract exact UTF-8 text excerpt from the cited HistoryRow, verifying digest and UTF-8 byte boundaries.
pub(crate) fn resolve_citation(
    store: &Store,
    citation: Citation,
) -> Result<HistoryExcerpt, EngineError> {
    if citation.utf8_start > citation.utf8_end || citation.row.ordinal == 0 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Invalid citation range or ordinal.",
        ));
    }

    let row = store
        .state
        .history_rows
        .iter()
        .find(|r| r.id == citation.row)
        .ok_or_else(|| {
            EngineError::new(EngineErrorCode::NotFound, "Cited history row not found.")
        })?;

    if row.content_digest != citation.content_digest {
        return Err(EngineError::new(
            EngineErrorCode::StaleRevision,
            "Citation content digest mismatch.",
        ));
    }

    let full_text = match &row.content {
        crate::rich::ContentRef::Inline(text) => {
            if store::digest(text.as_bytes()) != row.content_digest {
                return Err(EngineError::new(
                    EngineErrorCode::CorruptState,
                    "Inline row content digest corruption.",
                ));
            }
            text.clone()
        }
        crate::rich::ContentRef::Asset(asset) => {
            let bytes = read_asset_from_store(store, asset, 256 * 1024)?;
            String::from_utf8(bytes).map_err(|_| {
                EngineError::new(
                    EngineErrorCode::CorruptState,
                    "Cited asset content is not valid UTF-8.",
                )
            })?
        }
    };

    let start = citation.utf8_start as usize;
    let end = citation.utf8_end as usize;

    if end > full_text.len() {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Citation range exceeds content length.",
        ));
    }

    if !full_text.is_char_boundary(start) || !full_text.is_char_boundary(end) {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Citation offset does not fall on a valid UTF-8 character boundary.",
        ));
    }

    let text = full_text[start..end].to_string();

    Ok(HistoryExcerpt {
        citation,
        text,
        truncated: row.truncated,
    })
}

/// Lazily resolve stored asset JSON into AgentEvent.
pub fn read_agent_record(
    path: &Path,
    payload: &AgentRecordPayload,
    max_bytes: usize,
) -> Result<crate::AgentEvent, EngineError> {
    if max_bytes == 0 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Max bytes must be greater than zero.",
        ));
    }
    match payload {
        AgentRecordPayload::Inline(boxed_event) => {
            let bytes = serde_json::to_vec(boxed_event).map_err(|_| store::corrupt())?;
            if bytes.len() > max_bytes {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Inline agent record exceeds max bytes.",
                ));
            }
            Ok((**boxed_event).clone())
        }
        AgentRecordPayload::Stored(asset) => {
            if asset.mime_type != "application/x-flint-agent-event+json"
                && asset.mime_type != "application/json"
            {
                return Err(EngineError::new(
                    EngineErrorCode::InvalidCommand,
                    "Invalid MIME type for stored agent record.",
                ));
            }
            let bytes = read_asset(path, asset, max_bytes)?;
            serde_json::from_slice::<crate::AgentEvent>(&bytes).map_err(|_| {
                EngineError::new(
                    EngineErrorCode::CorruptState,
                    "Stored agent record payload is not valid AgentEvent JSON.",
                )
            })
        }
    }
}

/// Read bounded asset bytes from session directory or engine-objects, validating digest and size.
pub fn read_asset(
    path: &Path,
    asset: &crate::rich::AssetRef,
    max_bytes: usize,
) -> Result<Vec<u8>, EngineError> {
    if max_bytes == 0 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Max bytes must be greater than zero.",
        ));
    }
    if asset.bytes as usize > max_bytes {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Asset byte size exceeds maximum allowed bytes.",
        ));
    }
    let id_str = asset.id.as_str();
    if !store::valid_object(id_str) {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Invalid asset object ID.",
        ));
    }

    let candidate1 = path.join("engine-objects").join(id_str);
    let candidate2 = path.join(id_str);
    let target_path = if candidate1.exists() {
        candidate1
    } else if candidate2.exists() {
        candidate2
    } else {
        return Err(EngineError::new(
            EngineErrorCode::NotFound,
            "Asset object file not found.",
        ));
    };

    let bytes = std::fs::read(&target_path).map_err(store::storage)?;
    if bytes.len() as u64 != asset.bytes {
        return Err(EngineError::new(
            EngineErrorCode::CorruptState,
            "Asset file length does not match metadata.",
        ));
    }
    if store::digest(&bytes) != asset.digest {
        return Err(EngineError::new(
            EngineErrorCode::CorruptState,
            "Asset file digest mismatch.",
        ));
    }
    Ok(bytes)
}

fn read_asset_from_store(
    store: &Store,
    asset: &crate::rich::AssetRef,
    max_bytes: usize,
) -> Result<Vec<u8>, EngineError> {
    if max_bytes == 0 {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Max bytes must be greater than zero.",
        ));
    }
    if asset.bytes as usize > max_bytes {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Asset byte size exceeds maximum allowed bytes.",
        ));
    }
    let id_str = asset.id.as_str();
    if !store::valid_object(id_str) {
        return Err(EngineError::new(
            EngineErrorCode::InvalidCommand,
            "Invalid asset object ID.",
        ));
    }

    let bytes = store
        .objects_dir()
        .read(id_str, max_bytes)
        .map_err(|_| EngineError::new(EngineErrorCode::NotFound, "Asset object file not found."))?;

    if bytes.len() as u64 != asset.bytes {
        return Err(EngineError::new(
            EngineErrorCode::CorruptState,
            "Asset file length does not match metadata.",
        ));
    }
    if store::digest(&bytes) != asset.digest {
        return Err(EngineError::new(
            EngineErrorCode::CorruptState,
            "Asset file digest mismatch.",
        ));
    }
    Ok(bytes)
}

/// Write asset bytes into engine-objects, returning a validated AssetRef.
pub fn write_asset(
    path: &Path,
    mime_type: &str,
    bytes: &[u8],
) -> Result<crate::rich::AssetRef, EngineError> {
    let digest = store::digest(bytes);
    let id_str = digest.to_hex();
    let objects_dir = path.join("engine-objects");
    std::fs::create_dir_all(&objects_dir).map_err(store::storage)?;
    let target = objects_dir.join(&id_str);
    if !target.exists() {
        std::fs::write(&target, bytes).map_err(store::storage)?;
    }
    Ok(crate::rich::AssetRef {
        id: crate::rich::AssetId::new(id_str),
        mime_type: mime_type.to_string(),
        bytes: bytes.len() as u64,
        digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::store::digest;
    use crate::orchestration::{
        CommandId, EngineErrorCode, OrchestrationOptions, ProfileRef, PromptDisplay,
        ProviderTarget, RowRole, SessionId,
    };
    use crate::rich::ContentRef;
    use crate::AgentEvent;

    fn test_conversation() -> ConversationId {
        ConversationId {
            root: SessionId::parse("test-root").unwrap(),
            child: None,
        }
    }

    fn test_options() -> OrchestrationOptions {
        OrchestrationOptions {
            root_id: SessionId::parse("test-root").unwrap(),
            target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "profile-1".into(),
                    revision: 1,
                },
                model: "model-1".into(),
                effort: None,
            },
            limits: EngineLimits::default(),
        }
    }

    fn open_store(dir: &tempfile::TempDir) -> Store {
        Store::open(dir.path(), test_options()).unwrap()
    }

    fn sample_record(sequence: u64, text: &str) -> EngineRecord {
        EngineRecord {
            sequence,
            revision: 1,
            conversation: test_conversation(),
            row: None,
            body: EngineRecordBody::UserStarted {
                command_id: CommandId::fresh(),
                prompt: PromptDisplay {
                    request: text.into(),
                    image_names: Vec::new(),
                    citations: Vec::new(),
                },
            },
        }
    }

    fn sample_history_row(ordinal: u64, text: &str) -> HistoryRow {
        let content_digest = digest(text.as_bytes());
        HistoryRow {
            id: RowId {
                conversation: test_conversation(),
                ordinal,
            },
            turn_id: 1,
            role: RowRole::User,
            source_target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "profile-1".into(),
                    revision: 1,
                },
                model: "model-1".into(),
                effort: None,
            },
            content: ContentRef::Inline(text.into()),
            content_digest,
            citations: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn record_pagination_strictly_increasing_and_capped_by_limits() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir);

        for i in 1..=10 {
            append_record(&mut store, sample_record(i, &format!("record {i}"))).unwrap();
        }

        // Forward pagination: limit 3
        let page1 = records(&store, 0, 3).unwrap();
        assert_eq!(page1.records.len(), 3);
        assert_eq!(page1.records[0].sequence, 1);
        assert_eq!(page1.records[1].sequence, 2);
        assert_eq!(page1.records[2].sequence, 3);
        assert_eq!(page1.next_after_sequence, 3);
        assert!(page1.has_more);

        // Page 2
        let page2 = records(&store, page1.next_after_sequence, 3).unwrap();
        assert_eq!(page2.records.len(), 3);
        assert_eq!(page2.records[0].sequence, 4);
        assert_eq!(page2.records[1].sequence, 5);
        assert_eq!(page2.records[2].sequence, 6);
        assert_eq!(page2.next_after_sequence, 6);
        assert!(page2.has_more);

        // Cap by record_delivery
        store.limits.record_delivery = 4;
        let capped_page = records(&store, 0, 100).unwrap();
        assert_eq!(capped_page.records.len(), 4);
        assert_eq!(capped_page.next_after_sequence, 4);
        assert!(capped_page.has_more);

        // Limit == 0 errors
        assert_eq!(
            records(&store, 0, 0).unwrap_err().code,
            EngineErrorCode::InvalidCommand
        );

        // No matching records
        let empty_page = records(&store, 10, 5).unwrap();
        assert!(empty_page.records.is_empty());
        assert_eq!(empty_page.next_after_sequence, 10);
        assert!(!empty_page.has_more);
    }

    #[test]
    fn record_pagination_with_before_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir);

        for i in 1..=5 {
            append_record(&mut store, sample_record(i, &format!("record {i}"))).unwrap();
        }

        // Before sequence 4: records 1, 2, 3
        let page = records_before(&store, Some(4), 10).unwrap();
        assert_eq!(page.records.len(), 3);
        assert_eq!(page.records[0].sequence, 1);
        assert_eq!(page.records[1].sequence, 2);
        assert_eq!(page.records[2].sequence, 3);
        assert_eq!(page.next_after_sequence, 3);
        assert!(!page.has_more);

        // Before None: all 5 records
        let all_page = records_before(&store, None, 10).unwrap();
        assert_eq!(all_page.records.len(), 5);
        assert_eq!(all_page.next_after_sequence, 5);
        assert!(!all_page.has_more);
    }

    #[test]
    fn outbox_acknowledgement_advances_watermark_and_compacts_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir);

        for i in 1..=5 {
            append_record(&mut store, sample_record(i, &format!("record {i}"))).unwrap();
        }

        assert_eq!(store.state.outbox.len(), 5);
        assert_eq!(store.state.transcript.len(), 5);
        assert_eq!(store.state.delivery_watermark, 0);

        // Acknowledge up to sequence 3
        acknowledge_records(&mut store, 3).unwrap();
        assert_eq!(store.state.delivery_watermark, 3);
        assert_eq!(store.state.outbox.len(), 2);
        assert_eq!(store.state.outbox[0].sequence, 4);
        assert_eq!(store.state.outbox[1].sequence, 5);
        // Transcript remains fully intact!
        assert_eq!(store.state.transcript.len(), 5);

        // Idempotent duplicate or earlier acknowledgement
        acknowledge_records(&mut store, 2).unwrap();
        assert_eq!(store.state.delivery_watermark, 3);

        // Acknowledge exceeds latest record sequence fails
        assert_eq!(
            acknowledge_records(&mut store, 99).unwrap_err().code,
            EngineErrorCode::InvalidCommand
        );

        // Reopen store from disk and verify persistence of compacted outbox and intact transcript
        drop(store);
        let reopened = open_store(&dir);
        assert_eq!(reopened.state.delivery_watermark, 3);
        assert_eq!(reopened.state.outbox.len(), 2);
        assert_eq!(reopened.state.transcript.len(), 5);
    }

    #[test]
    fn outbox_byte_limit_enforcement_and_compaction_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir);

        let rec1 = sample_record(1, "record 1");
        let rec1_bytes = serde_json::to_vec(&rec1).unwrap().len();

        // Set outbox byte limit to just fit 2 records
        store.limits.outbox_bytes = rec1_bytes * 2 + 50;

        append_record(&mut store, rec1).unwrap();
        append_record(&mut store, sample_record(2, "record 2")).unwrap();

        // 3rd record exceeds outbox byte limit
        let err = append_record(&mut store, sample_record(3, "record 3")).unwrap_err();
        assert_eq!(err.code, EngineErrorCode::QueueFull);

        // Acknowledge first record, compacting outbox
        acknowledge_records(&mut store, 1).unwrap();

        // Now appending 3rd record succeeds!
        assert!(append_record(&mut store, sample_record(3, "record 3")).is_ok());
    }

    #[test]
    fn history_pagination_enforces_rows_and_byte_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir);

        for ord in 1..=10 {
            append_history_row(
                &mut store,
                sample_history_row(ord, &format!("history line {ord}: {}", "z".repeat(100))),
            )
            .unwrap();
        }

        // Query page: limit 3
        let page1 = history(
            &store,
            HistoryPageRequest {
                conversation: test_conversation(),
                before_ordinal: None,
                limit: 3,
                max_bytes: 256 * 1024,
            },
        )
        .unwrap();

        // Reverse ordinal order (10, 9, 8)
        assert_eq!(page1.rows.len(), 3);
        assert_eq!(page1.rows[0].id.ordinal, 10);
        assert_eq!(page1.rows[1].id.ordinal, 9);
        assert_eq!(page1.rows[2].id.ordinal, 8);
        assert_eq!(page1.next_before_ordinal, Some(8));

        // Query page 2 before ordinal 8
        let page2 = history(
            &store,
            HistoryPageRequest {
                conversation: test_conversation(),
                before_ordinal: page1.next_before_ordinal,
                limit: 3,
                max_bytes: 256 * 1024,
            },
        )
        .unwrap();
        assert_eq!(page2.rows.len(), 3);
        assert_eq!(page2.rows[0].id.ordinal, 7);
        assert_eq!(page2.rows[1].id.ordinal, 6);
        assert_eq!(page2.rows[2].id.ordinal, 5);
        assert_eq!(page2.next_before_ordinal, Some(5));

        // Byte bounding stops before overflowing max_bytes
        let single_row_bytes = serde_json::to_vec(&page1.rows[0]).unwrap().len();
        let tight_bytes = single_row_bytes + 100; // room for 1 row + envelope
        let tight_page = history(
            &store,
            HistoryPageRequest {
                conversation: test_conversation(),
                before_ordinal: None,
                limit: 10,
                max_bytes: tight_bytes,
            },
        )
        .unwrap();
        assert_eq!(tight_page.rows.len(), 1);
        assert_eq!(tight_page.next_before_ordinal, Some(10));

        // Reject limit 0 or > 100
        assert_eq!(
            history(
                &store,
                HistoryPageRequest {
                    conversation: test_conversation(),
                    before_ordinal: None,
                    limit: 0,
                    max_bytes: 256 * 1024,
                }
            )
            .unwrap_err()
            .code,
            EngineErrorCode::InvalidCommand
        );
        assert_eq!(
            history(
                &store,
                HistoryPageRequest {
                    conversation: test_conversation(),
                    before_ordinal: None,
                    limit: 101,
                    max_bytes: 256 * 1024,
                }
            )
            .unwrap_err()
            .code,
            EngineErrorCode::InvalidCommand
        );

        // Reject conversation mismatch
        let mut bad_conv = test_conversation();
        bad_conv.root = SessionId::parse("unrelated-root").unwrap();
        assert_eq!(
            history(
                &store,
                HistoryPageRequest {
                    conversation: bad_conv,
                    before_ordinal: None,
                    limit: 10,
                    max_bytes: 256 * 1024,
                }
            )
            .unwrap_err()
            .code,
            EngineErrorCode::NotOwned
        );
    }

    #[test]
    fn resolve_citation_extracts_exact_utf8_excerpt_and_verifies_digest() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir);

        let text = "Hello, world! 🦀 Rust concurrency and engine records.";
        let row = sample_history_row(1, text);
        let content_digest = row.content_digest.clone();
        append_history_row(&mut store, row).unwrap();

        // Exact citation of "Rust concurrency"
        let needle = "Rust concurrency";
        let start = text.find(needle).unwrap() as u64;
        let end = start + needle.len() as u64;

        let citation = Citation {
            row: RowId {
                conversation: test_conversation(),
                ordinal: 1,
            },
            utf8_start: start,
            utf8_end: end,
            content_digest: content_digest.clone(),
        };

        let excerpt = resolve_citation(&store, citation.clone()).unwrap();
        assert_eq!(excerpt.text, "Rust concurrency");
        assert!(!excerpt.truncated);

        // Digest mismatch fails with StaleRevision
        let mut bad_digest_cit = citation.clone();
        bad_digest_cit.content_digest = Digest([0xab; 32]);
        assert_eq!(
            resolve_citation(&store, bad_digest_cit).unwrap_err().code,
            EngineErrorCode::StaleRevision
        );

        // Non-existent row fails with NotFound
        let mut missing_row_cit = citation.clone();
        missing_row_cit.row.ordinal = 999;
        assert_eq!(
            resolve_citation(&store, missing_row_cit).unwrap_err().code,
            EngineErrorCode::NotFound
        );

        // UTF-8 boundary validation: split 🦀 (4 bytes)
        let crab_start = text.find('🦀').unwrap() as u64;
        let split_char_cit = Citation {
            row: RowId {
                conversation: test_conversation(),
                ordinal: 1,
            },
            utf8_start: crab_start + 1,
            utf8_end: crab_start + 2,
            content_digest,
        };
        assert_eq!(
            resolve_citation(&store, split_char_cit).unwrap_err().code,
            EngineErrorCode::InvalidCommand
        );
    }

    #[test]
    fn resolve_citation_with_asset_backed_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir);

        let text = "Asset backed history content for citation resolution.";
        let bytes = text.as_bytes();
        let asset_ref = write_asset(dir.path(), "text/plain; charset=utf-8", bytes).unwrap();

        let row = HistoryRow {
            id: RowId {
                conversation: test_conversation(),
                ordinal: 1,
            },
            turn_id: 1,
            role: RowRole::Assistant,
            source_target: ProviderTarget::Native {
                profile: ProfileRef {
                    id: "profile-1".into(),
                    revision: 1,
                },
                model: "model-1".into(),
                effort: None,
            },
            content: ContentRef::Asset(asset_ref.clone()),
            content_digest: asset_ref.digest.clone(),
            citations: Vec::new(),
            truncated: false,
        };
        append_history_row(&mut store, row).unwrap();

        let citation = Citation {
            row: RowId {
                conversation: test_conversation(),
                ordinal: 1,
            },
            utf8_start: 0,
            utf8_end: 12,
            content_digest: asset_ref.digest,
        };

        let excerpt = resolve_citation(&store, citation).unwrap();
        assert_eq!(excerpt.text, "Asset backed");
    }

    #[test]
    fn read_agent_record_resolves_inline_and_stored_assets() {
        let dir = tempfile::tempdir().unwrap();

        // 1. Inline payload
        let event = AgentEvent::TurnStarted {
            turn_id: 42,
        };
        let inline_payload = AgentRecordPayload::Inline(Box::new(event.clone()));
        let resolved_inline = read_agent_record(dir.path(), &inline_payload, 1024).unwrap();
        assert_eq!(resolved_inline, event);

        // Inline oversized fails
        assert_eq!(
            read_agent_record(dir.path(), &inline_payload, 5).unwrap_err().code,
            EngineErrorCode::InvalidCommand
        );

        // 2. Stored payload
        let event_json = serde_json::to_vec(&event).unwrap();
        let asset_ref = write_asset(
            dir.path(),
            "application/x-flint-agent-event+json",
            &event_json,
        )
        .unwrap();

        let stored_payload = AgentRecordPayload::Stored(asset_ref.clone());
        let resolved_stored = read_agent_record(dir.path(), &stored_payload, 1024).unwrap();
        assert_eq!(resolved_stored, event);

        // Max bytes == 0 rejected
        assert_eq!(
            read_agent_record(dir.path(), &stored_payload, 0).unwrap_err().code,
            EngineErrorCode::InvalidCommand
        );

        // Wrong MIME rejected
        let mut wrong_mime_asset = asset_ref.clone();
        wrong_mime_asset.mime_type = "image/png".into();
        assert_eq!(
            read_agent_record(
                dir.path(),
                &AgentRecordPayload::Stored(wrong_mime_asset),
                1024
            )
            .unwrap_err()
            .code,
            EngineErrorCode::InvalidCommand
        );

        // Corrupt digest rejected
        let mut bad_digest_asset = asset_ref;
        bad_digest_asset.digest = Digest([0xff; 32]);
        assert_eq!(
            read_agent_record(
                dir.path(),
                &AgentRecordPayload::Stored(bad_digest_asset),
                1024
            )
            .unwrap_err()
            .code,
            EngineErrorCode::CorruptState
        );
    }
}
