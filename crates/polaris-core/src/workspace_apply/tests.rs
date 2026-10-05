//! Conflict-safe apply-back, durable recovery artifacts, and failure injection tests.
use super::*;
use std::fs;
use std::os::unix::fs::symlink;

fn m2a_maximum_entry() -> EntryReport {
    EntryReport {
        relative_path: "a.txt".into(),
        source_parent_chain: vec![
            ("/".into(), (7, u64::MAX)),
            ("/fixture".into(), (7, u64::MAX)),
            ("/fixture/source".into(), (7, 11)),
        ],
        phase: Phase::PreserveIntent,
        old_retained: false,
        installed: false,
        deleted: false,
        restored: false,
        old_name: "old-0".into(),
        candidate_name: Some("candidate-0".into()),
        install_name: Some("install-0".into()),
        candidate_identity: Some((7, u64::MAX)),
        install_identity: Some((7, u64::MAX)),
    }
}

fn m2a_maximum_header() -> JournalHeaderV2 {
    use crate::{apply_intent::IdentityV1, management_effect::ManagementEffectHashV1 as Hash};
    JournalHeaderV2 {
        version: 2,
        operation_key: Hash::from_hex(
            "a25ca49138e9dfa338efc5b2dacd7f16e53ec9344af4ae134b09cae2080dd9a8",
        )
        .unwrap(),
        intent_descriptor_hash: Hash::from_hex(
            "e6cfad4399508070a4511d052309ce761a7f12a6ff5e372580c748e1d3ec44a2",
        )
        .unwrap(),
        recovery_parent_identity: IdentityV1 {
            device: 7,
            inode: 29,
        },
        source: "/fixture/source".into(),
        source_chain: m2a_maximum_entry().source_parent_chain,
        recovery_chain: vec![
            ("/".into(), (7, u64::MAX)),
            ("/fixture".into(), (7, u64::MAX)),
            ("/fixture/recovery".into(), (7, 29)),
            (
                "/fixture/recovery/polaris-apply-v1-d24e2fa10f887d84139e77f5a4385930".into(),
                (7, u64::MAX),
            ),
        ],
        entries: vec![JournalChange {
            after: Some(JournalAfter {
                bytes: 3,
                hash: [0xee; 32],
                mode: 0o777,
            }),
            before: Some(JournalBefore {
                bytes: 3,
                device: 7,
                hash: [0xff; 32],
                inode: u64::MAX,
                mode: 0o7777,
            }),
            path: "a.txt".into(),
        }],
    }
}

#[test]
fn m2a_journal_encoder_and_reader_reproduce_independent_frozen_bytes() {
    let frozen = include_bytes!(
        "../../tests/fixtures/apply-intent-v1-capacity-positive/workspace-journal-v2.jsonl"
    );
    let header = m2a_maximum_header();
    let mut encoded = header.canonical_bytes().unwrap();
    encoded.push(b'\n');
    for phase in [
        Phase::Prepared,
        Phase::PreserveIntent,
        Phase::Preserved,
        Phase::InstallIntent,
        Phase::RestoreIntent,
        Phase::Restored,
    ] {
        let mut entry = m2a_maximum_entry();
        entry.phase = phase;
        encoded.extend(serde_json::to_vec(&entry).unwrap());
        encoded.push(b'\n');
    }
    assert_eq!(encoded, frozen);
    assert_eq!(encoded.len(), 3442);
    let decoded = decode_journal(&encoded, encoded.len() as u64).unwrap();
    assert!(matches!(decoded.header, SavedJournalHeader::V2(h) if h == header));
    assert_eq!(decoded.entries.len(), 6);
    assert!(decode_journal(&encoded, encoded.len() as u64 - 1).is_err());
    assert_eq!(
        journal_tip_hash(&encoded, encoded.len() as u64)
            .unwrap()
            .as_hex(),
        "e4347404240688fd412291bd12e901188e4f7c4ddfb41f2a21243e27d5af4573"
    );
    assert_eq!(
        header.header_hash().unwrap().as_hex(),
        "524e4ba8d6bb9576133b937e5f2f9ce56b241a34d63d05c17b6afb7311dc000d"
    );
}

#[test]
fn m2a_journal_dispatch_preserves_v1_and_rejects_unknown_or_noncanonical_v2() {
    let legacy = include_bytes!("../../tests/fixtures/workspace-apply-journal-v1.jsonl");
    let before = legacy.to_vec();
    let decoded = decode_journal(legacy, legacy.len() as u64).unwrap();
    assert!(matches!(decoded.header, SavedJournalHeader::V1(_)));
    assert_eq!(decoded.entries.len(), 5);
    assert!(journal_tip_hash(legacy, legacy.len() as u64).is_err());
    assert_eq!(&before, legacy);
    let h = m2a_maximum_header();
    let mut valid = h.canonical_bytes().unwrap();
    valid.push(b'\n');
    assert!(decode_journal(&valid, valid.len() as u64).is_ok());
    for bytes in [
        String::from_utf8(valid.clone())
            .unwrap()
            .replacen("\"version\":2", "\"version\":3", 1)
            .into_bytes(),
        String::from_utf8(valid.clone())
            .unwrap()
            .replacen("\"version\":2", "\"version\":2,\"version\":2", 1)
            .into_bytes(),
        String::from_utf8(valid.clone())
            .unwrap()
            .replacen("\"version\":2", "\"version\":2,\"foreign\":null", 1)
            .into_bytes(),
        String::from_utf8(valid.clone())
            .unwrap()
            .replacen("\"version\":2", "\"version\":2.0", 1)
            .into_bytes(),
        String::from_utf8(valid.clone())
            .unwrap()
            .replacen("\"inode\":29", "\"inode\":30", 1)
            .into_bytes(),
        String::from_utf8(valid.clone())
            .unwrap()
            .replacen(
                "d24e2fa10f887d84139e77f5a4385930",
                "d24e2fa10f887d84139e77f5a4385931",
                1,
            )
            .into_bytes(),
        valid[..valid.len() - 1].to_vec(),
    ] {
        assert_ne!(bytes, valid);
        assert!(decode_journal(&bytes, bytes.len() as u64).is_err());
    }
}

fn m2a_maximum_report() -> ApplyReport {
    ApplyReport {
        source_path: "/fixture/source".into(),
        recovery_path: Some(
            "/fixture/recovery/polaris-apply-v1-d24e2fa10f887d84139e77f5a4385930".into(),
        ),
        recovery_identity: Some((7, u64::MAX)),
        recovery_directory: None,
        entries: vec![m2a_maximum_entry()],
        failure: Some(ApplyFailure {
            kind: FailureKind::ProtectionUnavailable,
            entry: Some(u64::MAX as usize),
            os_error: Some(i32::MIN),
        }),
    }
}

#[test]
fn m2a_report_encoder_and_reader_preserve_independent_exact_bytes() {
    let frozen = include_bytes!(
        "../../tests/fixtures/apply-intent-v1-capacity-positive/apply-report-v1.json"
    );
    let report = m2a_maximum_report();
    assert_eq!(report.canonical_bytes().unwrap(), frozen);
    assert_eq!(frozen.len(), 681);
    assert_eq!(
        report.evidence_hash().unwrap().as_hex(),
        "e79ce4d4cf92832811067a51e8e4644270946fcdc7c572125b0c9e11194221a0"
    );
    let reopened = decode_apply_report(frozen).unwrap();
    assert_eq!(reopened.canonical_bytes().unwrap(), frozen);
    assert_eq!(reopened.entries, report.entries);
    assert_eq!(reopened.failure, report.failure);
    assert!(reopened.recovery_directory().is_none());
    let legacy = include_bytes!("../../tests/fixtures/workspace-apply-report-v1.json");
    // The old fixture includes its text-file LF; report payload itself does not.
    let legacy = legacy.strip_suffix(b"\n").unwrap_or(legacy);
    assert_eq!(
        decode_apply_report(legacy)
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        legacy
    );
    for bytes in [
        String::from_utf8(frozen.to_vec())
            .unwrap()
            .replacen("\"failure\":", "\"foreign\":null,\"failure\":", 1)
            .into_bytes(),
        String::from_utf8(frozen.to_vec())
            .unwrap()
            .replacen(
                "\"recovery_identity\":",
                "\"recovery_identity\":null,\"recovery_identity\":",
                1,
            )
            .into_bytes(),
        [frozen.as_slice(), b" "].concat(),
    ] {
        assert!(decode_apply_report(&bytes).is_err());
    }
}

