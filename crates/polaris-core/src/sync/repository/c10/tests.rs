//! Owned filesystem/Git fixtures only. Never contact Keychain or a remote.
use super::*;
use crate::{
    management_effect::{
        ManagementEffectHashV1 as Hash, ManagementEffectScopeV1, ManagementEffectStoreV1,
        ManagementInventoryIdV1, ManagementPermissionScopeV1, ManagementRewindDispositionV1,
        ManagementUnknownReasonV1, PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1,
    },
    project_mutation::ProjectMutationOwnerV1,
    sync::{
        owner::C10IntentReservationV2,
        remote_intent::{
            C10CreatedCommitReopenProofV2, ExpectedSyncRemoteOidV1, SyncBeforeSendV2,
            SyncOutcomeUnknownV2, SyncPreviousStateV2, SyncRemoteIntentHistoryV2,
            SyncRemoteObservationV2, SyncUnknownReasonV2, c10_test_identity,
        },
    },
};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};

struct Fixture {
    _temporary: tempfile::TempDir,
    project: PathBuf,
    state: PathBuf,
    git: ExecutableIdentityV1,
}
struct Transaction {
    lease: ProjectMutationLeaseV1,
    management: ManagementEffectStoreV1,
    started: StartedManagementEffectV1,
    history: SyncRemoteIntentHistoryV2,
    reservation: C10IntentReservationV2,
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let project = root.join("project");
        let state = root.join("state");
        for path in [&project, &state] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // Pin project resolution to this owned fixture rather than an
        // ancestor project marker in the host's temporary directory.
        fs::create_dir(project.join(".polaris")).unwrap();
        fs::set_permissions(project.join(".polaris"), fs::Permissions::from_mode(0o700)).unwrap();
        let git_path = if cfg!(target_os = "macos") {
            Path::new("/Applications/Xcode.app/Contents/Developer/usr/bin/git")
        } else {
            Path::new("/usr/bin/git")
        };
        let git = ExecutableIdentityV1::inspect(git_path).expect("required bound Git");
        Self {
            _temporary: temporary,
            project,
            state,
            git,
        }
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.project.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn payload(&self) -> C10PayloadSnapshotV1 {
        C10PayloadSnapshotV1::capture(&self.project, &ShareManifest::default()).unwrap()
    }
    fn inspection(&self) -> C10RepositoryInspectionV1 {
        C10LocalRepositoryV1::inspect(&self.state, &self.git).unwrap()
    }
    fn snapshot(&self) -> C10RepositorySnapshotV1 {
        match self.inspection() {
            C10RepositoryInspectionV1::Absent(a) => a.snapshot_clean().unwrap(),
            C10RepositoryInspectionV1::Existing(r) => r.snapshot_clean().unwrap(),
        }
    }
    fn transaction(
        &self,
        number: u64,
        payload: &C10PayloadSnapshotV1,
        snapshot: &C10RepositorySnapshotV1,
    ) -> Transaction {
        let owner =
            ProjectMutationOwnerV1::at_store_root(&self.state.join("rewind-workspace")).unwrap();
        let lease = owner.try_mutation().unwrap();
        let management =
            ManagementEffectStoreV1::open_at_state_dir(&self.project, &self.state, &lease).unwrap();
        let mut identity = c10_test_identity(number);
        identity.project = SyncDigestV1::digest(self.project.as_os_str().as_encoded_bytes());
        identity.payload = payload.descriptor.clone();
        identity.expected_remote_oid = ExpectedSyncRemoteOidV1::Missing;
        identity.account_binding.git = self.git.clone();
        let prepared = SyncLocalPreparedV2 {
            payload: identity.payload.clone(),
            prior_head: snapshot.head.clone(),
            head_tree: snapshot.descriptor.clone(),
            agreed_commit: None,
            expected_remote: identity.expected_remote_oid.clone(),
            prior_index_digest: snapshot.index_digest,
            prior_staging_digest: snapshot.staging_digest,
        };
        let identity = identity.bind_local_request(&prepared).unwrap();
        let request = PrepareManagementEffectV1 {
            operation_id: identity.operation_id,
            inventory: ManagementInventoryIdV1::parse("C10").unwrap(),
            scope: ManagementEffectScopeV1 {
                project: Hash::from_hex(&identity.project.as_hex()).unwrap(),
                session: None,
                generation: 1,
                revision: 0,
                permission: ManagementPermissionScopeV1::Remote,
                account_binding: Some(identity.management_account_binding_hash().unwrap()),
            },
            request_hash: Hash::from_hex(&identity.request_hash.as_hex()).unwrap(),
            rewind: ManagementRewindDispositionV1::IrreversibleRemote,
        };
        let prepared_effect = match management
            .prepare(&lease, management.cursor().unwrap(), request)
            .unwrap()
        {
            PrepareManagementEffectOutcomeV1::Prepared(p) => p,
            _ => panic!("fresh operation"),
        };
        let started = management.start(&lease, &prepared_effect).unwrap();
        let history = SyncRemoteIntentHistoryV2::local_prepared(identity, prepared).unwrap();
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&self.state, &lease).unwrap();
        let mut reservation = store.reserve_operation(history.operation_id()).unwrap();
        store.publish(&mut reservation, &history).unwrap();
        drop(store);
        Transaction {
            lease,
            management,
            started,
            history,
            reservation,
        }
    }
    fn repository(&self, tx: &Transaction) -> C10LocalRepositoryV1 {
        match self.inspection() {
            C10RepositoryInspectionV1::Absent(repository) => repository
                .initialize_after_local_prepared(
                    &tx.management,
                    &tx.started,
                    &tx.lease,
                    &self.payload(),
                )
                .unwrap(),
            C10RepositoryInspectionV1::Existing(repository) => repository,
        }
    }
    fn first_commit(&self) -> (C10LocalRepositoryV1, C10CommitReadbackV1, Transaction) {
        let payload = self.payload();
        let tx = self.transaction(1, &payload, &self.snapshot());
        let repository = self.repository(&tx);
        let (repository, readback) = repository
            .commit_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
            .unwrap();
        (repository, readback, tx)
    }
    fn install_empty_controls(&self) {
        let source = Fixture::new();
        let payload = source.payload();
        let tx = source.transaction(1, &payload, &source.snapshot());
        let repository = source.repository(&tx);
        drop(repository);
        tx.finish();
        fs::rename(source.state.join("sync"), self.state.join("sync")).unwrap();
    }
    fn inventory(&self) -> BTreeMap<PathBuf, (u64, u32, Vec<u8>)> {
        fn walk(path: &Path, out: &mut BTreeMap<PathBuf, (u64, u32, Vec<u8>)>) {
            let metadata = fs::symlink_metadata(path).unwrap();
            out.insert(
                path.to_owned(),
                (
                    metadata.ino(),
                    metadata.mode(),
                    if metadata.is_file() {
                        fs::read(path).unwrap()
                    } else {
                        Vec::new()
                    },
                ),
            );
            if metadata.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    walk(&entry.unwrap().path(), out);
                }
            }
        }
        let mut output = BTreeMap::new();
        walk(&self.state, &mut output);
        walk(&self.project, &mut output);
        output
    }
}
impl Transaction {
    fn finish(self) {
        self.management
            .complete_started(
                &self.lease,
                &self.started,
                Hash::digest(b"synthetic foundation terminal"),
            )
            .unwrap();
    }
    fn publish(&mut self, state: SyncRemoteIntentHistoryV2) {
        let mut store =
            C10IntentStoreV2::inspect_at_state_dir(self.management.state_dir(), &self.lease)
                .unwrap();
        store.publish(&mut self.reservation, &state).unwrap();
        self.history = state;
    }
    fn mark_unknown(
        &mut self,
        intended: Option<&C10CommitReadbackV1>,
    ) -> UnknownManagementEffectV1 {
        if let Some(intended) = intended {
            self.publish(
                self.history
                    .before_send(SyncBeforeSendV2 {
                        intended_commit: intended.commit.clone(),
                        reopened_descriptor: intended.descriptor.clone(),
                        expected_remote: self
                            .history
                            .current()
                            .identity()
                            .expected_remote_oid
                            .clone(),
                    })
                    .unwrap(),
            );
        }
        let unknown = self
            .management
            .mark_outcome_unknown(
                &self.lease,
                &self.started,
                ManagementUnknownReasonV1::InterruptedAfterStart,
            )
            .unwrap();
        self.publish(
            self.history
                .outcome_unknown(SyncOutcomeUnknownV2 {
                    previous_state: if intended.is_some() {
                        SyncPreviousStateV2::BeforeSend
                    } else {
                        SyncPreviousStateV2::LocalPrepared
                    },
                    intended_commit: intended.map(|readback| readback.commit.clone()),
                    created_commit_proof: None,
                    last_observation: SyncRemoteObservationV2::Missing,
                    reason: SyncUnknownReasonV2::AbandonRequested,
                })
                .unwrap(),
        );
        unknown
    }
}
#[test]
fn c10_repository_absent_preview_writes_zero_bytes_and_entries() {
    let f = Fixture::new();
    f.write("shared.txt", b"payload");
    let before = f.inventory();
    let payload = f.payload();
    assert_eq!(payload.descriptor.entry_count(), 1);
    assert_eq!(f.snapshot(), empty_snapshot());
    assert_eq!(f.inventory(), before);
}
#[test]
fn c10_repository_shared_payload_skips_only_manifest_exclusions() {
    let f = Fixture::new();
    f.write("visible", b"a");
    f.write(".env.local", b"not shared");
    f.write("target/cache", b"not shared");
    f.write("other/local", b"b");
    let payload = C10PayloadSnapshotV1::capture(
        &f.project,
        &ShareManifest {
            version: 1,
            excluded: vec!["other".into()],
        },
    )
    .unwrap();
    assert_eq!(payload.descriptor.entry_count(), 1);
    assert_eq!(payload.excluded_entries(), 4);
}
#[test]
fn c10_repository_full_twenty_thousand_payload_count_is_preserved() {
    let f = Fixture::new();
    for index in 0..MAX_FILES {
        f.write(&format!("file-{index:05}"), b"x");
    }
    assert_eq!(f.payload().descriptor.entry_count(), MAX_FILES);
    f.write("overflow", b"x");
    assert!(matches!(
        C10PayloadSnapshotV1::capture(&f.project, &ShareManifest::default()),
        Err(C10RepositoryErrorV1::Bounds)
    ));
}
/// Guard compatibility only: this does not commit or prove 20k-file durability.
#[test]
fn c10_repository_actual_full_twenty_thousand_index_over_one_mib_passes_child_guard() {
    let f = Fixture::new();
    for index in 0..MAX_FILES {
        f.write(&format!("guard-compatible-file-{index:05}"), b"x");
    }
    let payload = f.payload();
    assert_eq!(payload.descriptor().entry_count(), MAX_FILES);
    let tx = f.transaction(1, &payload, &f.snapshot());
    let repository = f.repository(&tx);
    let blob = repository
        .oid_output(&[
            "hash-object".into(),
            "-w".into(),
            "--no-filters".into(),
            "--".into(),
            f.project
                .join(&payload.files[0].entry.path)
                .into_os_string(),
        ])
        .unwrap();
    assert_eq!(blob.as_str(), payload.files[0].git_oid);
    for batch in batches(&payload.files).unwrap() {
        let mut args = vec!["update-index".into(), "--add".into()];
        for file in batch {
            args.push("--cacheinfo".into());
            args.push(format!("100644,{},{}", file.git_oid, file.entry.path).into());
        }
        assert!(
            args.iter()
                .map(|arg: &OsString| arg.as_bytes().len() + 1)
                .sum::<usize>()
                <= ARG_BATCH_BYTES
        );
        let output = repository.run_fixture_child(&args).unwrap();
        assert!(
            output.success(),
            "the bound actual Git must generate every index batch"
        );
    }
    let before = fs::symlink_metadata(&repository.paths.index).unwrap();
    let bytes = fs::read(&repository.paths.index).unwrap();
    assert!(bytes.len() > 1024 * 1024);
    assert_eq!(
        u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize,
        MAX_FILES
    );
    assert_eq!(validate_full_index(&bytes).unwrap(), MAX_FILES);
    let output = repository.run_fixture_child(&["--version".into()]).unwrap();
    assert!(output.success());
    assert!(output.stdout().starts_with(b"git version "));
    let after = fs::symlink_metadata(&repository.paths.index).unwrap();
    assert!(same_metadata(&before, &after));
    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.mode(), after.mode());
    assert_eq!(fs::read(&repository.paths.index).unwrap(), bytes);
}

