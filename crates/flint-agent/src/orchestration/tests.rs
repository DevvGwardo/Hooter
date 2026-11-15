use super::store::Store;
use super::*;
use tempfile::TempDir;

fn options() -> OrchestrationOptions {
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
fn conversation() -> ConversationId {
    ConversationId {
        root: options().root_id,
        child: None,
    }
}
fn command(text: &str) -> EngineCommand {
    EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::Send {
            prompt: PromptSnapshot {
                request: text.into(),
                effective_text: format!("{text}\n[captured context]"),
                images: Vec::new(),
                citations: Vec::new(),
            },
            delivery: Delivery::NextTurn,
        },
    }
}
fn open(dir: &TempDir) -> Store {
    Store::open(dir.path(), options()).unwrap()
}

// Fixed version-1 bytes from the original sorted encoder, not regenerated
// from the encoder under test. Struct declaration order is deliberately not
// lexicographic, including nested objects in image/citation arrays.
const GOLDEN_V1: &[u8] = br#"{"command":{"conversation":{"child":null,"root":"saved-root"},"id":"01234567-89ab-4cde-8fab-0123456789ab","payload":{"Send":{"delivery":"NextTurn","prompt":{"citations":[{"content_digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7],"row":{"conversation":{"child":null,"root":"saved-root"},"ordinal":7},"utf8_end":6,"utf8_start":0}],"effective_text":"frozen\ncontext","images":[{"data":"AQID","mime_type":"image/png","name":"figure.png"}],"request":"golden request"}}}},"version":1}"#;
const GOLDEN_V1_SHA256: &str = "c6719e938d6bc0a15ee9986328dab0f5beed30745166c5a6f704c5d28066dfb7";

fn golden_command() -> EngineCommand {
    EngineCommand {
        id: CommandId::parse("01234567-89ab-4cde-8fab-0123456789ab").unwrap(),
        conversation: conversation(),
        payload: CommandPayload::Send {
            prompt: PromptSnapshot {
                request: "golden request".into(),
                effective_text: "frozen\ncontext".into(),
                images: vec![crate::ImageAttachment {
                    name: "figure.png".into(),
                    mime_type: "image/png".into(),
                    data: "AQID".into(),
                }],
                citations: vec![Citation {
                    row: RowId {
                        conversation: conversation(),
                        ordinal: 7,
                    },
                    utf8_start: 0,
                    utf8_end: 6,
                    content_digest: Digest([7; 32]),
                }],
            },
            delivery: Delivery::NextTurn,
        },
    }
}

#[test]
fn canonical_v1_nested_objects_match_existing_golden_bytes_and_digest() {
    let command = golden_command();
    // Demonstrate that the serialized declaration order is not the canonical
    // order, independently of which serde_json map implementation is enabled.
    assert!(
        serde_json::to_vec(&command)
            .unwrap()
            .starts_with(br#"{"id":"#)
    );
    let (bytes, digest) = store::canonical_command(&command).unwrap();
    assert_eq!(bytes, GOLDEN_V1);
    assert_eq!(bytes.len(), 503);
    let hex: String = digest.0.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(hex, GOLDEN_V1_SHA256);
    assert_eq!(store::digest(GOLDEN_V1), digest);
}

#[test]
fn same_id_retry_and_reopen_reuse_existing_v1_golden_object() {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let objects = fs::Directory::open(&dir.path().join("engine-objects")).unwrap();
    // Seed known pre-repair version-1 bytes. Acceptance must reuse that exact
    // object, never rewrite or migrate its encoding/digest.
    objects.immutable(GOLDEN_V1_SHA256, GOLDEN_V1).unwrap();
    let path = dir.path().join("engine-objects").join(GOLDEN_V1_SHA256);
    #[cfg(unix)]
    let inode = std::fs::metadata(&path).unwrap().ino();
    let command = golden_command();
    let first = store.submit(command.clone()).unwrap();
    assert_eq!(first.payload_digest, store::digest(GOLDEN_V1));
    assert_eq!(first.accepted_sequence, 1);
    let checkpoint = std::fs::read(dir.path().join("history.json")).unwrap();
    assert_eq!(store.submit(command.clone()).unwrap(), first);
    assert_eq!(
        std::fs::read(dir.path().join("history.json")).unwrap(),
        checkpoint
    );
    drop(store);
    let mut store = open(&dir);
    let recovered = store.receipt(&command.id).unwrap().unwrap();
    assert!(matches!(
        recovered.state,
        CommandState::Held(HoldReason::Restart { .. })
    ));
    assert_eq!(recovered.id, first.id);
    assert_eq!(recovered.conversation, first.conversation);
    assert_eq!(recovered.payload_digest, first.payload_digest);
    assert_eq!(recovered.accepted_sequence, first.accepted_sequence);
    assert_eq!(recovered.accepted_revision, first.accepted_revision);
    let checkpoint = std::fs::read(dir.path().join("history.json")).unwrap();
    assert_eq!(store.submit(command).unwrap(), recovered);
    assert_eq!(store.state.receipt_count, 1);
    assert_eq!(
        std::fs::read(dir.path().join("history.json")).unwrap(),
        checkpoint
    );
    assert_eq!(std::fs::read(&path).unwrap(), GOLDEN_V1);
    #[cfg(unix)]
    assert_eq!(std::fs::metadata(path).unwrap().ino(), inode);
}

#[test]
fn identical_retry_retains_digest_sequence_and_frozen_payload() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let command = command("authoritative request");
    let first = store.submit(command.clone()).unwrap();
    let bytes = std::fs::read(dir.path().join("history.json")).unwrap();
    assert_eq!(store.submit(command.clone()).unwrap(), first);
    assert_eq!(
        std::fs::read(dir.path().join("history.json")).unwrap(),
        bytes
    );
    assert_eq!(
        store.queued_prompt(&command.id).unwrap().request,
        "authoritative request"
    );
    assert_eq!(store.state.receipt_count, 1);
}

#[test]
fn identities_are_validated_during_construction_and_deserialization() {
    for value in [
        "",
        ".",
        "..",
        "../escape",
        "space name",
        "/root",
        "a/b",
        "a\nb",
    ] {
        assert!(SessionId::parse(value).is_err());
        assert!(serde_json::from_str::<SessionId>(&serde_json::to_string(value).unwrap()).is_err());
    }
    for value in ["agent-0", "agent-01", "agent--1", "agent-1/escape"] {
        assert!(ChildId::parse(value).is_err());
    }
    assert!(ChildId::parse("agent-1").is_ok());
    assert!(CommandId::parse("00000000-0000-0000-0000-000000000000").is_err());
    assert!(CommandId::parse("00000000-0000-4000-0000-000000000000").is_err());
    let id = CommandId::fresh();
    assert_eq!(CommandId::parse(id.as_str()).unwrap(), id);
    assert!(CommandId::parse(&id.as_str().to_uppercase()).is_err());
}

#[test]
fn conflicting_payload_or_conversation_cannot_rewrite_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let original = command("original");
    let receipt = store.submit(original.clone()).unwrap();
    let snapshot = std::fs::read(dir.path().join("history.json")).unwrap();
    let mut different = command("different");
    different.id = original.id.clone();
    assert_eq!(
        store.submit(different).unwrap_err().code,
        EngineErrorCode::IdentityConflict
    );
    let mut different = original.clone();
    different.payload = CommandPayload::PauseQueue {
        expected_queue_revision: 1,
    };
    assert_eq!(
        store.submit(different).unwrap_err().code,
        EngineErrorCode::IdentityConflict
    );
    let mut different = original;
    different.conversation.root = SessionId::parse("other-root").unwrap();
    assert_eq!(
        store.submit(different).unwrap_err().code,
        EngineErrorCode::IdentityConflict
    );
    assert_eq!(store.receipt(&receipt.id).unwrap(), Some(receipt));
    assert_eq!(
        std::fs::read(dir.path().join("history.json")).unwrap(),
        snapshot
    );
}

#[test]
fn fault_boundaries_never_report_acceptance_and_reopen_reconciles_rename() {
    for fault in [
        "object",
        "receipt",
        "snapshot",
        "before-rename",
        "after-rename",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        let command = command(fault);
        let before = std::fs::read(dir.path().join("history.json")).unwrap();
        store.fault = Some(fault);
        assert_eq!(
            store.submit(command.clone()).unwrap_err().code,
            EngineErrorCode::Storage
        );
        if matches!(fault, "before-rename" | "after-rename") {
            assert_eq!(
                store.receipt(&command.id).unwrap_err().code,
                EngineErrorCode::Storage
            );
            assert_eq!(
                store.queue(&conversation()).unwrap_err().code,
                EngineErrorCode::Storage
            );
            assert_eq!(
                store.summary(&conversation()).unwrap_err().code,
                EngineErrorCode::Storage
            );
            assert_eq!(
                store.recent(None, 1).unwrap_err().code,
                EngineErrorCode::Storage
            );
            assert_eq!(
                store.queued_prompt(&command.id).unwrap_err().code,
                EngineErrorCode::Storage
            );
        } else {
            assert!(store.receipt(&command.id).unwrap().is_none());
        }
        if fault != "after-rename" {
            assert_eq!(
                std::fs::read(dir.path().join("history.json")).unwrap(),
                before
            );
        }
        drop(store);
        let mut store = open(&dir);
        let recovered = store.receipt(&command.id).unwrap();
        assert_eq!(recovered.is_some(), fault == "after-rename");
        let receipt = store.submit(command.clone()).unwrap();
        assert_eq!(receipt.accepted_sequence, 1);
        assert_eq!(store.state.receipt_count, 1);
        assert_eq!(store.queued_prompt(&command.id).unwrap().request, fault);
    }
}

#[test]
fn rename_uncertainty_poison_prevents_later_acceptance_until_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store.fault = Some("after-rename");
    assert!(store.submit(command("committed but no receipt")).is_err());
    store.fault = None;
    assert!(
        store
            .submit(command("must not clobber the uncertain commit"))
            .is_err()
    );
    drop(store);
    assert_eq!(open(&dir).state.receipt_count, 1);
}