#[test]
fn m2a_report_publication_uses_same_fd_and_retains_collision() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), Permissions::from_mode(0o700)).unwrap();
    let directory = File::open(root.path()).unwrap();
    let mut report = m2a_maximum_report();
    report.recovery_path = Some(root.path().to_path_buf());
    report.recovery_identity = Some(identity(&directory.metadata().unwrap()));
    let expected = report.canonical_bytes().unwrap();
    assert_eq!(
        report.publish_exact(&directory).unwrap(),
        report.evidence_hash().unwrap()
    );
    let path = root.path().join("apply-report.v1.json");
    let inode = identity(&fs::metadata(&path).unwrap());
    assert_eq!(fs::read(&path).unwrap(), expected);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
    report.failure = None;
    assert!(report.publish_exact(&directory).is_err());
    assert_eq!(fs::read(&path).unwrap(), expected);
    assert_eq!(identity(&fs::metadata(&path).unwrap()), inode);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn m2a_capacity_request_reproduces_frozen_maximum_lengths_and_variants() {
    use crate::apply_intent::{CanonicalApplyEvidence, RequestV1};
    let request = RequestV1::from_canonical_bytes(include_bytes!(
        "../../tests/fixtures/apply-intent-v1-capacity-positive/request.v1.json"
    ))
    .unwrap();
    let header = m2a_maximum_header();
    let parent = &header.recovery_chain[..header.recovery_chain.len() - 1];
    let bound = capacity_upper_bound(
        &request,
        &header.source,
        &header.source_chain,
        &parent.last().unwrap().0,
        parent,
        header.operation_key,
    )
    .unwrap();
    assert_eq!(
        bound,
        WorkspaceCapacityUpperBound {
            journal_bytes: 3442,
            report_bytes: 681
        }
    );
    assert_eq!(
        bound.journal_bytes as usize,
        include_bytes!(
            "../../tests/fixtures/apply-intent-v1-capacity-positive/workspace-journal-v2.jsonl"
        )
        .len()
    );
    assert_eq!(
        bound.report_bytes as usize,
        include_bytes!(
            "../../tests/fixtures/apply-intent-v1-capacity-positive/apply-report-v1.json"
        )
        .len()
    );
    for kind in [
        FailureKind::ProtectionUnavailable,
        FailureKind::InvalidChangeSet,
        FailureKind::UnsupportedEntry,
        FailureKind::Conflict,
        FailureKind::Secret,
        FailureKind::AncestorChanged,
        FailureKind::CrossDevice,
        FailureKind::Io,
        FailureKind::RestoreConflict,
    ] {
        for phase in [
            Phase::Preparing,
            Phase::Prepared,
            Phase::PreserveIntent,
            Phase::Preserved,
            Phase::RestoreIntent,
            Phase::Restored,
            Phase::InstallIntent,
            Phase::Installed,
            Phase::DeleteIntent,
            Phase::Deleted,
        ] {
            let mut report = m2a_maximum_report();
            report.failure.as_mut().unwrap().kind = kind;
            report.entries[0].phase = phase;
            assert!(report.canonical_bytes().unwrap().len() as u64 <= bound.report_bytes);
            report.entries[0].old_retained = true;
            report.entries[0].installed = true;
            report.entries[0].deleted = true;
            report.entries[0].restored = true;
            assert!(report.canonical_bytes().unwrap().len() as u64 <= bound.report_bytes);
        }
    }
    let mut wrong = request.clone();
    wrong.source_identity.inode += 1;
    assert!(
        capacity_upper_bound(
            &wrong,
            &header.source,
            &header.source_chain,
            &parent.last().unwrap().0,
            parent,
            header.operation_key
        )
        .is_err()
    );
    let mut length = EncodedLength(u64::MAX);
    assert!(length.write_all(b"x").is_err());
    assert_eq!(length.0, u64::MAX);
}

#[test]
fn m2a_reserved_child_header_precedes_apply_and_reopens_without_mutation() {
    let f = Fixture::new();
    fs::write(f.source.join("a.txt"), b"old").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("a.txt"), b"new").unwrap();
    let requested = changes(&snapshot);
    let parent = pinned_parent(&f);
    let fixed = m2a_maximum_header();
    let reserved = prepare_reserved_recovery_v2(
        &snapshot,
        &requested,
        &secrets(),
        limits(),
        &parent,
        fixed.operation_key,
        fixed.intent_descriptor_hash,
    )
    .unwrap();
    let header_bytes = fs::read(reserved.path().join("journal.jsonl")).unwrap();
    assert_eq!(fs::read(f.source.join("a.txt")).unwrap(), b"old");
    assert_eq!(fs::read_dir(reserved.path()).unwrap().count(), 1);
    assert_eq!(
        decode_journal(&header_bytes, header_bytes.len() as u64)
            .unwrap()
            .entries
            .len(),
        0
    );
    let id = identity(&reserved.directory().metadata().unwrap());
    let reopened = inspect_reserved_recovery_v2(
        &parent,
        fixed.operation_key,
        fixed.intent_descriptor_hash,
        65536,
    )
    .unwrap()
    .unwrap();
    assert_eq!(reopened.header(), reserved.header());
    assert_eq!(identity(&reopened.directory().metadata().unwrap()), id);
    assert_eq!(
        fs::read(reserved.path().join("journal.jsonl")).unwrap(),
        header_bytes
    );
    let report = apply_reserved_v2(&snapshot, &requested, &secrets(), limits(), &reopened);
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(fs::read(f.source.join("a.txt")).unwrap(), b"new");
    assert_eq!(fs::read(reserved.path().join("old-0")).unwrap(), b"old");
    assert_eq!(
        fs::read(reserved.path().join("candidate-0")).unwrap(),
        b"new"
    );
    let journal = fs::read(reserved.path().join("journal.jsonl")).unwrap();
    let decoded = decode_journal(&journal, journal.len() as u64).unwrap();
    assert_eq!(decoded.entries.len(), 5);
    assert!(matches!(decoded.header, SavedJournalHeader::V2(h) if &h == reserved.header()));
    assert!(journal.starts_with(&header_bytes));
    assert_eq!(report.recovery_identity, Some(id));
    report.publish_exact(reserved.directory()).unwrap();
    let before = fs::read(reserved.path().join("apply-report.v1.json")).unwrap();
    let again = apply_reserved_v2(&snapshot, &requested, &secrets(), limits(), &reserved);
    assert!(again.failure.is_some());
    assert_eq!(
        fs::read(reserved.path().join("apply-report.v1.json")).unwrap(),
        before
    );
    assert_eq!(
        fs::read(reserved.path().join("journal.jsonl")).unwrap(),
        journal
    );
    assert_eq!(fs::read_dir(&f.recovery).unwrap().count(), 1);
}

#[test]
fn m2a_empty_reserved_apply_keeps_recovery_identity_and_exact_report() {
    let f = Fixture::new();
    let snapshot = f.snapshot();
    let requested = changes(&snapshot);
    assert!(requested.changes.is_empty());
    let parent = pinned_parent(&f);
    let fixed = m2a_maximum_header();
    let reserved = prepare_reserved_recovery_v2(
        &snapshot,
        &requested,
        &secrets(),
        limits(),
        &parent,
        fixed.operation_key,
        fixed.intent_descriptor_hash,
    )
    .unwrap();
    let before = fs::read(reserved.path().join("journal.jsonl")).unwrap();
    let report = apply_reserved_v2(&snapshot, &requested, &secrets(), limits(), &reserved);
    assert!(report.failure.is_none(), "{report:?}");
    assert!(report.entries.is_empty());
    assert_eq!(report.recovery_path.as_deref(), Some(reserved.path()));
    assert_eq!(
        report.recovery_identity,
        Some(identity(&reserved.directory().metadata().unwrap()))
    );
    report.publish_exact(reserved.directory()).unwrap();
    assert_eq!(
        decode_apply_report(&fs::read(reserved.path().join("apply-report.v1.json")).unwrap())
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        report.canonical_bytes().unwrap()
    );
    assert_eq!(
        fs::read(reserved.path().join("journal.jsonl")).unwrap(),
        before
    );
    assert_eq!(fs::read_dir(&f.source).unwrap().count(), 0);
}