#[test]
fn c10_repository_per_file_bound_is_inclusive_and_overflow_blocks() {
    let f = Fixture::new();
    let path = f.project.join("large");
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.set_len(MAX_FILE_BYTES).unwrap();
    file.sync_all().unwrap();
    assert_eq!(f.payload().descriptor.total_bytes(), MAX_FILE_BYTES);
    file.set_len(MAX_FILE_BYTES + 1).unwrap();
    assert!(matches!(
        C10PayloadSnapshotV1::capture(&f.project, &ShareManifest::default()),
        Err(C10RepositoryErrorV1::Bounds)
    ));
}
#[test]
fn c10_repository_total_bound_is_inclusive_and_overflow_blocks() {
    let f = Fixture::new();
    for index in 0..16 {
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(f.project.join(format!("part-{index}")))
            .unwrap();
        file.set_len(MAX_FILE_BYTES).unwrap();
    }
    assert_eq!(f.payload().descriptor.total_bytes(), MAX_TOTAL_BYTES);
    f.write("overflow", b"x");
    assert!(matches!(
        C10PayloadSnapshotV1::capture(&f.project, &ShareManifest::default()),
        Err(C10RepositoryErrorV1::Bounds)
    ));
}
#[test]
fn c10_repository_empty_directories_do_not_consume_file_slots() {
    let f = Fixture::new();
    for index in 0..64 {
        fs::create_dir(f.project.join(format!("empty-{index}"))).unwrap();
    }
    assert!(f.payload().descriptor.is_empty());
}
#[test]
fn c10_repository_source_symlink_and_hardlink_are_refused() {
    for hardlink in [false, true] {
        let f = Fixture::new();
        f.write("shared", b"value");
        if hardlink {
            fs::hard_link(f.project.join("shared"), f.project.join("other")).unwrap();
        } else {
            symlink(f.project.join("shared"), f.project.join("other")).unwrap();
        }
        assert!(matches!(
            C10PayloadSnapshotV1::capture(&f.project, &ShareManifest::default()),
            Err(C10RepositoryErrorV1::Protection)
        ));
    }
}
#[test]
fn c10_repository_source_content_and_mode_drift_are_refused() {
    for mode in [false, true] {
        let f = Fixture::new();
        f.write("shared", b"value");
        let snapshot = f.payload();
        if mode {
            fs::set_permissions(f.project.join("shared"), fs::Permissions::from_mode(0o700))
                .unwrap();
        } else {
            f.write("shared", b"other");
        }
        assert!(snapshot.revalidate().is_err());
    }
}
#[test]
fn c10_repository_parent_symlink_never_selects_the_control_store() {
    let f = Fixture::new();
    let alias = f._temporary.path().join("alias");
    symlink(&f.state, &alias).unwrap();
    assert!(C10LocalRepositoryV1::inspect(&alias, &f.git).is_err());
}
#[test]
fn c10_repository_partial_controls_are_not_treated_as_fully_absent() {
    let f = Fixture::new();
    fs::create_dir(f.state.join("sync")).unwrap();
    fs::create_dir(f.state.join("sync/repository")).unwrap();
    let before = f.inventory();
    assert!(matches!(
        C10LocalRepositoryV1::inspect(&f.state, &f.git),
        Err(C10RepositoryErrorV1::Corrupt)
    ));
    assert_eq!(before, f.inventory());
}

