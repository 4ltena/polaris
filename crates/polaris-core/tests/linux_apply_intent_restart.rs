//! Local XFS restart acceptance. Phase fixtures use the public canonical wire
//! encoders; generic events and recovery journals use their production writers.
//! Every recovery is performed by a new process through ApplyOwnerV1. This does
//! not claim that a production apply was killed at the fixture boundary.
#![cfg(target_os = "linux")]

use polaris_core::apply_intent::*;
use polaris_core::apply_owner::{ApplyOwnerRecoveryV1, ApplyOwnerV1};
use polaris_core::isolated_workspace::{
    Limits, RegisteredSecrets, protected_collect_changes, protected_snapshot,
};
use polaris_core::management_effect::*;
use polaris_core::project_mutation::ProjectMutationOwnerV1;
use polaris_core::workspace_apply::{self, RecoveryParent};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

type Hash = ManagementEffectHashV1;

fn identity(path: &Path) -> IdentityV1 {
    let m = fs::symlink_metadata(path).unwrap();
    assert!(!m.file_type().is_symlink());
    IdentityV1 {
        device: m.dev(),
        inode: m.ino(),
    }
}

fn directory(path: &Path) {
    fs::DirBuilder::new().create(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    File::open(path.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
}

fn save(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    File::open(path.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), bytes);
}

fn parent(path: &Path) -> RecoveryParent {
    let file = File::open(path).unwrap();
    let m = file.metadata().unwrap();
    RecoveryParent::pin(&file, (m.dev(), m.ino()), path).unwrap()
}

fn limits() -> Limits {
    Limits {
        max_entries: 64,
        max_files: 32,
        max_file_bytes: 4096,
        max_total_bytes: 65536,
        max_depth: 8,
    }
}

fn snapshot_bytes(root: &Path) -> BTreeMap<PathBuf, (IdentityV1, u32, Vec<u8>)> {
    fn walk(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, (IdentityV1, u32, Vec<u8>)>) {
        let m = fs::symlink_metadata(path).unwrap();
        assert!(!m.file_type().is_symlink());
        assert_eq!(m.uid(), unsafe { libc::geteuid() });
        assert!(result.len() < 1024);
        let bytes = if m.is_file() {
            assert_eq!(m.nlink(), 1);
            assert!(m.len() <= 65536);
            fs::read(path).unwrap()
        } else {
            assert!(m.is_dir());
            Vec::new()
        };
        result.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            (identity(path), m.mode() & 0o7777, bytes),
        );
        if m.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                walk(root, &entry.unwrap().path(), result);
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}

fn available(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    assert_eq!(
        unsafe { libc::statvfs(name.as_ptr(), value.as_mut_ptr()) },
        0
    );
    let value = unsafe { value.assume_init() };
    assert_eq!(value.f_frsize, 4096);
    value.f_bavail * value.f_frsize
}

fn fixture_plan(
    state: &Path,
    recovery: &Path,
    desktop: Option<&Path>,
    request: &RequestV1,
    before: &ObservedSourceV1,
    binding: Option<&D27SourceBindingV1>,
) -> CapacityPlanV1 {
    let mut plan = CapacityPlanV1::from_canonical_bytes(include_bytes!(
        "fixtures/apply-intent-v1-capacity-positive/capacity-plan.v1.json"
    ))
    .unwrap();
    plan.state_identity = identity(state);
    plan.state_device = plan.state_identity.device;
    plan.recovery_parent_identity = identity(recovery);
    plan.recovery_device = plan.recovery_parent_identity.device;
    assert_eq!(plan.state_device, plan.recovery_device);
    let free = available(state);
    plan.state_available_bytes_at_admission = free;
    plan.recovery_available_bytes_at_admission = free;
    plan.request_bytes = request.canonical_bytes().unwrap().len() as u64;
    plan.source_before_bytes = before.canonical_bytes().unwrap().len() as u64;
    plan.d27_source_binding_bytes = binding.map(|v| v.canonical_bytes().unwrap().len() as u64);
    plan.management_event_record_max_bytes = 16384;
    plan.management_event_journal_append_bytes = 65536;
    plan.management_record_limit_bytes_at_admission = 16384;
    plan.management_records_remaining_at_admission = 4096;
    plan.management_bytes_remaining_at_admission = 64 * 1024 * 1024;
    plan.management_event_journal_required_bytes = 65536;
    plan.generic_effect_bytes = 65536 + plan.management_generation_seal_required_bytes;
    plan.journal_bytes = 65536;
    plan.report_bytes = 65536;
    plan.archive_seal_bytes = 65536;
    if let Some(path) = desktop {
        let id = identity(path);
        assert_eq!(id.device, plan.state_device);
        plan.desktop_root_identity = Some(id);
        plan.desktop_device = Some(id.device);
        plan.desktop_allocation_unit = Some(4096);
        plan.desktop_available_bytes_at_admission = Some(free);
        plan.desktop_result_required_bytes = Some(1024 * 1024);
        plan.desktop_required_bytes = Some(plan.inventory_commit_bytes);
        plan.desktop_records_remaining_at_admission = Some(16);
        plan.desktop_bytes_remaining_at_admission = Some(8 * 1024 * 1024);
    }
    let alloc = |n| allocated_bytes(n, 4096).unwrap();
    plan.intent_final_bytes = alloc(plan.request_bytes)
        + alloc(plan.source_before_bytes)
        + alloc(plan.d27_source_binding_bytes.unwrap_or(0))
        + 12 * 65536;
    plan.intent_temporary_bytes = 65536 + alloc(plan.archive_seal_bytes);
    plan.state_required_bytes = plan.intent_final_bytes
        + plan.intent_temporary_bytes
        + plan.generic_effect_bytes
        + if desktop.is_some() {
            0
        } else {
            plan.inventory_commit_bytes
        }
        + alloc(plan.archive_seal_bytes)
        + DIRECTORY_OVERHEAD_BYTES;
    plan.recovery_required_bytes =
        alloc(plan.journal_bytes) + alloc(plan.report_bytes) + DIRECTORY_OVERHEAD_BYTES;
    plan.formula_hash = plan.recomputed_formula_hash().unwrap();
    plan.validate().unwrap();
    plan
}

fn append(
    path: &Path,
    previous: &ApplyIntentEnvelopeV1,
    evidence: ApplyEvidenceV1,
) -> ApplyIntentEnvelopeV1 {
    let generation = ApplyGenerationV1 {
        schema_version: 1,
        sequence: previous.generation.sequence + 1,
        previous_record_hash: previous.record_hash,
        descriptor_hash: previous.descriptor_hash,
        phase: evidence.phase(),
        evidence,
    };
    generation.verify_successor(&previous.generation).unwrap();
    let envelope = ApplyIntentEnvelopeV1 {
        schema_version: 1,
        descriptor: None,
        descriptor_hash: previous.descriptor_hash,
        record_hash: generation.record_hash().unwrap(),
        generation,
    };
    save(
        &path.join(format!("g{:02}.v1.json", envelope.generation.sequence)),
        &envelope.canonical_bytes().unwrap(),
    );
    envelope
}

fn create_phase(root: &Path, inventory: &str, phase: &str) {
    let project = root.join("project");
    directory(&project);
    save(&project.join("main.rs"), b"before\n");
    let state = polaris_core::project::state_dir(&project).unwrap();
    directory(&state.join("sync"));
    let recovery = state.join("sync/recovery");
    directory(&recovery);
    let participation = polaris_core::sync::participation::Participation {
        share: polaris_core::sync::share::ShareLink {
            remote: "file:///local-restart-fixture".into(),
            share_id: "restart-share".into(),
            invitation_id: "restart-invitation".into(),
            expires_unix_seconds: u64::MAX,
        },
        manifest: polaris_core::sync::share::ShareManifest::default(),
        joined_unix_seconds: 1,
        agreed_commit: Some("a".repeat(40)),
        applied_commit: None,
        unresolved: vec![],
    };
    save(
        &state.join("sync/participation.json"),
        &serde_json::to_vec(&participation).unwrap(),
    );
    let parent = parent(&recovery);
    let snapshot = protected_snapshot(&project, limits()).unwrap();
    fs::write(snapshot.path().join("main.rs"), b"after\n").unwrap();
    let changes = protected_collect_changes(&snapshot, limits()).unwrap();
    let m = fs::metadata(project.join("main.rs")).unwrap();
    let before_file = BeforeV1 {
        device: m.dev(),
        inode: m.ino(),
        hash: Hash::digest(b"before\n"),
        mode: m.mode() & 0o777,
        observed_mode: m.mode() & 0o7777,
        bytes: m.len(),
    };
    let request = RequestV1 {
        schema_version: 1,
        source_identity: identity(&project),
        entries: vec![RequestEntryV1 {
            path: "main.rs".into(),
            before: Some(before_file.clone()),
            after: Some(AfterV1 {
                hash: Hash::digest(b"after\n"),
                mode: before_file.mode,
                bytes: 6,
            }),
        }],
    };
    let before = ObservedSourceV1 {
        schema_version: 1,
        source_identity: request.source_identity,
        entries: vec![ObservedEntryV1 {
            path: "main.rs".into(),
            current: Some(before_file.clone()),
        }],
    };
    let binding = if inventory == "D27" {
        let after = fs::metadata(snapshot.path().join("main.rs")).unwrap();
        let manifest = D27ManifestProjectionV1 {
            schema_version: 1,
            source_path: project.to_str().unwrap().into(),
            source_identity: identity(&project),
            recovery_parent_path: recovery.to_str().unwrap().into(),
            recovery_parent_identity: identity(&recovery),
            entries: vec![D27ManifestProjectionEntryV1 {
                path: "main.rs".into(),
                before: Some(D27ManifestVersionV1 {
                    device: m.dev(),
                    inode: m.ino(),
                    hash: before_file.hash,
                    normal_mode: before_file.mode,
                    observed_mode: before_file.observed_mode,
                    bytes: m.len(),
                }),
                after: Some(D27ManifestVersionV1 {
                    device: after.dev(),
                    inode: after.ino(),
                    hash: Hash::digest(b"after\n"),
                    normal_mode: after.mode() & 0o777,
                    observed_mode: after.mode() & 0o7777,
                    bytes: after.len(),
                }),
            }],
        };
        assert_eq!(manifest.project_request().unwrap(), request);
        Some(D27SourceBindingV1 {
            schema_version: 1,
            payload_bytes: 6,
            payload_hash: Hash::digest(b"after\n"),
            manifest_bytes: manifest.canonical_bytes().unwrap().len() as u64,
            manifest_hash: manifest.manifest_hash().unwrap(),
            request_bytes: request.canonical_bytes().unwrap().len() as u64,
            request_hash: request.request_hash().unwrap(),
            source_before_bytes: before.canonical_bytes().unwrap().len() as u64,
            source_before_hash: before.observation_hash().unwrap(),
            manifest,
        })
    } else {
        None
    };
    let desktop = root.join("desktop");
    if binding.is_some() {
        directory(&desktop);
    }
    let plan = fixture_plan(
        &state,
        &recovery,
        binding.as_ref().map(|_| desktop.as_path()),
        &request,
        &before,
        binding.as_ref(),
    );
    let project_binding = Hash::digest(project.as_os_str().as_encoded_bytes());
    let operation = Hash::digest(b"local-restart-d27-operation");
    let management_id = if binding.is_some() {
        d27_management_operation_id(operation).unwrap()
    } else {
        ManagementOperationIdV1::random().unwrap()
    };
    let key = if binding.is_some() {
        d27_operation_key(project_binding, operation).unwrap()
    } else {
        c12_operation_key(project_binding, management_id).unwrap()
    };
    let nonzero = Hash::digest(b"restart-fixture-marker");
    let context = if binding.is_some() {
        let result_id = "local-restart-result".to_string();
        ApplyContextV1::Session {
            base_epoch: 1,
            session_binding: nonzero,
            base_session_revision: 1,
            consume_target_revision: 2,
            policy_revision: 1,
            source_operation_digest: operation,
            reserved_result_id_digest: d27_result_id_digest(&result_id).unwrap(),
            reserved_result_id: result_id,
            base_marker_evidence_hash: nonzero,
            consume_marker_evidence_hash: nonzero,
            consume_sidecar_offset: 0,
            consume_sidecar_hash: nonzero,
            precommit_hash: nonzero,
        }
    } else {
        ApplyContextV1::Participation {
            base_generation: 0,
            target_generation: 1,
            base_marker_hash: nonzero,
            mutation_id: key,
            target_participation_hash: Some(nonzero),
            target_marker_hash: nonzero,
        }
    };
    let revision = RevisionV1 {
        kind: if binding.is_some() {
            RevisionKindV1::Sha256
        } else {
            RevisionKindV1::GitSha1
        },
        value: if binding.is_some() {
            nonzero.as_hex()
        } else {
            "a".repeat(40)
        },
    };
    let descriptor = ApplyIntentDescriptorV1 {
        schema_version: 1,
        operation_key: key,
        management_operation_id: management_id,
        inventory_id: if binding.is_some() {
            ApplyInventoryV1::D27
        } else {
            ApplyInventoryV1::C12
        },
        project_binding,
        source_identity: request.source_identity,
        agreed_revision: revision.clone(),
        staged_revision: if binding.is_some() {
            RevisionV1 {
                kind: RevisionKindV1::Sha256,
                value: request.request_hash().unwrap().as_hex(),
            }
        } else {
            revision
        },
        previous_applied_revision: None,
        context,
        share_source_manifest_digest: binding.as_ref().map(|b| b.manifest_hash).unwrap_or(nonzero),
        changed_path_count: 1,
        path_set_digest: request.path_set_digest().unwrap(),
        workspace_apply_request_digest: request.request_hash().unwrap(),
        source_before_observation_digest: before.observation_hash().unwrap(),
        d27_source_binding_bytes: binding
            .as_ref()
            .map(|b| b.canonical_bytes().unwrap().len() as u64),
        d27_source_binding_hash: binding.as_ref().map(|b| b.binding_hash().unwrap()),
        recovery_parent_identity: identity(&recovery),
        recovery_provenance_digest: binding
            .as_ref()
            .map(|b| b.manifest.recovery_provenance_digest().unwrap())
            .unwrap_or(recovery_digest(key).unwrap()),
        recovery_child: recovery_child(key).unwrap(),
        capacity_plan_bytes: plan.canonical_bytes().unwrap().len() as u64,
        capacity_plan_hash: plan.plan_hash().unwrap(),
        capacity_plan: plan,
        phase: ApplyPhaseV1::Prepared,
    };
    let mut descriptor = descriptor;
    if let Some(binding) = &binding {
        descriptor.agreed_revision.value = binding.payload_hash.as_hex();
    }
    descriptor.verify_request(&request, &before).unwrap();
    if let Some(binding) = &binding {
        binding
            .verify_artifacts(&descriptor, &request, &before)
            .unwrap();
    }
    let descriptor_hash = descriptor.descriptor_hash().unwrap();
    let generation = ApplyGenerationV1 {
        schema_version: 1,
        sequence: 0,
        previous_record_hash: Hash::from_hex(&"0".repeat(64)).unwrap(),
        descriptor_hash,
        phase: ApplyPhaseV1::Prepared,
        evidence: ApplyEvidenceV1::Prepared {
            capacity_plan_hash: descriptor.capacity_plan_hash,
            request_hash: request.request_hash().unwrap(),
            request_bytes: descriptor.capacity_plan.request_bytes,
            source_before_hash: before.observation_hash().unwrap(),
            source_before_bytes: descriptor.capacity_plan.source_before_bytes,
        },
    };
    let zero = ApplyIntentEnvelopeV1 {
        schema_version: 1,
        descriptor: Some(descriptor.clone()),
        descriptor_hash,
        record_hash: generation.record_hash().unwrap(),
        generation,
    };
    let intents = state.join("apply-intents-v1");
    directory(&intents);
    for name in ["active", "archive", "staging"] {
        directory(&intents.join(name));
    }
    let active = intents.join("active").join(key.as_hex());
    directory(&active);
    save(
        &active.join("request.v1.json"),
        &request.canonical_bytes().unwrap(),
    );
    save(
        &active.join("source-before.v1.json"),
        &before.canonical_bytes().unwrap(),
    );
    if let Some(binding) = &binding {
        save(
            &active.join("d27-source-binding.v1.json"),
            &binding.canonical_bytes().unwrap(),
        );
    }
    save(
        &active.join("g00.v1.json"),
        &zero.canonical_bytes().unwrap(),
    );
    let mutation = ProjectMutationOwnerV1::for_project(&project).unwrap();
    let lease = mutation.try_mutation().unwrap();
    let store = ManagementEffectStoreV1::open(&project, &lease).unwrap();
    let prepared = match store
        .prepare(
            &lease,
            store.cursor().unwrap(),
            ApplyOwnerV1::management_request(&descriptor).unwrap(),
        )
        .unwrap()
    {
        PrepareManagementEffectOutcomeV1::Prepared(v) => v,
        other => panic!("{other:?}"),
    };
    if phase == "started" {
        let secrets = RegisteredSecrets::new(Vec::<PathBuf>::new()).unwrap();
        let child = workspace_apply::prepare_reserved_recovery_v2(
            &snapshot,
            &changes,
            &secrets,
            limits(),
            &parent,
            key,
            descriptor_hash,
        )
        .unwrap();
        let header = child.header();
        let proof = prepared.verified_event().evidence;
        let recovery = append(
            &active,
            &zero,
            ApplyEvidenceV1::RecoveryPrepared {
                child_identity: identity(child.path()),
                journal_header_hash: header.header_hash().unwrap(),
                journal_header_bytes: header.canonical_bytes().unwrap().len() as u64,
                directory_synced: true,
                generic_prepared_generation: proof.generation,
                generic_prepared_hash: proof.event_hash,
                generic_prepared_sequence: proof.sequence,
            },
        );
        let proof = store
            .start(&lease, &prepared)
            .unwrap()
            .verified_event()
            .evidence;
        append(
            &active,
            &recovery,
            ApplyEvidenceV1::EffectStarted {
                generic_generation: proof.generation,
                generic_event_hash: proof.event_hash,
                generic_sequence: proof.sequence,
            },
        );
    }
    drop(store);
    drop(lease);
    drop(mutation);
    drop(changes);
    drop(snapshot);
    if phase == "archived" {
        let owner = ApplyOwnerV1::acquire(&project).unwrap();
        assert!(
            matches!(owner.recover_current(&parent).unwrap(), ApplyOwnerRecoveryV1::Archived(v) if v == key)
        );
    }
    save(
        &root.join("published.json"),
        &serde_json::to_vec(&serde_json::json!({"creator_pid": std::process::id(), "key": key}))
            .unwrap(),
    );
}

fn verify_archive(state: &Path, key: Hash) {
    let path = state.join("apply-intents-v1/archive").join(key.as_hex());
    let mut names = fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with('g'))
        .collect::<Vec<_>>();
    names.sort();
    assert!(!names.is_empty());
    let envelopes = names
        .iter()
        .map(|name| {
            ApplyIntentEnvelopeV1::from_canonical_bytes(&fs::read(path.join(name)).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    for pair in envelopes.windows(2) {
        pair[1]
            .generation
            .verify_successor(&pair[0].generation)
            .unwrap();
    }
    let seal =
        ArchiveSealV1::from_canonical_bytes(&fs::read(path.join("archive-seal.v1.json")).unwrap())
            .unwrap();
    seal.verify_terminal(
        envelopes[0].descriptor.as_ref().unwrap(),
        &envelopes.last().unwrap().generation,
    )
    .unwrap();
    assert_eq!(seal.terminal_phase, ApplyPhaseV1::AbandonedBeforeEffect);
}

fn verify_phase(root: &Path, phase: &str) {
    let published: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("published.json")).unwrap()).unwrap();
    assert_ne!(
        published["creator_pid"].as_u64().unwrap(),
        u64::from(std::process::id())
    );
    let key: Hash = serde_json::from_value(published["key"].clone()).unwrap();
    let project = root.join("project");
    let state = polaris_core::project::state_dir(&project).unwrap();
    let parent = parent(&state.join("sync/recovery"));
    let source = snapshot_bytes(&project);
    let before = snapshot_bytes(root);
    let owner = ApplyOwnerV1::acquire(&project).unwrap();
    let active = owner.active().unwrap();
    if phase == "archived" {
        assert!(active.is_none());
        verify_archive(&state, key);
    } else if let Some(active) = &active {
        assert_eq!(active.descriptor().operation_key, key);
        assert_eq!(
            active.latest().phase,
            if phase == "prepared" {
                ApplyPhaseV1::Prepared
            } else if matches!(active.latest().phase, ApplyPhaseV1::OutcomeUnknown) {
                ApplyPhaseV1::OutcomeUnknown
            } else {
                ApplyPhaseV1::EffectStarted
            }
        );
    }
    drop(owner.inspect_management().unwrap());
    assert_eq!(
        before,
        snapshot_bytes(root),
        "inspect changed durable evidence"
    );
    if owner.active().unwrap().is_some() {
        // A second valid C12 candidate is blocked even when the active owner
        // belongs to D27. Refusal must precede new durable intent/effect bytes.
        let snapshot = protected_snapshot(&project, limits()).unwrap();
        fs::write(snapshot.path().join("main.rs"), b"second candidate\n").unwrap();
        let changes = protected_collect_changes(&snapshot, limits()).unwrap();
        assert!(changes.is_applicable());
        let mut target = polaris_core::sync::participation::read(&state).unwrap();
        target.applied_commit = target.agreed_commit.clone();
        let revision = RevisionV1 {
            kind: RevisionKindV1::GitSha1,
            value: "a".repeat(40),
        };
        let frozen = snapshot_bytes(root);
        assert!(
            owner
                .apply_c12(
                    polaris_core::apply_owner::C12ApplyAdmissionV1 {
                        management_operation_id: ManagementOperationIdV1::random().unwrap(),
                        agreed_revision: revision.clone(),
                        staged_revision: revision,
                        previous_applied_revision: None,
                        target_participation: serde_json::to_vec(&target).unwrap(),
                    },
                    &snapshot,
                    &changes,
                    limits(),
                    &parent
                )
                .is_err()
        );
        assert_eq!(
            frozen,
            snapshot_bytes(root),
            "second intent mutated durable state"
        );
    }
    if phase == "started" {
        let event = match owner.recover_current(&parent).unwrap() {
            ApplyOwnerRecoveryV1::OutcomeUnknown(e) => e,
            other => panic!("{other:?}"),
        };
        let frozen = snapshot_bytes(root);
        let repeated = match owner.recover_current(&parent).unwrap() {
            ApplyOwnerRecoveryV1::OutcomeUnknown(e) => e,
            other => panic!("{other:?}"),
        };
        assert_eq!(event, repeated);
        assert_eq!(
            frozen,
            snapshot_bytes(root),
            "unknown retry grew durable evidence"
        );
        assert_eq!(
            owner.active().unwrap().unwrap().latest().phase,
            ApplyPhaseV1::OutcomeUnknown
        );
    } else {
        let result = owner.recover_current(&parent).unwrap();
        assert!(
            matches!(result, ApplyOwnerRecoveryV1::Archived(v) if v == key)
                || matches!(result, ApplyOwnerRecoveryV1::Idle)
        );
        assert!(owner.active().unwrap().is_none());
        verify_archive(&state, key);
        assert!(matches!(
            owner.recover_current(&parent).unwrap(),
            ApplyOwnerRecoveryV1::Idle
        ));
    }
    assert_eq!(
        source,
        snapshot_bytes(&project),
        "recovery reapplied source"
    );
    assert_eq!(fs::read(project.join("main.rs")).unwrap(), b"before\n");
    let unresolved = owner.active().unwrap().is_some();
    drop(owner);
    if unresolved {
        // The durable blocker also protects legacy mutations after the
        // process holding the apply lease has exited.
        let frozen = snapshot_bytes(root);
        let held = polaris_core::sync::participation::read(&state).unwrap();
        assert!(polaris_core::sync::participation::write(&state, &held).is_err());
        assert!(polaris_core::sync::participation::leave(&state).is_err());
        assert_eq!(
            frozen,
            snapshot_bytes(root),
            "legacy mutation bypassed unresolved intent"
        );
    }
}

#[test]
#[ignore = "requires explicit local XFS disposable parent and receipt"]
fn linux_apply_intent_restart_child() {
    let root = PathBuf::from(std::env::var_os("POLARIS_LOCAL_RESTART_CHILD_ROOT").unwrap());
    assert_eq!(
        identity(&root).device,
        std::env::var("POLARIS_LOCAL_RESTART_DEVICE")
            .unwrap()
            .parse::<u64>()
            .unwrap()
    );
    let inventory = std::env::var("POLARIS_LOCAL_RESTART_INVENTORY").unwrap();
    let phase = std::env::var("POLARIS_LOCAL_RESTART_PHASE").unwrap();
    assert!(["C12", "D27"].contains(&inventory.as_str()));
    assert!(["prepared", "started", "archived"].contains(&phase.as_str()));
    match std::env::var("POLARIS_LOCAL_RESTART_ROLE")
        .unwrap()
        .as_str()
    {
        "create" => create_phase(&root, &inventory, &phase),
        "verify" => verify_phase(&root, &phase),
        role => panic!("unknown role {role}"),
    }
}

#[test]
#[ignore = "requires explicit local XFS disposable parent and receipt"]
fn linux_apply_intent_restart_suite() {
    let candidate = PathBuf::from(std::env::var_os("POLARIS_LOCAL_RESTART_ROOT").unwrap());
    let receipt = PathBuf::from(std::env::var_os("POLARIS_LOCAL_RESTART_RECEIPT").unwrap());
    let device = std::env::var("POLARIS_LOCAL_RESTART_DEVICE")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(
        candidate.is_absolute()
            && receipt.is_absolute()
            && !candidate.exists()
            && !receipt.exists()
    );
    let candidate_parent = candidate.parent().unwrap();
    assert_eq!(identity(candidate_parent).device, device);
    let parent_identity = identity(candidate_parent);
    save(&receipt, &serde_json::to_vec(&serde_json::json!({"state":"reserved", "creator_pid":std::process::id(), "root":candidate, "parent":parent_identity})).unwrap());
    directory(&candidate);
    let validated = polaris_core::linux_l04_parent::validate_disposable_parent_candidate(
        &candidate,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        device,
    )
    .unwrap();
    drop(validated);
    let root_identity = identity(&candidate);
    let cli = std::env::var_os("POLARIS_LOCAL_RESTART_CLI").map(PathBuf::from);
    if let Some(cli) = &cli {
        let metadata = fs::symlink_metadata(cli).unwrap();
        assert!(cli.is_absolute() && metadata.is_file() && metadata.mode() & 0o111 != 0);
    }
    let mut cli_process_count = 0;
    for inventory in ["C12", "D27"] {
        for phase in ["prepared", "started", "archived"] {
            let root = candidate.join(format!("{inventory}-{phase}"));
            directory(&root);
            directory(&root.join("data"));
            for role in ["create", "verify", "verify"] {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--ignored",
                        "--exact",
                        "linux_apply_intent_restart_child",
                        "--nocapture",
                    ])
                    .env("POLARIS_DATA_DIR", root.join("data"))
                    .env("POLARIS_LOCAL_RESTART_CHILD_ROOT", &root)
                    .env("POLARIS_LOCAL_RESTART_DEVICE", device.to_string())
                    .env("POLARIS_LOCAL_RESTART_INVENTORY", inventory)
                    .env("POLARIS_LOCAL_RESTART_PHASE", phase)
                    .env("POLARIS_LOCAL_RESTART_ROLE", role)
                    .output()
                    .unwrap();
                assert!(output.stdout.len() + output.stderr.len() < 1024 * 1024);
                assert!(
                    output.status.success(),
                    "{inventory}/{phase}/{role}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if phase == "started" {
                if let Some(cli) = &cli {
                    let frozen = snapshot_bytes(&root);
                    for command in ["status", "apply"] {
                        let output = Command::new(cli)
                            .args(["sync", command])
                            .current_dir(root.join("project"))
                            .env("POLARIS_DATA_DIR", root.join("data"))
                            .output()
                            .unwrap();
                        cli_process_count += 1;
                        assert!(output.stdout.len() + output.stderr.len() < 1024 * 1024);
                        let message = format!(
                            "{}{}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        );
                        if command == "status" {
                            assert!(
                                output.status.success()
                                    && message.contains("OutcomeUnknown")
                                    && message.contains("未完了"),
                                "{message}"
                            );
                        } else {
                            let expected = if inventory == "C12" {
                                "適用結果が未確定"
                            } else {
                                "Desktopの適用が未完了"
                            };
                            assert!(
                                !output.status.success() && message.contains(expected),
                                "{message}"
                            );
                        }
                        assert_eq!(
                            frozen,
                            snapshot_bytes(&root),
                            "CLI {inventory}/{command} changed unresolved evidence or source"
                        );
                    }
                }
            }
        }
    }
    assert_eq!(identity(candidate_parent), parent_identity);
    assert_eq!(identity(&candidate), root_identity);
    // The approved local reset retains artifacts rather than deleting targets.
    save(&receipt.with_extension("completed.json"), &serde_json::to_vec(&serde_json::json!({
        "state":"verified_retained", "root":candidate, "identity":root_identity,
        "case_count":6, "child_process_count":18, "cli_process_count":cli_process_count, "source_reapplied":false,
        "limit":"canonical phase fixtures; production apply kill and physical reconnect unverified"
    })).unwrap());
}