#[test]
fn m2a_reserved_child_rejects_collision_foreign_prefix_link_and_replacement() {
    for case in 0..6 {
        let f = Fixture::new();
        fs::write(f.source.join("a.txt"), b"old").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("a.txt"), b"new").unwrap();
        let requested = changes(&snapshot);
        let parent = pinned_parent(&f);
        let fixed = m2a_maximum_header();
        assert!(
            inspect_reserved_recovery_v2(
                &parent,
                fixed.operation_key,
                fixed.intent_descriptor_hash,
                65536
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(fs::read_dir(&f.recovery).unwrap().count(), 0);
        let reserved = prepare_reserved_recovery_v2(
            &snapshot,
            &requested,
            &secrets(),
            limits(),
            &parent,
            fixed.operation_key,
            fixed.intent_descriptor_hash,
        )
        .unwrap();
        let journal_path = reserved.path().join("journal.jsonl");
        let original = fs::read(&journal_path).unwrap();
        assert!(
            prepare_reserved_recovery_v2(
                &snapshot,
                &requested,
                &secrets(),
                limits(),
                &parent,
                fixed.operation_key,
                fixed.intent_descriptor_hash
            )
            .is_err()
        );
        assert_eq!(fs::read(&journal_path).unwrap(), original);
        let mut retained = reserved.path().to_path_buf();
        match case {
            0 => {
                fs::write(&journal_path, &original[..original.len() / 2]).unwrap();
            }
            1 => {
                fs::hard_link(&journal_path, reserved.path().join("foreign-link")).unwrap();
            }
            2 => {
                retained = f.recovery.join("retained-child");
                fs::rename(reserved.path(), &retained).unwrap();
                fs::create_dir(reserved.path()).unwrap();
                fs::write(reserved.path().join("sentinel"), b"keep replacement").unwrap();
            }
            3 => {
                fs::write(reserved.path().join("foreign"), b"retain").unwrap();
            }
            4 => {
                fs::rename(&journal_path, reserved.path().join("retained-journal")).unwrap();
                symlink(f.source.join("a.txt"), &journal_path).unwrap();
            }
            5 => {
                fs::set_permissions(&journal_path, Permissions::from_mode(0o644)).unwrap();
            }
            _ => unreachable!(),
        }
        let before_entries = fs::read_dir(&retained).unwrap().count();
        let before_bytes = fs::read(retained.join("journal.jsonl")).unwrap();
        let report = apply_reserved_v2(&snapshot, &requested, &secrets(), limits(), &reserved);
        assert!(report.failure.is_some(), "case {case}");
        assert_eq!(
            fs::read(f.source.join("a.txt")).unwrap(),
            b"old",
            "case {case}"
        );
        assert_eq!(fs::read_dir(&retained).unwrap().count(), before_entries);
        assert_eq!(
            fs::read(retained.join("journal.jsonl")).unwrap(),
            before_bytes
        );
        if case == 2 {
            assert_eq!(
                fs::read(reserved.path().join("sentinel")).unwrap(),
                b"keep replacement"
            );
        }
    }
}

fn m2a_request(snapshot: &Snapshot, requested: &ChangeSet) -> crate::apply_intent::RequestV1 {
    use crate::{
        apply_intent::{AfterV1, BeforeV1, IdentityV1, RequestEntryV1, RequestV1},
        management_effect::ManagementEffectHashV1 as Hash,
    };
    let hash = |raw: [u8; 32]| {
        Hash::from_hex(&raw.iter().map(|b| format!("{b:02x}")).collect::<String>()).unwrap()
    };
    RequestV1 {
        schema_version: 1,
        source_identity: IdentityV1 {
            device: snapshot.source_identity.device,
            inode: snapshot.source_identity.inode,
        },
        entries: requested
            .changes
            .iter()
            .map(|c| RequestEntryV1 {
                path: c.relative_path.to_str().unwrap().to_string(),
                before: c.before.as_ref().map(|e| BeforeV1 {
                    device: e.identity.device,
                    inode: e.identity.inode,
                    hash: hash(e.sha256.unwrap()),
                    mode: e.mode,
                    observed_mode: e.observed_mode(),
                    bytes: e.bytes,
                }),
                after: c.after.as_ref().map(|e| AfterV1 {
                    hash: hash(e.sha256.unwrap()),
                    mode: e.mode,
                    bytes: e.bytes,
                }),
            })
            .collect(),
    }
}

#[test]
fn m2a_capacity_bounds_actual_create_replace_delete_outputs() {
    let f = Fixture::new();
    fs::write(f.source.join("a-replace"), b"old").unwrap();
    fs::write(f.source.join("c-delete"), b"delete").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("a-replace"), b"new").unwrap();
    fs::write(snapshot.path().join("b-create"), b"create").unwrap();
    fs::remove_file(snapshot.path().join("c-delete")).unwrap();
    let requested = changes(&snapshot);
    assert_eq!(requested.changes.len(), 3);
    let parent = pinned_parent(&f);
    let fixed = m2a_maximum_header();
    let reserved = prepare_reserved_recovery_v2(
        &snapshot,
        &requested,
        &secrets(),
        limits(),
        &parent,
        fixed.operation_key,
        fixed.intent_descriptor_hash,
    )
    .unwrap();
    let header = reserved.header();
    let parent_chain = &header.recovery_chain[..header.recovery_chain.len() - 1];
    let bound = capacity_upper_bound(
        &m2a_request(&snapshot, &requested),
        &header.source,
        &header.source_chain,
        parent.provenance(),
        parent_chain,
        fixed.operation_key,
    )
    .unwrap();
    let report = apply_reserved_v2(&snapshot, &requested, &secrets(), limits(), &reserved);
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(fs::read(f.source.join("a-replace")).unwrap(), b"new");
    assert_eq!(fs::read(f.source.join("b-create")).unwrap(), b"create");
    assert!(!f.source.join("c-delete").exists());
    assert!(
        fs::metadata(reserved.path().join("journal.jsonl"))
            .unwrap()
            .len()
            <= bound.journal_bytes
    );
    assert!(report.canonical_bytes().unwrap().len() as u64 <= bound.report_bytes);
}

thread_local! {
    static PANIC_AT: std::cell::Cell<Option<Point>> = const { std::cell::Cell::new(None) };
}

pub(super) fn panic_at_checkpoint(point: Point) {
    PANIC_AT.with(|at| assert_ne!(at.get(), Some(point), "dummy apply panic"));
}

fn protected_fixture(case: &str) {
    let home = tempfile::tempdir().unwrap();
    let marker = tempfile::Builder::new()
        .prefix(".polaris-protected-apply-")
        .tempfile_in(home.path())
        .unwrap();
    fs::write(marker.path(), b"isolated-home-v1").unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "workspace_apply::tests::protected_apply_fixture",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", home.path().canonicalize().unwrap())
        .env("POLARIS_PROTECTED_APPLY_FIXTURE", case)
        .env(
            "POLARIS_PROTECTED_APPLY_MARKER",
            marker.path().file_name().unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("protected-apply-verified"));
}

#[test]
fn protected_apply_allows_ordinary_authorized_fixture() {
    protected_fixture("ordinary");
}

#[test]
fn protected_apply_pinned_allows_ordinary_authorized_fixture() {
    protected_fixture("pinned");
}

#[test]
fn protected_apply_rejects_registered_rotated_identity() {
    protected_fixture("rotated");
}

#[test]
fn protected_apply_refuses_inaccessible_registry_without_fallback() {
    protected_fixture("poisoned");
}

#[test]
fn protected_apply_keeps_registry_lock_through_source_install() {
    protected_fixture("lock_scope");
}