#[test]
fn c10_repository_real_commit_is_durable_and_existing_preview_writes_zero() {
    let f = Fixture::new();
    f.write("payload.txt", b"raw payload");
    let (repository, readback, _tx) = f.first_commit();
    assert!(readback.created());
    assert_eq!(readback.descriptor().entry_count(), 1);
    let before = f.inventory();
    let snapshot = repository.snapshot_clean().unwrap();
    assert_eq!(
        snapshot.head(),
        &SyncPriorHeadV2::Present {
            commit: readback.commit.clone()
        }
    );
    assert_eq!(snapshot.descriptor(), readback.descriptor());
    drop(repository);
    assert!(matches!(
        f.inspection(),
        C10RepositoryInspectionV1::Existing(_)
    ));
    assert_eq!(f.inventory(), before);
}
#[test]
fn c10_repository_literal_paths_modes_and_attributes_bind_raw_blob_bytes() {
    let f = Fixture::new();
    f.write(
        ".gitattributes",
        b"* text eol=crlf\n*.txt filter=unregistered\n",
    );
    for name in [
        "-leading.txt",
        ":(glob)*.txt",
        "comma,path.txt",
        "dir/[brackets].txt",
    ] {
        f.write(name, b"line\n");
    }
    f.write("run.sh", b"#!/bin/sh\nexit 0\n");
    fs::set_permissions(f.project.join("run.sh"), fs::Permissions::from_mode(0o700)).unwrap();
    let (repository, readback, _) = f.first_commit();
    assert_eq!(readback.descriptor().entry_count(), 6);
    assert_eq!(
        fs::read(repository.paths.work_tree.join("-leading.txt")).unwrap(),
        b"line\n"
    );
    assert!(repository.snapshot_clean().is_ok());
}
#[test]
fn c10_repository_same_tree_reuses_head_without_an_extra_commit() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, readback, tx) = f.first_commit();
    let commit = readback.commit.clone();
    tx.finish();
    let payload = f.payload();
    let snapshot = repository.snapshot_clean().unwrap();
    let tx = f.transaction(2, &payload, &snapshot);
    let (repository, readback) = repository
        .commit_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
        .unwrap();
    assert!(!readback.created());
    assert_eq!(readback.commit(), &commit);
    assert_eq!(
        repository.snapshot_clean().unwrap().head(),
        &SyncPriorHeadV2::Present { commit }
    );
}
#[test]
fn c10_repository_config_same_inode_drift_blocks_before_next_child() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, _, _) = f.first_commit();
    let path = repository.paths.git_dir.join("config");
    let before = fs::metadata(&path).unwrap().ino();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"[credential]\nhelper=bad\n").unwrap();
    assert_eq!(fs::metadata(path).unwrap().ino(), before);
    assert!(matches!(
        repository.probe_git_features(),
        Err(C10RepositoryErrorV1::Changed)
    ));
    assert!(matches!(
        repository.snapshot_clean(),
        Err(C10RepositoryErrorV1::Changed)
    ));
}
#[test]
fn c10_repository_unknown_control_symlink_and_hardlink_block() {
    for variant in 0..3 {
        let f = Fixture::new();
        f.write("payload", b"one");
        let (repository, _, _) = f.first_commit();
        let target = repository.paths.git_dir.join(if variant == 0 {
            "unknown"
        } else {
            "description"
        });
        if variant == 0 {
            fs::write(&target, b"unknown").unwrap();
        } else if variant == 1 {
            symlink(repository.paths.git_dir.join("config"), &target).unwrap();
        } else {
            fs::hard_link(repository.paths.git_dir.join("config"), &target).unwrap();
        }
        assert!(repository.snapshot_clean().is_err());
    }
}
#[test]
fn c10_repository_head_parse_failure_is_never_unborn() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, _, _) = f.first_commit();
    fs::write(repository.paths.git_dir.join("HEAD"), b"malformed\n").unwrap();
    assert!(matches!(
        repository.snapshot_clean(),
        Err(C10RepositoryErrorV1::Corrupt)
    ));
}
#[test]
fn c10_repository_raw_content_drift_cannot_hide_behind_the_git_stat_cache() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, _, _) = f.first_commit();
    fs::write(repository.paths.work_tree.join("payload"), b"two").unwrap();
    assert!(repository.snapshot_clean().is_err());
}
#[test]
fn c10_repository_mutation_requires_durable_local_prepared_and_current_started() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let payload = f.payload();
    let tx = f.transaction(1, &payload, &f.snapshot());
    let repository = f.repository(&tx);
    tx.management
        .complete_started(&tx.lease, &tx.started, Hash::digest(b"terminal"))
        .unwrap();
    let before = f.inventory();
    assert!(matches!(
        repository.commit_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload),
        Err(C10RepositoryErrorV1::Changed)
    ));
    assert_eq!(before, f.inventory());
}
#[test]
fn c10_repository_restore_unborn_reconstructs_clean_empty_index_and_staging() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, readback, mut tx) = f.first_commit();
    let unknown = tx.mark_unknown(Some(&readback));
    let expected = SyncPriorHeadV2::Present {
        commit: readback.commit.clone(),
    };
    let (repository, proof) = repository
        .restore_after_unknown(&f.project, &tx.management, &unknown, &tx.lease, &expected)
        .unwrap();
    assert_eq!(proof.restored_head(), &SyncPriorHeadV2::Unborn);
    assert!(proof.descriptor().is_empty());
    assert_eq!(repository.snapshot_clean().unwrap(), empty_snapshot());
}
#[test]
fn c10_repository_restore_prior_tree_uses_exact_bound_git_attribute_source() {
    let f = Fixture::new();
    f.write(".gitattributes", b"* text eol=crlf\n");
    f.write("payload.txt", b"one\n");
    let (repository, first, tx) = f.first_commit();
    let prior = first.commit.clone();
    tx.finish();
    f.write("payload.txt", b"two\n");
    f.write("later.txt", b"later\n");
    let payload = f.payload();
    let tx = f.transaction(2, &payload, &repository.snapshot_clean().unwrap());
    let (repository, second) = repository
        .commit_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
        .unwrap();
    let mut tx = tx;
    let unknown = tx.mark_unknown(Some(&second));
    let expected = SyncPriorHeadV2::Present {
        commit: second.commit.clone(),
    };
    let (repository, proof) = repository
        .restore_after_unknown(&f.project, &tx.management, &unknown, &tx.lease, &expected)
        .unwrap();
    assert_eq!(
        proof.restored_head(),
        &SyncPriorHeadV2::Present { commit: prior }
    );
    assert_eq!(
        fs::read(repository.paths.work_tree.join("payload.txt")).unwrap(),
        b"one\n"
    );
    assert!(!repository.paths.work_tree.join("later.txt").exists());
    assert!(repository.snapshot_clean().is_ok());
}
#[test]
fn c10_repository_restore_current_cas_mismatch_preserves_all_bytes() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, readback, mut tx) = f.first_commit();
    let unknown = tx.mark_unknown(Some(&readback));
    let before = f.inventory();
    let mismatch = SyncPriorHeadV2::Present {
        commit: SyncGitObjectIdV1::parse("c".repeat(40)).unwrap(),
    };
    assert!(matches!(
        repository.restore_after_unknown(
            &f.project,
            &tx.management,
            &unknown,
            &tx.lease,
            &mismatch
        ),
        Err(C10RepositoryErrorV1::Changed)
    ));
    assert_eq!(before, f.inventory());
}
#[test]
fn c10_repository_all_commit_faults_are_reached_and_never_return_a_proof() {
    for point in [
        FaultPoint::Staging,
        FaultPoint::Index,
        FaultPoint::CommitObject,
        FaultPoint::Reference,
        FaultPoint::Reopen,
        FaultPoint::Readback,
    ] {
        let f = Fixture::new();
        f.write("payload", b"one");
        let payload = f.payload();
        let tx = f.transaction(1, &payload, &f.snapshot());
        let repository = f.repository(&tx);
        FAILURE.with(|failure| failure.set(Some(point)));
        assert!(
            repository
                .commit_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
                .is_err()
        );
        assert!(
            FAILURE.with(|failure| failure.get().is_none()),
            "specified crash point must be reached"
        );
        assert!(
            tx.management
                .effect(tx.started.operation_id())
                .unwrap()
                .unwrap()
                .state
                .blocks_new_work()
        );
    }
}
#[test]
fn c10_repository_durable_reopen_faults_never_mint_a_created_commit_capability() {
    for point in [
        FaultPoint::FileFsync,
        FaultPoint::DirectoryFsync,
        FaultPoint::Reopen,
        FaultPoint::Readback,
    ] {
        let f = Fixture::new();
        f.write("payload", b"one");
        let (repository, _, tx) = f.first_commit();
        let unknown = tx
            .management
            .mark_outcome_unknown(
                &tx.lease,
                &tx.started,
                ManagementUnknownReasonV1::InterruptedAfterStart,
            )
            .unwrap();
        FAILURE.with(|failure| failure.set(Some(point)));
        assert!(
            repository
                .durable_current_after_unknown(&f.project, &tx.management, &unknown, &tx.lease)
                .is_err()
        );
        assert!(
            FAILURE.with(|failure| failure.get().is_none()),
            "specified durability point must be reached"
        );
    }
}
#[test]
fn c10_repository_commit_crash_before_before_send_reopens_before_unknown_record() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, _, mut tx) = f.first_commit();
    let unknown = tx
        .management
        .mark_outcome_unknown(
            &tx.lease,
            &tx.started,
            ManagementUnknownReasonV1::InterruptedAfterStart,
        )
        .unwrap();
    let (repository, readback) = repository
        .durable_current_after_unknown(&f.project, &tx.management, &unknown, &tx.lease)
        .unwrap();
    assert!(readback.created());
    let current = readback.commit.clone();
    let proof = C10CreatedCommitReopenProofV2::from_git_readback(readback).unwrap();
    tx.publish(
        tx.history
            .outcome_unknown_after_reopen(
                SyncOutcomeUnknownV2 {
                    previous_state: SyncPreviousStateV2::LocalPrepared,
                    intended_commit: Some(current.clone()),
                    created_commit_proof: None,
                    last_observation: SyncRemoteObservationV2::Missing,
                    reason: SyncUnknownReasonV2::AbandonRequested,
                },
                proof,
            )
            .unwrap(),
    );
    let expected = SyncPriorHeadV2::Present { commit: current };
    let (_, proof) = repository
        .restore_after_unknown(&f.project, &tx.management, &unknown, &tx.lease, &expected)
        .unwrap();
    assert_eq!(proof.restored_head(), &SyncPriorHeadV2::Unborn);
}
#[test]
fn c10_repository_partial_restore_and_reopen_failure_return_no_restore_proof() {
    for point in [
        FaultPoint::Restoration,
        FaultPoint::FileFsync,
        FaultPoint::Reopen,
        FaultPoint::Readback,
    ] {
        let f = Fixture::new();
        f.write("payload", b"one");
        let (repository, readback, mut tx) = f.first_commit();
        let unknown = tx.mark_unknown(Some(&readback));
        let expected = SyncPriorHeadV2::Present {
            commit: readback.commit.clone(),
        };
        FAILURE.with(|failure| failure.set(Some(point)));
        assert!(
            repository
                .restore_after_unknown(&f.project, &tx.management, &unknown, &tx.lease, &expected)
                .is_err()
        );
        assert!(
            FAILURE.with(|failure| failure.get().is_none()),
            "specified restoration point must be reached"
        );
        assert!(
            tx.management
                .effect(tx.started.operation_id())
                .unwrap()
                .unwrap()
                .state
                .blocks_new_work()
        );
    }
}
#[test]
fn c10_repository_batch_limits_include_path_bytes_and_record_overhead() {
    let f = Fixture::new();
    for index in 0..300 {
        f.write(&format!("long-name-{index:05}-{}", "x".repeat(100)), b"x");
    }
    let payload = f.payload();
    let groups =
        batches_with_prefix(&payload.files, f.state.as_os_str().as_bytes().len() + 32).unwrap();
    assert!(groups.len() > 1);
    for group in groups {
        assert!(
            group
                .iter()
                .map(|file| file.entry.path.len() + f.state.as_os_str().as_bytes().len() + 32 + 100)
                .sum::<usize>()
                <= ARG_BATCH_BYTES
        );
        assert!(
            group
                .iter()
                .map(|file| file.entry.path.len() + 64)
                .sum::<usize>()
                <= OUTPUT_BATCH_BYTES
        );
    }
}

