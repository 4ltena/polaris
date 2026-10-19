//! Windows composite sync admission and namespace refusal tests that preserve state.
use super::*;

struct NativeFixture {
    _serial: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    project: std::path::PathBuf,
    state: std::path::PathBuf,
    owner: crate::project_mutation::ProjectMutationOwnerV1,
}
impl NativeFixture {
    fn new(label: &str) -> Self {
        use polaris_auth::protection::{WindowsFamilyDirectory, WindowsPrivateRoot};
        use std::{
            ffi::OsStr,
            time::{SystemTime, UNIX_EPOCH},
        };
        let serial = crate::project::DATA_DIR_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = std::path::PathBuf::from(
            std::env::var_os("POLARIS_WINDOWS_STORAGE_TEST_ROOT")
                .expect("explicit native fixture root"),
        );
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = base.join(format!(
            "b2-composite-{label}-{}-{nonce}",
            std::process::id()
        ));
        WindowsPrivateRoot::create_new(&root).unwrap();
        let directory = WindowsFamilyDirectory::open_existing(&root).unwrap();
        directory.child(OsStr::new("project"), true).unwrap();
        let project = std::fs::canonicalize(root.join("project")).unwrap();
        let states = directory.child(OsStr::new("state"), true).unwrap();
        let id = crate::project::project_id(&project);
        states.child(OsStr::new(&id), true).unwrap();
        let state = std::fs::canonicalize(root.join("state").join(id)).unwrap();
        let owner = crate::project_mutation::ProjectMutationOwnerV1::at_store_root(
            &state.join("rewind-workspace"),
        )
        .unwrap();
        let previous = std::env::var_os("POLARIS_DATA_DIR");
        unsafe { std::env::set_var("POLARIS_DATA_DIR", &root) };
        Self {
            _serial: serial,
            previous,
            project,
            state,
            owner,
        }
    }
}
impl Drop for NativeFixture {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("POLARIS_DATA_DIR", value),
                None => std::env::remove_var("POLARIS_DATA_DIR"),
            }
        }
    }
}

#[test]
fn windows_composite_native_absence_and_terminal_generic_admission() {
    use crate::management_effect::*;
    let fixture = NativeFixture::new("terminal");
    let lease = fixture.owner.try_mutation().unwrap();
    let before = names(&fixture.state);
    C10CompositeBlockerV1::require_clear_with_mutation(&fixture.project, &fixture.state, &lease)
        .unwrap();
    assert_eq!(names(&fixture.state), before);
    let store =
        ManagementEffectStoreV1::open_at_state_dir(&fixture.project, &fixture.state, &lease)
            .unwrap();
    let request = PrepareManagementEffectV1 {
        operation_id: "00000000000000000000000000000001".parse().unwrap(),
        inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
        scope: ManagementEffectScopeV1 {
            project: store.project_scope_hash(),
            session: Some(ManagementEffectHashV1::digest(b"session-a")),
            generation: 7,
            revision: 11,
            permission: ManagementPermissionScopeV1::Project,
            account_binding: None,
        },
        request_hash: ManagementEffectHashV1::digest(b"actual management request"),
        rewind: ManagementRewindDispositionV1::IrreversibleLocal,
    };
    let prepared = match store
        .prepare(&lease, store.cursor().unwrap(), request)
        .unwrap()
    {
        PrepareManagementEffectOutcomeV1::Prepared(value) => value,
        _ => panic!("new prepare"),
    };
    assert!(
        C10CompositeBlockerV1::inspect_with_mutation(&fixture.project, &fixture.state, &lease)
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        C10CompositeBlockerV1::require_clear_with_mutation(
            &fixture.project,
            &fixture.state,
            &lease
        ),
        Err(C10CompositeErrorV1::Blocked)
    ));
    let started = store.start(&lease, &prepared).unwrap();
    store
        .complete_started(
            &lease,
            &started,
            ManagementEffectHashV1::digest(b"actual receipt"),
        )
        .unwrap();
    let journal = fixture.state.join("management-effects-v1/journal.v1.jsonl");
    let bytes = std::fs::read(&journal).unwrap();
    let before = names(&fixture.state);
    C10CompositeBlockerV1::require_clear_with_mutation(&fixture.project, &fixture.state, &lease)
        .unwrap();
    assert_eq!(std::fs::read(journal).unwrap(), bytes);
    assert_eq!(names(&fixture.state), before);
}