#[test]
fn protected_apply_fixture() {
    let case = match std::env::var("POLARIS_PROTECTED_APPLY_FIXTURE") {
        Ok(case)
            if matches!(
                case.as_str(),
                "ordinary" | "pinned" | "rotated" | "poisoned" | "lock_scope"
            ) =>
        {
            case
        }
        _ => return,
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let Some(marker_name) = std::env::var_os("POLARIS_PROTECTED_APPLY_MARKER") else {
        return;
    };
    let marker = Path::new(&marker_name);
    if marker.components().count() != 1
        || !matches!(marker.components().next(), Some(Component::Normal(_)))
        || !marker_name
            .to_string_lossy()
            .starts_with(".polaris-protected-apply-")
        || fs::read(home.join(&marker_name)).ok().as_deref() != Some(&b"isolated-home-v1"[..])
    {
        return;
    }
    let store = home.join("custom-store");
    polaris_auth::api_key::save_to(&store, "DUMMY_OLD_TOKEN").unwrap();
    let f = Fixture::new();
    if case == "rotated" {
        fs::rename(&store, f.source.join("ordinary.rs")).unwrap();
        polaris_auth::api_key::save_to(&store, "DUMMY_NEW_TOKEN").unwrap();
    } else {
        fs::write(f.source.join("ordinary.rs"), b"before").unwrap();
    }
    let before = fs::read(f.source.join("ordinary.rs")).unwrap();
    let before_identity = identity(&fs::metadata(f.source.join("ordinary.rs")).unwrap());
    // Deliberately use an empty caller registry to build a stale request that
    // contains the dummy rotated inode. protected_apply must import the real
    // process registry rather than trusting that omission or only current names.
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("ordinary.rs"), b"candidate").unwrap();
    let request = changes(&snapshot);
    assert_eq!(request.changes.len(), 1);
    if case == "poisoned" {
        let poisoned = std::panic::catch_unwind(|| {
            let _ =
                polaris_auth::protection::with_protected_paths(|_| panic!("dummy registry poison"));
        });
        assert!(poisoned.is_err());
    }
    if case == "lock_scope" {
        // A panic after install poisons the registry iff the wrapper still
        // holds its lock at that point. No scheduling or timeout assumption.
        PANIC_AT.with(|point| point.set(Some(Point::AfterInstall)));
        let interrupted = std::panic::catch_unwind(|| {
            protected_apply(&snapshot, &request, limits(), &f.recovery)
        });
        PANIC_AT.with(|point| point.set(None));
        assert!(interrupted.is_err());
        assert!(polaris_auth::protection::with_protected_paths(|_| ()).is_err());
        assert_eq!(
            fs::read(f.source.join("ordinary.rs")).unwrap(),
            b"candidate"
        );
        let recovery = fs::read_dir(&f.recovery)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(recovery.join("old-0")).unwrap(), before);
        println!("protected-apply-verified");
        return;
    }
    let report = if case == "pinned" {
        protected_apply_pinned(&snapshot, &request, limits(), &pinned_parent(&f))
    } else {
        protected_apply(&snapshot, &request, limits(), &f.recovery)
    };
    match case.as_str() {
        "ordinary" | "pinned" => {
            assert!(report.failure.is_none(), "{report:?}");
            assert!(report.entries[0].installed);
            assert_eq!(
                fs::read(f.source.join("ordinary.rs")).unwrap(),
                b"candidate"
            );
            assert_eq!(fs::read(old_path(&report, 0)).unwrap(), before);
            assert_eq!(
                polaris_auth::api_key::load_from(&store).unwrap().as_deref(),
                Some("DUMMY_OLD_TOKEN")
            );
        }
        "rotated" | "poisoned" => {
            assert!(report.failure.is_some(), "{report:?}");
            assert_eq!(
                report.failure.as_ref().unwrap().kind,
                if case == "poisoned" {
                    FailureKind::ProtectionUnavailable
                } else {
                    FailureKind::InvalidChangeSet
                }
            );
            assert!(report.entries.is_empty());
            assert!(report.recovery_path.is_none());
            assert!(fs::read_dir(&f.recovery).unwrap().next().is_none());
            assert_eq!(fs::read(f.source.join("ordinary.rs")).unwrap(), before);
            assert_eq!(
                identity(&fs::metadata(f.source.join("ordinary.rs")).unwrap()),
                before_identity
            );
            if case == "rotated" {
                assert_eq!(
                    polaris_auth::api_key::load_from(&store).unwrap().as_deref(),
                    Some("DUMMY_NEW_TOKEN")
                );
            }
        }
        _ => unreachable!(),
    }
    println!("protected-apply-verified");
}

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    recovery: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let source = base.join("source");
        let recovery = base.join("recovery");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&recovery).unwrap();
        Self {
            _temp: temp,
            source,
            recovery,
        }
    }
    fn snapshot(&self) -> Snapshot {
        copy::snapshot(&self.source, &secrets(), limits()).unwrap()
    }
}
fn limits() -> Limits {
    Limits {
        max_entries: 64,
        max_files: 32,
        max_file_bytes: 4096,
        max_total_bytes: 32768,
        max_depth: 8,
    }
}
fn secrets() -> RegisteredSecrets {
    RegisteredSecrets::new([]).unwrap()
}
fn changes(snapshot: &Snapshot) -> ChangeSet {
    copy::collect_changes(snapshot, &secrets(), limits()).unwrap()
}
fn failure() -> std::io::Error {
    std::io::Error::other("injected")
}
fn old_path(report: &ApplyReport, index: usize) -> PathBuf {
    report
        .recovery_path
        .as_ref()
        .unwrap()
        .join(&report.entries[index].old_name)
}
fn candidate_path(report: &ApplyReport, index: usize) -> PathBuf {
    report
        .recovery_path
        .as_ref()
        .unwrap()
        .join(report.entries[index].candidate_name.as_ref().unwrap())
}
fn run_fixture(f: &Fixture, snapshot: &Snapshot, hook: &mut Hook<'_>) -> ApplyReport {
    apply_with_hook(
        snapshot,
        &changes(snapshot),
        &secrets(),
        limits(),
        &f.recovery,
        hook,
    )
}

#[test]
fn applies_only_plain_files_and_keeps_originals_and_fixed_candidates_after_success() {
    let f = Fixture::new();
    fs::write(f.source.join("modify"), b"before").unwrap();
    fs::write(f.source.join("delete"), b"retained-delete").unwrap();
    fs::write(f.source.join("mode"), b"unchanged").unwrap();
    fs::set_permissions(f.source.join("mode"), Permissions::from_mode(0o600)).unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("modify"), b"after").unwrap();
    fs::write(snapshot.path().join("create"), b"created").unwrap();
    fs::remove_file(snapshot.path().join("delete")).unwrap();
    fs::set_permissions(snapshot.path().join("mode"), Permissions::from_mode(0o751)).unwrap();
    let report = run_fixture(&f, &snapshot, &mut |_, _, _| Ok(()));
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(report.entries.len(), 4);
    assert_eq!(fs::read(f.source.join("modify")).unwrap(), b"after");
    assert_eq!(fs::read(f.source.join("create")).unwrap(), b"created");
    assert!(!f.source.join("delete").exists());
    assert_eq!(
        fs::metadata(f.source.join("mode")).unwrap().mode() & 0o7777,
        0o751
    );
    for (i, entry) in report.entries.iter().enumerate() {
        if entry.relative_path != Path::new("create") {
            assert!(old_path(&report, i).is_file());
        }
        if entry.installed {
            assert!(candidate_path(&report, i).is_file());
            assert_ne!(
                identity(&fs::metadata(candidate_path(&report, i)).unwrap()),
                identity(&fs::metadata(f.source.join(&entry.relative_path)).unwrap())
            );
        }
    }
    let recovery = report.recovery_path.clone().unwrap();
    drop(snapshot);
    drop(report);
    assert!(
        recovery.join("journal.jsonl").is_file(),
        "no automatic cleanup"
    );
}

#[test]
fn rejects_tampered_omitted_duplicate_and_foreign_changesets_before_any_source_move() {
    for tamper in 0..5 {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"before").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"after").unwrap();
        let mut request = changes(&snapshot);
        match tamper {
            0 => request.changes[0].after.as_mut().unwrap().sha256 = Some([0; 32]),
            1 => request.changes.clear(),
            2 => request.changes.push(request.changes[0].clone()),
            3 => request.changes[0].relative_path = "../outside".into(),
            _ => {
                let other = f.snapshot();
                fs::write(other.path().join("file"), b"after").unwrap();
                request = changes(&other);
            }
        }
        let report = apply(&snapshot, &request, &secrets(), limits(), &f.recovery);
        assert!(report.failure.is_some(), "{tamper}");
        assert!(report.recovery_path.is_none());
        assert_eq!(fs::read(f.source.join("file")).unwrap(), b"before");
    }
}

#[test]
fn rejects_directory_deletion_type_replacement_and_external_recovery_scope() {
    for replace in [false, true] {
        let f = Fixture::new();
        fs::create_dir(f.source.join("dir")).unwrap();
        let snapshot = f.snapshot();
        fs::remove_dir(snapshot.path().join("dir")).unwrap();
        if replace {
            fs::write(snapshot.path().join("dir"), b"file").unwrap();
        }
        let report = apply(
            &snapshot,
            &changes(&snapshot),
            &secrets(),
            limits(),
            &f.recovery,
        );
        assert_eq!(report.failure.unwrap().kind, FailureKind::UnsupportedEntry);
        assert!(f.source.join("dir").is_dir());
        assert!(report.recovery_path.is_none());
    }
    let f = Fixture::new();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("new"), b"new").unwrap();
    for recovery in [f.source.as_path(), snapshot.scratch_home()] {
        assert!(
            apply(
                &snapshot,
                &changes(&snapshot),
                &secrets(),
                limits(),
                recovery
            )
            .failure
            .is_some()
        );
    }
}

#[test]
fn candidate_is_fixed_before_source_moves_and_detects_copy_races() {
    for mutate_before in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"before").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"approved").unwrap();
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if point
                == if mutate_before {
                    Point::BeforePrepare
                } else {
                    Point::AfterPrepared
                }
            {
                fs::write(snapshot.path().join("file"), b"unapproved")?;
            }
            Ok(())
        });
        if mutate_before {
            assert!(report.failure.is_some());
            assert_eq!(fs::read(f.source.join("file")).unwrap(), b"before");
        } else {
            assert!(report.failure.is_none(), "{report:?}");
            assert_eq!(fs::read(f.source.join("file")).unwrap(), b"approved");
            assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"approved");
        }
    }
}