#[test]
fn c10_repository_ancestor_proof_is_read_only_and_rejects_reverse_order() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, first, tx) = f.first_commit();
    let first_oid = first.commit.clone();
    tx.finish();
    f.write("payload", b"two");
    let payload = f.payload();
    let tx = f.transaction(2, &payload, &repository.snapshot_clean().unwrap());
    let (repository, second) = repository
        .commit_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
        .unwrap();
    let before = f.inventory();
    repository
        .require_ancestor(&first_oid, second.commit())
        .unwrap();
    assert!(
        repository
            .require_ancestor(second.commit(), &first_oid)
            .is_err()
    );
    assert_eq!(before, f.inventory());
}

#[test]
fn c10_repository_final_control_eof_addition_is_detected_and_preserved() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, _, _) = f.first_commit();
    let metadata = repository.repository.directory.metadata().unwrap();
    let path = repository.paths.git_dir.join("unknown");
    let injected = path.clone();
    AFTER_EOF.with(|slot| {
        *slot.borrow_mut() = Some((
            (metadata.dev(), metadata.ino()),
            Box::new(move || {
                let mut file = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&injected)
                    .unwrap();
                file.write_all(b"preserved unknown evidence").unwrap();
            }),
        ))
    });
    assert!(matches!(
        repository.snapshot_clean(),
        Err(C10RepositoryErrorV1::Changed)
    ));
    assert!(AFTER_EOF.with(|slot| slot.borrow().is_none()));
    assert_eq!(fs::read(path).unwrap(), b"preserved unknown evidence");
}
#[test]
fn c10_repository_initialization_fault_keeps_the_effect_blocked_and_explicitly_recovers() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let payload = f.payload();
    let mut tx = f.transaction(1, &payload, &f.snapshot());
    let C10RepositoryInspectionV1::Absent(absent) = f.inspection() else {
        panic!("initially absent")
    };
    FAILURE.with(|failure| failure.set(Some(FaultPoint::Initialization)));
    assert!(
        absent
            .initialize_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
            .is_err()
    );
    assert!(FAILURE.with(|failure| failure.get().is_none()));
    let unknown = tx.mark_unknown(None);
    let C10RepositoryInspectionV1::Existing(repository) = f.inspection() else {
        panic!("Git's complete control prefix is retained")
    };
    let (_, proof) = repository
        .restore_after_unknown(
            &f.project,
            &tx.management,
            &unknown,
            &tx.lease,
            &SyncPriorHeadV2::Unborn,
        )
        .unwrap();
    assert_eq!(proof.restored_head(), &SyncPriorHeadV2::Unborn);
    assert!(
        tx.management
            .effect(tx.started.operation_id())
            .unwrap()
            .unwrap()
            .state
            .blocks_new_work()
    );
}
#[test]
fn c10_repository_control_drift_after_child_is_always_after_spawn() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (mut repository, _, _) = f.first_commit();
    let helper = f.state.parent().unwrap().join("owned-fake-git");
    let config = repository.paths.git_dir.join("config");
    let script = format!(
        "#!/bin/sh\nprintf '\\n[credential]\\nhelper = rejected\\n' >> '{}'\nprintf 'git version 2.50.1\\n'\n",
        config.display()
    );
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&helper)
        .unwrap();
    file.write_all(script.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    repository.git = ExecutableIdentityV1::inspect(&helper).unwrap();
    assert!(matches!(
        repository.git(&["--version"]),
        Err(C10RepositoryErrorV1::AfterSpawn)
    ));
    assert!(fs::read(config).unwrap().ends_with(b"helper = rejected\n"));
}

#[test]
fn c10_repository_crlf_and_binary_blobs_reopen_and_restore_without_conversion() {
    let f = Fixture::new();
    f.write(
        ".gitattributes",
        b"* text eol=lf\n*.bin filter=unregistered working-tree-encoding=UTF-16\n",
    );
    f.write("payload.txt", b"one\r\nraw\r\n");
    let mut binary = vec![0u8; MAX_FILE_BYTES as usize];
    for (index, byte) in binary.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    f.write("payload.bin", &binary);
    let expected = f.payload().descriptor.clone();
    let (repository, first, tx) = f.first_commit();
    assert_eq!(first.descriptor(), &expected);
    assert_eq!(
        fs::read(repository.paths.work_tree.join("payload.txt")).unwrap(),
        b"one\r\nraw\r\n"
    );
    assert_eq!(
        fs::read(repository.paths.work_tree.join("payload.bin")).unwrap(),
        binary
    );
    assert_eq!(repository.snapshot_clean().unwrap().descriptor(), &expected);
    let prior = first.commit.clone();
    tx.finish();
    f.write("payload.txt", b"two\r\n");
    f.write("payload.bin", b"changed\0binary");
    let payload = f.payload();
    let mut tx = f.transaction(2, &payload, &repository.snapshot_clean().unwrap());
    let (repository, second) = repository
        .commit_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
        .unwrap();
    let unknown = tx.mark_unknown(Some(&second));
    let current = SyncPriorHeadV2::Present {
        commit: second.commit.clone(),
    };
    let (repository, restored) = repository
        .restore_after_unknown(&f.project, &tx.management, &unknown, &tx.lease, &current)
        .unwrap();
    assert_eq!(
        restored.restored_head(),
        &SyncPriorHeadV2::Present { commit: prior }
    );
    assert_eq!(restored.descriptor(), &expected);
    assert_eq!(
        fs::read(repository.paths.work_tree.join("payload.txt")).unwrap(),
        b"one\r\nraw\r\n"
    );
    assert_eq!(
        fs::read(repository.paths.work_tree.join("payload.bin")).unwrap(),
        binary
    );
}

#[test]
fn c10_repository_empty_attribute_source_is_bound_and_read_only_without_stored_empty_object() {
    let f = Fixture::new();
    f.write(".gitattributes", b"* text eol=lf\n");
    f.write("payload.txt", b"raw\r\n");
    let (repository, _, _) = f.first_commit();
    let object = repository
        .paths
        .git_dir
        .join("objects")
        .join(&EMPTY_TREE[..2])
        .join(&EMPTY_TREE[2..]);
    fs::remove_file(object).unwrap();
    let before = f.inventory();
    probe_git_features(&f.git, &repository.paths.index).unwrap();
    assert!(repository.snapshot_clean().is_ok());
    assert_eq!(f.inventory(), before);
}

#[test]
fn c10_repository_reused_head_cannot_mint_a_created_commit_reopen_proof() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, _, tx) = f.first_commit();
    tx.finish();
    let payload = f.payload();
    let tx = f.transaction(2, &payload, &repository.snapshot_clean().unwrap());
    let (_, readback) = repository
        .commit_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
        .unwrap();
    assert!(!readback.created());
    assert!(C10CreatedCommitReopenProofV2::from_git_readback(readback).is_err());
}

