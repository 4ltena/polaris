use super::*;

fn native_fixture(
    label: &str,
) -> (
    PathBuf,
    PathBuf,
    crate::project_mutation::ProjectMutationOwnerV1,
) {
    use polaris_auth::protection::{WindowsFamilyDirectory, WindowsPrivateRoot};
    use std::time::{SystemTime, UNIX_EPOCH};
    let base = PathBuf::from(
        std::env::var_os("POLARIS_WINDOWS_STORAGE_TEST_ROOT")
            .expect("explicit native fixture root"),
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = base.join(format!(
        "b2-management-{label}-{}-{nonce}",
        std::process::id()
    ));
    WindowsPrivateRoot::create_new(&root).unwrap();
    let directory = WindowsFamilyDirectory::open_existing(&root).unwrap();
    directory
        .child(std::ffi::OsStr::new("project"), true)
        .unwrap();
    directory
        .child(std::ffi::OsStr::new("state"), true)
        .unwrap();
    let project = root.join("project");
    let state = root.join("state");
    let owner = crate::project_mutation::ProjectMutationOwnerV1::at_store_root(
        &state.join("rewind-workspace"),
    )
    .unwrap();
    (project, state, owner)
}
fn native_request(store: &ManagementEffectStoreV1, number: u128) -> PrepareManagementEffectV1 {
    PrepareManagementEffectV1 {
        operation_id: format!("{number:032x}").parse().unwrap(),
        inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
        scope: ManagementEffectScopeV1 {
            project: store.project_scope_hash(),
            session: Some(ManagementEffectHashV1::digest(b"session-a")),
            generation: 7,
            revision: 11,
            permission: ManagementPermissionScopeV1::Project,
            account_binding: None,
        },
        request_hash: ManagementEffectHashV1::digest(format!("request-{number}").as_bytes()),
        rewind: ManagementRewindDispositionV1::IrreversibleLocal,
    }
}
fn native_prepare(
    store: &ManagementEffectStoreV1,
    lease: &ProjectMutationLeaseV1,
    number: u128,
) -> PreparedManagementEffectV1 {
    match store
        .prepare(
            lease,
            store.cursor().unwrap(),
            native_request(store, number),
        )
        .unwrap()
    {
        PrepareManagementEffectOutcomeV1::Prepared(prepared) => prepared,
        _ => panic!("new real operation must prepare"),
    }
}
fn native_limits(records: usize) -> ManagementEffectLimitsV1 {
    ManagementEffectLimitsV1 {
        max_journal_bytes: 1024 * 1024,
        max_records: records,
        max_effects: 16,
        max_record_bytes: 2048,
    }
}
fn native_reopen(
    state: &Path,
    scope: ManagementEffectHashV1,
    lease: &ProjectMutationLeaseV1,
    limits: ManagementEffectLimitsV1,
) -> ManagementEffectStoreV1 {
    ManagementEffectStoreV1::test_open(state, lease.store_root().to_owned(), scope, lease, limits)
        .unwrap()
}

#[test]
fn windows_management_effect_native_lifecycle_replay_denial_and_inspection() {
    let (project, state, owner) = native_fixture("lifecycle");
    let lease = owner.try_mutation().unwrap();
    assert!(matches!(
        ManagementEffectStoreV1::inspect_open_at_state_dir(&project, &state, &lease).unwrap(),
        ManagementEffectInspectOpenV1::Absent
    ));
    let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    let request = native_request(&store, 1);
    let prepared = native_prepare(&store, &lease, 1);
    let started = store.start(&lease, &prepared).unwrap();
    store.start(&lease, &prepared).unwrap();
    let hash = ManagementEffectHashV1::digest(b"actual completion");
    store.complete_started(&lease, &started, hash).unwrap();
    let before = fs::read(&store.journal_path).unwrap();
    store.complete_started(&lease, &started, hash).unwrap();
    assert!(matches!(
        store
            .prepare(&lease, store.cursor().unwrap(), request.clone())
            .unwrap(),
        PrepareManagementEffectOutcomeV1::Existing(_)
    ));
    assert_eq!(fs::read(&store.journal_path).unwrap(), before);
    let denied = native_prepare(&store, &lease, 2);
    store
        .deny(&lease, &denied, ManagementDenialReasonV1::UserDeclined)
        .unwrap();
    assert!(store.start(&lease, &denied).is_err());
    let cursor = store.cursor().unwrap();
    let reopened = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    assert_eq!(reopened.cursor().unwrap(), cursor);
    assert_eq!(
        reopened
            .effect(request.operation_id)
            .unwrap()
            .unwrap()
            .request,
        request
    );
    let ManagementEffectInspectOpenV1::Present(inspected) =
        ManagementEffectStoreV1::inspect_open_at_state_dir(&project, &state, &lease).unwrap()
    else {
        panic!("present")
    };
    inspected.verify_read_snapshot().unwrap();
    assert_eq!(inspected.effects().count(), 2);
    assert_eq!(inspected.cursor(), cursor);
    assert_ne!(inspected.root_identity().unwrap(), (0, 0));
}

#[test]
fn windows_management_effect_native_wrong_lease_scope_stale_and_conflict_preserve_journal() {
    let (project, state, owner) = native_fixture("conflict");
    let lease = owner.try_mutation().unwrap();
    let (_, _, other) = native_fixture("other");
    let wrong = other.try_mutation().unwrap();
    let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    let stale = store.cursor().unwrap();
    let prepared = native_prepare(&store, &lease, 1);
    store
        .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
        .unwrap();
    let before = fs::read(&store.journal_path).unwrap();
    let before_names = names(&store.root);
    let cursor = store.cursor().unwrap();
    assert!(matches!(
        store.prepare(&wrong, cursor, native_request(&store, 2)),
        Err(ManagementEffectStoreError::WrongProjectLease)
    ));
    let mut request = native_request(&store, 2);
    request.scope.project = ManagementEffectHashV1::digest(b"wrong project");
    assert!(matches!(
        store.prepare(&lease, cursor, request),
        Err(ManagementEffectStoreError::WrongProjectScope)
    ));
    assert!(matches!(
        store.prepare(&lease, stale, native_request(&store, 2)),
        Err(ManagementEffectStoreError::StaleCursor)
    ));
    let mut request = native_request(&store, 1);
    request.request_hash = ManagementEffectHashV1::digest(b"changed request");
    assert!(matches!(
        store.prepare(&lease, cursor, request),
        Err(ManagementEffectStoreError::RequestConflict)
    ));
    assert_eq!(fs::read(&store.journal_path).unwrap(), before);
    assert_eq!(names(&store.root), before_names);
    assert_eq!(store.cursor().unwrap(), cursor);
}

#[test]
fn windows_management_effect_native_restart_unknown_and_rescue_preserve_operation() {
    let (project, state, owner) = native_fixture("rescue");
    let lease = owner.try_mutation().unwrap();
    let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    let prepared = native_prepare(&store, &lease, 1);
    store.start(&lease, &prepared).unwrap();
    let legacy = fs::read(&store.journal_path).unwrap();
    let scope = store.project_scope_hash();
    drop(store);
    let reopened = native_reopen(&state, scope, &lease, native_limits(3));
    assert_eq!(reopened.capacity().unwrap().generation, 1);
    assert_eq!(fs::read(&reopened.journal_path).unwrap(), legacy);
    assert!(matches!(
        reopened
            .effect(prepared.operation_id())
            .unwrap()
            .unwrap()
            .state,
        ManagementEffectStateV1::OutcomeUnknown {
            reason: ManagementUnknownReasonV1::ProcessRestart
        }
    ));
    assert!(matches!(
        reopened.prepare(
            &lease,
            reopened.cursor().unwrap(),
            native_request(&reopened, 2)
        ),
        Err(ManagementEffectStoreError::UnresolvedEffect)
    ));
    let unknown = reopened
        .unknown_effect(prepared.operation_id())
        .unwrap()
        .unwrap();
    reopened
        .complete_unknown(
            &lease,
            &unknown,
            ManagementEffectHashV1::digest(b"reconciled receipt"),
        )
        .unwrap();
    assert!(reopened.blocker().unwrap().is_none());
    assert!(reopened.capacity().unwrap().generation >= 1);
    let cursor = reopened.cursor().unwrap();
    let again = native_reopen(&state, scope, &lease, native_limits(3));
    assert_eq!(again.cursor().unwrap(), cursor);
    assert!(again.blocker().unwrap().is_none());
    assert_eq!(fs::read(&again.journal_path).unwrap(), legacy);
}

#[test]
fn windows_management_effect_native_capacity_and_unsupported_inventory_preserve_bytes() {
    let (project, state, owner) = native_fixture("unsupported");
    let lease = owner.try_mutation().unwrap();
    let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    let mut tiny = native_reopen(
        &state,
        store.project_scope_hash(),
        &lease,
        ManagementEffectLimitsV1 {
            max_journal_bytes: 4096,
            max_records: 16,
            max_effects: 8,
            max_record_bytes: 128,
        },
    );
    let before = fs::read(&tiny.journal_path).unwrap();
    assert!(matches!(
        tiny.prepare(&lease, tiny.cursor().unwrap(), native_request(&tiny, 1)),
        Err(ManagementEffectStoreError::StoreFull)
    ));
    tiny.limits = ManagementEffectLimitsV1::default();
    for (number, inventory) in ["C08", "C10", "C12", "D27"].into_iter().enumerate() {
        let mut request = native_request(&tiny, number as u128 + 1);
        request.inventory = ManagementInventoryIdV1::parse(inventory).unwrap();
        if inventory == "C10" {
            request.scope.permission = ManagementPermissionScopeV1::Remote;
            request.scope.account_binding = Some(ManagementEffectHashV1::digest(b"bound account"));
            request.rewind = ManagementRewindDispositionV1::IrreversibleRemote;
        }
        assert_unsupported(tiny.prepare(&lease, tiny.cursor().unwrap(), request));
        assert_eq!(fs::read(&tiny.journal_path).unwrap(), before);
        // Existing test-only legacy writer supplies a structurally valid refusal fixture.
        let (project, state, owner) = native_fixture("unsupported-existing");
        let actual = owner.try_mutation().unwrap();
        let legacy = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &actual).unwrap();
        let mut retained = native_request(&legacy, 1);
        retained.inventory = ManagementInventoryIdV1::parse(inventory).unwrap();
        if inventory == "C10" {
            retained.scope.permission = ManagementPermissionScopeV1::Remote;
            retained.scope.account_binding = Some(ManagementEffectHashV1::digest(b"bound account"));
            retained.rewind = ManagementRewindDispositionV1::IrreversibleRemote;
        }
        legacy
            .test_append_legacy_prepared(&actual, retained)
            .unwrap();
        let body = fs::read(&legacy.journal_path).unwrap();
        let old_names = names(&legacy.root);
        assert_unsupported(ManagementEffectStoreV1::inspect_open_at_state_dir(
            &project, &state, &actual,
        ));
        assert_unsupported(ManagementEffectStoreV1::open_at_state_dir(
            &project, &state, &actual,
        ));
        assert_eq!(fs::read(&legacy.journal_path).unwrap(), body);
        assert_eq!(names(&legacy.root), old_names);
    }
    assert!(tiny.effects().unwrap().is_empty());
}

