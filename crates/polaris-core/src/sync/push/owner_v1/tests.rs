//! Owner-only regression fixtures.  Every subprocess is an owned wrapper or
//! the measured local Git binary; no Keychain item or network endpoint exists.

use super::*;
use crate::{
    project_mutation::{ProjectMutationOwnerV1, open_directory_nofollow},
    sync::{
        participation::{self, Participation, ParticipationReadV2, ParticipationV2},
        share::{ShareLink, ShareManifest, SyncPaths},
    },
};
use polaris_auth::git_credentials::{
    AuthenticationTypeV1, CanonicalHttpsRemoteV1, ExecutableIdentityV1, GitCredentialOwnerV1,
    OptionalAttributeHashV1,
};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const REMOTE: &str = "https://owner-fixture.invalid:443/team/project.git";
const REF: &str = "refs/heads/polaris";

#[derive(Default)]
struct HostLog {
    confirmations: usize,
    recoveries: usize,
    abandons: usize,
    abandon_checks: usize,
    abandon_confirmation: Option<(Operation, SyncDigestV1)>,
    abandon_invocation: Option<(Operation, SyncDigestV1)>,
    deny_confirmation: bool,
    deny_recovery: bool,
    deny_abandon_context: bool,
}

struct FixtureHost(Arc<Mutex<HostLog>>);

impl SyncPushHostV1 for FixtureHost {
    fn require_confirmation_path(&self, _: &SyncPushConfirmationSummaryV1) -> Result<()> {
        let mut log = self.0.lock().unwrap();
        log.confirmations += 1;
        if log.deny_confirmation {
            Err(SyncPushErrorV1::Permission)
        } else {
            Ok(())
        }
    }

    fn require_recovery_invocation(&self, _: Operation) -> Result<()> {
        let mut log = self.0.lock().unwrap();
        log.recoveries += 1;
        if log.deny_recovery {
            Err(SyncPushErrorV1::Permission)
        } else {
            Ok(())
        }
    }

    fn confirm_abandon(&self, operation: Operation, request_hash: SyncDigestV1) -> Result<()> {
        let mut log = self.0.lock().unwrap();
        log.abandons += 1;
        log.abandon_confirmation = Some((operation, request_hash));
        Ok(())
    }

    fn require_abandon_invocation(
        &self,
        operation: Operation,
        request_hash: SyncDigestV1,
    ) -> Result<()> {
        let mut log = self.0.lock().unwrap();
        log.abandon_checks += 1;
        log.abandon_invocation = Some((operation, request_hash));
        if log.deny_abandon_context {
            Err(SyncPushErrorV1::Permission)
        } else {
            Ok(())
        }
    }
}