#[test]
fn reopen_holds_accepted_work_and_keeps_started_work_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let first = store.submit(command("queued")).unwrap();
    let second = store.submit(command("started fixture")).unwrap();
    let mut state = store.state.clone();
    state.pending[1].receipt.state = CommandState::Started {
        run_id: RunId::parse(CommandId::fresh().as_str()).unwrap(),
        turn_id: 9,
        safe_boundary_step: None,
    };
    store.commit(state).unwrap();
    let generation = store.state.generation;
    drop(store);
    let store = open(&dir);
    assert_eq!(store.state.generation, generation + 1);
    assert!(matches!(
        store.receipt(&first.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::Restart { .. })
    ));
    assert!(matches!(
        store.receipt(&second.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::AmbiguousEffects { .. })
    ));
    assert!(store.queue(&conversation()).unwrap().paused);
    assert!(!store.summary(&conversation()).unwrap().running);
    drop(store);
    assert!(matches!(
        open(&dir).receipt(&second.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::AmbiguousEffects { .. })
    ));
}

#[test]
fn writer_lease_excludes_second_store_and_stopped_controls() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let before = std::fs::read(dir.path().join("history.json")).unwrap();
    assert!(
        matches!(Store::open(dir.path(), options()), Err(error) if error.code == EngineErrorCode::Busy)
    );
    let review = crate::TaskReview {
        turn_id: 1,
        task_id: 1,
        contract_revision: 1,
        work_revision: 0,
        decision: crate::ReviewDecision::Accept,
    };
    assert!(
        crate::review_saved_task(dir.path(), dir.path(), &review)
            .unwrap_err()
            .contains("writer")
    );
    assert!(
        crate::edit_saved_task(
            dir.path(),
            1,
            crate::TaskEdit {
                task_id: 1,
                expected_revision: 1,
                objective: "must not run".into(),
                mode: crate::TaskMode::Work,
                requirements: vec![],
                restrictions: vec![],
                clear_reported_blockers: false,
            }
        )
        .unwrap_err()
        .contains("writer")
    );
    assert!(crate::persist::write_atomic(dir.path(), b"{}").is_err());
    assert_eq!(
        std::fs::read(dir.path().join("history.json")).unwrap(),
        before
    );
    drop(store);
    assert!(Store::open(dir.path(), options()).is_ok());
}

#[test]
fn lease_is_released_by_a_process_exit_not_a_pid_file() {
    // Spawn this exact test in a subprocess. The child obtains the real flock;
    // killing it releases the lease even though the lock inode remains.
    const FLAG: &str = "FLINT_B1_LEASE_FIXTURE";
    if let Some(path) = std::env::var_os(FLAG) {
        let _lease = fs::WriterLease::acquire(std::path::Path::new(&path)).unwrap();
        println!("LEASE_ACQUIRED");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "orchestration::tests::lease_is_released_by_a_process_exit_not_a_pid_file",
                "--nocapture",
            ])
            .env(FLAG, dir.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    use std::io::BufRead;
    let mut lines = std::io::BufReader::new(child.0.stdout.take().unwrap()).lines();
    assert!(lines.any(|line| line.unwrap().contains("LEASE_ACQUIRED")));
    assert!(fs::WriterLease::acquire(dir.path()).is_err());
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    assert!(fs::WriterLease::acquire(dir.path()).is_ok());
}

#[test]
fn legacy_versions_migrate_without_rewriting_messages_task_or_extensions() {
    for version in [1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let task = crate::TaskRecord::new(7, "protected request".into(), 4);
        let original = serde_json::json!({
            "version": version, "turn_id": 7, "messages": [{"user":"historical request"}],
            "task":task, "future_legacy_extension": {"keep":true}
        });
        std::fs::write(
            dir.path().join("history.json"),
            serde_json::to_vec(&original).unwrap(),
        )
        .unwrap();
        let store = open(&dir);
        assert_eq!(store.document["version"], 3);
        for field in ["turn_id", "messages", "task", "future_legacy_extension"] {
            assert_eq!(store.document[field], original[field]);
        }
        assert_eq!(crate::saved_task(dir.path()).unwrap().unwrap().0, 7);
    }
}

#[test]
fn unknown_corrupt_and_invalid_v3_histories_are_kept_in_place() {
    for bytes in [
        b"partial".as_slice(),
        br#"{"version":97,"turn_id":0,"messages":[]}"#,
        br#"{"version":3,"turn_id":0,"messages":[]}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("history.json"), bytes).unwrap();
        assert!(Store::open(dir.path(), options()).is_err());
        assert!(crate::persist::load(dir.path()).is_err());
        assert!(
            crate::persist::write_atomic(dir.path(), &crate::persist::snapshot(1, &[])).is_err()
        );
        assert_eq!(
            std::fs::read(dir.path().join("history.json")).unwrap(),
            bytes
        );
        assert!(!dir.path().join("history.json.corrupt").exists());
    }
}

#[test]
fn no_follow_refuses_history_lock_object_directory_and_session_symlinks() {
    #[cfg(windows)]
    fn create_junction(link: &std::path::Path, target: &std::path::Path) {
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed with {status}");
    }

    #[cfg(windows)]
    fn try_symlink(target: &std::path::Path, link: &std::path::Path, is_dir: bool) -> bool {
        let result = if is_dir {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        match result {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(1314) => false, // ERROR_PRIVILEGE_NOT_HELD
            Err(error) => panic!("unexpected symlink error: {error}"),
        }
    }

    #[cfg(unix)]
    fn try_symlink(target: &std::path::Path, link: &std::path::Path, _is_dir: bool) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    {
        // Unprivileged Windows users can always create NTFS directory junctions.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("keep");
        std::fs::write(&target, b"unrelated").unwrap();
        create_junction(&dir.path().join("engine-objects"), outside.path());
        assert!(Store::open(dir.path(), options()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unrelated");

        let parent = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        create_junction(&parent.path().join("alias"), target_dir.path());
        assert!(Store::open(&parent.path().join("alias"), options()).is_err());
        assert!(!target_dir.path().join("history.json").exists());
    }

    for name in ["history.json", ".flint-writer.lock", "engine-objects"] {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let is_dir = name == "engine-objects";
        let file_target = outside.path().join("keep");
        std::fs::write(&file_target, b"unrelated").unwrap();
        let target = if is_dir {
            outside.path()
        } else {
            file_target.as_path()
        };
        if try_symlink(target, &dir.path().join(name), is_dir) {
            assert!(Store::open(dir.path(), options()).is_err());
            assert_eq!(std::fs::read(&file_target).unwrap(), b"unrelated");
            assert!(
                std::fs::symlink_metadata(dir.path().join(name))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }
    let parent = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    if try_symlink(target.path(), &parent.path().join("alias"), true) {
        assert!(Store::open(&parent.path().join("alias"), options()).is_err());
        assert!(!target.path().join("history.json").exists());
    }
}

#[test]
fn private_files_and_unique_temporaries_preserve_unrelated_temp_paths() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("history.json.tmp"), b"unrelated temporary").unwrap();
    let mut store = open(&dir);
    store.submit(command("private")).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("history.json.tmp")).unwrap(),
        b"unrelated temporary"
    );
    #[cfg(unix)]
    for path in [
        dir.path().join("history.json"),
        dir.path().join(".flint-writer.lock"),
    ] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    for entry in std::fs::read_dir(dir.path().join("engine-objects")).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_name().to_string_lossy().ends_with(".tmp"));
        #[cfg(unix)]
        assert_eq!(
            entry.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn active_queue_count_bytes_previews_and_receipt_pages_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    for _ in 0..20 {
        store.submit(command(&"界".repeat(300))).unwrap();
    }
    assert_eq!(
        store.submit(command("full")).unwrap_err().code,
        EngineErrorCode::QueueFull
    );
    let queue = store.queue(&conversation()).unwrap();
    assert_eq!(queue.items.len(), 20);
    assert!(queue.items.iter().all(|item| item.preview.len() <= 512));
    for limit in [0, 101, usize::MAX] {
        assert!(store.recent(None, limit).is_err());
    }
    assert_eq!(store.recent(None, 3).unwrap().receipts.len(), 3);
    drop(store);
    let dir = tempfile::tempdir().unwrap();
    let mut bounded = options();
    bounded.limits.queued_bytes = 512;
    let mut store = Store::open(dir.path(), bounded).unwrap();
    assert_eq!(
        store.submit(command(&"x".repeat(1024))).unwrap_err().code,
        EngineErrorCode::QueueFull
    );
    assert_eq!(store.state.receipt_count, 0);
}

#[test]
fn historical_receipts_reopen_without_loading_historical_payloads_or_a_receipt_map() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let mut retained = Vec::new();
    for i in 0..128 {
        let receipt = store
            .submit(command(&format!("historical {i}: {}", "x".repeat(4096))))
            .unwrap();
        retained.push(receipt.id);
        // Simulate a later slice retiring active work without deleting identity
        // tombstones. B1 exposes no scheduler or retirement operation.
        let payload = store.state.pending[0].object.clone();
        let mut state = store.state.clone();
        state.pending.clear();
        store.commit(state).unwrap();
        // Missing historical payload must not be loaded just to reopen/query
        // receipts. The immutable receipt/digest tombstone still exists.
        std::fs::remove_file(dir.path().join("engine-objects").join(payload)).unwrap();
    }
    drop(store);
    let store = open(&dir);
    assert!(store.state.pending.is_empty());
    assert_eq!(store.state.receipt_count, 128);
    assert!(store.receipt(&retained[0]).unwrap().is_some());
    let mut before = None;
    let mut count = 0;
    loop {
        let page = store.recent(before, 7).unwrap();
        assert!(page.receipts.len() <= 7);
        count += page.receipts.len();
        before = page.next_before_sequence;
        if before.is_none() {
            break;
        }
    }
    assert_eq!(count, 128);
    eprintln!(
        "B1_HISTORY historical_receipts=128 active_pending={} page_limit=7",
        store.state.pending.len()
    );
}

#[test]
fn missing_or_changed_active_objects_fail_without_reset_or_relocation() {
    for missing in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        store.submit(command("active retained")).unwrap();
        let object = store.state.pending[0].object.clone();
        drop(store);
        let before = std::fs::read(dir.path().join("history.json")).unwrap();
        let object = dir.path().join("engine-objects").join(object);
        if missing {
            std::fs::remove_file(object).unwrap();
        } else {
            std::fs::write(object, "corrupt object").unwrap();
        }
        assert!(Store::open(dir.path(), options()).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("history.json")).unwrap(),
            before
        );
        assert!(!dir.path().join("history.json.corrupt").exists());
    }
}