#[test]
fn source_entry_swapped_after_preflight_is_exclusively_restored() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"baseline").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("file"), b"candidate").unwrap();
    let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
        if point == Point::BeforePreserve {
            fs::write(f.source.join("editor-save"), b"editor")?;
            fs::rename(f.source.join("editor-save"), f.source.join("file"))?;
        }
        Ok(())
    });
    assert_eq!(report.failure.as_ref().unwrap().kind, FailureKind::Conflict);
    assert!(report.entries[0].restored);
    assert!(!report.entries[0].installed);
    assert_eq!(fs::read(f.source.join("file")).unwrap(), b"editor");
    assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"candidate");
}

#[test]
fn exclusive_install_and_restore_never_overwrite_a_racing_editor() {
    for restoring in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"baseline").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"candidate").unwrap();
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if restoring && point == Point::BeforePreserve {
                fs::write(f.source.join("file"), b"changed-before-capture")?;
            }
            if point
                == if restoring {
                    Point::BeforeRestore
                } else {
                    Point::BeforeInstall
                }
            {
                fs::write(f.source.join("file"), b"editor-current")?;
            }
            Ok(())
        });
        assert!(report.failure.is_some());
        assert!(report.entries[0].old_retained);
        assert!(!report.entries[0].installed);
        assert_eq!(fs::read(f.source.join("file")).unwrap(), b"editor-current");
        assert_eq!(
            fs::read(old_path(&report, 0)).unwrap(),
            if restoring {
                &b"changed-before-capture"[..]
            } else {
                &b"baseline"[..]
            }
        );
        assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"candidate");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_rename_noreplace_succeeds_and_refuses_collision_or_missing_source() {
    let f = Fixture::new();
    fs::write(f.source.join("candidate"), b"candidate").unwrap();
    let source = File::open(&f.source).unwrap();
    let recovery = File::open(&f.recovery).unwrap();

    exclusive(
        &source,
        OsStr::new("candidate"),
        &recovery,
        OsStr::new("installed"),
    )
    .unwrap();
    assert_eq!(
        fs::read(f.recovery.join("installed")).unwrap(),
        b"candidate"
    );

    fs::write(f.source.join("candidate"), b"second").unwrap();
    let collision = exclusive(
        &source,
        OsStr::new("candidate"),
        &recovery,
        OsStr::new("installed"),
    )
    .unwrap_err();
    assert_eq!(collision.kind, FailureKind::Conflict);
    assert_eq!(collision.os_error, Some(libc::EEXIST));
    assert_eq!(fs::read(f.source.join("candidate")).unwrap(), b"second");
    assert_eq!(
        fs::read(f.recovery.join("installed")).unwrap(),
        b"candidate"
    );

    let missing = exclusive(
        &source,
        OsStr::new("missing"),
        &recovery,
        OsStr::new("other"),
    )
    .unwrap_err();
    assert_eq!(missing.kind, FailureKind::Conflict);
    assert_eq!(missing.os_error, Some(libc::ENOENT));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_unsupported_noreplace_primitive_stays_an_io_failure() {
    for code in [libc::ENOSYS, libc::EINVAL, libc::EOPNOTSUPP] {
        let failure = exclusive_error(std::io::Error::from_raw_os_error(code));
        assert_eq!(failure.kind, FailureKind::Io);
        assert_eq!(failure.os_error, Some(code));
    }
}

#[test]
fn opened_editor_fd_keeps_writing_retained_inode_even_after_success() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"baseline").unwrap();
    let mut editor = fs::OpenOptions::new()
        .write(true)
        .open(f.source.join("file"))
        .unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("file"), b"candidate").unwrap();
    let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
        if point == Point::AfterVerified {
            editor.set_len(0)?;
            editor.write_all(b"late-editor")?;
            editor.sync_all()?;
        }
        Ok(())
    });
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(fs::read(f.source.join("file")).unwrap(), b"candidate");
    assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"late-editor");
    editor.write_all(b"-after-return").unwrap();
    editor.sync_all().unwrap();
    assert_eq!(
        fs::read(old_path(&report, 0)).unwrap(),
        b"late-editor-after-return"
    );
}

#[test]
fn parent_directory_rename_stops_and_preserves_without_targeting_replacement_directory() {
    let f = Fixture::new();
    fs::create_dir(f.source.join("dir")).unwrap();
    fs::write(f.source.join("dir/file"), b"baseline").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("dir/file"), b"candidate").unwrap();
    let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
        if point == Point::AfterVerified {
            fs::rename(f.source.join("dir"), f.source.join("moved"))?;
            fs::create_dir(f.source.join("dir"))?;
            fs::write(f.source.join("dir/file"), b"replacement-dir")?;
        }
        Ok(())
    });
    assert_eq!(
        report.failure.as_ref().unwrap().kind,
        FailureKind::AncestorChanged
    );
    assert!(report.entries[0].old_retained);
    assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"baseline");
    assert_eq!(
        fs::read(f.source.join("dir/file")).unwrap(),
        b"replacement-dir"
    );
    assert!(!f.source.join("moved/file").exists());
}

#[test]
fn post_install_ancestor_change_reports_effect_and_keeps_recovery_handle() {
    for move_recovery in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"baseline").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"candidate").unwrap();
        let moved = f.source.parent().unwrap().join("moved");
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if point == Point::AfterInstall {
                fs::rename(
                    if move_recovery {
                        &f.recovery
                    } else {
                        &f.source
                    },
                    &moved,
                )?;
            }
            Ok(())
        });
        assert_eq!(
            report.failure.as_ref().unwrap().kind,
            FailureKind::AncestorChanged
        );
        assert!(report.entries[0].installed);
        let pinned = report.recovery_directory().unwrap();
        assert_eq!(
            Some(identity(&pinned.metadata().unwrap())),
            report.recovery_identity
        );
        let mut old = open_at(
            pinned,
            OsStr::new(&report.entries[0].old_name),
            false,
            false,
        )
        .unwrap();
        let mut body = String::new();
        old.read_to_string(&mut body).unwrap();
        assert_eq!(body, "baseline");
        assert_eq!(
            fs::read(if move_recovery {
                f.source.join("file")
            } else {
                moved.join("file")
            })
            .unwrap(),
            b"candidate"
        );
    }
}

#[test]
fn late_secret_inode_or_symlink_swap_is_not_hashed_and_is_restored() {
    for link in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"baseline").unwrap();
        let protected = f.recovery.join("registered");
        // Larger than the read cap: success of the Secret branch, rather than
        // a size/hash conflict, shows registration was checked before content.
        fs::write(&protected, vec![b'x'; 8192]).unwrap();
        let registry = RegisteredSecrets::new([protected.clone()]).unwrap();
        let snapshot = copy::snapshot(&f.source, &registry, limits()).unwrap();
        fs::write(snapshot.path().join("file"), b"candidate").unwrap();
        let request = copy::collect_changes(&snapshot, &registry, limits()).unwrap();
        let report = apply_with_hook(
            &snapshot,
            &request,
            &registry,
            limits(),
            &f.recovery,
            &mut |point, _, _| {
                if point == Point::BeforePreserve {
                    fs::remove_file(f.source.join("file"))?;
                    if link {
                        symlink(&protected, f.source.join("file"))?;
                    } else {
                        fs::rename(&protected, f.source.join("file"))?;
                    }
                }
                Ok(())
            },
        );
        assert!(report.failure.is_some());
        if !link {
            assert_eq!(report.failure.as_ref().unwrap().kind, FailureKind::Secret);
        }
        assert!(report.entries[0].restored);
        assert!(!report.entries[0].installed);
    }
}

#[test]
fn delete_and_create_races_preserve_every_existing_version() {
    for deleting in [false, true] {
        let f = Fixture::new();
        if deleting {
            fs::write(f.source.join("file"), b"old").unwrap();
        }
        let snapshot = f.snapshot();
        if deleting {
            fs::remove_file(snapshot.path().join("file")).unwrap();
        } else {
            fs::write(snapshot.path().join("file"), b"candidate").unwrap();
        }
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if point
                == if deleting {
                    Point::BeforeDelete
                } else {
                    Point::BeforeInstall
                }
            {
                fs::write(f.source.join("file"), b"racing")?;
            }
            Ok(())
        });
        assert!(report.failure.is_some());
        assert_eq!(fs::read(f.source.join("file")).unwrap(), b"racing");
        assert!(!report.entries[0].deleted && !report.entries[0].installed);
        if deleting {
            assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"old");
        } else {
            assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"candidate");
        }
    }
}