#[test]
fn c10_repository_overlapping_project_in_either_direction_blocks_commit_without_writes() {
    for source_inside_control in [true, false] {
        let mut f = Fixture::new();
        if !source_inside_control {
            f.state = f.project.join(".polaris");
            fs::create_dir_all(&f.state).unwrap();
            fs::set_permissions(&f.state, fs::Permissions::from_mode(0o700)).unwrap();
        }
        f.install_empty_controls();
        if source_inside_control {
            f.project = f.state.join("sync/staging/nested-project");
            fs::create_dir(&f.project).unwrap();
            fs::set_permissions(&f.project, fs::Permissions::from_mode(0o700)).unwrap();
            fs::create_dir(f.project.join(".polaris")).unwrap();
            fs::set_permissions(
                f.project.join(".polaris"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let payload = f.payload();
        assert!(payload.descriptor().is_empty());
        let tx = f.transaction(2, &payload, &f.snapshot());
        let C10RepositoryInspectionV1::Existing(repository) = f.inspection() else {
            panic!("existing controls");
        };
        let before = f.inventory();
        assert!(matches!(
            repository.commit_after_local_prepared(
                &tx.management,
                &tx.started,
                &tx.lease,
                &payload
            ),
            Err(C10RepositoryErrorV1::Protection)
        ));
        assert_eq!(f.inventory(), before);
        assert!(f.project.is_dir());
    }
}

#[test]
fn c10_repository_nested_project_blocks_unknown_recovery_before_any_control_write() {
    for restore in [false, true] {
        let mut f = Fixture::new();
        f.install_empty_controls();
        f.project = f.state.join("sync/staging/nested-project");
        fs::create_dir(&f.project).unwrap();
        fs::set_permissions(&f.project, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(f.project.join(".polaris")).unwrap();
        fs::set_permissions(
            f.project.join(".polaris"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let payload = f.payload();
        let mut tx = f.transaction(2, &payload, &f.snapshot());
        let unknown = tx.mark_unknown(None);
        let C10RepositoryInspectionV1::Existing(repository) = f.inspection() else {
            panic!("existing controls");
        };
        let before = f.inventory();
        if restore {
            assert!(matches!(
                repository.restore_after_unknown(
                    &f.project,
                    &tx.management,
                    &unknown,
                    &tx.lease,
                    &SyncPriorHeadV2::Unborn
                ),
                Err(C10RepositoryErrorV1::Protection)
            ));
        } else {
            assert!(matches!(
                repository.durable_current_after_unknown(
                    &f.project,
                    &tx.management,
                    &unknown,
                    &tx.lease
                ),
                Err(C10RepositoryErrorV1::Protection)
            ));
        }
        assert_eq!(f.inventory(), before);
        assert!(f.project.is_dir());
    }
}

#[test]
fn c10_repository_project_containing_controls_blocks_initialization_without_writes() {
    let mut f = Fixture::new();
    f.state = f.project.join(".polaris");
    fs::create_dir_all(&f.state).unwrap();
    fs::set_permissions(&f.state, fs::Permissions::from_mode(0o700)).unwrap();
    let payload = f.payload();
    let tx = f.transaction(1, &payload, &f.snapshot());
    let C10RepositoryInspectionV1::Absent(repository) = f.inspection() else {
        panic!("fully absent controls");
    };
    let before = f.inventory();
    assert!(matches!(
        repository.initialize_after_local_prepared(
            &tx.management,
            &tx.started,
            &tx.lease,
            &payload
        ),
        Err(C10RepositoryErrorV1::Protection)
    ));
    assert_eq!(f.inventory(), before);
}

#[cfg(target_os = "macos")]
#[test]
fn c10_repository_firmlink_alias_overlap_blocks_commit_without_writes() {
    for source_inside_control in [true, false] {
        let home =
            PathBuf::from(std::env::var_os("HOME").expect("owned home for macOS firmlink fixture"));
        let temporary = tempfile::tempdir_in(&home).unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        assert!(root.starts_with("/Users"));
        let mut f = Fixture::new();
        f._temporary = temporary;
        f.project = root.join("project");
        f.state = root.join("state");
        for path in [&f.project, &f.state] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        if !source_inside_control {
            f.state = f.project.join(".polaris");
            fs::create_dir_all(&f.state).unwrap();
            fs::set_permissions(&f.state, fs::Permissions::from_mode(0o700)).unwrap();
        }
        f.install_empty_controls();
        let physical_project = if source_inside_control {
            let nested = f.state.join("sync/staging/nested-project");
            fs::create_dir(&nested).unwrap();
            fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
            // Keep project-root resolution at the explicitly named alias,
            // rather than adopting the ordinary HOME marker through firmlink.
            fs::create_dir(nested.join(".polaris")).unwrap();
            fs::set_permissions(nested.join(".polaris"), fs::Permissions::from_mode(0o700))
                .unwrap();
            nested
        } else {
            f.project.clone()
        };
        f.project =
            Path::new("/System/Volumes/Data").join(physical_project.strip_prefix("/").unwrap());
        assert_eq!(fs::canonicalize(&f.project).unwrap(), f.project);
        assert_ne!(f.project, physical_project);
        let physical = fs::metadata(&physical_project).unwrap();
        let alias = fs::metadata(&f.project).unwrap();
        assert_eq!((physical.dev(), physical.ino()), (alias.dev(), alias.ino()));
        let paths = SyncPaths::under(&f.state);
        assert!(!paths.overlaps(&f.project));
        assert!(!f.project.starts_with(&paths.work_tree));
        let payload = f.payload();
        let tx = f.transaction(2, &payload, &f.snapshot());
        let C10RepositoryInspectionV1::Existing(repository) = f.inspection() else {
            panic!("existing controls");
        };
        let before = f.inventory();
        assert!(matches!(
            repository.commit_after_local_prepared(
                &tx.management,
                &tx.started,
                &tx.lease,
                &payload
            ),
            Err(C10RepositoryErrorV1::Protection)
        ));
        assert_eq!(f.inventory(), before);
    }
}

#[test]
fn c10_repository_initialization_normalizes_generated_config_but_existing_worktree_is_refused() {
    let f = Fixture::new();
    f.write("payload", b"source remains unchanged");
    let project_before: BTreeMap<_, _> = f
        .inventory()
        .into_iter()
        .filter(|(path, _)| path.starts_with(&f.project))
        .collect();
    let payload = f.payload();
    let tx = f.transaction(1, &payload, &f.snapshot());
    let repository = f.repository(&tx);
    assert_eq!(
        repository.snapshot_clean().unwrap().head(),
        &SyncPriorHeadV2::Unborn
    );
    let config = repository.paths.git_dir.join("config");
    let bytes = fs::read(&config).unwrap();
    assert!(parse_config(&bytes).is_ok());
    assert!(
        !bytes
            .windows(b"worktree".len())
            .any(|window| window == b"worktree")
    );
    let project_after: BTreeMap<_, _> = f
        .inventory()
        .into_iter()
        .filter(|(path, _)| path.starts_with(&f.project))
        .collect();
    assert_eq!(project_before, project_after);
    let mut file = fs::OpenOptions::new().append(true).open(&config).unwrap();
    writeln!(file, "worktree = {}", repository.paths.work_tree.display()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let before = f.inventory();
    assert!(matches!(
        C10LocalRepositoryV1::inspect(&f.state, &f.git),
        Err(C10RepositoryErrorV1::Protection)
    ));
    assert!(repository.revalidate_controls().is_err());
    assert_eq!(before, f.inventory());
}

#[test]
fn c10_repository_raw_initialization_crash_preserves_partial_config_and_blocks() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let payload = f.payload();
    let tx = f.transaction(1, &payload, &f.snapshot());
    let C10RepositoryInspectionV1::Absent(absent) = f.inspection() else {
        panic!("initially absent")
    };
    FAILURE.with(|failure| failure.set(Some(FaultPoint::RawInitialization)));
    assert!(
        absent
            .initialize_after_local_prepared(&tx.management, &tx.started, &tx.lease, &payload)
            .is_err()
    );
    assert!(FAILURE.with(|failure| failure.get().is_none()));
    let before = f.inventory();
    assert!(matches!(
        C10LocalRepositoryV1::inspect(&f.state, &f.git),
        Err(C10RepositoryErrorV1::Protection)
    ));
    assert!(
        tx.management
            .effect(tx.started.operation_id())
            .unwrap()
            .unwrap()
            .state
            .blocks_new_work()
    );
    assert_eq!(before, f.inventory());
}

#[test]
fn c10_repository_created_proof_accepts_fresh_actual_readback_after_object_inventory_growth() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, readback, mut tx) = f.first_commit();
    let old_inventory = readback.objects_digest();
    let intended = readback.commit().clone();
    let cap = C10CreatedCommitReopenProofV2::from_git_readback(readback).unwrap();
    let unknown = tx
        .management
        .mark_outcome_unknown(
            &tx.lease,
            &tx.started,
            ManagementUnknownReasonV1::InterruptedAfterStart,
        )
        .unwrap();
    tx.publish(
        tx.history
            .outcome_unknown_after_reopen(
                SyncOutcomeUnknownV2 {
                    previous_state: SyncPreviousStateV2::LocalPrepared,
                    intended_commit: Some(intended),
                    created_commit_proof: None,
                    last_observation: SyncRemoteObservationV2::Missing,
                    reason: SyncUnknownReasonV2::RestartedAfterLocalPrepared,
                },
                cap,
            )
            .unwrap(),
    );
    let proof = match tx.history.current().state() {
        SyncRemoteIntentStateV2::OutcomeUnknown { unknown } => {
            unknown.created_commit_proof.as_ref().unwrap().clone()
        }
        _ => panic!("stored actual proof"),
    };
    let blob = f.state.join("owned-recovery-object-source");
    fs::write(&blob, b"additional recovery object inventory").unwrap();
    repository
        .git_output(&[
            "hash-object".into(),
            "-w".into(),
            "--no-filters".into(),
            "--".into(),
            blob.into_os_string(),
        ])
        .unwrap();
    let (_, fresh) = repository
        .durable_current_after_unknown(&f.project, &tx.management, &unknown, &tx.lease)
        .unwrap();
    assert_ne!(old_inventory, fresh.objects_digest());
    proof.require_actual_readback(&fresh).unwrap();
}

#[test]
fn c10_repository_created_proof_rejects_an_actual_readback_from_another_request() {
    let first = Fixture::new();
    first.write("payload", b"first");
    let (_, readback, mut tx) = first.first_commit();
    let intended = readback.commit().clone();
    let cap = C10CreatedCommitReopenProofV2::from_git_readback(readback).unwrap();
    tx.management
        .mark_outcome_unknown(
            &tx.lease,
            &tx.started,
            ManagementUnknownReasonV1::InterruptedAfterStart,
        )
        .unwrap();
    tx.publish(
        tx.history
            .outcome_unknown_after_reopen(
                SyncOutcomeUnknownV2 {
                    previous_state: SyncPreviousStateV2::LocalPrepared,
                    intended_commit: Some(intended),
                    created_commit_proof: None,
                    last_observation: SyncRemoteObservationV2::Missing,
                    reason: SyncUnknownReasonV2::RestartedAfterLocalPrepared,
                },
                cap,
            )
            .unwrap(),
    );
    let proof = match tx.history.current().state() {
        SyncRemoteIntentStateV2::OutcomeUnknown { unknown } => {
            unknown.created_commit_proof.as_ref().unwrap()
        }
        _ => panic!("stored actual proof"),
    };
    let second = Fixture::new();
    second.write("payload", b"second");
    let (_, foreign, _) = second.first_commit();
    assert!(proof.require_actual_readback(&foreign).is_err());
}

#[test]
fn c10_repository_absent_observation_root_ignores_an_ambient_owned_git_repository() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, _, _) = f.first_commit();
    let root = C10ObservationRootV1::inspect().unwrap();
    let before = f.inventory();
    let ambient = run_bound_local_git(
        &f.git,
        &[
            "-C".into(),
            repository.paths.git_dir.clone().into_os_string(),
            "rev-parse".into(),
            "--absolute-git-dir".into(),
        ],
        &repository.paths.index,
    )
    .unwrap();
    assert!(
        ambient.success(),
        "the owned ambient repository must actually be discoverable"
    );
    let mut args = vec![
        "-C".into(),
        repository.paths.git_dir.clone().into_os_string(),
    ];
    args.extend(observation_root_arguments());
    args.extend(["rev-parse".into(), "--absolute-git-dir".into()]);
    let output = run_bound_local_git(&f.git, &args, &repository.paths.index).unwrap();
    assert_eq!(output.exit_code(), Some(128));
    assert!(output.stdout().is_empty());
    root.verify().unwrap();
    assert_eq!(before, f.inventory());
}

#[test]
fn c10_repository_absent_observation_control_entries_are_rejected_without_reading_or_repair() {
    for component in [".git", "HEAD"] {
        for dangling in [false, true] {
            let f = Fixture::new();
            let directory = open_directory_nofollow(&f.state).unwrap();
            require_observation_controls_absent(&directory).unwrap();
            let path = f.state.join(component);
            if dangling {
                symlink(f.state.join("absent-target"), &path).unwrap();
            } else {
                fs::write(&path, b"unreadable owned control").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            }
            // Do not let the test's byte snapshot open an unreadable file.
            let before = fs::symlink_metadata(&path).unwrap();
            assert!(matches!(
                require_observation_controls_absent(&directory),
                Err(C10RepositoryErrorV1::Protection)
            ));
            let after = fs::symlink_metadata(&path).unwrap();
            assert!(same_metadata(&before, &after));
        }
    }
}

#[test]
fn c10_repository_absent_observation_rejects_foreign_binding_before_native_credentials() {
    let f = Fixture::new();
    let C10RepositoryInspectionV1::Absent(absent) = f.inspection() else {
        panic!("initially absent")
    };
    let owner = GitCredentialOwnerV1::new(f._temporary.path(), &f.git.path, &f.git.path).unwrap();
    let mut binding = c10_test_identity(1).account_binding;
    let remote = binding.remote.clone();
    let before = f.inventory();
    // Structurally valid foreign binary binding is rejected before Keychain
    // reads or child spawn, so the test needs no native item or secret.
    assert!(matches!(
        absent.observe_exact_child(&owner, &binding, &remote),
        Err(C10RepositoryErrorV1::BeforeSpawn)
    ));
    binding.git = f.git.clone();
    let foreign =
        CanonicalHttpsRemoteV1::parse("https://other.invalid:443/team/project.git").unwrap();
    assert!(matches!(
        absent.observe_exact_child(&owner, &binding, &foreign),
        Err(C10RepositoryErrorV1::BeforeSpawn)
    ));
    assert_eq!(before, f.inventory());
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
            assert!(entry.file_type().unwrap().is_file());
            fs::copy(from, &to).unwrap();
            fs::set_permissions(to, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
fn primary_repository_bytes(f: &Fixture) -> BTreeMap<PathBuf, (u64, u32, Vec<u8>)> {
    let paths = SyncPaths::under(&f.state);
    f.inventory()
        .into_iter()
        .filter(|(path, _)| {
            path.starts_with(&paths.work_tree)
                || path == &paths.index
                || path == &paths.git_dir.join("HEAD")
                || path.starts_with(paths.git_dir.join("refs/heads"))
        })
        .collect()
}
fn reset_owned_local_to_unborn(repository: &C10LocalRepositoryV1, commit: &SyncGitObjectIdV1) {
    repository
        .git(&["update-ref", "-d", "refs/heads/polaris", commit.as_str()])
        .unwrap();
    repository.git(&["read-tree", "--empty"]).unwrap();
    clear_directory(&repository.staging.directory).unwrap();
}
fn fetched_fixture_cap(
    repository: &C10LocalRepositoryV1,
    remote: &Path,
    tx: &Transaction,
    commit: &SyncGitObjectIdV1,
) -> C10ExactRecoveryFetchV1 {
    let expected = format!("{}\trefs/heads/polaris\n", commit.as_str());
    let observation = repository
        .git_output(&[
            "-c".into(),
            "protocol.file.allow=always".into(),
            "ls-remote".into(),
            "--refs".into(),
            "--exit-code".into(),
            "--".into(),
            remote.to_owned().into_os_string(),
            "refs/heads/polaris".into(),
        ])
        .unwrap();
    assert_eq!(observation, expected.as_bytes());
    let reference = format!(
        "refs/polaris/recovery/{}",
        SyncDigestV1::digest(tx.started.operation_id().as_hex().as_bytes()).as_hex()
    );
    repository
        .git_output(&[
            "-c".into(),
            "protocol.file.allow=always".into(),
            "fetch".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "--no-write-fetch-head".into(),
            "--no-auto-maintenance".into(),
            "--".into(),
            remote.to_owned().into_os_string(),
            format!("refs/heads/polaris:{reference}").into(),
        ])
        .unwrap();
    crate::sync::push::c10_test_exact_recovery_fetch(
        tx.history.current().identity().clone(),
        commit.clone(),
    )
}
fn mark_unknown_for_fixture_intended(
    tx: &mut Transaction,
    commit: &SyncGitObjectIdV1,
) -> UnknownManagementEffectV1 {
    tx.publish(
        tx.history
            .before_send(SyncBeforeSendV2 {
                intended_commit: commit.clone(),
                reopened_descriptor: tx.history.current().identity().payload.clone(),
                expected_remote: tx.history.current().identity().expected_remote_oid.clone(),
            })
            .unwrap(),
    );
    let unknown = tx
        .management
        .mark_outcome_unknown(
            &tx.lease,
            &tx.started,
            ManagementUnknownReasonV1::InterruptedAfterStart,
        )
        .unwrap();
    tx.publish(
        tx.history
            .outcome_unknown(SyncOutcomeUnknownV2 {
                previous_state: SyncPreviousStateV2::BeforeSend,
                intended_commit: Some(commit.clone()),
                created_commit_proof: None,
                last_observation: SyncRemoteObservationV2::Missing,
                reason: SyncUnknownReasonV2::TransportUncertain,
            })
            .unwrap(),
    );
    unknown
}

#[test]
fn c10_repository_exact_fetched_missing_commit_repairs_only_after_durable_blob_readback() {
    let f = Fixture::new();
    f.write(
        ".gitattributes",
        b"* text eol=lf\n*.bin filter=unregistered\n",
    );
    f.write("payload.txt", b"raw\r\n");
    f.write("payload.bin", &vec![0x80; 128 * 1024]);
    let expected = f.payload().descriptor.clone();
    let (repository, readback, mut tx) = f.first_commit();
    let commit = readback.commit().clone();
    let remote = f.state.parent().unwrap().join("owned-fetch-remote");
    copy_owned_git_tree(&repository.paths.git_dir, &remote);
    let unknown = tx.mark_unknown(Some(&readback));
    reset_owned_local_to_unborn(&repository, &commit);
    fs::remove_file(
        repository
            .paths
            .git_dir
            .join("objects")
            .join(&commit.as_str()[..2])
            .join(&commit.as_str()[2..]),
    )
    .unwrap();
    assert!(
        repository
            .git(&["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())])
            .is_err()
    );
    let cap = fetched_fixture_cap(&repository, &remote, &tx, &commit);
    let project_before: BTreeMap<_, _> = f
        .inventory()
        .into_iter()
        .filter(|(path, _)| path.starts_with(&f.project))
        .collect();
    let (repository, repaired) = repository
        .repair_fetched_after_unknown(
            &f.project,
            &tx.management,
            &unknown,
            &tx.lease,
            &SyncPriorHeadV2::Unborn,
            cap,
        )
        .unwrap();
    assert_eq!(repaired.commit(), &commit);
    assert_eq!(repaired.descriptor(), &expected);
    assert!(repaired.created());
    assert_eq!(
        repository.head_for_recovery().unwrap(),
        SyncPriorHeadV2::Present { commit }
    );
    assert_eq!(
        fs::read(repository.paths.work_tree.join("payload.txt")).unwrap(),
        b"raw\r\n"
    );
    assert_eq!(
        fs::read(repository.paths.work_tree.join("payload.bin")).unwrap(),
        vec![0x80; 128 * 1024]
    );
    assert_eq!(
        project_before,
        f.inventory()
            .into_iter()
            .filter(|(path, _)| path.starts_with(&f.project))
            .collect()
    );
    assert!(
        tx.management
            .effect(tx.started.operation_id())
            .unwrap()
            .unwrap()
            .state
            .blocks_new_work()
    );
}

#[test]
fn c10_repository_exact_recovery_ref_object_mismatch_preserves_primary_index_and_staging() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, readback, mut tx) = f.first_commit();
    let commit = readback.commit().clone();
    let remote = f.state.parent().unwrap().join("owned-fetch-remote");
    copy_owned_git_tree(&repository.paths.git_dir, &remote);
    let unknown = tx.mark_unknown(Some(&readback));
    reset_owned_local_to_unborn(&repository, &commit);
    let cap = fetched_fixture_cap(&repository, &remote, &tx, &commit);
    let tree = repository
        .oid_output(&[
            "rev-parse".into(),
            "--verify".into(),
            format!("{}^{{tree}}", commit.as_str()).into(),
        ])
        .unwrap();
    let foreign = repository
        .oid_output(&[
            "commit-tree".into(),
            tree.as_str().into(),
            "-m".into(),
            "different exact recovery object".into(),
        ])
        .unwrap();
    repository
        .git_output(&[
            "update-ref".into(),
            cap.recovery_ref().into(),
            foreign.as_str().into(),
            commit.as_str().into(),
        ])
        .unwrap();
    let before = primary_repository_bytes(&f);
    assert!(matches!(
        repository.repair_fetched_after_unknown(
            &f.project,
            &tx.management,
            &unknown,
            &tx.lease,
            &SyncPriorHeadV2::Unborn,
            cap,
        ),
        Err(C10RepositoryErrorV1::Changed)
    ));
    assert_eq!(primary_repository_bytes(&f), before);
}

#[test]
fn c10_repository_fetched_payload_mismatch_never_changes_primary_head_index_or_staging() {
    let foreign = Fixture::new();
    foreign.write("payload", b"other bytes");
    let (remote_repository, foreign_readback, _) = foreign.first_commit();
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, readback, mut tx) = f.first_commit();
    let local = readback.commit().clone();
    let intended = foreign_readback.commit().clone();
    let remote = f.state.parent().unwrap().join("owned-fetch-remote");
    copy_owned_git_tree(&remote_repository.paths.git_dir, &remote);
    let unknown = mark_unknown_for_fixture_intended(&mut tx, &intended);
    reset_owned_local_to_unborn(&repository, &local);
    let cap = fetched_fixture_cap(&repository, &remote, &tx, &intended);
    let before = primary_repository_bytes(&f);
    assert!(matches!(
        repository.repair_fetched_after_unknown(
            &f.project,
            &tx.management,
            &unknown,
            &tx.lease,
            &SyncPriorHeadV2::Unborn,
            cap,
        ),
        Err(C10RepositoryErrorV1::Changed)
    ));
    assert_eq!(primary_repository_bytes(&f), before);
    assert!(
        tx.management
            .effect(tx.started.operation_id())
            .unwrap()
            .unwrap()
            .state
            .blocks_new_work()
    );
}

#[test]
fn c10_repository_fetched_recovery_faults_never_return_a_partial_readback() {
    for point in [FaultPoint::FetchedReadback, FaultPoint::RecoveryReference] {
        let f = Fixture::new();
        f.write("payload", b"one");
        let (repository, readback, mut tx) = f.first_commit();
        let commit = readback.commit().clone();
        let remote = f.state.parent().unwrap().join("owned-fetch-remote");
        copy_owned_git_tree(&repository.paths.git_dir, &remote);
        let unknown = tx.mark_unknown(Some(&readback));
        reset_owned_local_to_unborn(&repository, &commit);
        let cap = fetched_fixture_cap(&repository, &remote, &tx, &commit);
        let before = primary_repository_bytes(&f);
        FAILURE.with(|failure| failure.set(Some(point)));
        assert!(
            repository
                .repair_fetched_after_unknown(
                    &f.project,
                    &tx.management,
                    &unknown,
                    &tx.lease,
                    &SyncPriorHeadV2::Unborn,
                    cap,
                )
                .is_err()
        );
        assert!(FAILURE.with(|failure| failure.get().is_none()));
        if point == FaultPoint::FetchedReadback {
            assert_eq!(primary_repository_bytes(&f), before);
        }
        assert!(
            tx.management
                .effect(tx.started.operation_id())
                .unwrap()
                .unwrap()
                .state
                .blocks_new_work()
        );
    }
}

#[test]
fn c10_repository_head_for_recovery_is_readonly_with_dirty_private_staging() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, readback, _) = f.first_commit();
    fs::write(
        repository.paths.work_tree.join("payload"),
        b"dirty interrupted staging",
    )
    .unwrap();
    let before = f.inventory();
    assert_eq!(
        repository.head_for_recovery().unwrap(),
        SyncPriorHeadV2::Present {
            commit: readback.commit().clone()
        }
    );
    assert_eq!(f.inventory(), before);
}

#[test]
fn c10_repository_standard_v2_v3_v4_indexes_remain_readonly_compatible() {
    let f = Fixture::new();
    f.write("dir/common-one", b"one");
    f.write("dir/common-two", b"two");
    let (repository, _, _) = f.first_commit();
    for version in [2, 3, 4] {
        if version == 3 {
            repository
                .git(&["update-index", "--skip-worktree", "--", "dir/common-one"])
                .unwrap();
        }
        repository
            .git(&["update-index", &format!("--index-version={version}")])
            .unwrap();
        let bytes = fs::read(&repository.paths.index).unwrap();
        assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), version);
        assert_eq!(validate_full_index(&bytes).unwrap(), 2);
        let before = f.inventory();
        assert!(repository.snapshot_clean().is_ok());
        assert_eq!(f.inventory(), before);
    }
}