#[test]
fn windows_management_effect_native_generation_archives_and_cursor_pages() {
    let (project, state, owner) = native_fixture("generation");
    let lease = owner.try_mutation().unwrap();
    let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    let store = native_reopen(&state, store.project_scope_hash(), &lease, native_limits(8));
    for number in 1..=8 {
        let prepared = native_prepare(&store, &lease, number);
        store
            .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
            .unwrap();
    }
    let capacity = store.capacity().unwrap();
    assert!(capacity.generation >= 2);
    assert!(capacity.retained_archive_bytes > 0);
    let cursor = store.cursor().unwrap();
    let mut total = 0;
    for generation in 1..=capacity.generation + 1 {
        let page = store.effects_page(cursor, generation, None, 256).unwrap();
        assert!(page.iter().all(|effect| effect.state.is_terminal()));
        total += page.len();
    }
    assert_eq!(total, 8);
    for number in 1..=8 {
        assert!(
            store
                .effect(native_request(&store, number).operation_id)
                .unwrap()
                .unwrap()
                .state
                .is_terminal()
        );
    }
    let reopened = native_reopen(&state, store.project_scope_hash(), &lease, native_limits(8));
    assert_eq!(reopened.cursor().unwrap(), cursor);
}

#[test]
fn windows_management_effect_native_seal_faults_keep_legacy_orphans_and_pointer_evidence() {
    for point in 1..=6 {
        let (project, state, owner) = native_fixture("fault");
        let lease = owner.try_mutation().unwrap();
        let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        let store = native_reopen(&state, store.project_scope_hash(), &lease, native_limits(8));
        for number in 1..=3 {
            let prepared = native_prepare(&store, &lease, number);
            store
                .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
                .unwrap();
        }
        let legacy = fs::read(&store.journal_path).unwrap();
        store.test_fail_seal_once_at(point);
        assert!(
            store
                .prepare(&lease, store.cursor().unwrap(), native_request(&store, 4))
                .is_err()
        );
        let root = store.root.clone();
        let before_names = names(&root);
        let reopened = native_reopen(&state, store.project_scope_hash(), &lease, native_limits(8));
        let prepared = native_prepare(&reopened, &lease, 4);
        reopened
            .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
            .unwrap();
        assert_eq!(fs::read(&reopened.journal_path).unwrap(), legacy);
        assert_eq!(reopened.capacity().unwrap().generation, 1);
        for name in before_names {
            assert!(root.join(name).exists(), "interrupted evidence retained");
        }
        assert!(root.join("CURRENT").exists());
        if point < 6 {
            assert!(
                names(&root.join("generations"))
                    .iter()
                    .any(|name| name.to_string_lossy().starts_with(".orphan-1-"))
            );
        }
    }
}

