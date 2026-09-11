//! Source-apply controller authorization, single dispatch, and recovery tests.

use super::*;
use polaris_core::{
    desktop_execution::SandboxMode,
    desktop_store::{InitialState, PrototypeRoot},
    isolated_run::PreparedWorkspace,
};
use polaris_desktop_protocol::{
    request::*,
    run_state::Observation,
    snapshot::{Configuration, Draft},
};
use polaris_provider::Message;
use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, Ordering},
};
struct Clock(AtomicU64);
impl SourceApplyClock for Clock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
fn initial() -> InitialState {
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
        policy_revision: DecimalU64::new(7),
    }
}
fn terminal_writer(root: &PrototypeRoot, session: &str) -> Writer {
    let sid = SessionId::new(session).unwrap();
    let mut w = root
        .create(ProjectId::new("project").unwrap(), sid.clone(), initial())
        .unwrap();
    let target = RunTarget {
        run_id: RunId::new("run").unwrap(),
        attempt_id: AttemptId::new("attempt").unwrap(),
    };
    w.apply(
        &Request {
            protocol_version: Default::default(),
            client_id: ClientId::new("client").unwrap(),
            request_id: RequestId::new("start").unwrap(),
            body: RequestBody::RunStart(
                sid,
                RunStart {
                    expected_draft_revision: DecimalU64::new(0),
                    expected_configuration_revision: DecimalU64::new(0),
                    expected_policy_revision: DecimalU64::new(7),
                },
            ),
        },
        Some(target.clone()),
    )
    .unwrap();
    let op = OperationId::new("run-operation").unwrap();
    w.record_intent(&target, op.clone()).unwrap();
    w.finish_with_messages(
        &target,
        &op,
        Observation::Succeeded,
        ResultId::new("run-result").unwrap(),
        vec![Message::assistant("answer")],
    )
    .unwrap();
    w
}
struct Fixture {
    c: SourceApplyController,
    w: Writer,
    root: PrototypeRoot,
    temp: tempfile::TempDir,
    clock: Arc<Clock>,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let source = base.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("ordinary"), "before").unwrap();
        let helper = base.join("helper");
        fs::write(&helper, "never launched").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let limits = Limits {
            max_entries: 1024,
            max_files: 1024,
            max_file_bytes: 1024,
            max_total_bytes: 1048576,
            max_depth: 4,
        };
        let prepared = Arc::new(
            PreparedWorkspace::prepare(&source, &helper, SandboxMode::WorkspaceWrite, &[], limits)
                .unwrap(),
        );
        let identity = prepared.snapshot().source_identity;
        let root = PrototypeRoot::new().unwrap();
        let w = terminal_writer(&root, "session");
        let receipt = TrustedRunCompletion::for_source_apply_test(prepared, &w.snapshot().unwrap());
        fs::write(
            receipt.prepared().snapshot().path().join("ordinary"),
            "after",
        )
        .unwrap();
        let clock = Arc::new(Clock(AtomicU64::new(10)));
        let c = SourceApplyController::new(
            receipt,
            CurrentSourcePolicy {
                source_path: source,
                source_identity: source_identity((identity.device, identity.inode)),
                policy_revision: DecimalU64::new(7),
                read_allowed: true,
                write_allowed: true,
            },
            clock.clone(),
            limits,
        );
        Self {
            c,
            w,
            root,
            temp,
            clock,
        }
    }
    fn rev(&self) -> DecimalU64 {
        self.w.snapshot().unwrap().marker.session_revision
    }
    fn prepare(&mut self) {
        self.try_prepare().unwrap();
    }
    fn try_prepare(&mut self) -> Result<()> {
        self.try_prepare_path(false)
    }
    fn try_prepare_path(&mut self, mismatch: bool) -> Result<()> {
        self.collect_or_prepare(mismatch, false)
    }
    fn collect_or_prepare(&mut self, mismatch: bool, collect_only: bool) -> Result<()> {
        let base = self.temp.path().canonicalize().unwrap();
        let path = base.join("recovery");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let fd = File::open(&path).unwrap();
        fd.sync_all().unwrap();
        File::open(&base).unwrap().sync_all().unwrap();
        use std::os::unix::fs::MetadataExt;
        let m = fd.metadata().unwrap();
        let parent = RecoveryParent::pin(&fd, (m.dev(), m.ino()), &path).unwrap();
        let revision = self.rev();
        let request = SourceApplyRequest {
            approval_id: ApprovalId::new("apply-approval").unwrap(),
            operation_id: OperationId::new("apply-operation").unwrap(),
            result_id: ResultId::new("apply-result").unwrap(),
            expected_session_revision: revision,
            expires_at_unix_ms: DecimalU64::new(100),
        };
        let recovery = PreparedSourceRecovery {
            parent,
            path: if mismatch { PathBuf::from("/x") } else { path },
        };
        if collect_only {
            self.c.collect(request, recovery).map(|_| ())
        } else {
            self.c.prepare(&mut self.w, request, recovery).map(|_| ())
        }
    }

    fn allow(&mut self) {
        let rev = self.rev();
        self.c
            .resolve(&mut self.w, ApprovalDecision::Allow, rev)
            .unwrap();
    }
    fn commit(&mut self) {
        let rev = self.rev();
        let hash = self.c.candidate().unwrap().payload_hash.clone();
        assert_eq!(
            self.c.commit_intent(&mut self.w, rev, &hash).unwrap(),
            IntentReceipt::NewlyPublished
        );
    }
    fn source(&self) -> String {
        fs::read_to_string(self.c.policy.source_path.join("ordinary")).unwrap()
    }
}
#[test]
fn source_apply_precommit_cancel_and_expiry_do_not_apply() {
    for expire in [false, true] {
        let mut f = Fixture::new();
        f.prepare();
        f.allow();
        let rev = f.rev();
        let hash = f.c.candidate().unwrap().payload_hash.clone();
        if expire {
            f.clock.0.store(100, Ordering::SeqCst);
        } else {
            assert_eq!(
                f.c.cancel(&mut f.w, rev).unwrap(),
                SourceApplyState::Invalidated
            );
        }
        assert!(f.c.commit_intent(&mut f.w, rev, &hash).is_err());
        assert!(f.c.apply().is_err());
        assert_eq!(f.source(), "before");
    }
}
#[test]
fn source_apply_postcommit_changes_preserve_report_and_dispatch_once() {
    let mut f = Fixture::new();
    f.prepare();
    f.allow();
    f.commit();
    let rev = f.rev();
    assert_eq!(
        f.c.cancel(&mut f.w, rev).unwrap(),
        SourceApplyState::IntentCommitted
    );
    let mut policy = f.c.policy.clone();
    policy.policy_revision = DecimalU64::new(8);
    policy.write_allowed = false;
    f.c.update_policy(&mut f.w, policy, rev).unwrap();
    f.clock.0.store(1000, Ordering::SeqCst);
    assert!(f.c.apply().unwrap().failure.is_none());
    assert_eq!(f.source(), "after");
    assert!(f.c.apply().is_err());
    let rev = f.rev();
    f.c.save_result(&mut f.w, rev).unwrap();
    let rev = f.rev();
    f.c.save_result(&mut f.w, rev).unwrap();
    assert_eq!(f.c.state(), SourceApplyState::Saved);
    assert!(f.c.report().is_some());
}
#[test]
fn source_apply_wrong_session_rejected_before_cancel_and_recovery() {
    let mut f = Fixture::new();
    f.prepare();
    f.allow();
    f.commit();
    let mut other = terminal_writer(&f.root, "other");
    let rev = other.snapshot().unwrap().marker.session_revision;
    assert!(matches!(
        f.c.cancel(&mut other, rev),
        Err(SourceApplyError::Authority)
    ));
    assert!(!f.c.cancelled);
    assert_eq!(f.c.state(), SourceApplyState::IntentCommitted);
    f.c.apply().unwrap();
    let rev = f.rev();
    assert!(
        f.c.save_result(&mut f.w, DecimalU64::new(rev.get() + 1))
            .is_err()
    );
    assert!(matches!(
        f.c.retry_result_save(&mut other),
        Err(SourceApplyError::Authority)
    ));
    assert_eq!(f.c.state(), SourceApplyState::RecoveryRequired);
    fs::write(f.c.policy.source_path.join("ordinary"), "external later").unwrap();
    f.c.retry_result_save(&mut f.w).unwrap();
    assert_eq!(f.source(), "external later");
    assert_eq!(f.c.state(), SourceApplyState::Saved);
}