#[test]
fn stopped_review_and_edit_roundtrip_every_v3_ledger_field() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut task = crate::TaskRecord::new(7, "Produce output.".into(), 4);
    task.assess(crate::TurnEndReason::Completed, true);
    crate::persist::write_atomic(
        dir.path(),
        &crate::persist::snapshot_with_task(
            7,
            &[crate::provider::Message::User("Produce output.".into())],
            Some(&task),
        ),
    )
    .unwrap();
    let mut store = open(&dir);
    let receipt = store
        .submit(command("queued independent instruction"))
        .unwrap();
    let mut state = store.state.clone();
    state.extra.insert(
        "future_intent_extension".into(),
        serde_json::json!({"keep":true}),
    );
    store.commit(state).unwrap();
    let before = store.document.clone();
    drop(store);
    crate::review_saved_task(
        dir.path(),
        workspace.path(),
        &crate::TaskReview {
            turn_id: 7,
            task_id: 7,
            contract_revision: 1,
            work_revision: 4,
            decision: crate::ReviewDecision::Accept,
        },
    )
    .unwrap();
    crate::edit_saved_task(
        dir.path(),
        7,
        crate::TaskEdit {
            task_id: 7,
            expected_revision: 1,
            objective: "Explicit revised goal".into(),
            mode: crate::TaskMode::ReadOnly,
            requirements: vec![crate::RequirementSpec::Review("Explain the result.".into())],
            restrictions: vec!["Do not publish".into()],
            clear_reported_blockers: false,
        },
    )
    .unwrap();
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("history.json")).unwrap()).unwrap();
    assert_eq!(after["version"], 3);
    assert_eq!(after["messages"], before["messages"]);
    for field in [
        "pending",
        "receipt_head",
        "receipt_count",
        "next_sequence",
        "generation",
        "future_intent_extension",
    ] {
        assert_eq!(
            after["orchestration"][field],
            before["orchestration"][field]
        );
    }
    assert_eq!(
        after["orchestration"]["revision"].as_u64().unwrap(),
        before["orchestration"]["revision"].as_u64().unwrap() + 2
    );
    assert!(
        !crate::saved_task(dir.path())
            .unwrap()
            .unwrap()
            .1
            .is_accepted()
    );
    assert_eq!(
        open(&dir)
            .receipt(&receipt.id)
            .unwrap()
            .unwrap()
            .accepted_sequence,
        receipt.accepted_sequence
    );
}

#[tokio::test]
async fn caller_cancellation_after_intake_is_reconciled_by_identical_retry() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open_acceptance_session(dir.path(), options()).unwrap();
    let (entered, seen) = tokio::sync::oneshot::channel();
    let (release, waiting) = std::sync::mpsc::channel();
    handle
        .commands
        .tx
        .send(Box::new(move |_| {
            entered.send(()).unwrap();
            waiting.recv().unwrap();
        }))
        .await
        .unwrap();
    seen.await.unwrap();
    let command = command("accepted even if caller disappears");
    let client = handle.commands.clone();
    let pending = command.clone();
    let caller = tokio::spawn(async move { client.submit(pending).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while handle.commands.tx.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    release.send(()).unwrap();
    let receipt = handle.commands.submit(command.clone()).await.unwrap();
    assert_eq!(receipt.accepted_sequence, 1);
    assert_eq!(
        handle
            .commands
            .recent_receipts(None, 100)
            .await
            .unwrap()
            .receipts
            .len(),
        1
    );
    assert_eq!(
        handle
            .commands
            .queued_prompt(&command.id)
            .await
            .unwrap()
            .request,
        "accepted even if caller disappears"
    );
    handle.controls.shutdown().await.unwrap();
}

fn lineage_disk_bytes(
    dir: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn collect(
        root: &std::path::Path,
        dir: &std::path::Path,
        files: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(root, &path, files);
            } else if path.file_name().and_then(|name| name.to_str()) == Some(".flint-writer.lock")
            {
                // The active writer lock is not lineage payload. Reading a
                // byte-locked region fails on Windows; skipping it keeps the
                // comparison about history content on every platform.
                continue;
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    collect(dir, dir, &mut files);
    files
}

fn lineage_fixture(dir: &TempDir, damage: &str) {
    let mut task = crate::TaskRecord::new(7, "Produce output.".into(), 4);
    task.assess(crate::TurnEndReason::Completed, true);
    crate::persist::write_atomic(
        dir.path(),
        &crate::persist::snapshot_with_task(
            7,
            &[crate::provider::Message::User("Produce output.".into())],
            Some(&task),
        ),
    )
    .unwrap();
    let mut store = open(dir);
    store.submit(command("canonical A")).unwrap();
    store.submit(command("canonical B")).unwrap();
    let objects = fs::Directory::open(&dir.path().join("engine-objects")).unwrap();
    let mut document = store.document.clone();
    match damage {
        "intact" => {}
        "missing-head" => {
            document["orchestration"]["receipt_head"] = serde_json::Value::Null;
            document["orchestration"]["receipt_count"] = serde_json::json!(0);
        }
        "orphan" => {
            // A hash-valid node with identical accepted fields, but a different
            // immutable object identity, is not the referenced canonical node.
            let original = std::fs::read(
                dir.path()
                    .join("engine-objects")
                    .join(&store.state.pending[0].acceptance),
            )
            .unwrap();
            let mut orphan = vec![b' '];
            orphan.extend(original);
            let name: String = store::digest(&orphan)
                .0
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            objects.immutable(&name, &orphan).unwrap();
            document["orchestration"]["pending"][0]["acceptance"] = serde_json::json!(name);
        }
        "different-branch" => {
            // A valid two-node alternative branch: root A' is another actual
            // Send; B has the same accepted fields but references A', not A.
            let alternative = command("other valid branch root");
            let (bytes, digest) = store::canonical_command(&alternative).unwrap();
            let object: String = digest.0.iter().map(|byte| format!("{byte:02x}")).collect();
            objects.immutable(&object, &bytes).unwrap();
            let mut first: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    dir.path()
                        .join("engine-objects")
                        .join(&store.state.pending[0].acceptance),
                )
                .unwrap(),
            )
            .unwrap();
            first["command_object"] = serde_json::json!(object);
            first["receipt"]["id"] = serde_json::json!(alternative.id);
            first["receipt"]["payload_digest"] = serde_json::json!(digest);
            let bytes = serde_json::to_vec(&first).unwrap();
            let first_name: String = store::digest(&bytes)
                .0
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            objects.immutable(&first_name, &bytes).unwrap();
            let mut second: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    dir.path()
                        .join("engine-objects")
                        .join(&store.state.pending[1].acceptance),
                )
                .unwrap(),
            )
            .unwrap();
            second["previous"] = serde_json::json!(first_name);
            let bytes = serde_json::to_vec(&second).unwrap();
            let head: String = store::digest(&bytes)
                .0
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            objects.immutable(&head, &bytes).unwrap();
            document["orchestration"]["receipt_head"] = serde_json::json!(head);
        }
        _ => panic!("unknown fixture damage"),
    }
    drop(store);
    std::fs::write(
        dir.path().join("history.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
}

fn lineage_assert_reopen_rejected(damage: &str) {
    let dir = tempfile::tempdir().unwrap();
    lineage_fixture(&dir, damage);
    let before = lineage_disk_bytes(dir.path());
    let result = Store::open(dir.path(), options());
    if let Ok(store) = &result {
        eprintln!(
            "B1_LINEAGE unexpectedly_recovered={damage} pending={} receipt_count={}",
            store.state.pending.len(),
            store.state.receipt_count
        );
    }
    assert!(
        matches!(result, Err(error) if error.code == EngineErrorCode::CorruptState),
        "{damage} must fail before recovery commit"
    );
    assert_eq!(lineage_disk_bytes(dir.path()), before);
    assert!(!dir.path().join("history.json.corrupt").exists());
}

#[test]
fn lineage_reopen_rejects_missing_canonical_head_and_count() {
    lineage_assert_reopen_rejected("missing-head");
}

#[test]
fn lineage_reopen_rejects_hash_valid_orphan_acceptance() {
    lineage_assert_reopen_rejected("orphan");
}

#[test]
fn lineage_reopen_rejects_other_valid_receipt_branch() {
    lineage_assert_reopen_rejected("different-branch");
}

#[test]
fn lineage_stopped_review_and_edit_refuse_corruption_without_writes() {
    for damage in ["missing-head", "orphan", "different-branch"] {
        for review in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            lineage_fixture(&dir, damage);
            let before = lineage_disk_bytes(dir.path());
            let result = if review {
                crate::review_saved_task(
                    dir.path(),
                    workspace.path(),
                    &crate::TaskReview {
                        turn_id: 7,
                        task_id: 7,
                        contract_revision: 1,
                        work_revision: 4,
                        decision: crate::ReviewDecision::Accept,
                    },
                )
                .map(|_| ())
            } else {
                crate::edit_saved_task(
                    dir.path(),
                    7,
                    crate::TaskEdit {
                        task_id: 7,
                        expected_revision: 1,
                        objective: "Explicit revised goal".into(),
                        mode: crate::TaskMode::ReadOnly,
                        requirements: vec![crate::RequirementSpec::Review(
                            "Explain the result.".into(),
                        )],
                        restrictions: vec![],
                        clear_reported_blockers: false,
                    },
                )
                .map(|_| ())
            };
            assert!(
                result.is_err(),
                "damaged={damage}, review={review} must refuse mutation"
            );
            assert_eq!(lineage_disk_bytes(dir.path()), before);
        }
    }
}

#[test]
fn lineage_read_only_corruption_preflight_preserves_all_files() {
    for damage in ["missing-head", "orphan", "different-branch"] {
        let dir = tempfile::tempdir().unwrap();
        lineage_fixture(&dir, damage);
        let before = lineage_disk_bytes(dir.path());
        assert!(crate::saved_task_projection(dir.path()).is_err());
        assert!(crate::persist::load(dir.path()).is_err());
        assert!(matches!(read_conversation_summary(dir.path()),
            Err(error) if error.code == EngineErrorCode::CorruptState));
        assert_eq!(lineage_disk_bytes(dir.path()), before);
    }
}