struct Fixture {
    _temporary: tempfile::TempDir,
    project: PathBuf,
    state: PathBuf,
    home: PathBuf,
    wrapper: PathBuf,
    askpass: PathBuf,
    calls: PathBuf,
    remote_oid: PathBuf,
    remote_repo: PathBuf,
    binding: GitCredentialBindingV1,
    host: Arc<Mutex<HostLog>>,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let project = root.join("project");
        let state = root.join("state");
        let home = root.join("home");
        for directory in [&project, &state, &home] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let marker = project.join(".polaris");
        fs::create_dir(&marker).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o700)).unwrap();

        let calls = root.join("calls");
        let remote_oid = root.join("remote-oid");
        let remote_repo = root.join("remote-repository");
        let wrapper = root.join("bound-git");
        let askpass = root.join("bound-askpass");
        let local_git = if cfg!(target_os = "macos") {
            "/Applications/Xcode.app/Contents/Developer/usr/bin/git"
        } else {
            "/usr/bin/git"
        };
        let quote =
            |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        let script = format!(
            "#!/bin/bash\nCALLS={}\nREMOTE={}\nFIXTURE_REMOTE={}\nLOCAL={}\nprintf '%s\\n' \"$*\" >> \"$CALLS\"\ncommand=\nfor arg in \"$@\"; do case \"$arg\" in ls-remote|push|fetch) command=\"$arg\"; break ;; esac; done\ncase \"$command\" in\n  ls-remote) if [ -f \"$REMOTE\" ]; then printf '%s\\t{}\\n' \"$(/bin/cat \"$REMOTE\")\"; exit 0; fi; exit 2 ;;\n  push) for arg in \"$@\"; do case \"$arg\" in *:{}) printf '%s' \"${{arg%%:*}}\" > \"$REMOTE\" ;; esac; done; exit 0 ;;\n  fetch) args=(); for arg in \"$@\"; do if [ \"$arg\" = '{}' ]; then args+=(\"file://$FIXTURE_REMOTE\"); else args+=(\"$arg\"); fi; done; exec \"$LOCAL\" -c protocol.file.allow=always \"${{args[@]}}\" ;;\nesac\nexec {} \"$@\"\n",
            quote(&calls),
            quote(&remote_oid),
            quote(&remote_repo),
            quote(Path::new(local_git)),
            REF,
            REF,
            REMOTE,
            quote(Path::new(local_git)),
        );
        write_script(&wrapper, &script);
        write_script(&askpass, "#!/bin/sh\nexit 0\n");

        let remote = CanonicalHttpsRemoteV1::parse(REMOTE).unwrap();
        let binding = GitCredentialBindingV1 {
            namespace: "macos-keychain.internet-password.v1".into(),
            credential_item_id: "1".repeat(64),
            credential_item_version: "2".repeat(64),
            remote,
            account: "owner-fixture-account".into(),
            authentication_type: AuthenticationTypeV1::HttpBasic,
            security_domain: OptionalAttributeHashV1::Absent,
            access_group: OptionalAttributeHashV1::Absent,
            synchronizable: false,
            generation: 1,
            git: ExecutableIdentityV1::inspect(&wrapper).unwrap(),
            askpass: ExecutableIdentityV1::inspect(&askpass).unwrap(),
        };
        let fixture = Self {
            _temporary: temporary,
            project,
            state,
            home,
            wrapper,
            askpass,
            calls,
            remote_oid,
            remote_repo,
            binding,
            host: Arc::new(Mutex::new(HostLog::default())),
        };
        fixture.install_participation();
        fixture
    }

    fn install_participation(&self) {
        let part = ParticipationV2 {
            schema_version: 2,
            participation: Participation {
                share: ShareLink {
                    remote: REMOTE.into(),
                    share_id: "owner-fixture-share".into(),
                    invitation_id: "owner-fixture-invitation".into(),
                    expires_unix_seconds: 2,
                },
                manifest: ShareManifest::default(),
                joined_unix_seconds: 1,
                agreed_commit: None,
                applied_commit: None,
                unresolved: Vec::new(),
            },
            credential_binding: self.binding.clone(),
        };
        let lock =
            ProjectMutationOwnerV1::at_store_root(&self.state.join("rewind-workspace")).unwrap();
        let lease = lock.try_mutation().unwrap();
        participation::publish_v2(&self.state, &lease, None, &part).unwrap();
    }

    fn owner(&self, stop_at: Option<&'static str>) -> SyncPushOwnerV1 {
        self.owner_with_fault(stop_at, None)
    }

    fn owner_with_fault(
        &self,
        stop_at: Option<&'static str>,
        participation_fault: Option<&'static str>,
    ) -> SyncPushOwnerV1 {
        SyncPushOwnerV1 {
            project: self.project.clone(),
            state: self.state.clone(),
            state_pin: open_directory_nofollow(&self.state).unwrap(),
            credentials: GitCredentialOwnerV1::new(&self.home, &self.wrapper, &self.askpass)
                .unwrap(),
            host: Box::new(FixtureHost(self.host.clone())),
            test_binding: Some(self.binding.clone()),
            stop_at,
            participation_fault,
        }
    }

    fn calls(&self) -> Vec<String> {
        match fs::read_to_string(&self.calls) {
            Ok(value) => value.lines().map(str::to_owned).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read owned call log: {error}"),
        }
    }

    fn command_count(&self, command: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.split_whitespace().any(|token| token == command))
            .count()
    }

    fn state_bytes(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn collect(path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let metadata = fs::symlink_metadata(path).unwrap();
            if metadata.is_file() {
                out.insert(path.to_owned(), fs::read(path).unwrap());
            } else if metadata.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    collect(&entry.unwrap().path(), out);
                }
            }
        }
        let mut files = BTreeMap::new();
        collect(&self.state, &mut files);
        files
    }

    fn state_inventory(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn collect(path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let metadata = fs::symlink_metadata(path).unwrap();
            if metadata.is_file() {
                out.insert(path.to_owned(), fs::read(path).unwrap());
            } else if metadata.is_dir() {
                out.insert(path.to_owned(), Vec::new());
                for entry in fs::read_dir(path).unwrap() {
                    collect(&entry.unwrap().path(), out);
                }
            }
        }
        let mut entries = BTreeMap::new();
        collect(&self.state, &mut entries);
        entries
    }

    fn write_payload(&self, name: &str, bytes: &[u8]) {
        let path = self.project.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn set_commits_with_exact_cas(&self, agreed: Option<&str>, applied: Option<&str>) {
        let lock =
            ProjectMutationOwnerV1::at_store_root(&self.state.join("rewind-workspace")).unwrap();
        let lease = lock.try_mutation().unwrap();
        let snapshot = participation::read_snapshot(&self.state).unwrap();
        let expected = snapshot.exact_digest;
        let ParticipationReadV2::StaleCredentialBindingOnly(mut part) = snapshot.classification
        else {
            panic!("owned fixture must retain its canonical V2 participation");
        };
        part.participation.agreed_commit = agreed.map(str::to_owned);
        part.participation.applied_commit = applied.map(str::to_owned);
        participation::publish_v2(&self.state, &lease, expected, &part).unwrap();
    }

    fn copy_current_repository_to_fetch_remote(&self) {
        let paths = SyncPaths::under(&self.state);
        copy_owned_git_tree(&paths.git_dir, &self.remote_repo);
        let oid = fs::read_to_string(paths.git_dir.join("refs/heads/polaris")).unwrap();
        fs::write(&self.remote_oid, oid.trim()).unwrap();
        File::open(&self.remote_oid).unwrap().sync_all().unwrap();
        File::open(&self.remote_repo).unwrap().sync_all().unwrap();
    }
}