#[test]
fn source_apply_existing_intent_never_grants_replay() {
    let mut f = Fixture::new();
    f.prepare();
    f.allow();
    let candidate = f.c.candidate().unwrap().clone();
    let proof = f.c.check_authority().unwrap();
    let guard = f.c.guard(&f.w, f.rev()).unwrap();
    f.w.consume_source_apply_intent(
        f.c.completion.target(),
        &candidate.approval_id,
        &candidate.payload_hash,
        guard,
        proof,
    )
    .unwrap();
    let rev = f.rev();
    assert_eq!(
        f.c.commit_intent(&mut f.w, rev, &candidate.payload_hash)
            .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    assert_eq!(f.c.state(), SourceApplyState::RecoveryRequired);
    f.w.recover().unwrap();
    assert!(f.c.apply().is_err());
    assert!(f.c.retry_result_save(&mut f.w).is_err());
    assert_eq!(f.source(), "before");
}
#[test]
fn source_apply_precommit_policy_and_payload_refuse_writes() {
    for policy_change in [false, true] {
        let mut f = Fixture::new();
        f.prepare();
        f.allow();
        let rev = f.rev();
        if policy_change {
            let mut policy = f.c.policy.clone();
            policy.policy_revision = DecimalU64::new(8);
            policy.write_allowed = false;
            f.c.update_policy(&mut f.w, policy, rev).unwrap();
        }
        let rev = f.rev();
        assert!(
            f.c.commit_intent(&mut f.w, rev, "different payload")
                .is_err()
        );
        assert!(f.c.apply().is_err());
        assert_eq!(f.source(), "before");
    }
}
#[test]
fn source_apply_report_bound_refuses_before_publication() {
    let mut f = Fixture::new();
    // Flat additions are valid and small as a candidate, but each report repeats
    // the full source ancestry. Cross the conservative report bound by one entry.
    let empty = SourceApplyPayload {
        source_path: f.c.policy.source_path.to_str().unwrap().into(),
        source_identity: f.c.policy.source_identity,
        recovery_parent_path: f
            .temp
            .path()
            .canonicalize()
            .unwrap()
            .join("recovery")
            .to_str()
            .unwrap()
            .into(),
        recovery_parent_identity: source_identity((1, 2)),
        entries: vec![],
    };
    let result = ResultId::new("apply-result").unwrap();
    let mut payload = empty;
    let cap = polaris_core::desktop_store::MAX_SOURCE_APPLY_BYTES;
    let mut previous = 0;
    for i in 0..1023 {
        previous = report_upper_bound(&payload, &result);
        payload.entries.push(SourceApplyEntry {
            relative_path: format!("added-{i:04}"),
            before: None,
            after: Some(SourceApplyVersion {
                hash: "a".repeat(64),
                mode: 0o600,
            }),
        });
        if report_upper_bound(&payload, &result) > cap {
            break;
        }
    }
    assert!(previous <= cap);
    assert!(report_upper_bound(&payload, &result) > cap);
    assert!(serde_json::to_vec(&payload).unwrap().len() < cap);
    for entry in &payload.entries {
        fs::write(
            f.c.completion
                .prepared()
                .snapshot()
                .path()
                .join(&entry.relative_path),
            "small",
        )
        .unwrap();
    }
    let rev = f.rev();
    assert!(matches!(f.try_prepare(), Err(SourceApplyError::Candidate)));
    assert_eq!(f.rev(), rev);
    assert!(f.c.apply().is_err());
    assert_eq!(f.source(), "before");
}

#[test]
fn source_apply_ambiguous_publication_retains_ownership_without_dispatch() {
    let mut f = Fixture::new();
    f.prepare();
    f.allow();
    let candidate = f.c.candidate().unwrap().clone();
    let proof = f.c.check_authority().unwrap();
    let guard = f.c.guard(&f.w, f.rev()).unwrap();
    f.w.consume_source_apply_intent(
        f.c.completion.target(),
        &candidate.approval_id,
        &candidate.payload_hash,
        guard,
        proof,
    )
    .unwrap();
    // Inject an unknown outcome at the controller's save-result boundary after
    // real ledger publication. This does not pretend to inject a disk fsync fault.
    assert!(
        f.c.saved::<IntentReceipt>(Err(StoreError::RecoveryRequired))
            .is_err()
    );
    f.w.recover().unwrap();
    assert!(f.c.apply().is_err());
    assert!(f.c.retry_result_save(&mut f.w).is_err());
    assert!(f.c.completion.prepared().snapshot().path().exists());
    assert!(f.c.recovery_parent().is_some());
    assert_eq!(f.source(), "before");
}
#[test]
fn source_apply_ambiguous_result_only_retry_preserves_live_fd() {
    use std::os::fd::AsRawFd;
    let mut f = Fixture::new();
    f.prepare();
    f.allow();
    f.commit();
    f.c.apply().unwrap();
    let fd =
        f.c.report()
            .unwrap()
            .recovery_directory()
            .unwrap()
            .as_raw_fd();
    let rev = f.rev();
    f.c.save_result(&mut f.w, rev).unwrap();
    // Model a lost publication acknowledgement, retaining the original report.
    assert!(f.c.saved::<()>(Err(StoreError::RecoveryRequired)).is_err());
    let before = f.rev();
    fs::write(f.c.policy.source_path.join("ordinary"), "subsequent source").unwrap();
    f.c.retry_result_save(&mut f.w).unwrap();
    assert_eq!(f.rev(), before);
    assert_eq!(
        f.c.report()
            .unwrap()
            .recovery_directory()
            .unwrap()
            .as_raw_fd(),
        fd
    );
    assert_eq!(f.source(), "subsequent source");
    assert!(f.c.apply().is_err());
}

#[test]
fn source_apply_mismatched_recovery_provenance_refuses_before_capture() {
    let mut f = Fixture::new();
    let rev = f.rev();
    assert!(matches!(
        f.try_prepare_path(true),
        Err(SourceApplyError::RecoveryDirectory)
    ));
    assert!(f.c.recovery_parent().is_none());
    assert!(f.c.candidate().is_none());
    assert_eq!(f.rev(), rev);
    assert!(f.c.apply().is_err());
    assert_eq!(f.source(), "before");
}

#[test]
fn source_apply_collection_is_unpublished_and_requires_owner_publication() {
    let mut f = Fixture::new();
    let before = f.w.snapshot().unwrap();
    f.collect_or_prepare(false, true).unwrap();
    assert_eq!(f.c.state(), SourceApplyState::Collected);
    assert_eq!(f.w.snapshot().unwrap().marker, before.marker);
    assert_eq!(f.w.snapshot().unwrap().state, before.state);
    let rev = f.rev();
    assert!(f.c.resolve(&mut f.w, ApprovalDecision::Allow, rev).is_err());
    assert!(f.c.apply().is_err());
    f.c.publish(&mut f.w, rev).unwrap();
    assert_eq!(f.c.state(), SourceApplyState::Pending);
    f.allow();
    f.commit();
    assert!(f.c.apply().unwrap().failure.is_none());
    assert_eq!(f.source(), "after");
}

#[test]
fn source_apply_collection_return_cannot_override_cancel_policy_or_expiry() {
    for change in 0..4 {
        let mut f = Fixture::new();
        f.collect_or_prepare(false, true).unwrap();
        let rev = f.rev();
        match change {
            0 => {
                assert_eq!(
                    f.c.cancel(&mut f.w, rev).unwrap(),
                    SourceApplyState::Invalidated
                );
                assert_eq!(f.rev(), rev);
            }
            1 => {
                let mut policy = f.c.policy.clone();
                policy.write_allowed = false;
                policy.policy_revision = DecimalU64::new(8);
                f.c.update_policy(&mut f.w, policy, rev).unwrap();
            }
            2 => f.clock.0.store(100, Ordering::SeqCst),
            _ => f.w.set_policy_revision(DecimalU64::new(8)).unwrap(),
        }
        let rev = f.rev();
        assert!(f.c.publish(&mut f.w, rev).is_err());
        assert_eq!(f.rev(), rev);
        assert!(!f.c.published);
        assert!(f.c.apply().is_err());
        assert_eq!(f.source(), "before");
        assert!(f.c.recovery_parent().is_some());
        assert!(f.c.completion().prepared().snapshot().path().exists());
    }
}

#[test]
fn source_apply_publication_rechecks_writer_binding_and_source_fd() {
    let mut f = Fixture::new();
    f.collect_or_prepare(false, true).unwrap();
    let mut other = terminal_writer(&f.root, "other");
    let other_rev = other.snapshot().unwrap().marker.session_revision;
    assert!(matches!(
        f.c.publish(&mut other, other_rev),
        Err(SourceApplyError::Authority)
    ));
    assert_eq!(f.c.state(), SourceApplyState::Collected);
    let source = f.c.policy.source_path.clone();
    let moved = source.with_file_name("source-moved");
    fs::rename(&source, &moved).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("ordinary"), "replacement").unwrap();
    let rev = f.rev();
    assert!(matches!(
        f.c.publish(&mut f.w, rev),
        Err(SourceApplyError::RecoveryDirectory)
    ));
    assert_eq!(f.rev(), rev);
    assert!(f.c.apply().is_err());
    assert_eq!(f.source(), "replacement");
    assert_eq!(
        fs::read_to_string(moved.join("ordinary")).unwrap(),
        "before"
    );
}