#[test]
fn lineage_reachable_node_must_match_all_pending_acceptance_fields() {
    for field in [
        "id",
        "payload_digest",
        "accepted_sequence",
        "accepted_revision",
        "object",
    ] {
        let dir = tempfile::tempdir().unwrap();
        lineage_fixture(&dir, "intact");
        let path = dir.path().join("history.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        match field {
            "id" => {
                document["orchestration"]["pending"][0]["receipt"][field] =
                    serde_json::json!(CommandId::fresh())
            }
            "payload_digest" => {
                document["orchestration"]["pending"][0]["receipt"][field] =
                    serde_json::json!(Digest([0; 32]))
            }
            "accepted_sequence" => {
                document["orchestration"]["pending"][0]["receipt"][field] = serde_json::json!(2)
            }
            "accepted_revision" => {
                document["orchestration"]["pending"][0]["receipt"][field] = serde_json::json!(1)
            }
            "object" => {
                document["orchestration"]["pending"][0][field] =
                    document["orchestration"]["pending"][1][field].clone()
            }
            _ => unreachable!(),
        }
        std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
        let before = lineage_disk_bytes(dir.path());
        assert!(
            matches!(Store::open(dir.path(), options()),
            Err(error) if error.code == EngineErrorCode::CorruptState),
            "changed={field}"
        );
        assert!(
            crate::review_saved_task(
                dir.path(),
                dir.path(),
                &crate::TaskReview {
                    turn_id: 7,
                    task_id: 7,
                    contract_revision: 1,
                    work_revision: 4,
                    decision: crate::ReviewDecision::Accept,
                }
            )
            .is_err(),
            "stopped review, changed={field}"
        );
        assert_eq!(lineage_disk_bytes(dir.path()), before);
    }
}

#[test]
fn lineage_read_only_helpers_create_no_directories_locks_or_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(&dir));
    std::fs::remove_dir(dir.path().join("engine-objects")).unwrap();
    std::fs::remove_file(dir.path().join(".flint-writer.lock")).unwrap();
    let before = lineage_disk_bytes(dir.path());
    assert!(crate::saved_task_projection(dir.path()).unwrap().is_some());
    assert!(crate::persist::load(dir.path()).unwrap().is_some());
    assert!(read_conversation_summary(dir.path()).unwrap().is_some());
    assert_eq!(lineage_disk_bytes(dir.path()), before);
    assert!(!dir.path().join("engine-objects").exists());
    assert!(!dir.path().join(".flint-writer.lock").exists());
    let missing = dir.path().join("missing-session");
    assert!(crate::saved_task_projection(&missing).unwrap().is_none());
    assert!(crate::persist::load(&missing).unwrap().is_none());
    assert!(read_conversation_summary(&missing).is_err());
    assert!(!missing.exists());
}

#[test]
fn lineage_long_history_reopens_with_twenty_active_entries_and_bounded_pages() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let mut oldest = None;
    for index in 0..128 {
        if store.state.pending.len() == 20 {
            // Test-only retirement keeps the canonical node chain while
            // removing old payloads; B1 exposes no public retirement method.
            let mut state = store.state.clone();
            let retired = state.pending.remove(0).object;
            store.commit(state).unwrap();
            std::fs::remove_file(dir.path().join("engine-objects").join(retired)).unwrap();
        }
        let receipt = store
            .submit(command(&format!("history {index}: {}", "x".repeat(1024))))
            .unwrap();
        if index == 0 {
            oldest = Some(receipt.id);
        }
    }
    drop(store);
    let store = open(&dir);
    assert_eq!(store.state.pending.len(), 20);
    assert_eq!(store.state.receipt_count, 128);
    assert!(store.state.pending.iter().all(|entry| matches!(
        entry.receipt.state,
        CommandState::Held(HoldReason::Restart { .. })
    )));
    assert!(store.receipt(&oldest.unwrap()).unwrap().is_some());
    let mut before = None;
    let mut count = 0;
    loop {
        let page = store.recent(before, 7).unwrap();
        assert!(page.receipts.len() <= 7);
        assert!(serde_json::to_vec(&page).unwrap().len() <= store.limits.history_page_bytes);
        count += page.receipts.len();
        before = page.next_before_sequence;
        if before.is_none() {
            break;
        }
    }
    assert_eq!(count, 128);
    let unchanged = lineage_disk_bytes(dir.path());
    assert_eq!(
        read_conversation_summary(dir.path())
            .unwrap()
            .unwrap()
            .queued,
        20
    );
    assert_eq!(lineage_disk_bytes(dir.path()), unchanged);
    eprintln!("B1_LINEAGE historical_nodes=128 active_pending=20 page_limit=7");
}

#[test]
fn boxed_engine_event_payloads_keep_unboxed_json_and_roundtrip() {
    let record = EngineRecord {
        sequence: 1,
        revision: 1,
        conversation: conversation(),
        row: None,
        body: EngineRecordBody::RecoveryHeld {
            generation: 1,
            reason: HoldReason::Paused,
        },
    };
    let progress = ProgressUpdate {
        conversation: conversation(),
        run_id: RunId::parse("01234567-89ab-4cde-8fab-0123456789ab").unwrap(),
        row: None,
        call_id: None,
        terminal_id: None,
        kind: ProgressKind::Text,
        generation: 1,
        value: ProgressValue::TextPreview {
            utf8_start: 0,
            text: "bounded preview".into(),
            total_utf8_bytes: 15,
        },
    };
    for (event, expected) in [
        (
            EngineEvent::Record(Box::new(record.clone())),
            serde_json::json!({"Record": record}),
        ),
        (
            EngineEvent::Progress(Box::new(progress.clone())),
            serde_json::json!({"Progress": progress}),
        ),
    ] {
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value, expected);
        assert_eq!(serde_json::from_value::<EngineEvent>(value).unwrap(), event);
    }
}

#[tokio::test]
async fn nonexecuting_public_client_rejects_later_operations_and_has_no_legacy_ops() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open_acceptance_session(dir.path(), options()).unwrap();
    assert!(
        handle
            .ops
            .send(crate::Op::UserMessage("must not execute".into()))
            .await
            .is_err()
    );
    let mut unsupported = command("steering");
    if let CommandPayload::Send { delivery, .. } = &mut unsupported.payload {
        *delivery = Delivery::NativeSafeBoundary;
    }
    assert_eq!(
        handle.commands.submit(unsupported).await.unwrap_err().code,
        EngineErrorCode::Unsupported
    );
    assert_eq!(
        handle
            .controls
            .stop(StopScope::Root)
            .await
            .unwrap_err()
            .code,
        EngineErrorCode::Unsupported
    );
    assert!(matches!(
        handle.secrets.recv().await,
        Err(crate::secrets::SecretError::Unsupported)
    ));
    assert_eq!(
        handle
            .commands
            .recent_receipts(None, 1)
            .await
            .unwrap()
            .receipts
            .len(),
        0
    );
    assert!(handle.events.recv().await.is_err());
    let original = command("accepted");
    let receipt = handle.commands.submit(original.clone()).await.unwrap();
    let mut different = original;
    different.payload = CommandPayload::PauseQueue {
        expected_queue_revision: 1,
    };
    assert_eq!(
        handle.commands.submit(different).await.unwrap_err().code,
        EngineErrorCode::IdentityConflict
    );
    assert_eq!(
        handle.commands.receipt(&receipt.id).await.unwrap(),
        Some(receipt)
    );
    for limit in [0, 101] {
        assert_eq!(
            handle
                .commands
                .history(HistoryPageRequest {
                    conversation: conversation(),
                    before_ordinal: None,
                    limit,
                    max_bytes: 256 * 1024,
                })
                .await
                .unwrap_err()
                .code,
            EngineErrorCode::InvalidCommand
        );
    }
    assert_eq!(
        handle
            .commands
            .history(HistoryPageRequest {
                conversation: conversation(),
                before_ordinal: None,
                limit: 1,
                max_bytes: 256 * 1024,
            })
            .await
            .unwrap_err()
            .code,
        EngineErrorCode::Unsupported
    );
    handle.controls.shutdown().await.unwrap();
}

#[test]
fn lightweight_projection_reports_v3_without_advancing_generation() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store.submit(command("summary")).unwrap();
    let before = std::fs::read(dir.path().join("history.json")).unwrap();
    let summary = read_conversation_summary(dir.path()).unwrap().unwrap();
    assert_eq!(summary.queued, 1);
    assert!(!summary.running);
    assert_eq!(summary.preview, "summary");
    assert_eq!(
        std::fs::read(dir.path().join("history.json")).unwrap(),
        before
    );
}

#[test]
fn legacy_coalescing_writer_refuses_v3_and_retains_canonical_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    drop(store);
    let before = std::fs::read(dir.path().join("history.json")).unwrap();
    assert!(crate::persist::Saver::try_new(dir.path().into()).is_err());
    assert!(crate::persist::write_atomic(dir.path(), &crate::persist::snapshot(1, &[])).is_err());
    assert_eq!(
        std::fs::read(dir.path().join("history.json")).unwrap(),
        before
    );
}

#[test]
fn receipt_byte_boundary_including_the_last_node_retains_a_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let first = store.submit(command("first")).unwrap();
    let second = store.submit(command("second")).unwrap();
    store.limits.history_page_bytes = serde_json::to_vec(&first)
        .unwrap()
        .len()
        .max(serde_json::to_vec(&second).unwrap().len())
        + serde_json::to_vec(&ReceiptPage {
            receipts: Vec::new(),
            next_before_sequence: Some(u64::MAX),
        })
        .unwrap()
        .len();
    let page = store.recent(None, 100).unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() <= store.limits.history_page_bytes);
    assert_eq!(page.receipts, vec![second.clone()]);
    assert_eq!(page.next_before_sequence, Some(second.accepted_sequence));
    let page = store.recent(page.next_before_sequence, 100).unwrap();
    assert_eq!(page.receipts, vec![first]);
    assert_eq!(page.next_before_sequence, None);
}

#[test]
fn immutable_publication_reuse_and_conflict_keep_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let directory = fs::Directory::open(dir.path()).unwrap();
    directory.immutable("retained", b"original").unwrap();
    directory.immutable("retained", b"original").unwrap();
    assert!(directory.immutable("retained", b"changed").is_err());
    assert_eq!(directory.read("retained", 100).unwrap(), b"original");
}