fn copy_owned_git_tree(source: &Path, target: &Path) {
    fs::create_dir(target).unwrap();
    fs::set_permissions(target, fs::Permissions::from_mode(0o700)).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_owned_git_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
            fs::set_permissions(&to, fs::Permissions::from_mode(0o600)).unwrap();
            File::open(&to).unwrap().sync_all().unwrap();
        }
    }
    File::open(target).unwrap().sync_all().unwrap();
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(ExecutableIdentityV1::inspect(path).unwrap().path, path);
}

#[test]
fn preview_is_read_only_and_uses_only_exact_remote_observation() {
    let fixture = Fixture::new();
    let before = fs::read_dir(&fixture.state).unwrap().count();
    let preview = fixture.owner(None).preview().unwrap();
    assert_eq!(preview.summary().action, SyncPushActionV1::CreateCommit);
    assert_eq!(fixture.calls().len(), 1);
    assert!(
        fixture.calls()[0].ends_with(&format!("ls-remote --refs --exit-code -- {REMOTE} {REF}"))
    );
    assert_eq!(fs::read_dir(&fixture.state).unwrap().count(), before);
}

#[test]
fn unapplied_agreement_blocks_preview_without_commands_confirmation_or_state_changes() {
    for (agreed, applied) in [
        (Some("1111111111111111111111111111111111111111"), None),
        (
            Some("1111111111111111111111111111111111111111"),
            Some("2222222222222222222222222222222222222222"),
        ),
        (None, Some("2222222222222222222222222222222222222222")),
    ] {
        let fixture = Fixture::new();
        fixture.set_commits_with_exact_cas(agreed, applied);
        let before = fixture.state_inventory();
        let calls = fixture.calls();
        let confirmations = fixture.host.lock().unwrap().confirmations;
        assert!(matches!(
            fixture.owner(None).preview(),
            Err(SyncPushErrorV1::Blocked)
        ));
        assert_eq!(fixture.calls(), calls);
        assert_eq!(fixture.host.lock().unwrap().confirmations, confirmations);
        assert_eq!(fixture.state_inventory(), before);
    }
}