#[test]
fn c10_repository_actual_sparse_index_is_refused_before_any_git_child_or_object_write() {
    let f = Fixture::new();
    f.write("d/one", b"one");
    f.write("d/two", b"two");
    f.write("keep/file", b"keep");
    let (mut repository, _, _) = f.first_commit();
    // Raw checkout preserves blob bytes without saving stat-cache entries.
    // Refresh this owned fixture so Git can remove paths outside the cone.
    repository.git(&["update-index", "--refresh"]).unwrap();
    let subtree = repository
        .oid_output(&["rev-parse".into(), "--verify".into(), "HEAD:d".into()])
        .unwrap();
    let config = fs::read(repository.paths.git_dir.join("config")).unwrap();
    let trace = f.state.join("owned-sparse-read-child");
    let wrapper = f.state.join("owned-sparse-git-counter");
    let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
    let script = format!(
        "#!/bin/sh\nprintf 'child\\n' >> {}\nexec {} \"$@\"\n",
        quote(&trace),
        quote(&f.git.path),
    );
    fs::write(&wrapper, script.as_bytes()).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    repository.git = ExecutableIdentityV1::inspect(&wrapper).unwrap();
    let version = repository.run_fixture_child(&["--version".into()]).unwrap();
    assert!(version.success());
    assert_eq!(fs::read(&trace).unwrap(), b"child\n");
    fs::remove_file(&trace).unwrap();
    let sparse_args = [
        "--git-dir".into(),
        repository.paths.git_dir.clone().into_os_string(),
        "--work-tree".into(),
        repository.paths.work_tree.clone().into_os_string(),
        "-c".into(),
        "core.sparseCheckout=true".into(),
        "-c".into(),
        "core.sparseCheckoutCone=true".into(),
        "-c".into(),
        "index.sparse=true".into(),
        "-c".into(),
        "index.threads=1".into(),
        "sparse-checkout".into(),
        "set".into(),
        "--cone".into(),
        "--sparse-index".into(),
        "keep".into(),
    ];
    let output = run_bound_local_git(&f.git, &sparse_args, &repository.paths.index).unwrap();
    assert!(output.success());
    let mut check_args = sparse_args[..12].to_vec();
    check_args.extend(["ls-files".into(), "--sparse".into(), "--stage".into()]);
    let check = run_bound_local_git(&f.git, &check_args, &repository.paths.index).unwrap();
    assert!(check.success());
    let sparse_line = format!("040000 {} 0\td/", subtree.as_str());
    let kept_line = format!("100644 {} 0\tkeep/file", git_blob_oid_from_bytes(b"keep"));
    let actual = std::str::from_utf8(check.stdout())
        .unwrap()
        .lines()
        .collect::<Vec<_>>();
    assert_eq!(actual, [sparse_line.as_str(), kept_line.as_str()]);
    // Normalize only this owned adversarial fixture's generated controls.
    fs::write(repository.paths.git_dir.join("config"), config).unwrap();
    for path in [
        repository.paths.git_dir.join("config.worktree"),
        repository.paths.git_dir.join("info/sparse-checkout"),
    ] {
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    repository.config = pin_config(&repository.repository.directory).unwrap();
    let index = fs::read(&repository.paths.index).unwrap();
    assert!(matches!(
        validate_full_index(&index),
        Err(C10RepositoryErrorV1::Protection)
    ));
    let before = f.inventory();
    assert!(matches!(
        repository.snapshot_clean(),
        Err(C10RepositoryErrorV1::Protection)
    ));
    assert!(matches!(
        repository.head_for_recovery(),
        Err(C10RepositoryErrorV1::Protection)
    ));
    assert!(
        !trace.exists(),
        "the proven actual Git wrapper must not run"
    );
    assert_eq!(f.inventory(), before);
}

#[test]
fn c10_repository_index_mandatory_extensions_are_framed_not_substring_matched() {
    let f = Fixture::new();
    f.write("payload", b"one");
    let (repository, _, _) = f.first_commit();
    let original = fs::read(&repository.paths.index).unwrap();
    let count = validate_full_index(&original).unwrap();
    for (signature, allowed) in [
        (b"TEST", true),
        (b"sdir", false),
        (b"link", false),
        (b"abcd", false),
    ] {
        let mut bytes = original[..original.len() - 20].to_vec();
        bytes.extend_from_slice(signature);
        bytes.extend_from_slice(&8_u32.to_be_bytes());
        bytes.extend_from_slice(b"sdirlink");
        bytes.extend_from_slice(&original[original.len() - 20..]);
        if allowed {
            assert_eq!(validate_full_index(&bytes).unwrap(), count);
        } else {
            assert!(matches!(
                validate_full_index(&bytes),
                Err(C10RepositoryErrorV1::Protection)
            ));
        }
    }
}

#[test]
fn c10_repository_absent_unknown_abandon_proves_empty_state_without_creating_controls() {
    let f = Fixture::new();
    f.write("payload", b"still private source");
    let payload = f.payload();
    let snapshot = f.snapshot();
    let mut tx = f.transaction(1, &payload, &snapshot);
    let unknown = tx.mark_unknown(None);
    let absent = match f.inspection() {
        C10RepositoryInspectionV1::Absent(absent) => absent,
        _ => panic!("LP did not initialize repository"),
    };
    let before = f.inventory();
    let restored = absent
        .restore_absent_after_unknown(&f.project, &tx.management, &unknown, &tx.lease)
        .unwrap();
    assert_eq!(restored.operation_id(), tx.started.operation_id());
    assert_eq!(
        restored.request_hash().as_hex(),
        unknown.request().request_hash.as_hex()
    );
    assert_eq!(restored.expected_current(), &SyncPriorHeadV2::Unborn);
    assert_eq!(restored.restored_head(), &SyncPriorHeadV2::Unborn);
    assert_eq!(restored.descriptor(), &snapshot.descriptor);
    assert_eq!(restored.index_digest(), snapshot.index_digest);
    assert_eq!(restored.staging_digest(), snapshot.staging_digest);
    assert_eq!(restored.objects_digest(), ObjectProjection::new().finish());
    assert_ne!(restored.objects_digest(), SyncDigestV1::digest(b""));
    assert_eq!(f.inventory(), before);
    assert!(matches!(
        f.inspection(),
        C10RepositoryInspectionV1::Absent(_)
    ));
    assert!(
        tx.management
            .effect(tx.started.operation_id())
            .unwrap()
            .unwrap()
            .state
            .blocks_new_work()
    );
}

#[test]
fn c10_repository_absent_abandon_rejects_nonempty_history_partial_controls_and_foreign_scope() {
    for case in 0..5 {
        let f = Fixture::new();
        f.write("payload", b"source");
        let payload = f.payload();
        let mut snapshot = f.snapshot();
        if case == 0 {
            snapshot.index_digest = SyncDigestV1::digest(b"nonempty prior index");
        }
        let mut tx = f.transaction(1, &payload, &snapshot);
        let absent = match f.inspection() {
            C10RepositoryInspectionV1::Absent(absent) => absent,
            _ => panic!("absent fixture"),
        };
        let unknown = match case {
            1 => mark_unknown_for_fixture_intended(
                &mut tx,
                &SyncGitObjectIdV1::parse("1".repeat(40)).unwrap(),
            ),
            4 => tx
                .management
                .mark_outcome_unknown(
                    &tx.lease,
                    &tx.started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                )
                .unwrap(),
            _ => tx.mark_unknown(None),
        };
        if case == 2 {
            let sync = f.state.join("sync");
            if !sync.exists() {
                fs::create_dir(&sync).unwrap();
                fs::set_permissions(&sync, fs::Permissions::from_mode(0o700)).unwrap();
            }
            fs::write(sync.join("index"), b"interrupted partial controls").unwrap();
        }
        let foreign = f.state.parent().unwrap().join("other-source");
        let project = if case == 3 {
            fs::create_dir(&foreign).unwrap();
            fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
            &foreign
        } else {
            &f.project
        };
        let before = f.inventory();
        assert!(
            absent
                .restore_absent_after_unknown(project, &tx.management, &unknown, &tx.lease,)
                .is_err()
        );
        assert_eq!(f.inventory(), before);
        assert!(
            tx.management
                .effect(tx.started.operation_id())
                .unwrap()
                .unwrap()
                .state
                .blocks_new_work()
        );
    }
}

#[test]
fn c10_repository_absent_abandon_faults_cannot_mint_partial_readback() {
    for point in [
        FaultPoint::DirectoryFsync,
        FaultPoint::Reopen,
        FaultPoint::Readback,
    ] {
        let f = Fixture::new();
        f.write("payload", b"source");
        let payload = f.payload();
        let mut tx = f.transaction(1, &payload, &f.snapshot());
        let unknown = tx.mark_unknown(None);
        let absent = match f.inspection() {
            C10RepositoryInspectionV1::Absent(absent) => absent,
            _ => panic!("absent fixture"),
        };
        let before = f.inventory();
        FAILURE.with(|failure| failure.set(Some(point)));
        assert!(
            absent
                .restore_absent_after_unknown(&f.project, &tx.management, &unknown, &tx.lease,)
                .is_err()
        );
        assert!(FAILURE.with(|failure| failure.get().is_none()));
        assert_eq!(f.inventory(), before);
        assert!(
            tx.management
                .effect(tx.started.operation_id())
                .unwrap()
                .unwrap()
                .state
                .blocks_new_work()
        );
    }
}