#[test]
fn failures_after_rename_report_partial_apply_and_leave_durable_intent_without_blind_rollback() {
    for point in [Point::AfterPreserve, Point::AfterInstall] {
        let f = Fixture::new();
        for name in ["a", "b"] {
            fs::write(f.source.join(name), b"old").unwrap();
        }
        let snapshot = f.snapshot();
        for name in ["a", "b"] {
            fs::write(snapshot.path().join(name), b"new").unwrap();
        }
        let report = run_fixture(&f, &snapshot, &mut |at, index, _| {
            if at == point && index == 1 {
                Err(failure())
            } else {
                Ok(())
            }
        });
        assert_eq!(report.failure.as_ref().unwrap().entry, Some(1));
        assert!(report.entries[0].installed);
        assert_eq!(fs::read(f.source.join("a")).unwrap(), b"new");
        assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"old");
        assert_eq!(fs::read(old_path(&report, 1)).unwrap(), b"old");
        assert_eq!(report.entries[1].installed, point == Point::AfterInstall);
        let log = fs::read_to_string(report.recovery_path.as_ref().unwrap().join("journal.jsonl"))
            .unwrap();
        let last: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(
            last["phase"],
            if point == Point::AfterInstall {
                "InstallIntent"
            } else {
                "PreserveIntent"
            }
        );
        assert!(!report.entries[1].restored);
    }
}

#[test]
fn journal_failure_prevents_source_mutation_and_exdev_has_no_copy_unlink_fallback() {
    for cross_device in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"old").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"new").unwrap();
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if point
                == if cross_device {
                    Point::BeforeInstall
                } else {
                    Point::BeforeJournal
                }
            {
                return Err(if cross_device {
                    std::io::Error::from_raw_os_error(libc::EXDEV)
                } else {
                    failure()
                });
            }
            Ok(())
        });
        assert_eq!(
            report.failure.as_ref().unwrap().kind,
            if cross_device {
                FailureKind::CrossDevice
            } else {
                FailureKind::Io
            }
        );
        assert!(!report.entries[0].installed);
        if cross_device {
            assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"old");
            assert!(!f.source.join("file").exists());
        } else {
            assert_eq!(fs::read(f.source.join("file")).unwrap(), b"old");
        }
    }
}

#[test]
fn failure_after_prepared_journal_sync_keeps_source_unchanged_and_recovery_evidence() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"old").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("file"), b"new").unwrap();

    let mut after_journal = 0;
    let report = run_fixture(&f, &snapshot, &mut |point, index, recovery| {
        if point == Point::AfterJournal {
            after_journal += 1;
            assert_eq!(index, 0);
            let log = fs::read_to_string(recovery.join("journal.jsonl"))?;
            let last: serde_json::Value =
                serde_json::from_str(log.lines().last().unwrap()).map_err(std::io::Error::other)?;
            assert_eq!(last["phase"], "Prepared");
            return Err(failure());
        }
        Ok(())
    });

    assert_eq!(after_journal, 1);
    assert_eq!(report.failure.as_ref().unwrap().kind, FailureKind::Io);
    assert_eq!(report.failure.as_ref().unwrap().entry, Some(0));
    assert_eq!(report.entries[0].phase, Phase::Prepared);
    assert!(!report.entries[0].installed && !report.entries[0].old_retained);
    assert_eq!(fs::read(f.source.join("file")).unwrap(), b"old");
    assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"new");
    assert!(!old_path(&report, 0).exists());
}

fn pinned_parent(f: &Fixture) -> RecoveryParent {
    fs::set_permissions(&f.recovery, Permissions::from_mode(0o700)).unwrap();
    let directory = File::open(&f.recovery).unwrap();
    RecoveryParent::pin(
        &directory,
        identity(&directory.metadata().unwrap()),
        &f.recovery,
    )
    .unwrap()
}

#[test]
fn pinned_recovery_rejects_wrong_identity_before_source_changes() {
    let f = Fixture::new();
    fs::write(f.source.join("a"), b"before").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("a"), b"after").unwrap();
    let request = changes(&snapshot);
    let expected = pinned_parent(&f);
    let wrong = tempfile::tempdir().unwrap();
    let wrong_fd = File::open(wrong.path()).unwrap();
    assert!(RecoveryParent::pin(&wrong_fd, expected.expected, &f.recovery).is_err());
    // Also exercise the apply boundary independently of the public constructor.
    let wrong = RecoveryParent {
        directory: wrong_fd,
        expected: expected.expected,
        provenance: f.recovery.clone(),
    };
    let report = apply_pinned_with_hook(
        &snapshot,
        &request,
        &secrets(),
        limits(),
        &wrong,
        &mut |_, _, _| Ok(()),
    );
    assert_eq!(report.failure.unwrap().kind, FailureKind::AncestorChanged);
    assert!(report.recovery_path.is_none());
    assert_eq!(fs::read(f.source.join("a")).unwrap(), b"before");
    assert!(fs::read_dir(&f.recovery).unwrap().next().is_none());
}

#[test]
fn pinned_recovery_checks_private_directory_and_actual_ancestry() {
    let f = Fixture::new();
    fs::write(f.source.join("a"), b"before").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("a"), b"after").unwrap();
    let request = changes(&snapshot);
    let file = File::open(f.source.join("a")).unwrap();
    assert!(RecoveryParent::pin(&file, identity(&file.metadata().unwrap()), &f.recovery).is_err());
    let pinned = pinned_parent(&f);
    assert_eq!(
        pinned.identity(),
        identity(&fs::metadata(&f.recovery).unwrap())
    );
    pinned.validate_identity().unwrap();
    fs::set_permissions(&f.recovery, Permissions::from_mode(0o755)).unwrap();
    assert!(pinned.validate_identity().is_err());
    assert!(
        apply_pinned_with_hook(
            &snapshot,
            &request,
            &secrets(),
            limits(),
            &pinned,
            &mut |_, _, _| Ok(()),
        )
        .failure
        .is_some()
    );
    for inside in [f.source.join("private"), snapshot.path().join("private")] {
        fs::create_dir(&inside).unwrap();
        fs::set_permissions(&inside, Permissions::from_mode(0o700)).unwrap();
        let file = File::open(&inside).unwrap();
        // An innocent-looking provenance path must not authorize an inside FD.
        let pinned =
            RecoveryParent::pin(&file, identity(&file.metadata().unwrap()), &f.recovery).unwrap();
        assert!(pinned.validate_for_snapshot(&snapshot).is_err());
        let report = apply_pinned_with_hook(
            &snapshot,
            &request,
            &secrets(),
            limits(),
            &pinned,
            &mut |_, _, _| Ok(()),
        );
        assert_eq!(report.failure.unwrap().kind, FailureKind::InvalidChangeSet);
        assert!(fs::read_dir(&inside).unwrap().next().is_none());
    }
    assert_eq!(fs::read(f.source.join("a")).unwrap(), b"before");
}

#[test]
fn pinned_recovery_parent_replacement_never_selects_new_path() {
    for at in [None, Some(Point::AfterPreserve), Some(Point::AfterInstall)] {
        let f = Fixture::new();
        fs::write(f.source.join("a"), b"before").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("a"), b"after").unwrap();
        let request = changes(&snapshot);
        let pinned = pinned_parent(&f);
        let moved = f.recovery.with_file_name("moved-recovery");
        let replace = || {
            fs::rename(&f.recovery, &moved).unwrap();
            fs::create_dir(&f.recovery).unwrap();
            fs::write(f.recovery.join("sentinel"), b"untouched").unwrap();
        };
        if at.is_none() {
            replace();
        }
        pinned.validate_for_snapshot(&snapshot).unwrap();
        let mut replaced = at.is_none();
        let report = apply_pinned_with_hook(
            &snapshot,
            &request,
            &secrets(),
            limits(),
            &pinned,
            &mut |point, _, _| {
                if !replaced && at == Some(point) {
                    replace();
                    replaced = true;
                }
                Ok(())
            },
        );
        assert!(replaced);
        assert!(report.failure.is_none(), "{:?}", report.failure);
        assert!(report.entries[0].installed && report.entries[0].old_retained);
        drop(pinned);
        assert_eq!(fs::read(f.source.join("a")).unwrap(), b"after");
        assert_eq!(fs::read_dir(&f.recovery).unwrap().count(), 1);
        assert_eq!(fs::read(f.recovery.join("sentinel")).unwrap(), b"untouched");
        let recovery = report.recovery_directory().unwrap();
        assert_eq!(
            Some(identity(&recovery.metadata().unwrap())),
            report.recovery_identity
        );
        let mut old = open_at(recovery, OsStr::new("old-0"), false, false).unwrap();
        let mut body = String::new();
        old.read_to_string(&mut body).unwrap();
        assert_eq!(body, "before");
        assert!(open_at(recovery, OsStr::new("journal.jsonl"), false, false).is_ok());
        let actual = moved.join(report.recovery_path.as_ref().unwrap().file_name().unwrap());
        assert_eq!(
            identity(&fs::metadata(actual).unwrap()),
            report.recovery_identity.unwrap()
        );
    }
}