#[test]
fn unapplied_agreement_after_preview_blocks_prepare_without_commands_or_state_changes() {
    for (agreed, applied) in [
        (Some("1111111111111111111111111111111111111111"), None),
        (
            Some("1111111111111111111111111111111111111111"),
            Some("2222222222222222222222222222222222222222"),
        ),
        (None, Some("2222222222222222222222222222222222222222")),
    ] {
        let fixture = Fixture::new();
        let preview = fixture.owner(None).preview().unwrap();
        fixture.set_commits_with_exact_cas(agreed, applied);
        let before = fixture.state_inventory();
        let calls = fixture.calls();
        let confirmations = fixture.host.lock().unwrap().confirmations;
        assert!(matches!(
            fixture.owner(None).prepare(preview),
            Err(SyncPushErrorV1::Blocked)
        ));
        assert_eq!(fixture.calls(), calls);
        assert_eq!(fixture.host.lock().unwrap().confirmations, confirmations);
        assert_eq!(fixture.state_inventory(), before);
    }
}

#[test]
fn denied_prepared_token_never_pushes() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    let outcome = fixture
        .owner(None)
        .prepare(preview)
        .unwrap()
        .deny()
        .unwrap();
    assert!(matches!(outcome, SyncPushOutcomeV1::Denied { .. }));
    assert_eq!(fixture.command_count("push"), 0);
}

#[test]
fn dropping_prepared_token_never_pushes() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    drop(fixture.owner(None).prepare(preview).unwrap());
    assert_eq!(fixture.command_count("push"), 0);
}

#[test]
fn dropped_prepared_token_can_only_be_restarted_as_denied_without_send() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    let token = fixture.owner(None).prepare(preview).unwrap();
    let operation = token.summary().operation_id;
    drop(token);
    let outcome = fixture.owner(None).abandon(operation).unwrap();
    assert!(matches!(
        outcome,
        SyncPushOutcomeV1::Denied { operation_id: actual } if actual == operation
    ));
    assert_eq!(fixture.command_count("push"), 0);
}

#[test]
fn stale_payload_rejects_prepare_before_push() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    fixture.write_payload("changed.txt", b"changed after preview");
    assert!(matches!(
        fixture.owner(None).prepare(preview),
        Err(SyncPushErrorV1::Stale)
    ));
    assert_eq!(fixture.command_count("push"), 0);
}

#[test]
fn confirmation_refusal_is_send_zero() {
    let fixture = Fixture::new();
    fixture.host.lock().unwrap().deny_confirmation = true;
    assert!(matches!(
        fixture.owner(None).preview(),
        Err(SyncPushErrorV1::Permission)
    ));
    assert!(fixture.calls().is_empty());
}

#[test]
fn busy_exclusive_lease_blocks_prepare_before_network() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    let lock =
        ProjectMutationOwnerV1::at_store_root(&fixture.state.join("rewind-workspace")).unwrap();
    let _lease = lock.try_mutation().unwrap();
    assert!(matches!(
        fixture.owner(None).prepare(preview),
        Err(SyncPushErrorV1::Blocked)
    ));
    assert_eq!(fixture.calls().len(), 1);
}

#[test]
fn stale_binding_is_send_zero_before_remote_observation() {
    let fixture = Fixture::new();
    let owner = fixture.owner(None);
    fs::set_permissions(&fixture.wrapper, fs::Permissions::from_mode(0o722)).unwrap();
    assert!(matches!(owner.preview(), Err(SyncPushErrorV1::Credential)));
    assert!(fixture.calls().is_empty());
}