#[test]
fn source_apply_publication_cannot_refresh_submitted_session_generation() {
    let mut f = Fixture::new();
    f.collect_or_prepare(false, true).unwrap();
    let before = f.w.snapshot().unwrap();
    f.w.apply(
        &Request {
            protocol_version: Default::default(),
            client_id: ClientId::new("client").unwrap(),
            request_id: RequestId::new("later-draft").unwrap(),
            body: RequestBody::DraftUpdate(
                before.marker.session_id.clone(),
                DraftUpdate {
                    expected_draft_revision: before.state.draft.draft_revision,
                    text: "later draft".into(),
                    attachment_ids: vec![],
                },
            ),
        },
        None,
    )
    .unwrap();
    let current = f.rev();
    assert_ne!(current, before.marker.session_revision);
    assert!(matches!(
        f.c.publish(&mut f.w, current),
        Err(SourceApplyError::Authority)
    ));
    assert!(
        f.c.publish(&mut f.w, before.marker.session_revision)
            .is_err()
    );
    assert_eq!(f.rev(), current);
    assert!(!f.c.published);
    assert!(f.c.apply().is_err());
    assert_eq!(f.source(), "before");
}

fn source_answer(f: &Fixture, id: &str, decision: ApprovalDecision) -> Request {
    let candidate = f.c.candidate().unwrap();
    Request {
        protocol_version: Default::default(),
        client_id: ClientId::new("answer-client").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body: RequestBody::SourceApplyResolve(
            f.c.completion.session_id().clone(),
            SourceApplyResolve {
                run_id: candidate.run_id.clone(),
                attempt_id: candidate.attempt_id.clone(),
                approval_id: candidate.approval_id.clone(),
                payload_hash: candidate.payload_hash.clone(),
                expected_session_revision: f.rev(),
                expected_policy_revision: candidate.policy_revision,
                decision,
            },
        ),
    }
}