#[test]
fn pinned_pre_intent_scope_rejects_replaced_source() {
    let f = Fixture::new();
    fs::write(f.source.join("a"), b"before").unwrap();
    let snapshot = f.snapshot();
    let pinned = pinned_parent(&f);
    pinned.validate_for_snapshot(&snapshot).unwrap();
    let moved = f.source.with_file_name("old-source");
    fs::rename(&f.source, &moved).unwrap();
    fs::create_dir(&f.source).unwrap();
    fs::write(f.source.join("sentinel"), b"untouched").unwrap();
    assert!(pinned.validate_for_snapshot(&snapshot).is_err());
    assert_eq!(fs::read(f.source.join("sentinel")).unwrap(), b"untouched");
    assert!(fs::read_dir(&f.recovery).unwrap().next().is_none());
}

// The fixed specimen predates intent-bound journal v2. Keep this v1 layout
// readable as JSON evidence and compare it with the platform backend's output.
// These types are a test-only schema oracle. M2 owns the product reader.
#[allow(dead_code)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
struct PresentNullable<T>(Option<T>);

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyBeforeV1 {
    device: u64,
    inode: u64,
    hash: [u8; 32],
    mode: u32,
    bytes: u64,
}

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyAfterV1 {
    hash: [u8; 32],
    mode: u32,
    bytes: u64,
}

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyChangeV1 {
    path: PathBuf,
    before: PresentNullable<LegacyBeforeV1>,
    after: PresentNullable<LegacyAfterV1>,
}

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyHeaderV1 {
    version: u8,
    source: PathBuf,
    source_chain: Vec<(PathBuf, (u64, u64))>,
    recovery_chain: Vec<(PathBuf, (u64, u64))>,
    entries: Vec<LegacyChangeV1>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
enum LegacyPhaseV1 {
    Preparing,
    Prepared,
    PreserveIntent,
    Preserved,
    RestoreIntent,
    Restored,
    InstallIntent,
    Installed,
    DeleteIntent,
    Deleted,
}

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyEntryV1 {
    relative_path: PathBuf,
    source_parent_chain: Vec<(PathBuf, (u64, u64))>,
    phase: LegacyPhaseV1,
    old_retained: bool,
    installed: bool,
    deleted: bool,
    restored: bool,
    old_name: String,
    candidate_name: PresentNullable<String>,
    install_name: PresentNullable<String>,
    candidate_identity: PresentNullable<(u64, u64)>,
    install_identity: PresentNullable<(u64, u64)>,
}

fn require_legacy_v1_fields(
    value: &serde_json::Value,
    required: &[&str],
) -> std::result::Result<(), String> {
    let fields = value
        .as_object()
        .ok_or("legacy journal row is not an object")?;
    for field in required {
        if !fields.contains_key(*field) {
            return Err(format!("missing legacy journal field {field}"));
        }
    }
    Ok(())
}

fn decode_legacy_v1_fixture(
    text: &str,
) -> std::result::Result<(LegacyHeaderV1, Vec<LegacyEntryV1>), String> {
    if !text.ends_with('\n') {
        return Err("unterminated legacy journal line".into());
    }
    let mut lines = text.lines();
    let first = lines.next().ok_or("missing journal header")?;
    let header_value: serde_json::Value = serde_json::from_str(first).map_err(|e| e.to_string())?;
    require_legacy_v1_fields(
        &header_value,
        &[
            "version",
            "source",
            "source_chain",
            "recovery_chain",
            "entries",
        ],
    )?;
    let changes = header_value["entries"]
        .as_array()
        .ok_or("legacy journal entries is not an array")?;
    for change in changes {
        require_legacy_v1_fields(change, &["path", "before", "after"])?;
    }
    let header: LegacyHeaderV1 = serde_json::from_str(first).map_err(|e| e.to_string())?;
    if header.version != 1 {
        return Err(format!("unknown journal version {}", header.version));
    }
    let entries: Vec<LegacyEntryV1> = lines
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
            require_legacy_v1_fields(
                &value,
                &[
                    "relative_path",
                    "source_parent_chain",
                    "phase",
                    "old_retained",
                    "installed",
                    "deleted",
                    "restored",
                    "old_name",
                    "candidate_name",
                    "install_name",
                    "candidate_identity",
                    "install_identity",
                ],
            )?;
            serde_json::from_str(line).map_err(|e| e.to_string())
        })
        .collect::<std::result::Result<_, _>>()?;
    // This oracle covers the fixed one-entry replacement specimen, including
    // its complete writer sequence. It is not the future product v1 reader.
    if header.entries.len() != 1
        || header.entries[0].before.0.is_none()
        || header.entries[0].after.0.is_none()
    {
        return Err("not a one-entry replacement fixture".into());
    }
    let phases = [
        LegacyPhaseV1::Prepared,
        LegacyPhaseV1::PreserveIntent,
        LegacyPhaseV1::Preserved,
        LegacyPhaseV1::InstallIntent,
        LegacyPhaseV1::Installed,
    ];
    if entries.len() != phases.len() {
        return Err("incomplete legacy fixture sequence".into());
    }
    for (entry, expected) in entries.iter().zip(phases) {
        if entry.relative_path != header.entries[0].path || entry.phase != expected {
            return Err("legacy fixture path or phase mismatch".into());
        }
    }
    Ok((header, entries))
}

fn assert_live_v1_chain(value: &serde_json::Value, path: &Path) {
    use serde_json::json;

    assert!(path.is_absolute());
    let mut prefix = PathBuf::from("/");
    let mut expected = vec![json!([&prefix, identity(&fs::metadata(&prefix).unwrap())])];
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                prefix.push(name);
                expected.push(json!([&prefix, identity(&fs::metadata(&prefix).unwrap())]));
            }
            _ => panic!("unexpected non-normal path component"),
        }
    }
    // The full prefix walk checks cardinality, parent linkage and every
    // intermediate device/inode before the dynamic chain is normalized.
    assert_eq!(value, &serde_json::Value::Array(expected));
}

fn normalize_v1_entry(value: &mut serde_json::Value, source: &Path) {
    use serde_json::{Value, json};

    let fields = value.as_object_mut().unwrap();
    assert_live_v1_chain(fields.get("source_parent_chain").unwrap(), source);
    *fields.get_mut("source_parent_chain").unwrap() = json!([
        ["/", [11, 10]],
        ["/legacy", [11, 11]],
        ["/legacy/source", [11, 13]]
    ]);
    for (field, pinned) in [
        ("candidate_identity", json!([11, 14])),
        ("install_identity", json!([11, 15])),
    ] {
        let identity = fields.get(field).and_then(Value::as_array).unwrap();
        assert_eq!(identity.len(), 2);
        assert!(identity.iter().all(Value::is_u64));
        *fields.get_mut(field).unwrap() = pinned;
    }
}