#[test]
fn windows_management_effect_native_inspection_detects_restored_writes_and_corrupt_tail() {
    let (project, state, owner) = native_fixture("inspection");
    let lease = owner.try_mutation().unwrap();
    let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    let prepared = native_prepare(&store, &lease, 1);
    store
        .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
        .unwrap();
    let original = fs::read(&store.journal_path).unwrap();
    let ManagementEffectInspectOpenV1::Present(inspected) =
        ManagementEffectStoreV1::inspect_open_at_state_dir(&project, &state, &lease).unwrap()
    else {
        panic!("present")
    };
    fs::write(&store.journal_path, b"changed").unwrap();
    fs::write(&store.journal_path, &original).unwrap();
    assert!(inspected.verify_read_snapshot().is_err());
    drop(inspected);
    let mut torn = original.clone();
    torn.extend_from_slice(b"{torn");
    fs::write(&store.journal_path, &torn).unwrap();
    let ManagementEffectInspectOpenV1::Present(inspected) =
        ManagementEffectStoreV1::inspect_open_at_state_dir(&project, &state, &lease).unwrap()
    else {
        panic!("present")
    };
    assert!(matches!(
        inspected.verify_read_snapshot(),
        Err(ManagementEffectStoreError::TornTail)
    ));
    assert_eq!(fs::read(&store.journal_path).unwrap(), torn);
    drop(inspected);
    let mut corrupt = original;
    corrupt.extend_from_slice(b"{}\n");
    fs::write(&store.journal_path, &corrupt).unwrap();
    assert!(ManagementEffectStoreV1::inspect_open_at_state_dir(&project, &state, &lease).is_err());
    assert!(ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).is_err());
    assert_eq!(fs::read(&store.journal_path).unwrap(), corrupt);
}