#[test]
fn source_apply_request_receipt_distinguishes_fresh_and_replayed_answers() {
    let mut f = Fixture::new();
    f.prepare();
    let request = source_answer(&f, "answer", ApprovalDecision::Allow);
    let first = f.c.resolve_request(&mut f.w, &request).unwrap();
    assert!(first.newly_recorded);
    assert_eq!(f.c.state(), SourceApplyState::Allowed);
    let revision = f.rev();
    let replay = f.c.resolve_request(&mut f.w, &request).unwrap();
    assert!(!replay.newly_recorded);
    assert_eq!(replay.acceptance, first.acceptance);
    assert_eq!(f.rev(), revision);
    assert_eq!(f.c.state(), SourceApplyState::Allowed);
    f.commit();
    let revision = f.rev();
    let replay = f.c.resolve_request(&mut f.w, &request).unwrap();
    assert!(!replay.newly_recorded);
    assert_eq!(f.rev(), revision);
    assert_eq!(f.c.state(), SourceApplyState::IntentCommitted);
    f.c.apply().unwrap();
    let replay = f.c.resolve_request(&mut f.w, &request).unwrap();
    assert!(!replay.newly_recorded);
    assert_eq!(f.c.state(), SourceApplyState::ResultPendingSave);
    assert!(f.c.apply().is_err());
}