#[test]
fn unknown_c10_entry_blocks_prepare_without_changing_inventory_or_management() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    let operation = preview.identity.operation_id;
    let lock =
        ProjectMutationOwnerV1::at_store_root(&fixture.state.join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let management = fixture.owner(None).management(&lease).unwrap();
    let cursor = management.cursor().unwrap();
    let root = fixture.state.join("sync/remote-intents-v2");
    fs::create_dir_all(root.join("archive")).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(root.join("archive"), fs::Permissions::from_mode(0o700)).unwrap();
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(root.join("owner.lock"))
        .unwrap();
    fs::write(root.join("unknown"), b"unknown owned entry").unwrap();
    fs::set_permissions(root.join("unknown"), fs::Permissions::from_mode(0o600)).unwrap();
    let before = fixture.state_inventory();
    drop(management);
    drop(lease);

    assert!(matches!(
        fixture.owner(None).prepare(preview),
        Err(SyncPushErrorV1::Blocked)
    ));
    assert_eq!(fixture.command_count("push"), 0);
    assert_eq!(fixture.state_inventory(), before);

    let lease = lock.try_mutation().unwrap();
    let ManagementEffectInspectOpenV1::Present(inspected) =
        ManagementEffectStoreV1::inspect_open_at_state_dir(
            &fixture.project,
            &fixture.state,
            &lease,
        )
        .unwrap()
    else {
        panic!("the actual management store seeded for this fixture must be present");
    };
    assert_eq!(inspected.cursor(), cursor);
    assert!(
        inspected
            .effects()
            .all(|effect| effect.request.operation_id != operation)
    );
}

#[test]
fn active_c10_terminal_pair_capacity_blocks_before_prepared_effect() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    assert!(matches!(
        preview.prepared.expected_remote,
        ExpectedSyncRemoteOidV1::Missing
    ));
    let lock =
        ProjectMutationOwnerV1::at_store_root(&fixture.state.join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let management = fixture.owner(None).management(&lease).unwrap();
    let mut intents = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    for _ in 0..256 {
        let mut identity = preview.identity.clone();
        identity.operation_id = Operation::random().unwrap();
        let identity = identity.bind_local_request(&preview.prepared).unwrap();
        let mut reservation = intents.reserve_operation(identity.operation_id).unwrap();
        let cursor = management.cursor().unwrap();
        let PrepareManagementEffectOutcomeV1::Prepared(effect) = management
            .prepare(
                &lease,
                cursor,
                request(&identity, cursor.generation()).unwrap(),
            )
            .unwrap()
        else {
            panic!("fresh actual management request must prepare");
        };
        let started = management.start(&lease, &effect).unwrap();
        let unknown = management
            .mark_outcome_unknown(
                &lease,
                &started,
                ManagementUnknownReasonV1::InterruptedAfterStart,
            )
            .unwrap();
        let history = SyncRemoteIntentHistoryV2::no_intent_abandon(NoIntentAbandonEvidenceV2 {
            identity,
            prior_head: preview.prepared.prior_head.clone(),
            current_head: preview.prepared.prior_head.clone(),
            agreed_commit: preview.prepared.agreed_commit.clone(),
            prior_descriptor: preview.prepared.head_tree.clone(),
            current_descriptor: preview.prepared.head_tree.clone(),
            prior_index_digest: preview.prepared.prior_index_digest,
            current_index_digest: preview.prepared.prior_index_digest,
            prior_staging_digest: preview.prepared.prior_staging_digest,
            current_staging_digest: preview.prepared.prior_staging_digest,
            last_observation: SyncRemoteObservationV2::Missing,
            active_inventory_digest: intents.inventory_digest(),
            confirmation: SyncAbandonConfirmationV2::ExplicitUserConfirmation,
        })
        .unwrap();
        intents.publish(&mut reservation, &history).unwrap();
        management
            .reconcile_abandon(
                &lease,
                &unknown,
                management_hash(history.terminal_evidence_hash().unwrap()).unwrap(),
            )
            .unwrap();
    }
    drop(intents);
    drop(management);
    drop(lease);

    let preview = fixture.owner(None).preview().unwrap();
    let operation = preview.identity.operation_id;
    let before = fixture.state_inventory();
    let lease = lock.try_mutation().unwrap();
    let cursor = fixture
        .owner(None)
        .management(&lease)
        .unwrap()
        .cursor()
        .unwrap();
    drop(lease);
    assert!(matches!(
        fixture.owner(None).prepare(preview),
        Err(SyncPushErrorV1::Blocked)
    ));
    assert_eq!(fixture.command_count("push"), 0);
    assert_eq!(fixture.state_inventory(), before);
    let lease = lock.try_mutation().unwrap();
    let ManagementEffectInspectOpenV1::Present(inspected) =
        ManagementEffectStoreV1::inspect_open_at_state_dir(
            &fixture.project,
            &fixture.state,
            &lease,
        )
        .unwrap()
    else {
        panic!("the actual management store seeded for this fixture must be present");
    };
    assert_eq!(inspected.cursor(), cursor);
    assert!(
        inspected
            .effects()
            .all(|effect| effect.request.operation_id != operation)
    );
}