#[test]
fn v1_report_and_journal_match_fixed_pre_m2_fixture() {
    use serde_json::{Value, json};

    let fixed_report = ApplyReport {
        source_path: "/legacy/source".into(),
        recovery_path: Some(
            "/legacy/recovery/polaris-apply-0123456789abcdef0123456789abcdef".into(),
        ),
        recovery_identity: Some((11, 12)),
        recovery_directory: None,
        entries: vec![EntryReport {
            relative_path: "file.txt".into(),
            source_parent_chain: vec![
                ("/".into(), (11, 10)),
                ("/legacy".into(), (11, 11)),
                ("/legacy/source".into(), (11, 13)),
            ],
            phase: Phase::Installed,
            old_retained: true,
            installed: true,
            deleted: false,
            restored: false,
            old_name: "old-0".into(),
            candidate_name: Some("candidate-0".into()),
            install_name: Some("install-0".into()),
            candidate_identity: Some((11, 14)),
            install_identity: Some((11, 15)),
        }],
        failure: None,
    };
    assert_eq!(
        serde_json::to_string(&fixed_report).unwrap(),
        include_str!("../../tests/fixtures/workspace-apply-report-v1.json").trim_end()
    );
    let expected_report: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/workspace-apply-report-v1.json"
    ))
    .unwrap();
    let fixture_journal = include_str!("../../tests/fixtures/workspace-apply-journal-v1.jsonl");
    let (legacy_header, legacy_rows) = decode_legacy_v1_fixture(fixture_journal).unwrap();
    assert_eq!(legacy_header.version, 1);
    assert_eq!(legacy_header.source.as_path(), Path::new("/legacy/source"));
    assert_eq!(legacy_header.source_chain.len(), 3);
    assert_eq!(legacy_header.recovery_chain.len(), 4);
    assert_eq!(legacy_header.entries.len(), 1);
    assert_eq!(
        legacy_header.entries[0].path.as_path(),
        Path::new("file.txt")
    );
    assert_eq!(legacy_header.entries[0].before.0.as_ref().unwrap().bytes, 6);
    assert_eq!(legacy_header.entries[0].after.0.as_ref().unwrap().bytes, 5);
    assert_eq!(legacy_rows.len(), 5);
    assert_eq!(
        legacy_rows.iter().map(|row| row.phase).collect::<Vec<_>>(),
        [
            LegacyPhaseV1::Prepared,
            LegacyPhaseV1::PreserveIntent,
            LegacyPhaseV1::Preserved,
            LegacyPhaseV1::InstallIntent,
            LegacyPhaseV1::Installed,
        ]
    );
    let expected_journal: Vec<Value> = fixture_journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(expected_journal.len(), 6);
    assert_eq!(expected_journal[0]["version"], 1);
    assert_eq!(
        serde_json::to_value(&legacy_header).unwrap(),
        expected_journal[0]
    );
    for (typed, raw) in legacy_rows.iter().zip(expected_journal.iter().skip(1)) {
        assert_eq!(serde_json::to_value(typed).unwrap(), *raw);
    }

    let f = Fixture::new();
    let source_file = f.source.join("file.txt");
    fs::write(&source_file, b"before").unwrap();
    fs::set_permissions(&source_file, Permissions::from_mode(0o600)).unwrap();
    let original = fs::metadata(&source_file).unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("file.txt"), b"after").unwrap();
    fs::set_permissions(
        snapshot.path().join("file.txt"),
        Permissions::from_mode(0o600),
    )
    .unwrap();
    let report = run_fixture(&f, &snapshot, &mut |_, _, _| Ok(()));
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(report.entries.len(), 1);
    assert_eq!(fs::read(&source_file).unwrap(), b"after");
    assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"before");
    assert_eq!(
        report.entries[0].candidate_identity,
        Some(identity(&fs::metadata(candidate_path(&report, 0)).unwrap()))
    );
    assert_eq!(
        report.entries[0].install_identity,
        Some(identity(&fs::metadata(&source_file).unwrap()))
    );
    let recovery_path = report.recovery_path.as_ref().unwrap();
    let recovery_identity = identity(&fs::metadata(recovery_path).unwrap());
    assert_eq!(report.recovery_identity, Some(recovery_identity));

    let mut actual_report = serde_json::to_value(&report).unwrap();
    let report_fields = actual_report.as_object_mut().unwrap();
    assert_eq!(report_fields.get("source_path"), Some(&json!(&f.source)));
    assert_eq!(
        report_fields.get("recovery_path"),
        Some(&json!(report.recovery_path.as_ref().unwrap()))
    );
    assert_eq!(
        report_fields.get("recovery_identity"),
        Some(&json!(report.recovery_identity.unwrap()))
    );
    *report_fields.get_mut("source_path").unwrap() = json!("/legacy/source");
    *report_fields.get_mut("recovery_path").unwrap() =
        json!("/legacy/recovery/polaris-apply-0123456789abcdef0123456789abcdef");
    *report_fields.get_mut("recovery_identity").unwrap() = json!([11, 12]);
    let entries = report_fields
        .get_mut("entries")
        .unwrap()
        .as_array_mut()
        .unwrap();
    normalize_v1_entry(&mut entries[0], &f.source);
    assert_eq!(actual_report, expected_report);

    let journal_path = report.recovery_path.as_ref().unwrap().join("journal.jsonl");
    let actual_journal_text = fs::read_to_string(journal_path).unwrap();
    let (live_header, live_rows) = decode_legacy_v1_fixture(&actual_journal_text).unwrap();
    assert_eq!(live_header.entries.len(), 1);
    assert_eq!(live_rows.len(), 5);
    let journal_recovery = live_header.recovery_chain.last().unwrap();
    assert_eq!(journal_recovery.0.as_path(), recovery_path.as_path());
    assert_eq!(journal_recovery.1, recovery_identity);
    let mut actual_journal: Vec<Value> = actual_journal_text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(actual_journal.len(), expected_journal.len());
    let header = actual_journal[0].as_object_mut().unwrap();
    assert_eq!(header.get("source"), Some(&json!(&f.source)));
    for (field, path, replacement) in [
        (
            "source_chain",
            f.source.as_path(),
            json!([
                ["/", [11, 10]],
                ["/legacy", [11, 11]],
                ["/legacy/source", [11, 13]]
            ]),
        ),
        (
            "recovery_chain",
            report.recovery_path.as_deref().unwrap(),
            json!([
                ["/", [11, 10]],
                ["/legacy", [11, 11]],
                ["/legacy/recovery", [11, 16]],
                [
                    "/legacy/recovery/polaris-apply-0123456789abcdef0123456789abcdef",
                    [11, 12]
                ]
            ]),
        ),
    ] {
        assert_live_v1_chain(header.get(field).unwrap(), path);
        *header.get_mut(field).unwrap() = replacement;
    }
    *header.get_mut("source").unwrap() = json!("/legacy/source");
    let changes = header.get_mut("entries").unwrap().as_array_mut().unwrap();
    assert_eq!(changes.len(), 1);
    let before = changes[0]
        .get_mut("before")
        .unwrap()
        .as_object_mut()
        .unwrap();
    assert_eq!(before.get("device"), Some(&json!(original.dev())));
    assert_eq!(before.get("inode"), Some(&json!(original.ino())));
    *before.get_mut("device").unwrap() = json!(11);
    *before.get_mut("inode").unwrap() = json!(17);
    for entry in actual_journal.iter_mut().skip(1) {
        normalize_v1_entry(entry, &f.source);
    }
    assert_eq!(actual_journal, expected_journal);
}

#[test]
fn v1_schema_oracle_rejects_unknown_and_malformed_saved_lines() {
    use serde_json::{Value, json};

    let fixture = include_str!("../../tests/fixtures/workspace-apply-journal-v1.jsonl");
    assert!(decode_legacy_v1_fixture(fixture).is_ok());
    let baseline: Vec<Value> = fixture
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let encode = |lines: &[Value]| {
        let mut text = lines
            .iter()
            .map(|line| serde_json::to_string(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        text
    };
    let mut invalid = Vec::new();

    let mut lines = baseline.clone();
    lines[0]["version"] = json!(2);
    invalid.push(("unknown version", lines));

    let mut lines = baseline.clone();
    lines[0]
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), json!(true));
    invalid.push(("unknown header field", lines));

    let mut lines = baseline.clone();
    lines[0].as_object_mut().unwrap().remove("source_chain");
    invalid.push(("missing header field", lines));

    let mut lines = baseline.clone();
    lines[0]["entries"][0]
        .as_object_mut()
        .unwrap()
        .remove("before");
    invalid.push(("missing nullable change field", lines));

    let mut lines = baseline.clone();
    lines[0]["entries"][0]["before"]["hash"] = json!([1, 2]);
    invalid.push(("short hash", lines));

    let mut lines = baseline.clone();
    lines[0]["entries"][0]["before"]
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), json!(true));
    invalid.push(("unknown nested field", lines));

    let mut lines = baseline.clone();
    lines[1]
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), json!(true));
    invalid.push(("unknown journal entry field", lines));

    let mut lines = baseline.clone();
    lines[1].as_object_mut().unwrap().remove("candidate_name");
    invalid.push(("missing nullable journal entry field", lines));

    let mut lines = baseline.clone();
    lines[1]["phase"] = json!("FuturePhase");
    invalid.push(("unknown phase", lines));

    let mut lines = baseline.clone();
    lines[1].as_object_mut().unwrap().remove("phase");
    invalid.push(("missing phase", lines));

    let mut lines = baseline.clone();
    lines[1]["relative_path"] = json!("other.txt");
    invalid.push(("entry path differs from header", lines));

    let mut lines = baseline.clone();
    lines[0]["entries"][0]["path"] = json!("other.txt");
    invalid.push(("header path differs from journal", lines));

    let mut lines = baseline.clone();
    lines.swap(2, 3);
    invalid.push(("out-of-order phase", lines));

    let mut lines = baseline.clone();
    lines[2]["phase"] = json!("Prepared");
    invalid.push(("duplicate phase", lines));

    let mut lines = baseline.clone();
    lines.remove(3);
    invalid.push(("missing row", lines));

    let mut lines = baseline.clone();
    let duplicate = lines.last().unwrap().clone();
    lines.push(duplicate);
    invalid.push(("extra row", lines));

    for (case, lines) in invalid {
        assert!(decode_legacy_v1_fixture(&encode(&lines)).is_err(), "{case}");
    }
    assert!(decode_legacy_v1_fixture(&encode(&baseline[..1])).is_err());
    assert!(decode_legacy_v1_fixture(fixture.trim_end_matches('\n')).is_err());
    assert!(decode_legacy_v1_fixture(&format!("{}\n{{", fixture.lines().next().unwrap())).is_err());
}