fn store(state: &Path) -> ManagementEffectStoreV1 {
    // Unopened configuration data; no lease or durable effect is fabricated.
    ManagementEffectStoreV1 {
        state_dir: state.to_owned(),
        root: state.join("management-effects-v1"),
        journal_path: state.join("management-effects-v1/journal.v1.jsonl"),
        expected_lease_root: state.join("rewind-workspace"),
        project_scope: ManagementEffectHashV1::digest(b"public fixture project"),
        limits: ManagementEffectLimitsV1::default(),
        windows: None,
    }
}

fn assert_unsupported<T>(result: Result<T, ManagementEffectStoreError>) {
    match result {
        Err(ManagementEffectStoreError::Io(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        }
        _ => panic!("unsupported journal operation must fail before accessing storage"),
    }
}

fn assert_reads_unavailable(store: &ManagementEffectStoreV1) {
    let operation: ManagementOperationIdV1 = "00000000000000000000000000000001".parse().unwrap();
    let cursor = ManagementEffectCursorV1 {
        generation: 7,
        chain_hash: ManagementEffectHashV1::digest(b"public fixture chain"),
        sequence: 9,
        journal_hash: ManagementEffectHashV1::digest(b"public fixture journal"),
    };
    assert_unsupported(store.capacity());
    assert_unsupported(store.cursor());
    assert_unsupported(store.effects());
    for limit in [0, usize::MAX] {
        assert_unsupported(store.effects_page(cursor, 7, Some(operation), limit));
    }
    assert_unsupported(store.effect(operation));
    assert_unsupported(store.blocker());
    assert_unsupported(store.prepared_effect(operation));
    assert_unsupported(store.unknown_effect(operation));
}

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn windows_management_effect_unavailable_preserves_absent_storage_and_real_getters() {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("absent-state");
    let before = names(parent.path());
    let store = store(&state);
    assert_eq!(store.state_dir(), state);
    assert_eq!(
        store.project_scope_hash(),
        ManagementEffectHashV1::digest(b"public fixture project")
    );
    assert_reads_unavailable(&store);
    assert!(!state.exists());
    assert_eq!(names(parent.path()), before);
}

#[test]
fn windows_management_effect_unavailable_preserves_existing_journal_bytes_and_names() {
    let state = tempfile::tempdir().unwrap();
    let root = state.path().join("management-effects-v1");
    let nested = root.join("retained");
    fs::create_dir_all(&nested).unwrap();
    let files = [
        root.join("journal.v1.jsonl"),
        root.join("owner.lock"),
        nested.join("retained.bin"),
    ];
    let contents: [&[u8]; 3] = [
        b"retained journal with incomplete tail\0\xff",
        b"retained owner lock",
        b"retained\0\xffnested",
    ];
    for (path, bytes) in files.iter().zip(contents) {
        fs::write(path, bytes).unwrap();
    }
    let before = [names(state.path()), names(&root), names(&nested)];
    let store = store(state.path());
    assert_eq!(store.state_dir(), state.path());
    assert_reads_unavailable(&store);
    for (path, bytes) in files.iter().zip(contents) {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert_eq!([names(state.path()), names(&root), names(&nested)], before);
}

// Fixed v1 wire bytes are hashed independently of the Rust journal reducer.
// These fixtures prove pure replay, not a lease, durable owner or storage admission.
const REPLAY_COMPLETED: &str = r#"{"version":1,"sequence":1,"previous_hash":"0000000000000000000000000000000000000000000000000000000000000000","event":{"event":"prepared","request":{"operation_id":"00000000000000000000000000000001","inventory":"T10","scope":{"project":"f80d536c504cd6b46bda8be35a87ce891fbb356edc13326e6e1d8eae50fd2289","session":"141b7df05288e058f03a5e41744963dd5d137e46a39db7b103e2747b76bf6cfb","generation":7,"revision":11,"permission":"project","account_binding":null},"request_hash":"e3ba056a07cff61cc040132b46a68f0d024ce95f6789ac06ff768405426cacf0","rewind":"irreversible_local"}},"event_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d"}
{"version":1,"sequence":2,"previous_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d","event":{"event":"effect_started","operation_id":"00000000000000000000000000000001"},"event_hash":"08f00f390f685822975a8ce8dd7b354d8a965a26542084bfeb4dcc4df6c1895b"}
{"version":1,"sequence":3,"previous_hash":"08f00f390f685822975a8ce8dd7b354d8a965a26542084bfeb4dcc4df6c1895b","event":{"event":"completed","operation_id":"00000000000000000000000000000001","evidence_hash":"cc639a48441be13adf956a6c782089883da0f4f58faaef1c03bdc83f1c8f2e73"},"event_hash":"6aefd778837e5130a99cf20953aaaa9d97a941edfc44770e4b0245957e6b1d07"}"#;

const REPLAY_RECONCILED: &str = r#"{"version":1,"sequence":1,"previous_hash":"0000000000000000000000000000000000000000000000000000000000000000","event":{"event":"prepared","request":{"operation_id":"00000000000000000000000000000001","inventory":"T10","scope":{"project":"f80d536c504cd6b46bda8be35a87ce891fbb356edc13326e6e1d8eae50fd2289","session":"141b7df05288e058f03a5e41744963dd5d137e46a39db7b103e2747b76bf6cfb","generation":7,"revision":11,"permission":"project","account_binding":null},"request_hash":"e3ba056a07cff61cc040132b46a68f0d024ce95f6789ac06ff768405426cacf0","rewind":"irreversible_local"}},"event_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d"}
{"version":1,"sequence":2,"previous_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d","event":{"event":"effect_started","operation_id":"00000000000000000000000000000001"},"event_hash":"08f00f390f685822975a8ce8dd7b354d8a965a26542084bfeb4dcc4df6c1895b"}
{"version":1,"sequence":3,"previous_hash":"08f00f390f685822975a8ce8dd7b354d8a965a26542084bfeb4dcc4df6c1895b","event":{"event":"outcome_unknown","operation_id":"00000000000000000000000000000001","reason":"process_restart"},"event_hash":"dbb8e2478dac65f5ffab8693b757f60305b2312b74b75316b2d3c18f39185f66"}
{"version":1,"sequence":4,"previous_hash":"dbb8e2478dac65f5ffab8693b757f60305b2312b74b75316b2d3c18f39185f66","event":{"event":"reconciled_abandon","operation_id":"00000000000000000000000000000001","evidence_hash":"d8c467d512be5ba89477e94f71c132cf9fce2832151c8262699bd2638538acf3"},"event_hash":"98df4e74227eab8ef3f969ac0af8beea937aeaeb07915e8645a83eb52b8a8d5c"}"#;

const REPLAY_DENIED: &str = r#"{"version":1,"sequence":1,"previous_hash":"0000000000000000000000000000000000000000000000000000000000000000","event":{"event":"prepared","request":{"operation_id":"00000000000000000000000000000001","inventory":"T10","scope":{"project":"f80d536c504cd6b46bda8be35a87ce891fbb356edc13326e6e1d8eae50fd2289","session":"141b7df05288e058f03a5e41744963dd5d137e46a39db7b103e2747b76bf6cfb","generation":7,"revision":11,"permission":"project","account_binding":null},"request_hash":"e3ba056a07cff61cc040132b46a68f0d024ce95f6789ac06ff768405426cacf0","rewind":"irreversible_local"}},"event_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d"}
{"version":1,"sequence":2,"previous_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d","event":{"event":"denied","operation_id":"00000000000000000000000000000001","reason":"user_declined"},"event_hash":"453c64cc65027e0fd889671c1bdbccbf788ef04d263d82ef8116062278f41835"}"#;

const DUPLICATE_PREPARED: &str = r#"{"version":1,"sequence":2,"previous_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d","event":{"event":"prepared","request":{"operation_id":"00000000000000000000000000000001","inventory":"T10","scope":{"project":"f80d536c504cd6b46bda8be35a87ce891fbb356edc13326e6e1d8eae50fd2289","session":"141b7df05288e058f03a5e41744963dd5d137e46a39db7b103e2747b76bf6cfb","generation":7,"revision":11,"permission":"project","account_binding":null},"request_hash":"e3ba056a07cff61cc040132b46a68f0d024ce95f6789ac06ff768405426cacf0","rewind":"irreversible_local"}},"event_hash":"b3587ca0b360122bfa336ff08e195c85bce5eedf05734bc2f92eb72283249dc5"}"#;

const AFTER_TERMINAL: &str = r#"{"version":1,"sequence":4,"previous_hash":"6aefd778837e5130a99cf20953aaaa9d97a941edfc44770e4b0245957e6b1d07","event":{"event":"effect_started","operation_id":"00000000000000000000000000000001"},"event_hash":"87b5bc830e67fabd42f397419469841492e5f79b10101e3ff46065e67a87410c"}"#;

const START_WITHOUT_PREPARE: &str = r#"{"version":1,"sequence":1,"previous_hash":"0000000000000000000000000000000000000000000000000000000000000000","event":{"event":"effect_started","operation_id":"00000000000000000000000000000001"},"event_hash":"7b4f7def7ff7c9bf3fec4d37a9a45f62ab9a76f86c85106087228371b9fe8c0c"}"#;

const COMPLETE_WITHOUT_START: &str = r#"{"version":1,"sequence":2,"previous_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d","event":{"event":"completed","operation_id":"00000000000000000000000000000001","evidence_hash":"cc639a48441be13adf956a6c782089883da0f4f58faaef1c03bdc83f1c8f2e73"},"event_hash":"680069be05e8faeef261e13122901ad72049c1f5631032f10bb1cf4d7c352fc2"}"#;

const UNKNOWN_WITHOUT_START: &str = r#"{"version":1,"sequence":2,"previous_hash":"e6bdbc3c38aa2bd4d21329a78ab7102eae00815982a7fc0f6fee3f8c43c6a41d","event":{"event":"outcome_unknown","operation_id":"00000000000000000000000000000001","reason":"process_restart"},"event_hash":"f65f14ee76cc83468434908f7cfad355d241c34ac8988ae8d2e2efe60406082f"}"#;

const ABANDON_WITHOUT_UNKNOWN: &str = r#"{"version":1,"sequence":3,"previous_hash":"08f00f390f685822975a8ce8dd7b354d8a965a26542084bfeb4dcc4df6c1895b","event":{"event":"reconciled_abandon","operation_id":"00000000000000000000000000000001","evidence_hash":"d8c467d512be5ba89477e94f71c132cf9fce2832151c8262699bd2638538acf3"},"event_hash":"99df99bd2c914bcf08e633b23ac9cbbc70d7b2a0a243ea4ae576ee70841aca3b"}"#;

const ZERO_COMPLETED: &str = r#"{"version":1,"sequence":3,"previous_hash":"08f00f390f685822975a8ce8dd7b354d8a965a26542084bfeb4dcc4df6c1895b","event":{"event":"completed","operation_id":"00000000000000000000000000000001","evidence_hash":"0000000000000000000000000000000000000000000000000000000000000000"},"event_hash":"5cbfa00e2f017f8342a6bf682ba98b0b90db3fa2351b2a7fcdaa2f2fdd2248e2"}"#;

const ZERO_ABANDONED: &str = r#"{"version":1,"sequence":4,"previous_hash":"dbb8e2478dac65f5ffab8693b757f60305b2312b74b75316b2d3c18f39185f66","event":{"event":"reconciled_abandon","operation_id":"00000000000000000000000000000001","evidence_hash":"0000000000000000000000000000000000000000000000000000000000000000"},"event_hash":"187ccac4102a6407c155e9dafd607b56abfc990657ab8708aa79087353b8e44a"}"#;

fn replay_prefix(wire: &str, records: usize) -> JournalStateV1 {
    let mut state = JournalStateV1::default();
    for line in wire.lines().take(records) {
        state.apply(serde_json::from_str(line).unwrap()).unwrap();
    }
    state
}

fn replay_operation() -> ManagementOperationIdV1 {
    "00000000000000000000000000000001".parse().unwrap()
}

fn replay_request() -> PrepareManagementEffectV1 {
    match serde_json::from_str::<JournalEnvelopeV1>(REPLAY_COMPLETED.lines().next().unwrap())
        .unwrap()
        .event
    {
        JournalEventV1::Prepared { request } => request,
        _ => panic!("fixed wire must begin with Prepared"),
    }
}

fn assert_corrupt_replay(state: &mut JournalStateV1, wire: &str) {
    let cursor = state.cursor();
    let effects = state.effects.clone();
    let records = state.records;
    let events = state.events.len();
    assert!(matches!(
        state.apply(serde_json::from_str(wire).unwrap()),
        Err(ManagementEffectStoreError::CorruptJournal)
    ));
    assert_eq!(state.cursor(), cursor);
    assert_eq!(state.effects, effects);
    assert_eq!(state.records, records);
    assert_eq!(state.events.len(), events);
}

#[test]
fn windows_management_effect_replay_completed_preserves_exact_ids_snapshot_and_cursor() {
    let state = replay_prefix(REPLAY_COMPLETED, 3);
    let request = replay_request();
    assert_eq!(state.records, 3);
    assert_eq!(state.events.len(), 3);
    assert_eq!(state.effects.len(), 1);
    assert_eq!(
        state.effects[&replay_operation()],
        ManagementEffectSnapshotV1 {
            request,
            state: ManagementEffectStateV1::Completed {
                evidence_hash: ManagementEffectHashV1::from_hex(
                    "cc639a48441be13adf956a6c782089883da0f4f58faaef1c03bdc83f1c8f2e73"
                )
                .unwrap(),
            },
            last_sequence: 3,
        }
    );
    assert_eq!(
        state.cursor(),
        ManagementEffectCursorV1 {
            generation: 0,
            chain_hash: ManagementEffectHashV1::zero(),
            sequence: 3,
            journal_hash: ManagementEffectHashV1::from_hex(
                "6aefd778837e5130a99cf20953aaaa9d97a941edfc44770e4b0245957e6b1d07"
            )
            .unwrap(),
        }
    );
    for sequence in 1..=3 {
        assert_eq!(state.events[&sequence].evidence.sequence, sequence);
        assert_eq!(state.events[&sequence].evidence.generation, 0);
    }
}

#[test]
fn windows_management_effect_replay_restart_and_reconciled_abandon_keep_the_same_operation() {
    let unknown = replay_prefix(REPLAY_RECONCILED, 3);
    assert_eq!(
        unknown.effects[&replay_operation()].state,
        ManagementEffectStateV1::OutcomeUnknown {
            reason: ManagementUnknownReasonV1::ProcessRestart,
        }
    );
    assert!(unknown.effects[&replay_operation()].state.blocks_new_work());
    let state = replay_prefix(REPLAY_RECONCILED, 4);
    assert_eq!(
        state.effects[&replay_operation()],
        ManagementEffectSnapshotV1 {
            request: replay_request(),
            state: ManagementEffectStateV1::ReconciledAbandon {
                evidence_hash: ManagementEffectHashV1::from_hex(
                    "d8c467d512be5ba89477e94f71c132cf9fce2832151c8262699bd2638538acf3"
                )
                .unwrap(),
            },
            last_sequence: 4,
        }
    );
    assert!(!state.effects[&replay_operation()].state.blocks_new_work());
    assert_eq!(state.records, 4);
    assert_eq!(state.effects.len(), 1);
    assert_eq!(state.cursor().sequence, 4);
    assert_eq!(
        state.cursor().journal_hash,
        ManagementEffectHashV1::from_hex(
            "98df4e74227eab8ef3f969ac0af8beea937aeaeb07915e8645a83eb52b8a8d5c"
        )
        .unwrap()
    );
}

#[test]
fn windows_management_effect_replay_user_denial_is_a_terminal_snapshot() {
    let state = replay_prefix(REPLAY_DENIED, 2);
    assert_eq!(
        state.effects[&replay_operation()],
        ManagementEffectSnapshotV1 {
            request: replay_request(),
            state: ManagementEffectStateV1::Denied {
                reason: ManagementDenialReasonV1::UserDeclined,
            },
            last_sequence: 2,
        }
    );
    assert!(state.effects[&replay_operation()].state.is_terminal());
    assert_eq!(state.records, 2);
    assert_eq!(
        state.cursor().journal_hash,
        ManagementEffectHashV1::from_hex(
            "453c64cc65027e0fd889671c1bdbccbf788ef04d263d82ef8116062278f41835"
        )
        .unwrap()
    );
}

#[test]
fn windows_management_effect_replay_corrupt_version_sequence_and_hashes_are_rejected() {
    let first = REPLAY_COMPLETED.lines().next().unwrap();
    for (field, value) in [
        ("version", serde_json::json!(2)),
        ("sequence", serde_json::json!(2)),
        (
            "previous_hash",
            serde_json::json!("cc639a48441be13adf956a6c782089883da0f4f58faaef1c03bdc83f1c8f2e73"),
        ),
        (
            "event_hash",
            serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000"),
        ),
    ] {
        let mut wire: serde_json::Value = serde_json::from_str(first).unwrap();
        wire[field] = value;
        assert_corrupt_replay(
            &mut JournalStateV1::default(),
            &serde_json::to_string(&wire).unwrap(),
        );
    }
    let mut wire: serde_json::Value = serde_json::from_str(first).unwrap();
    wire["unrecognized"] = serde_json::json!(true);
    assert!(serde_json::from_value::<JournalEnvelopeV1>(wire).is_err());
}

#[test]
fn windows_management_effect_replay_duplicate_operation_and_terminal_transition_are_rejected() {
    assert_corrupt_replay(&mut replay_prefix(REPLAY_COMPLETED, 1), DUPLICATE_PREPARED);
    assert_corrupt_replay(&mut replay_prefix(REPLAY_COMPLETED, 3), AFTER_TERMINAL);
}

#[test]
fn windows_management_effect_replay_illegal_order_and_zero_evidence_are_rejected() {
    for (prefix, records, wire) in [
        (REPLAY_COMPLETED, 0, START_WITHOUT_PREPARE),
        (REPLAY_COMPLETED, 1, COMPLETE_WITHOUT_START),
        (REPLAY_COMPLETED, 1, UNKNOWN_WITHOUT_START),
        (REPLAY_COMPLETED, 2, ABANDON_WITHOUT_UNKNOWN),
        (REPLAY_COMPLETED, 2, ZERO_COMPLETED),
        (REPLAY_RECONCILED, 3, ZERO_ABANDONED),
    ] {
        assert_corrupt_replay(&mut replay_prefix(prefix, records), wire);
    }
}

#[test]
fn windows_management_effect_replay_cursor_fence_accepts_current_and_rejects_every_stale_field() {
    let state = replay_prefix(REPLAY_COMPLETED, 3);
    let cursor = state.cursor();
    ensure_cursor(&state, cursor).unwrap();
    let stale = [
        ManagementEffectCursorV1 {
            generation: 1,
            ..cursor
        },
        ManagementEffectCursorV1 {
            chain_hash: ManagementEffectHashV1::from_hex(
                "cc639a48441be13adf956a6c782089883da0f4f58faaef1c03bdc83f1c8f2e73",
            )
            .unwrap(),
            ..cursor
        },
        ManagementEffectCursorV1 {
            sequence: 2,
            ..cursor
        },
        ManagementEffectCursorV1 {
            journal_hash: ManagementEffectHashV1::zero(),
            ..cursor
        },
    ];
    for value in stale {
        assert!(matches!(
            ensure_cursor(&state, value),
            Err(ManagementEffectStoreError::StaleCursor)
        ));
    }
    assert_eq!(state.cursor(), cursor);
}

#[test]
fn windows_management_effect_replay_request_validation_keeps_scope_and_inventory_contracts() {
    let request = replay_request();
    validate_request_shape(request.scope.project, &request).unwrap();
    validate_new_inventory_contract(&request).unwrap();
    assert!(matches!(
        validate_request_shape(ManagementEffectHashV1::zero(), &request),
        Err(ManagementEffectStoreError::WrongProjectScope)
    ));
    let mut zero_id = request.clone();
    zero_id.operation_id = "00000000000000000000000000000000".parse().unwrap();
    let mut zero_request = request.clone();
    zero_request.request_hash = ManagementEffectHashV1::zero();
    let mut zero_session = request.clone();
    zero_session.scope.session = Some(ManagementEffectHashV1::zero());
    let mut zero_account = request.clone();
    zero_account.scope.account_binding = Some(ManagementEffectHashV1::zero());
    for invalid in [zero_id, zero_request, zero_session, zero_account] {
        assert!(matches!(
            validate_request_shape(request.scope.project, &invalid),
            Err(ManagementEffectStoreError::InvalidValue)
        ));
    }
    let mut wrong_permission = request.clone();
    wrong_permission.scope.permission = ManagementPermissionScopeV1::Remote;
    let mut wrong_rewind = request.clone();
    wrong_rewind.rewind = ManagementRewindDispositionV1::CapturedRestorable;
    let mut wrong_account = request.clone();
    wrong_account.scope.account_binding = Some(request.request_hash);
    for invalid in [wrong_permission, wrong_rewind, wrong_account] {
        assert!(matches!(
            validate_new_inventory_contract(&invalid),
            Err(ManagementEffectStoreError::InvalidInventoryContract)
        ));
    }
}