#[test]
fn windows_composite_native_wrong_project_state_and_activity_refuse() {
    let fixture = NativeFixture::new("binding");
    let lease = fixture.owner.try_mutation().unwrap();
    let other = fixture.project.parent().unwrap().join("other-project");
    polaris_auth::protection::WindowsPrivateRoot::create_new(&other).unwrap();
    assert!(C10CompositeBlockerV1::inspect_with_mutation(&other, &fixture.state, &lease).is_err());
    assert!(
        C10CompositeBlockerV1::inspect_with_mutation(&fixture.project, &other, &lease).is_err()
    );
    assert_invalid(&fixture.project, &fixture.state);
    let before = names(&fixture.state);
    drop(lease);
    let activity = fixture.owner.try_activity().unwrap();
    assert!(
        C10CompositeBlockerV1::inspect_with_activity(&fixture.project, &fixture.state, &activity)
            .is_err()
    );
    assert!(
        inspect_c10_pairs(
            &fixture.project,
            &fixture.state,
            ProjectReadLeaseV1::Activity(&activity),
            &ManagementEffectInspectOpenV1::Absent
        )
        .is_err()
    );
    assert_eq!(names(&fixture.state), before);
    assert!(!fixture.state.join("management-effects-v1").exists());
}

#[test]
fn windows_composite_native_unsupported_namespaces_preserve_empty_and_retained() {
    use polaris_auth::protection::WindowsFamilyDirectory;
    for name in ["sync", "apply-intents-v1", "SYNC", "APPLY-INTENTS-V1"] {
        let fixture = NativeFixture::new("namespace");
        let lease = fixture.owner.try_mutation().unwrap();
        let state = WindowsFamilyDirectory::open_existing(&fixture.state).unwrap();
        let namespace = state.child(std::ffi::OsStr::new(name), true).unwrap();
        let before = names(&fixture.state);
        assert!(
            C10CompositeBlockerV1::inspect_with_mutation(&fixture.project, &fixture.state, &lease)
                .is_err()
        );
        assert_eq!(names(&fixture.state), before);
        let mut file = namespace
            .open_file(std::ffi::OsStr::new("retained.tmp"), true, true)
            .unwrap();
        std::io::Write::write_all(&mut file, b"unsupported\0retained\xff").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let bytes = std::fs::read(fixture.state.join(name).join("retained.tmp")).unwrap();
        assert!(
            C10CompositeBlockerV1::require_clear_with_mutation(
                &fixture.project,
                &fixture.state,
                &lease
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(fixture.state.join(name).join("retained.tmp")).unwrap(),
            bytes
        );
        assert_eq!(names(&fixture.state), before);
        assert!(!fixture.state.join("management-effects-v1").exists());
    }
}

#[test]
fn windows_composite_native_namespace_files_and_foreign_entries_refuse_without_repair() {
    for name in ["sync", "apply-intents-v1"] {
        let fixture = NativeFixture::new("foreign");
        let lease = fixture.owner.try_mutation().unwrap();
        let path = fixture.state.join(name);
        std::fs::write(&path, b"foreign\0retained\xff").unwrap();
        let before = names(&fixture.state);
        assert!(
            C10CompositeBlockerV1::inspect_with_mutation(&fixture.project, &fixture.state, &lease)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign\0retained\xff");
        assert_eq!(names(&fixture.state), before);
        assert!(!fixture.state.join("management-effects-v1").exists());
    }
}

fn assert_invalid(project: &Path, state: &Path) {
    assert!(matches!(
        C10CompositeBlockerV1::inspect_readonly(project, state),
        Err(C10CompositeErrorV1::Invalid)
    ));
}

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn windows_sync_composite_inspection_refuses_before_creating_state_or_lock() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("absent-project");
    let state = root.path().join("absent-state");
    let before = names(root.path());
    assert_invalid(&project, &state);
    assert_invalid(Path::new("relative-project"), Path::new("relative-state"));
    assert!(!project.exists());
    assert!(!state.exists());
    assert_eq!(names(root.path()), before);
}

#[test]
fn windows_sync_composite_inspection_refuses_and_preserves_existing_names_and_bytes() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let rewind = state.join("rewind-workspace");
    let sync = state.join("sync");
    for path in [&state, &rewind, &sync] {
        std::fs::create_dir(path).unwrap();
    }
    let files = [
        root.path().join("marker.bin"),
        rewind.join("owner.lock"),
        rewind.join("namespace.json"),
        sync.join("retained.bin"),
    ];
    let contents: [&[u8]; 4] = [
        b"retained\0\xffmarker",
        b"retained-lock",
        b"retained-namespace",
        b"retained\0\xffsync",
    ];
    for (path, bytes) in files.iter().zip(contents) {
        std::fs::write(path, bytes).unwrap();
    }
    let before = [
        names(root.path()),
        names(&state),
        names(&rewind),
        names(&sync),
    ];
    assert_invalid(root.path(), &state);
    assert_invalid(&files[0], &files[2]);
    for (path, bytes) in files.iter().zip(contents) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(
        [
            names(root.path()),
            names(&state),
            names(&rewind),
            names(&sync)
        ],
        before
    );
}