#[test]
fn normal_confirm_performs_one_cas_and_records_remote_head() {
    let fixture = Fixture::new();
    fixture.write_payload("payload.txt", b"owned fixture payload");
    let preview = fixture.owner(None).preview().unwrap();
    let outcome = fixture
        .owner(None)
        .prepare(preview)
        .unwrap()
        .confirm()
        .unwrap();
    let SyncPushOutcomeV1::Completed { commit, .. } = outcome else {
        panic!("confirmation must complete");
    };
    let snapshot = participation::read_snapshot(&fixture.state).unwrap();
    let ParticipationReadV2::StaleCredentialBindingOnly(part) = snapshot.classification else {
        panic!("fixture must retain canonical participation");
    };
    assert_eq!(
        part.participation.agreed_commit.as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(
        part.participation.applied_commit.as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(fixture.command_count("push"), 1);
    assert!(fixture.remote_oid.exists());
}

#[test]
fn completed_archive_then_no_changes_leaves_management_intent_and_participation_bytes_unchanged() {
    let fixture = Fixture::new();
    fixture.write_payload("payload.txt", b"owned fixture payload");
    let preview = fixture.owner(None).preview().unwrap();
    assert!(matches!(
        fixture
            .owner(None)
            .prepare(preview)
            .unwrap()
            .confirm()
            .unwrap(),
        SyncPushOutcomeV1::Completed { .. }
    ));
    let before = fixture.state_bytes();
    let preview = fixture.owner(None).preview().unwrap();
    assert_eq!(preview.summary().action, SyncPushActionV1::NoChanges);
    assert!(matches!(
        fixture
            .owner(None)
            .prepare(preview)
            .unwrap()
            .confirm()
            .unwrap(),
        SyncPushOutcomeV1::NoChanges
    ));
    assert_eq!(fixture.state_bytes(), before);
    assert_eq!(fixture.command_count("push"), 1);
}

#[test]
fn completed_archive_with_canonical_agreement_clear_reconciles_without_new_push() {
    let fixture = Fixture::new();
    fixture.write_payload("payload.txt", b"owned fixture payload");
    let preview = fixture.owner(None).preview().unwrap();
    assert!(matches!(
        fixture
            .owner(None)
            .prepare(preview)
            .unwrap()
            .confirm()
            .unwrap(),
        SyncPushOutcomeV1::Completed { .. }
    ));
    fixture.set_commits_with_exact_cas(None, None);
    let preview = fixture.owner(None).preview().unwrap();
    assert_eq!(
        preview.summary().action,
        SyncPushActionV1::ReconcileKnownHead
    );
    assert!(matches!(
        fixture
            .owner(None)
            .prepare(preview)
            .unwrap()
            .confirm()
            .unwrap(),
        SyncPushOutcomeV1::Completed { .. }
    ));
    assert_eq!(fixture.command_count("push"), 1);
}

#[test]
fn before_send_checkpoint_recovery_observes_only_and_never_auto_resends() {
    let fixture = Fixture::new();
    fixture.write_payload("payload.txt", b"owned fixture payload");
    let preview = fixture.owner(None).preview().unwrap();
    let outcome = fixture
        .owner(Some("before-send"))
        .prepare(preview)
        .unwrap()
        .confirm()
        .unwrap();
    let SyncPushOutcomeV1::OutcomeUnknown { operation_id } = outcome else {
        panic!("before-send interruption must remain unknown");
    };
    let recovered = fixture.owner(None).recover(operation_id).unwrap();
    assert!(matches!(
        recovered,
        SyncPushOutcomeV1::OutcomeUnknown { operation_id: actual } if actual == operation_id
    ));
    assert_eq!(fixture.command_count("push"), 0);
    assert!(fixture.host.lock().unwrap().recoveries >= 1);
}

#[test]
fn before_send_and_created_proof_recover_exact_owned_file_remote_without_push() {
    for checkpoint in ["before-send", "commit"] {
        let fixture = Fixture::new();
        fixture.write_payload("payload.txt", b"owned fixture payload");
        let preview = fixture.owner(None).preview().unwrap();
        let outcome = fixture
            .owner(Some(checkpoint))
            .prepare(preview)
            .unwrap()
            .confirm()
            .unwrap();
        let SyncPushOutcomeV1::OutcomeUnknown { operation_id } = outcome else {
            panic!("{checkpoint} checkpoint must be unknown");
        };
        fixture.copy_current_repository_to_fetch_remote();
        assert!(matches!(
            fixture.owner(None).recover(operation_id).unwrap(),
            SyncPushOutcomeV1::Completed { operation_id: actual, .. } if actual == operation_id
        ));
        assert_eq!(fixture.command_count("push"), 0);
        assert!(fixture.command_count("fetch") >= 1);
    }
}

#[test]
fn stale_confirmation_denies_durable_prepared_request_without_push_or_restart_blocker() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    let token = fixture.owner(None).prepare(preview).unwrap();
    fixture.write_payload("changed-after-prepare", b"stale confirmation");
    assert!(matches!(token.confirm(), Err(SyncPushErrorV1::Stale)));
    assert_eq!(fixture.command_count("push"), 0);
    assert!(fixture.owner(None).preview().is_ok());
}

#[test]
fn started_without_intent_abandon_uses_abandon_context_not_recovery_and_sends_zero() {
    let fixture = Fixture::new();
    let preview = fixture.owner(None).preview().unwrap();
    let outcome = fixture
        .owner(Some("started"))
        .prepare(preview)
        .unwrap()
        .confirm()
        .unwrap();
    let SyncPushOutcomeV1::OutcomeUnknown { operation_id } = outcome else {
        panic!("started checkpoint must leave an explicit unknown operation");
    };
    fixture.host.lock().unwrap().deny_recovery = true;
    let outcome = fixture.owner(None).abandon(operation_id).unwrap();
    assert!(matches!(
        outcome,
        SyncPushOutcomeV1::ReconciledAbandon { operation_id: actual } if actual == operation_id
    ));
    let host = fixture.host.lock().unwrap();
    assert_eq!(host.recoveries, 0);
    assert!(host.abandon_checks >= 1);
    assert_eq!(host.abandon_confirmation, host.abandon_invocation);
    assert_eq!(
        host.abandon_invocation.as_ref().map(|(actual, _)| *actual),
        Some(operation_id)
    );
    assert_eq!(fixture.command_count("push"), 0);
}

#[test]
fn participation_publication_faults_are_partial_complete_then_recover_without_resend() {
    for stage in [
        "body-publish",
        "generation-publish",
        "cleanup-body",
        "cleanup-operation",
        "cleanup-directory",
    ] {
        let fixture = Fixture::new();
        fixture.write_payload("payload.txt", b"owned fixture payload");
        let preview = fixture.owner(None).preview().unwrap();
        let outcome = fixture
            .owner_with_fault(None, Some(stage))
            .prepare(preview)
            .unwrap()
            .confirm()
            .unwrap();
        let SyncPushOutcomeV1::PartialComplete { operation_id } = outcome else {
            panic!("{stage} must preserve a partial-complete recovery record");
        };
        let recovered = fixture.owner(None).recover(operation_id).unwrap();
        assert!(matches!(
            recovered,
            SyncPushOutcomeV1::Completed { operation_id: actual, .. } if actual == operation_id
        ));
        assert_eq!(
            fixture.command_count("push"),
            1,
            "{stage} recovery must not resend"
        );
    }
}

#[test]
fn durable_completed_checkpoints_recover_and_archive_without_resend() {
    for checkpoint in ["receipt", "participation", "management"] {
        let fixture = Fixture::new();
        fixture.write_payload("payload.txt", b"owned fixture payload");
        let preview = fixture.owner(None).preview().unwrap();
        let outcome = fixture
            .owner(Some(checkpoint))
            .prepare(preview)
            .unwrap()
            .confirm()
            .unwrap();
        let SyncPushOutcomeV1::PartialComplete { operation_id } = outcome else {
            panic!("{checkpoint} must leave a durable completed pair");
        };
        assert!(matches!(
            fixture.owner(None).recover(operation_id).unwrap(),
            SyncPushOutcomeV1::Completed { operation_id: actual, .. } if actual == operation_id
        ));
        assert_eq!(
            fixture.command_count("push"),
            1,
            "{checkpoint} must not resend"
        );
    }
}

#[test]
fn legacy_completed_receipt_is_preserved_without_repair_or_resend() {
    let fixture = Fixture::new();
    fixture.write_payload("payload.txt", b"owned fixture payload");
    let preview = fixture.owner(None).preview().unwrap();
    let mut legacy_target = preview.participation.clone();
    let outcome = fixture
        .owner(Some("receipt"))
        .prepare(preview)
        .unwrap()
        .confirm()
        .unwrap();
    let SyncPushOutcomeV1::PartialComplete { operation_id } = outcome else {
        panic!("receipt checkpoint must retain completed history");
    };
    let root = fixture.state.join("sync/remote-intents-v2");
    let histories: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter_map(|path| {
            let bytes = fs::read(&path).ok()?;
            let history = SyncRemoteIntentHistoryV2::from_canonical_bytes(&bytes).ok()?;
            Some((path, history))
        })
        .collect();
    assert_eq!(histories.len(), 1);
    let (path, history) = &histories[0];
    assert_eq!(history.operation_id(), operation_id);
    let SyncRemoteIntentStateV2::Completed { receipt } = history.current().state() else {
        panic!("fixture must contain a completed receipt");
    };
    let mut receipt = receipt.as_ref().clone();
    // Recreate the previous writer's valid receipt and hash chain. It advanced
    // agreed_commit only; malformed bytes would test a different rejection.
    legacy_target.participation.agreed_commit = Some(receipt.intended_commit.as_str().to_owned());
    receipt.participation_digest = SyncDigestV1::digest(&legacy_target.canonical_bytes().unwrap());
    let mut prefix = serde_json::to_value(history).unwrap();
    prefix["records"].as_array_mut().unwrap().pop().unwrap();
    let prefix: SyncRemoteIntentHistoryV2 = serde_json::from_value(prefix).unwrap();
    let legacy = prefix
        .completed(receipt)
        .unwrap()
        .canonical_bytes()
        .unwrap();
    SyncRemoteIntentHistoryV2::from_canonical_bytes(&legacy).unwrap();
    fs::write(path, legacy).unwrap();
    File::open(path).unwrap().sync_all().unwrap();
    let before = fixture.state_inventory();
    let calls = fixture.calls();
    assert!(matches!(
        fixture.owner(None).recover(operation_id),
        Err(SyncPushErrorV1::Protection)
    ));
    assert_eq!(fixture.calls(), calls);
    assert_eq!(fixture.state_inventory(), before);
    assert_eq!(fixture.command_count("push"), 1);
}

#[test]
fn completed_recovery_c12_fault_remains_partial_until_a_healthy_recovery() {
    let fixture = Fixture::new();
    fixture.write_payload("payload.txt", b"owned fixture payload");
    let preview = fixture.owner(None).preview().unwrap();
    let outcome = fixture
        .owner(Some("receipt"))
        .prepare(preview)
        .unwrap()
        .confirm()
        .unwrap();
    let SyncPushOutcomeV1::PartialComplete { operation_id } = outcome else {
        panic!("receipt checkpoint must leave a completed partial pair");
    };
    assert!(matches!(
        fixture
            .owner_with_fault(None, Some("body-publish"))
            .recover(operation_id)
            .unwrap(),
        SyncPushOutcomeV1::PartialComplete { operation_id: actual } if actual == operation_id
    ));
    assert_eq!(fixture.command_count("push"), 1);
    assert!(matches!(
        fixture.owner(None).recover(operation_id).unwrap(),
        SyncPushOutcomeV1::Completed { operation_id: actual, .. } if actual == operation_id
    ));
    assert_eq!(fixture.command_count("push"), 1);
}