#[tokio::test]
async fn oversized_inputs_are_rejected_before_actor_intake() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open_acceptance_session(dir.path(), options()).unwrap();
    let mut command = command("oversized");
    if let CommandPayload::Send { prompt, .. } = &mut command.payload {
        prompt.effective_text = "x".repeat(32 * 1024 * 1024 + 1);
    }
    assert_eq!(
        handle.commands.submit(command).await.unwrap_err().code,
        EngineErrorCode::QueueFull
    );
    assert_eq!(
        handle
            .commands
            .recent_receipts(None, 1)
            .await
            .unwrap()
            .receipts
            .len(),
        0
    );
    handle.controls.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_drops_unaccepted_requests_and_waits_for_writer_release() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open_acceptance_session(dir.path(), options()).unwrap();
    let (entered, seen) = tokio::sync::oneshot::channel();
    let (release, waiting) = std::sync::mpsc::channel();
    handle
        .commands
        .tx
        .send(Box::new(move |_| {
            entered.send(()).unwrap();
            waiting.recv().unwrap();
        }))
        .await
        .unwrap();
    seen.await.unwrap();
    let commands = handle.commands.clone();
    let caller = tokio::spawn(async move { commands.submit(command("not accepted")).await });
    while handle.commands.tx.is_empty() {
        tokio::task::yield_now().await;
    }
    let controls = handle.controls.clone();
    let shutdown = tokio::spawn(async move { controls.shutdown().await });
    while !handle.controls.stopping.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), caller)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        EngineErrorCode::Cancelled
    );
    assert_eq!(open(&dir).state.receipt_count, 0);
}

fn config_for(dir: &TempDir, workspace: &TempDir, base_url: String) -> AgentConfig {
    AgentConfig {
        base_url,
        model: "offline-test-model".into(),
        subagent_model: None,
        api_key: String::new(),
        workspace: workspace.path().into(),
        general: false,
        approval: crate::ApprovalMode::AskForChanges,
        jev: None,
        session_dir: Some(dir.path().into()),
        context_budget_tokens: 0,
        reasoning_effort: None,
        sandbox: true,
        mcp_servers: vec![],
    }
}

#[tokio::test]
async fn v3_legacy_startup_never_contacts_provider_or_launches_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    drop(open(&dir));
    let before = std::fs::read(dir.path().join("history.json")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = config_for(
        &dir,
        &workspace,
        format!("http://{}/v1", listener.local_addr().unwrap()),
    );
    let marker = workspace.path().join("mcp-started");
    config.mcp_servers.push(crate::McpServerConfig {
        name: "must-not-start".into(),
        command: "/usr/bin/touch".into(),
        args: vec![marker.display().to_string()],
        env: vec![],
    });
    let handle = crate::spawn_session(config);
    let mut events = Vec::new();
    while let Ok(event) =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
    {
        events.push(event);
    }
    assert!(
        events.iter().any(
            |event| matches!(event, crate::AgentEvent::Error(message) if message.contains("v3"))
        )
    );
    assert!(events.iter().any(|event| matches!(
        event,
        crate::AgentEvent::SessionStopped {
            history_saved: false
        }
    )));
    assert!(!marker.exists());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(dir.path().join("history.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn orchestrated_entry_point_accepts_without_resolving_profiles_or_execution() {
    struct NoResolver;
    impl TargetResolver for NoResolver {
        fn resolve(&self, _: ProviderTarget) -> BoxFuture<Result<ResolvedTarget, EngineError>> {
            panic!("B1 must not resolve a profile");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = config_for(
        &dir,
        &workspace,
        format!("http://{}/v1", listener.local_addr().unwrap()),
    );
    let handle = spawn_orchestrated_session(
        config,
        options(),
        HostServices {
            profiles: Arc::new(NoResolver),
            external: None,
            pr_watch: None,
            destinations: None,
        },
    )
    .unwrap();
    let receipt = handle
        .commands
        .submit(command("durable only"))
        .await
        .unwrap();
    assert!(matches!(receipt.state, CommandState::Accepted));
    assert!(
        !handle
            .commands
            .summary(&conversation())
            .await
            .unwrap()
            .running
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    handle.controls.shutdown().await.unwrap();
    let handle = open_acceptance_session(dir.path(), options()).unwrap();
    assert!(matches!(
        handle
            .commands
            .receipt(&receipt.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        CommandState::Held(HoldReason::Restart { .. })
    ));
    handle.controls.shutdown().await.unwrap();
}

#[test]
fn summary_reports_actual_restart_and_ambiguous_effects_holds() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store.submit(command("accepted")).unwrap();
    drop(store);
    let summary = read_conversation_summary(dir.path()).unwrap().unwrap();
    assert_eq!(summary.hold, Some(HoldReason::Paused));

    let store = open(&dir);
    let generation = store.state.generation;
    assert_eq!(
        store.summary(&conversation()).unwrap().hold,
        Some(HoldReason::Restart { generation })
    );
    drop(store);
    assert_eq!(
        read_conversation_summary(dir.path()).unwrap().unwrap().hold,
        Some(HoldReason::Restart { generation })
    );

    let mut store = open(&dir);
    let run_id = RunId::parse(CommandId::fresh().as_str()).unwrap();
    let mut state = store.state.clone();
    state.pending[0].receipt.state = CommandState::Started {
        run_id: run_id.clone(),
        turn_id: 3,
        safe_boundary_step: None,
    };
    store.commit(state).unwrap();
    drop(store);

    let store = open(&dir);
    let expected = Some(HoldReason::AmbiguousEffects {
        run_id,
        intent_ids: Vec::new(),
    });
    assert_eq!(store.summary(&conversation()).unwrap().hold, expected);
    drop(store);
    assert_eq!(
        read_conversation_summary(dir.path()).unwrap().unwrap().hold,
        expected
    );
}

#[test]
fn reopen_preserves_durable_holds_and_finished_status_revision() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let accepted = store.submit(command("accepted")).unwrap();
    let task_review = store.submit(command("task review")).unwrap();
    let interrupted = store.submit(command("interrupted")).unwrap();
    let paused = store.submit(command("paused")).unwrap();
    let provider_unavail = store.submit(command("provider unavailable")).unwrap();
    let consent = store.submit(command("consent")).unwrap();
    let storage_unavail = store.submit(command("storage unavailable")).unwrap();
    let finished = store.submit(command("finished")).unwrap();

    let review_hold = HoldReason::TaskReview {
        revision: TaskRevision {
            turn_id: 1,
            task_id: 1,
            contract_revision: 1,
            work_revision: 2,
        },
        outcome: crate::TaskOutcome::NeedsReview,
    };
    let finished_state = CommandState::Finished {
        run_id: None,
        turn_id: Some(1),
        reason: CommandFinish::Applied,
    };
    let mut state = store.state.clone();
    state.pending[1].receipt.state = CommandState::Held(review_hold.clone());
    state.pending[2].receipt.state = CommandState::Held(HoldReason::Interrupted);
    state.pending[3].receipt.state = CommandState::Held(HoldReason::Paused);
    state.pending[4].receipt.state = CommandState::Held(HoldReason::ProviderUnavailable);
    state.pending[5].receipt.state = CommandState::Held(HoldReason::ResumeNeedsConsent);
    state.pending[6].receipt.state = CommandState::Held(HoldReason::StorageUnavailable);
    state.pending[7].receipt.state = finished_state.clone();
    let finished_rev = state.pending[7].receipt.status_revision;
    let hold_rev = state.pending[1].receipt.status_revision;
    store.commit(state).unwrap();
    drop(store);

    let store = open(&dir);
    assert!(matches!(
        store.receipt(&accepted.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::Restart { .. })
    ));
    let r1 = store.receipt(&task_review.id).unwrap().unwrap();
    assert_eq!(r1.state, CommandState::Held(review_hold));
    assert_eq!(r1.status_revision, hold_rev);
    assert_eq!(
        store.receipt(&interrupted.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::Interrupted)
    );
    assert_eq!(
        store.receipt(&paused.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::Paused)
    );
    assert_eq!(
        store.receipt(&provider_unavail.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::ProviderUnavailable)
    );
    assert_eq!(
        store.receipt(&consent.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::ResumeNeedsConsent)
    );
    assert_eq!(
        store.receipt(&storage_unavail.id).unwrap().unwrap().state,
        CommandState::Held(HoldReason::StorageUnavailable)
    );
    let rf = store.receipt(&finished.id).unwrap().unwrap();
    assert_eq!(rf.state, finished_state);
    assert_eq!(rf.status_revision, finished_rev);
}

#[test]
fn validate_state_and_read_only_helpers_reject_invalid_saved_target() {
    for mutation in ["zero_revision", "empty_model", "too_many_acp_options"] {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir);
        let mut document = store.document.clone();
        drop(store);
        match mutation {
            "zero_revision" => {
                document["orchestration"]["target"]["Native"]["profile"]["revision"] =
                    serde_json::json!(0);
            }
            "empty_model" => {
                document["orchestration"]["target"]["Native"]["model"] = serde_json::json!("   ");
            }
            "too_many_acp_options" => {
                let opts: std::collections::BTreeMap<String, String> =
                    (0..33).map(|i| (format!("k{i}"), "v".into())).collect();
                document["orchestration"]["target"] = serde_json::json!({
                    "Acp": {
                        "profile": { "id": "local-profile", "revision": 1 },
                        "agent_id": "agent",
                        "options": opts
                    }
                });
            }
            _ => unreachable!(),
        }
        std::fs::write(
            dir.path().join("history.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        assert!(
            matches!(read_conversation_summary(dir.path()), Err(e) if e.code == EngineErrorCode::CorruptState),
            "{mutation}"
        );
        assert!(crate::persist::load(dir.path()).is_err(), "{mutation}");
        assert!(
            crate::saved_task_projection(dir.path()).is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn validate_limits_rejects_zero_or_over_bounds_for_all_fields() {
    let cases: Vec<(&str, fn(&mut EngineLimits, usize))> = vec![
        ("queued_commands", |l, v| l.queued_commands = v),
        ("queued_bytes", |l, v| l.queued_bytes = v),
        ("command_inbox", |l, v| l.command_inbox = v),
        ("record_delivery", |l, v| l.record_delivery = v),
        ("progress_keys", |l, v| l.progress_keys = v),
        ("progress_bytes", |l, v| l.progress_bytes = v),
        ("outbox_bytes", |l, v| l.outbox_bytes = v),
        ("active_children", |l, v| l.active_children = v),
        ("retained_children", |l, v| l.retained_children = v),
        ("history_page_rows", |l, v| l.history_page_rows = v),
        ("history_page_bytes", |l, v| l.history_page_bytes = v),
    ];
    let defaults = EngineLimits::default();
    for (name, set) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = options();
        set(&mut opts.limits, 0);
        assert!(
            matches!(Store::open(dir.path(), opts), Err(e) if e.code == EngineErrorCode::InvalidCommand),
            "{name}=0"
        );
        let mut opts = options();
        let over = match name {
            "queued_commands" => defaults.queued_commands + 1,
            "queued_bytes" => defaults.queued_bytes + 1,
            "command_inbox" => defaults.command_inbox + 1,
            "record_delivery" => defaults.record_delivery + 1,
            "progress_keys" => defaults.progress_keys + 1,
            "progress_bytes" => defaults.progress_bytes + 1,
            "outbox_bytes" => defaults.outbox_bytes + 1,
            "active_children" => defaults.active_children + 1,
            "retained_children" => defaults.retained_children + 1,
            "history_page_rows" => defaults.history_page_rows + 1,
            "history_page_bytes" => defaults.history_page_bytes + 1,
            _ => unreachable!(),
        };
        set(&mut opts.limits, over);
        assert!(
            matches!(Store::open(dir.path(), opts), Err(e) if e.code == EngineErrorCode::InvalidCommand),
            "{name}=over"
        );
    }
}

#[test]
fn read_conversation_summary_ignores_orchestration_on_non_v3_histories() {
    let dir = tempfile::tempdir().unwrap();
    let v2_with_stray_orch = serde_json::json!({
        "version": 2,
        "turn_id": 1,
        "messages": [],
        "orchestration": {
            "version": 1,
            "root": "saved-root",
            "target": options().target,
            "revision": 1,
            "generation": 1,
            "next_sequence": 1,
            "receipt_head": null,
            "receipt_count": 0,
            "pending": []
        }
    });
    std::fs::write(
        dir.path().join("history.json"),
        serde_json::to_vec(&v2_with_stray_orch).unwrap(),
    )
    .unwrap();
    assert!(read_conversation_summary(dir.path()).unwrap().is_none());
}

#[test]
fn retained_index_answers_historical_receipts_without_walking_disk_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let first = store.submit(command("first")).unwrap();
    let first_node = store.state.receipt_head.clone().unwrap();
    let first_obj = store.state.pending[0].object.clone();
    // Retire `first` from pending (historical receipt).
    let mut state = store.state.clone();
    state.pending.clear();
    store.commit(state).unwrap();
    let second = store.submit(command("second")).unwrap();
    drop(store);

    // Reopen populates retained_index from disk once.
    let mut store = open(&dir);
    // Remove the historical node file after open to prove `receipt()` and
    // `submit()` consult the in-memory index instead of walking disk nodes.
    std::fs::remove_file(dir.path().join("engine-objects").join(&first_node)).unwrap();
    std::fs::remove_file(dir.path().join("engine-objects").join(&first_obj)).unwrap();

    assert_eq!(store.receipt(&first.id).unwrap(), Some(first.clone()));
    assert!(store.receipt(&second.id).unwrap().is_some());
    let third = store.submit(command("third")).unwrap();
    assert_eq!(third.accepted_sequence, 3);
    assert_eq!(store.receipt(&third.id).unwrap().unwrap().id, third.id);
    // Retrying `first` (now retired) still resolves via index as an identical/conflicting id.
    let mut conflict = command("changed");
    conflict.id = first.id;
    assert_eq!(
        store.submit(conflict).unwrap_err().code,
        EngineErrorCode::IdentityConflict
    );
}

#[test]
fn store_commit_succeeds_while_concurrent_reader_holds_history_json_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let reader_dir = fs::Directory::open(dir.path()).unwrap();
    let reader = reader_dir
        .reader("history.json", store::MAX_SNAPSHOT_BYTES)
        .unwrap();
    let receipt = store.submit(command("concurrent reader")).unwrap();
    assert_eq!(receipt.accepted_sequence, 1);
    drop(reader);
    assert_eq!(store.receipt(&receipt.id).unwrap(), Some(receipt));
}

#[test]
fn queue_cancel_and_replace_with_revision_conflict_checking() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let cmd_a = command("command A");
    let receipt_a = store.submit(cmd_a.clone()).unwrap();
    assert_eq!(receipt_a.state, CommandState::Accepted);

    let queue = store.queue(&conversation()).unwrap();
    let q_rev = queue.revision;

    // Cancel with stale expected_queue_revision fails
    let stale_cancel = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::CancelQueued {
            command_id: cmd_a.id.clone(),
            expected_queue_revision: q_rev + 1,
        },
    };
    assert_eq!(
        store.submit(stale_cancel).unwrap_err().code,
        EngineErrorCode::StaleRevision
    );

    // Cancel with correct revision succeeds
    let cancel = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::CancelQueued {
            command_id: cmd_a.id.clone(),
            expected_queue_revision: q_rev,
        },
    };
    let cancel_receipt = store.submit(cancel).unwrap();
    assert_eq!(
        cancel_receipt.state,
        CommandState::Finished {
            run_id: None,
            turn_id: None,
            reason: CommandFinish::Applied,
        }
    );

    // Command A is now CancelledBeforeStart
    assert_eq!(
        store.receipt(&cmd_a.id).unwrap().unwrap().state,
        CommandState::Finished {
            run_id: None,
            turn_id: None,
            reason: CommandFinish::CancelledBeforeStart,
        }
    );
    assert_eq!(store.queue(&conversation()).unwrap().items.len(), 0);

    // Submit command B
    let cmd_b = command("command B");
    store.submit(cmd_b.clone()).unwrap();
    let q_rev2 = store.queue(&conversation()).unwrap().revision;

    // Replace with stale revision fails
    let cmd_c = command("command C (replacement)");
    let stale_replace = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ReplaceQueued {
            command_id: cmd_b.id.clone(),
            expected_queue_revision: q_rev2 + 10,
            replacement: Box::new(cmd_c.clone()),
        },
    };
    assert_eq!(
        store.submit(stale_replace).unwrap_err().code,
        EngineErrorCode::StaleRevision
    );

    // Replace with correct revision succeeds
    let replace = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ReplaceQueued {
            command_id: cmd_b.id.clone(),
            expected_queue_revision: q_rev2,
            replacement: Box::new(cmd_c.clone()),
        },
    };
    let replace_receipt = store.submit(replace).unwrap();
    assert_eq!(
        replace_receipt.state,
        CommandState::Finished {
            run_id: None,
            turn_id: None,
            reason: CommandFinish::Applied,
        }
    );

    // Command B is superseded by Command C
    assert_eq!(
        store.receipt(&cmd_b.id).unwrap().unwrap().state,
        CommandState::Finished {
            run_id: None,
            turn_id: None,
            reason: CommandFinish::Superseded {
                replacement: cmd_c.id.clone(),
            },
        }
    );
    // Command C is in queue and Accepted
    assert_eq!(
        store.receipt(&cmd_c.id).unwrap().unwrap().state,
        CommandState::Accepted
    );
    assert_eq!(store.queue(&conversation()).unwrap().items.len(), 1);

    // Reopen store: Command B remains Superseded
    drop(store);
    let store2 = open(&dir);
    assert_eq!(
        store2.receipt(&cmd_b.id).unwrap().unwrap().state,
        CommandState::Finished {
            run_id: None,
            turn_id: None,
            reason: CommandFinish::Superseded {
                replacement: cmd_c.id.clone(),
            },
        }
    );
}