#[test]
fn source_apply_request_replay_does_not_recreate_missing_local_authority() {
    let mut f = Fixture::new();
    f.prepare();
    let request = source_answer(&f, "external-answer", ApprovalDecision::Allow);
    f.w.resolve_source_apply_request(&request, 10).unwrap();
    let receipt = f.c.resolve_request(&mut f.w, &request).unwrap();
    assert!(!receipt.newly_recorded);
    assert_eq!(f.c.state(), SourceApplyState::Pending);
    let rev = f.rev();
    let hash = f.c.candidate().unwrap().payload_hash.clone();
    assert!(f.c.commit_intent(&mut f.w, rev, &hash).is_err());
    assert!(f.c.apply().is_err());
    assert_eq!(f.source(), "before");
}

#[test]
fn source_apply_typed_answer_rejects_stale_and_mismatched_guards() {
    for mutation in 0..4 {
        let mut f = Fixture::new();
        f.prepare();
        let mut request = source_answer(&f, "stale", ApprovalDecision::Allow);
        let RequestBody::SourceApplyResolve(_, params) = &mut request.body else {
            unreachable!()
        };
        match mutation {
            0 => params.expected_session_revision = DecimalU64::new(0),
            1 => params.expected_policy_revision = DecimalU64::new(0),
            2 => params.run_id = RunId::new("other-run").unwrap(),
            _ => params.payload_hash = "different".into(),
        }
        let rev = f.rev();
        assert!(f.c.resolve_request(&mut f.w, &request).is_err());
        assert_eq!(f.rev(), rev);
        assert_eq!(f.c.state(), SourceApplyState::Pending);
        assert!(f.c.apply().is_err());
        assert_eq!(f.source(), "before");
    }
}

#[test]
fn source_apply_typed_deny_and_conflicting_replay_never_allow_dispatch() {
    let mut f = Fixture::new();
    f.prepare();
    let request = source_answer(&f, "deny", ApprovalDecision::Deny);
    assert!(
        f.c.resolve_request(&mut f.w, &request)
            .unwrap()
            .newly_recorded
    );
    assert_eq!(f.c.state(), SourceApplyState::Invalidated);
    assert!(
        !f.c.resolve_request(&mut f.w, &request)
            .unwrap()
            .newly_recorded
    );
    let mut changed = request.clone();
    let RequestBody::SourceApplyResolve(_, params) = &mut changed.body else {
        unreachable!()
    };
    params.decision = ApprovalDecision::Allow;
    assert!(f.c.resolve_request(&mut f.w, &changed).is_err());
    assert_eq!(f.c.state(), SourceApplyState::RecoveryRequired);
    assert!(f.c.apply().is_err());
    assert!(f.c.recovery_parent().is_some());
    assert_eq!(f.source(), "before");
}
