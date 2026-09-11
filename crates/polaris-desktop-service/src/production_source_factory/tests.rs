//! Private source application placement, policy and retained recovery evidence tests.
use super::*;
use polaris_core::{
    desktop_store::{InitialState, PrototypeRoot, SourceApplyIdentity},
    isolated_run::PreparedWorkspace,
    isolated_workspace::Limits,
};
use polaris_desktop_protocol::{
    ids::*,
    request::*,
    run_state::Observation,
    snapshot::{Configuration, Draft},
};
use std::{fs, os::unix::fs::PermissionsExt};

struct Fixture {
    _temp: tempfile::TempDir,
    _store: PrototypeRoot,
    receipt: TrustedRunCompletion,
    saved: Published,
    grant: Arc<ConfirmedSourceGrant>,
    recovery: PathBuf,
    request: SourceApplyRequest,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let source = base.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("ordinary"), "before").unwrap();
        let helper = base.join("helper");
        fs::write(&helper, "dummy, never executed").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let prepared = Arc::new(
            PreparedWorkspace::prepare(
                &source,
                &helper,
                polaris_core::desktop_execution::SandboxMode::WorkspaceWrite,
                &[],
                Limits {
                    max_entries: 16,
                    max_files: 8,
                    max_file_bytes: 1024,
                    max_total_bytes: 4096,
                    max_depth: 4,
                },
            )
            .unwrap(),
        );
        let root = PrototypeRoot::new().unwrap();
        let mut writer = root
            .create(
                ProjectId::new("project").unwrap(),
                SessionId::new("session").unwrap(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: "input".into(),
                        attachment_ids: vec![],
                    },
                    configuration: Configuration {
                        history_mode: Default::default(),
                        configuration_revision: DecimalU64::new(0),
                        provider: "fake".into(),
                        model: "offline".into(),
                        effort: "none".into(),
                    },
                    policy_revision: DecimalU64::new(0),
                },
            )
            .unwrap();
        let target = RunTarget {
            run_id: RunId::new("run").unwrap(),
            attempt_id: AttemptId::new("attempt").unwrap(),
        };
        writer
            .apply(
                &Request {
                    protocol_version: Default::default(),
                    client_id: ClientId::new("client").unwrap(),
                    request_id: RequestId::new("start").unwrap(),
                    body: RequestBody::RunStart(
                        SessionId::new("session").unwrap(),
                        RunStart {
                            expected_draft_revision: DecimalU64::new(0),
                            expected_configuration_revision: DecimalU64::new(0),
                            expected_policy_revision: DecimalU64::new(0),
                        },
                    ),
                },
                Some(target.clone()),
            )
            .unwrap();
        let operation = OperationId::new("run-operation").unwrap();
        writer.record_intent(&target, operation.clone()).unwrap();
        writer
            .finish_with_messages(
                &target,
                &operation,
                Observation::Succeeded,
                ResultId::new("run-result").unwrap(),
                vec![polaris_provider::Message::assistant("done")],
            )
            .unwrap();
        let saved = writer.snapshot().unwrap();
        let identity = prepared.snapshot().source_identity;
        let receipt = TrustedRunCompletion::for_source_apply_test(prepared, &saved);
        let grant = Arc::new(ConfirmedSourceGrant {
            policy: CurrentSourcePolicy {
                source_path: source,
                source_identity: SourceApplyIdentity {
                    device: DecimalU64::new(identity.device),
                    inode: DecimalU64::new(identity.inode),
                },
                policy_revision: saved.state.policy_revision,
                read_allowed: true,
                write_allowed: true,
            },
            tier: BootstrapTier::ReadCreate,
        });
        let recovery = base.join("recovery");
        fs::create_dir(&recovery).unwrap();
        fs::set_permissions(&recovery, fs::Permissions::from_mode(0o700)).unwrap();
        let request = SourceApplyRequest {
            approval_id: ApprovalId::new("approval").unwrap(),
            operation_id: OperationId::new("apply").unwrap(),
            result_id: ResultId::new("result").unwrap(),
            expected_session_revision: saved.marker.session_revision,
            expires_at_unix_ms: DecimalU64::new(100),
        };
        Self {
            _temp: temp,
            _store: root,
            receipt,
            saved,
            grant,
            recovery,
            request,
        }
    }
    fn factory(&self) -> ProductionSourceFactory {
        let file = File::open(&self.recovery).unwrap();
        let meta = file.metadata().unwrap();
        ProductionSourceFactory::new(
            self.grant.clone(),
            file,
            (meta.dev(), meta.ino()),
            self.recovery.clone(),
        )
        .unwrap()
    }
    fn count(&self) -> usize {
        fs::read_dir(&self.recovery).unwrap().count()
    }
}
#[test]
fn creates_private_pinned_directory_and_never_reuses_operation() {
    let f = Fixture::new();
    let factory = f.factory();
    let (policy, recovery) = factory.prepare(&f.receipt, &f.saved, &f.request).unwrap();
    assert_eq!(policy, f.grant.policy);
    assert_eq!(f.count(), 1);
    assert_eq!(fs::metadata(&recovery.path).unwrap().mode() & 0o7777, 0o700);
    recovery
        .parent
        .validate_for_snapshot(f.receipt.prepared().snapshot())
        .unwrap();
    assert!(factory.prepare(&f.receipt, &f.saved, &f.request).is_err());
    assert_eq!(f.count(), 1);
    drop(recovery);
    drop(factory);
    assert_eq!(f.count(), 1);
}
#[test]
fn wrong_identity_or_nonprivate_base_is_refused_without_children() {
    let f = Fixture::new();
    let file = File::open(&f.recovery).unwrap();
    let m = file.metadata().unwrap();
    assert!(
        ProductionSourceFactory::new(
            f.grant.clone(),
            file,
            (m.dev(), m.ino() + 1),
            f.recovery.clone()
        )
        .is_err()
    );
    fs::set_permissions(&f.recovery, fs::Permissions::from_mode(0o755)).unwrap();
    let file = File::open(&f.recovery).unwrap();
    assert!(
        ProductionSourceFactory::new(
            f.grant.clone(),
            file,
            (m.dev(), m.ino()),
            f.recovery.clone()
        )
        .is_err()
    );
    assert_eq!(f.count(), 0);
}
#[test]
fn stale_coordinates_terminal_revision_and_policy_make_no_directory() {
    for variant in 0..7 {
        let mut f = Fixture::new();
        let factory = f.factory();
        match variant {
            0 => f.saved.marker.project_id = ProjectId::new("other").unwrap(),
            1 => f.saved.marker.session_id = SessionId::new("other").unwrap(),
            2 => f.saved.state.runs[0].run.run_id = RunId::new("other").unwrap(),
            3 => f.saved.state.runs[0].run.attempt_id = AttemptId::new("other").unwrap(),
            4 => f.saved.state.runs[0].result_id = None,
            5 => f.request.expected_session_revision = DecimalU64::new(99),
            _ => f.saved.state.policy_revision = DecimalU64::new(99),
        }
        assert!(factory.prepare(&f.receipt, &f.saved, &f.request).is_err());
        assert_eq!(f.count(), 0);
    }
}
#[test]
fn read_only_grant_and_replaced_source_make_no_directory() {
    let mut f = Fixture::new();
    let mut policy = f.grant.policy.clone();
    policy.write_allowed = false;
    f.grant = Arc::new(ConfirmedSourceGrant {
        policy,
        tier: BootstrapTier::ReadOnly,
    });
    assert!(
        f.factory()
            .prepare(&f.receipt, &f.saved, &f.request)
            .is_err()
    );
    assert_eq!(f.count(), 0);
    let f = Fixture::new();
    let factory = f.factory();
    fs::rename(
        &f.grant.policy.source_path,
        f.grant.policy.source_path.with_extension("old"),
    )
    .unwrap();
    fs::create_dir(&f.grant.policy.source_path).unwrap();
    assert!(factory.prepare(&f.receipt, &f.saved, &f.request).is_err());
    assert_eq!(f.count(), 0);
}
#[test]
fn source_and_copy_descendant_bases_are_refused_before_mkdir() {
    let f = Fixture::new();
    for path in [
        &f.grant.policy.source_path,
        f.receipt.prepared().snapshot().path(),
    ] {
        let base = path.join("recovery-test");
        fs::create_dir(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let file = File::open(&base).unwrap();
        let m = file.metadata().unwrap();
        let factory =
            ProductionSourceFactory::new(f.grant.clone(), file, (m.dev(), m.ino()), base.clone())
                .unwrap();
        assert!(factory.prepare(&f.receipt, &f.saved, &f.request).is_err());
        assert_eq!(fs::read_dir(base).unwrap().count(), 0);
    }
}
#[test]
fn renamed_base_uses_original_fd_and_retains_provenance() {
    let f = Fixture::new();
    let factory = f.factory();
    let moved = f.recovery.with_extension("moved");
    fs::rename(&f.recovery, &moved).unwrap();
    fs::create_dir(&f.recovery).unwrap();
    let (_, recovery) = factory.prepare(&f.receipt, &f.saved, &f.request).unwrap();
    assert!(recovery.path.starts_with(&f.recovery));
    assert_eq!(f.count(), 0);
    let actual = moved.join(recovery.path.file_name().unwrap());
    let m = fs::metadata(actual).unwrap();
    assert_eq!(recovery.parent.identity(), (m.dev(), m.ino()));
    recovery
        .parent
        .validate_for_snapshot(f.receipt.prepared().snapshot())
        .unwrap();
}
#[test]
fn operation_name_is_bounded_for_unicode_and_distinct_operations() {
    let mut f = Fixture::new();
    let factory = f.factory();
    f.request.operation_id = OperationId::new("界".repeat(42)).unwrap();
    let (_, first) = factory.prepare(&f.receipt, &f.saved, &f.request).unwrap();
    f.request.operation_id = OperationId::new("x".repeat(128)).unwrap();
    let (_, second) = factory.prepare(&f.receipt, &f.saved, &f.request).unwrap();
    assert_ne!(first.path, second.path);
    assert_eq!(first.path.file_name().unwrap().len(), 70);
    assert_eq!(second.path.file_name().unwrap().len(), 70);
}

#[test]
fn failure_after_mkdir_retains_directory_and_retry_refuses_reuse() {
    let f = Fixture::new();
    let factory = f.factory();
    let result = factory.prepare_recovery(&f.receipt, &f.saved, &f.request, |base, name| {
        let name = CString::new(name).unwrap();
        // Synthetic post-mkdir validation failure, confined to this dummy child.
        assert_eq!(
            unsafe { libc::fchmodat(base.as_raw_fd(), name.as_ptr(), 0o500, 0) },
            0
        );
    });
    assert!(result.is_err());
    assert_eq!(f.count(), 1);
    let child = fs::read_dir(&f.recovery)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(factory.prepare(&f.receipt, &f.saved, &f.request).is_err());
    assert!(child.is_dir());
}