#[test]
fn queue_pause_and_resume_with_revision_and_generation_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let cmd = command("test command");
    store.submit(cmd.clone()).unwrap();

    let q_rev = store.queue(&conversation()).unwrap().revision;

    // Pause with stale revision fails
    let stale_pause = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::PauseQueue {
            expected_queue_revision: q_rev + 5,
        },
    };
    assert_eq!(
        store.submit(stale_pause).unwrap_err().code,
        EngineErrorCode::StaleRevision
    );

    // Pause with valid revision succeeds
    let pause = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::PauseQueue {
            expected_queue_revision: q_rev,
        },
    };
    assert_eq!(
        store.submit(pause).unwrap().state,
        CommandState::Finished {
            run_id: None,
            turn_id: None,
            reason: CommandFinish::Applied,
        }
    );
    assert!(store.queue(&conversation()).unwrap().paused);

    let q_rev2 = store.queue(&conversation()).unwrap().revision;
    let generation = store.state.generation;

    // Resume with wrong generation fails
    let bad_gen_resume = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ResumeQueue {
            expected_queue_revision: q_rev2,
            restart_generation: generation + 99,
        },
    };
    assert_eq!(
        store.submit(bad_gen_resume).unwrap_err().code,
        EngineErrorCode::StaleRevision
    );

    // Resume with valid revision and generation succeeds
    let resume = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ResumeQueue {
            expected_queue_revision: q_rev2,
            restart_generation: generation,
        },
    };
    assert_eq!(
        store.submit(resume).unwrap().state,
        CommandState::Finished {
            run_id: None,
            turn_id: None,
            reason: CommandFinish::Applied,
        }
    );
    assert!(!store.queue(&conversation()).unwrap().paused);
}

#[tokio::test]
async fn queue_continuation_permit_issuance_and_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open_acceptance_session(dir.path(), options()).unwrap();
    let cmd = command("review hold command");
    let cmd_id = cmd.id.clone();
    let receipt = handle.commands.submit(cmd).await.unwrap();
    assert_eq!(receipt.state, CommandState::Accepted);

    let task_rev = TaskRevision {
        turn_id: 1,
        task_id: 1,
        contract_revision: 2,
        work_revision: 3,
    };

    // Simulate hold state for review
    handle
        .commands
        .query({
            let cmd_id = cmd_id.clone();
            let task_rev = task_rev.clone();
            move |store| {
                let entry = store
                    .state
                    .pending
                    .iter_mut()
                    .find(|e| e.receipt.id == cmd_id)
                    .unwrap();
                entry.receipt.state = CommandState::Held(HoldReason::TaskReview {
                    revision: task_rev,
                    outcome: crate::TaskOutcome::NeedsReview,
                });
                Ok(())
            }
        })
        .await
        .unwrap();

    let generation = handle
        .commands
        .query(|store| Ok(store.state.generation))
        .await
        .unwrap();

    // Mismatched generation
    let err = match handle
        .commands
        .confirm_queue_continuation(
            conversation(),
            task_rev.clone(),
            cmd_id.clone(),
            generation + 1,
        )
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected stale revision"),
    };
    assert_eq!(err.code, EngineErrorCode::StaleRevision);

    // Mismatched task revision
    let wrong_rev = TaskRevision {
        turn_id: 1,
        task_id: 1,
        contract_revision: 99,
        work_revision: 99,
    };
    let err = match handle
        .commands
        .confirm_queue_continuation(conversation(), wrong_rev, cmd_id.clone(), generation)
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected stale revision"),
    };
    assert_eq!(err.code, EngineErrorCode::StaleRevision);

    // Correct confirmation issues a permit
    let permit = handle
        .commands
        .confirm_queue_continuation(conversation(), task_rev.clone(), cmd_id.clone(), generation)
        .await
        .unwrap();

    // Consume permit to resume
    let resumed_receipt = handle.commands.resume_one(permit).await.unwrap();
    assert_eq!(resumed_receipt.state, CommandState::Accepted);

    // Command in queue is now Accepted
    let q = handle.commands.queue(&conversation()).await.unwrap();
    assert_eq!(q.items[0].receipt.state, CommandState::Accepted);
    handle.controls.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_effect_intent_recording_and_crash_recovery_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open_acceptance_session(dir.path(), options()).unwrap();
    let cmd = command("intent command");
    let cmd_id = cmd.id.clone();
    handle.commands.submit(cmd).await.unwrap();

    let run_id = RunId::fresh();
    let intent1 = SideEffectIntent {
        id: 101,
        command_id: cmd_id.clone(),
        conversation: conversation(),
        run_id: run_id.clone(),
        call_id: "tool_call_1".into(),
        effect: EffectClass::WorkspaceMutation,
        argument_digest: Digest([1; 32]),
        work_revision: 1,
        state: IntentState::Dispatching,
    };
    let intent2 = SideEffectIntent {
        id: 102,
        command_id: cmd_id.clone(),
        conversation: conversation(),
        run_id: run_id.clone(),
        call_id: "tool_call_2".into(),
        effect: EffectClass::ExternalOrUnknown,
        argument_digest: Digest([2; 32]),
        work_revision: 1,
        state: IntentState::Dispatching,
    };

    handle
        .commands
        .record_intent(intent1.clone())
        .await
        .unwrap();
    handle
        .commands
        .record_intent(intent2.clone())
        .await
        .unwrap();

    // Complete intent 1
    let completed = handle.commands.complete_intent(101, true).await.unwrap();
    assert_eq!(completed.state, IntentState::Completed { success: true });

    // Mark command as Started
    handle
        .commands
        .query({
            let cmd_id = cmd_id.clone();
            let run_id = run_id.clone();
            move |store| {
                let mut state = store.state.clone();
                let entry = state
                    .pending
                    .iter_mut()
                    .find(|e| e.receipt.id == cmd_id)
                    .unwrap();
                entry.receipt.state = CommandState::Started {
                    run_id,
                    turn_id: 1,
                    safe_boundary_step: None,
                };
                store.commit(state)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    handle.controls.shutdown().await.unwrap();

    // Crash / Reopen: Dispatching intent 2 becomes Ambiguous, Started command becomes Held(AmbiguousEffects)
    let store2 = open(&dir);
    let int2 = store2.state.intents.iter().find(|i| i.id == 102).unwrap();
    assert_eq!(int2.state, IntentState::Ambiguous);

    let r = store2.receipt(&cmd_id).unwrap().unwrap();
    assert_eq!(
        r.state,
        CommandState::Held(HoldReason::AmbiguousEffects {
            run_id: run_id.clone(),
            intent_ids: vec![102],
        })
    );
}

#[tokio::test]
async fn control_client_stop_and_approval_routing() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = config_for(&dir, &workspace, "http://127.0.0.1:9999/v1".into());
    struct DummyResolver;
    impl TargetResolver for DummyResolver {
        fn resolve(
            &self,
            target: ProviderTarget,
        ) -> BoxFuture<Result<ResolvedTarget, EngineError>> {
            Box::pin(async move {
                Ok(ResolvedTarget::Native(Box::new(ResolvedNative {
                    target: target.clone(),
                    base_url: "http://127.0.0.1:9999/v1".into(),
                    credential: crate::secrets::SecretBytes::new(b"secret".to_vec()).unwrap(),
                    capabilities: ProviderCapabilities {
                        target,
                        safe_boundary_steering: FeatureSupport::Supported,
                        stop_then_send: FeatureSupport::Supported,
                        session_resume: FeatureSupport::Supported,
                        prompt_images: FeatureSupport::Supported,
                        rich_output: FeatureSupport::Supported,
                        native_task_authority: FeatureSupport::Supported,
                        shared_workspace_evidence: FeatureSupport::Supported,
                        inherited_read_only: FeatureSupport::Supported,
                        inherited_native_sandbox: FeatureSupport::Supported,
                        portable_transfer: FeatureSupport::Supported,
                        provider_native_fork: FeatureSupport::Supported,
                        private_host_secret: FeatureSupport::Supported,
                        history_retrieval_tool: FeatureSupport::Supported,
                    },
                })))
            })
        }
    }
    let handle = spawn_orchestrated_session(
        config,
        options(),
        HostServices {
            profiles: Arc::new(DummyResolver),
            external: None,
            pr_watch: None,
            destinations: None,
        },
    )
    .unwrap();

    let run_a = RunId::fresh();
    let run_b = RunId::fresh();
    let conv_a = conversation();
    let mut conv_b = conversation();
    conv_b.root = SessionId::parse("child-session").unwrap();

    let task_handle = TaskHandle {
        conversation: conv_b.clone(),
        command_id: CommandId::fresh(),
    };

    let flag_a = handle
        .controls
        .register_run(run_a.clone(), conv_a.clone(), None);
    let flag_b =
        handle
            .controls
            .register_run(run_b.clone(), conv_b.clone(), Some(task_handle.clone()));

    assert!(!flag_a.load(Ordering::SeqCst));
    assert!(!flag_b.load(Ordering::SeqCst));

    // Scoped stop by Conversation A
    let ack = handle
        .controls
        .stop(StopScope::Conversation(conv_a))
        .await
        .unwrap();
    assert_eq!(ack.affected_runs, vec![run_a.clone()]);
    assert!(flag_a.load(Ordering::SeqCst));
    assert!(!flag_b.load(Ordering::SeqCst));

    // Scoped stop by Task
    let ack_b = handle
        .controls
        .stop(StopScope::Task(task_handle))
        .await
        .unwrap();
    assert_eq!(ack_b.affected_runs, vec![run_b.clone()]);
    assert!(flag_b.load(Ordering::SeqCst));

    // Approval routing
    let rx = handle
        .controls
        .register_approval(run_a.clone(), "call_99".into());
    let approve_addr = ApprovalAddress {
        conversation: conversation(),
        run_id: run_a.clone(),
        call_id: "call_99".into(),
    };

    // Approve delivery
    handle
        .controls
        .approve(approve_addr.clone(), ApprovalDecision::Approve)
        .await
        .unwrap();
    let decision = rx.await.unwrap();
    assert_eq!(decision, ApprovalDecision::Approve);

    // Second approve for same address fails (one-use)
    assert_eq!(
        handle
            .controls
            .approve(approve_addr, ApprovalDecision::Deny)
            .await
            .unwrap_err()
            .code,
        EngineErrorCode::NotFound
    );

    handle.controls.shutdown().await.unwrap();
}

#[test]
fn queue_cancel_started_command_or_unknown_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let cmd = command("to cancel");
    let receipt = store.submit(cmd.clone()).unwrap();
    assert_eq!(receipt.state, CommandState::Accepted);

    let q_rev = store.queue(&conversation()).unwrap().revision;

    // 1. Unknown command ID -> NotFound
    let unknown_cancel = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::CancelQueued {
            command_id: CommandId::fresh(),
            expected_queue_revision: q_rev,
        },
    };
    assert_eq!(
        store.submit(unknown_cancel).unwrap_err().code,
        EngineErrorCode::NotFound
    );

    // 2. expected_queue_revision == 0 -> InvalidCommand
    let zero_rev_cancel = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::CancelQueued {
            command_id: cmd.id.clone(),
            expected_queue_revision: 0,
        },
    };
    assert_eq!(
        store.submit(zero_rev_cancel).unwrap_err().code,
        EngineErrorCode::InvalidCommand
    );

    // 3. Mark command as Started, then attempt cancel -> InvalidCommand
    store.state.pending[0].receipt.state = CommandState::Started {
        run_id: RunId::fresh(),
        turn_id: 1,
        safe_boundary_step: None,
    };
    let started_cancel = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::CancelQueued {
            command_id: cmd.id.clone(),
            expected_queue_revision: q_rev,
        },
    };
    assert_eq!(
        store.submit(started_cancel).unwrap_err().code,
        EngineErrorCode::InvalidCommand
    );
}

#[test]
fn queue_replace_edge_cases_and_validations() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let cmd = command("original command");
    store.submit(cmd.clone()).unwrap();

    let q_rev = store.queue(&conversation()).unwrap().revision;

    // 1. Unknown command ID -> NotFound
    let unknown_replace = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ReplaceQueued {
            command_id: CommandId::fresh(),
            expected_queue_revision: q_rev,
            replacement: Box::new(command("replacement")),
        },
    };
    assert_eq!(
        store.submit(unknown_replace).unwrap_err().code,
        EngineErrorCode::NotFound
    );

    // 2. Replacement has same ID as target -> InvalidCommand
    let same_id_cmd = EngineCommand {
        id: cmd.id.clone(),
        conversation: conversation(),
        payload: CommandPayload::Send {
            prompt: PromptSnapshot {
                request: "same id".into(),
                effective_text: "same id".into(),
                images: Vec::new(),
                citations: Vec::new(),
            },
            delivery: Delivery::NextTurn,
        },
    };
    let same_id_replace = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ReplaceQueued {
            command_id: cmd.id.clone(),
            expected_queue_revision: q_rev,
            replacement: Box::new(same_id_cmd),
        },
    };
    assert_eq!(
        store.submit(same_id_replace).unwrap_err().code,
        EngineErrorCode::InvalidCommand
    );

    // 3. Replacement conversation mismatch -> InvalidCommand
    let mut other_conv = conversation();
    other_conv.root = SessionId::parse("other-root").unwrap();
    let mut other_conv_cmd = command("other conv");
    other_conv_cmd.conversation = other_conv;
    let mismatch_conv_replace = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ReplaceQueued {
            command_id: cmd.id.clone(),
            expected_queue_revision: q_rev,
            replacement: Box::new(other_conv_cmd),
        },
    };
    assert_eq!(
        store.submit(mismatch_conv_replace).unwrap_err().code,
        EngineErrorCode::InvalidCommand
    );

    // 4. Replacement payload is nested control -> InvalidCommand
    let nested_control_cmd = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::PauseQueue {
            expected_queue_revision: 1,
        },
    };
    let nested_replace = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ReplaceQueued {
            command_id: cmd.id.clone(),
            expected_queue_revision: q_rev,
            replacement: Box::new(nested_control_cmd),
        },
    };
    assert_eq!(
        store.submit(nested_replace).unwrap_err().code,
        EngineErrorCode::InvalidCommand
    );

    // 5. Replacement ID collides with existing accepted command -> IdentityConflict
    let second_cmd = command("second command");
    store.submit(second_cmd.clone()).unwrap();
    let q_rev2 = store.queue(&conversation()).unwrap().revision;

    let colliding_replace = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ReplaceQueued {
            command_id: cmd.id.clone(),
            expected_queue_revision: q_rev2,
            replacement: Box::new(second_cmd.clone()),
        },
    };
    assert_eq!(
        store.submit(colliding_replace).unwrap_err().code,
        EngineErrorCode::IdentityConflict
    );

    // 6. Target command is actively Started -> InvalidCommand
    store.state.pending[0].receipt.state = CommandState::Started {
        run_id: RunId::fresh(),
        turn_id: 1,
        safe_boundary_step: None,
    };
    let started_replace = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ReplaceQueued {
            command_id: cmd.id.clone(),
            expected_queue_revision: q_rev2,
            replacement: Box::new(command("new replace")),
        },
    };
    assert_eq!(
        store.submit(started_replace).unwrap_err().code,
        EngineErrorCode::InvalidCommand
    );
}

#[test]
fn queue_pause_resume_unholds_paused_and_restart_commands() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let cmd1 = command("cmd 1");
    let cmd2 = command("cmd 2");
    store.submit(cmd1.clone()).unwrap();
    store.submit(cmd2.clone()).unwrap();

    let q_rev = store.queue(&conversation()).unwrap().revision;

    // Pause queue
    let pause = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::PauseQueue {
            expected_queue_revision: q_rev,
        },
    };
    store.submit(pause).unwrap();

    // Verify queue projection is paused and has HoldReason::Paused
    let queue = store.queue(&conversation()).unwrap();
    assert!(queue.paused);
    assert_eq!(queue.hold, Some(HoldReason::Paused));

    // Manually set cmd1 to Held(Paused) and cmd2 to Held(Restart)
    let generation = store.state.generation;
    store.state.pending[0].receipt.state = CommandState::Held(HoldReason::Paused);
    store.state.pending[1].receipt.state = CommandState::Held(HoldReason::Restart { generation });

    // Resume queue
    let q_rev2 = store.queue(&conversation()).unwrap().revision;
    let resume = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::ResumeQueue {
            expected_queue_revision: q_rev2,
            restart_generation: generation,
        },
    };
    store.submit(resume).unwrap();

    // Both should now be Accepted
    assert_eq!(store.state.pending[0].receipt.state, CommandState::Accepted);
    assert_eq!(store.state.pending[1].receipt.state, CommandState::Accepted);
    let queue2 = store.queue(&conversation()).unwrap();
    assert!(!queue2.paused);
    assert_eq!(queue2.hold, None);
}

#[test]
fn side_effect_intent_limits_conflicts_and_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let cmd = command("intent cmd");
    store.submit(cmd.clone()).unwrap();

    let run_id = RunId::fresh();
    let intent = SideEffectIntent {
        id: 1,
        command_id: cmd.id.clone(),
        conversation: conversation(),
        run_id: run_id.clone(),
        call_id: "c1".into(),
        effect: EffectClass::WorkspaceMutation,
        argument_digest: Digest([0; 32]),
        work_revision: 1,
        state: IntentState::Dispatching,
    };

    // 1. Record intent succeeds
    store.record_intent(intent.clone()).unwrap();

    // 2. Duplicate intent ID -> IdentityConflict
    assert_eq!(
        store.record_intent(intent.clone()).unwrap_err().code,
        EngineErrorCode::IdentityConflict
    );

    // 3. Complete nonexistent intent -> NotFound
    assert_eq!(
        store.complete_intent(999, true).unwrap_err().code,
        EngineErrorCode::NotFound
    );

    // 4. Fill to 32 intents, 33rd fails with QueueFull
    for i in 2..=32 {
        let extra_intent = SideEffectIntent {
            id: i,
            command_id: cmd.id.clone(),
            conversation: conversation(),
            run_id: run_id.clone(),
            call_id: format!("c{i}"),
            effect: EffectClass::ReadOnly,
            argument_digest: Digest([0; 32]),
            work_revision: 1,
            state: IntentState::Prepared,
        };
        store.record_intent(extra_intent).unwrap();
    }
    let overflow_intent = SideEffectIntent {
        id: 33,
        command_id: cmd.id.clone(),
        conversation: conversation(),
        run_id: run_id.clone(),
        call_id: "c33".into(),
        effect: EffectClass::ReadOnly,
        argument_digest: Digest([0; 32]),
        work_revision: 1,
        state: IntentState::Prepared,
    };
    assert_eq!(
        store.record_intent(overflow_intent).unwrap_err().code,
        EngineErrorCode::QueueFull
    );
}

#[tokio::test]
async fn queue_continuation_permit_rejection_conditions() {
    let dir = tempfile::tempdir().unwrap();
    let handle = open_acceptance_session(dir.path(), options()).unwrap();
    let cmd = command("permit test");
    let cmd_id = cmd.id.clone();
    handle.commands.submit(cmd).await.unwrap();

    let task_rev = TaskRevision {
        turn_id: 1,
        task_id: 1,
        contract_revision: 1,
        work_revision: 1,
    };
    let generation = handle
        .commands
        .query(|s| Ok(s.state.generation))
        .await
        .unwrap();

    // 1. Command is still Accepted (not Held) -> InvalidCommand
    let err = match handle
        .commands
        .confirm_queue_continuation(conversation(), task_rev.clone(), cmd_id.clone(), generation)
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    };
    assert_eq!(err.code, EngineErrorCode::InvalidCommand);

    // 2. Nonexistent command ID -> NotFound
    let err = match handle
        .commands
        .confirm_queue_continuation(
            conversation(),
            task_rev.clone(),
            CommandId::fresh(),
            generation,
        )
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    };
    assert_eq!(err.code, EngineErrorCode::NotFound);

    // 3. Conversation not owned -> NotOwned
    let mut unowned = conversation();
    unowned.root = SessionId::parse("unowned-root").unwrap();
    let err = match handle
        .commands
        .confirm_queue_continuation(unowned, task_rev.clone(), cmd_id.clone(), generation)
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    };
    assert_eq!(err.code, EngineErrorCode::NotOwned);

    // 4. Put command in Held(TaskReview)
    handle
        .commands
        .query({
            let cmd_id = cmd_id.clone();
            let task_rev = task_rev.clone();
            move |s| {
                let entry = s
                    .state
                    .pending
                    .iter_mut()
                    .find(|e| e.receipt.id == cmd_id)
                    .unwrap();
                entry.receipt.state = CommandState::Held(HoldReason::TaskReview {
                    revision: task_rev,
                    outcome: crate::TaskOutcome::NeedsReview,
                });
                Ok(())
            }
        })
        .await
        .unwrap();

    let permit = handle
        .commands
        .confirm_queue_continuation(conversation(), task_rev, cmd_id.clone(), generation)
        .await
        .unwrap();

    // Cancel the command before permit is consumed
    let q_rev = handle
        .commands
        .queue(&conversation())
        .await
        .unwrap()
        .revision;
    let cancel = EngineCommand {
        id: CommandId::fresh(),
        conversation: conversation(),
        payload: CommandPayload::CancelQueued {
            command_id: cmd_id.clone(),
            expected_queue_revision: q_rev,
        },
    };
    handle.commands.submit(cancel).await.unwrap();

    // Attempting resume_one on cancelled command -> NotFound
    let err = handle.commands.resume_one(permit).await.unwrap_err();
    assert_eq!(err.code, EngineErrorCode::NotFound);

    handle.controls.shutdown().await.unwrap();
}

#[tokio::test]
async fn control_client_stop_root_scope_cancels_all() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = config_for(&dir, &workspace, "http://127.0.0.1:9999/v1".into());
    struct DummyResolver;
    impl TargetResolver for DummyResolver {
        fn resolve(
            &self,
            target: ProviderTarget,
        ) -> BoxFuture<Result<ResolvedTarget, EngineError>> {
            Box::pin(async move {
                Ok(ResolvedTarget::Native(Box::new(ResolvedNative {
                    target: target.clone(),
                    base_url: "http://127.0.0.1:9999/v1".into(),
                    credential: crate::secrets::SecretBytes::new(b"secret".to_vec()).unwrap(),
                    capabilities: ProviderCapabilities {
                        target,
                        safe_boundary_steering: FeatureSupport::Supported,
                        stop_then_send: FeatureSupport::Supported,
                        session_resume: FeatureSupport::Supported,
                        prompt_images: FeatureSupport::Supported,
                        rich_output: FeatureSupport::Supported,
                        native_task_authority: FeatureSupport::Supported,
                        shared_workspace_evidence: FeatureSupport::Supported,
                        inherited_read_only: FeatureSupport::Supported,
                        inherited_native_sandbox: FeatureSupport::Supported,
                        portable_transfer: FeatureSupport::Supported,
                        provider_native_fork: FeatureSupport::Supported,
                        private_host_secret: FeatureSupport::Supported,
                        history_retrieval_tool: FeatureSupport::Supported,
                    },
                })))
            })
        }
    }
    let handle = spawn_orchestrated_session(
        config,
        options(),
        HostServices {
            profiles: Arc::new(DummyResolver),
            external: None,
            pr_watch: None,
            destinations: None,
        },
    )
    .unwrap();

    let run1 = RunId::fresh();
    let run2 = RunId::fresh();
    let conv1 = conversation();
    let mut conv2 = conversation();
    conv2.root = SessionId::parse("root-2").unwrap();

    let flag1 = handle.controls.register_run(run1.clone(), conv1, None);
    let flag2 = handle.controls.register_run(run2.clone(), conv2, None);

    assert!(!flag1.load(Ordering::SeqCst));
    assert!(!flag2.load(Ordering::SeqCst));

    let ack = handle.controls.stop(StopScope::Root).await.unwrap();
    assert_eq!(ack.scope, StopScope::Root);
    assert!(flag1.load(Ordering::SeqCst));
    assert!(flag2.load(Ordering::SeqCst));
    assert_eq!(ack.affected_runs.len(), 2);

    handle.controls.shutdown().await.unwrap();
}
