//! Migration document reads, staged payloads and source-content preservation tests.
use super::*;
use crate::{
    registration::{Capabilities, Destination, ProtocolProfile, Server, Timeouts},
    store::{Secrets, documents_v2::DocumentPresenceV2},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root_path: PathBuf,
    root: File,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let canonical_temp = temp.path().canonicalize().unwrap();
        let root_path = canonical_temp.join("source");
        fs::create_dir(&root_path).unwrap();
        fs::set_permissions(&root_path, fs::Permissions::from_mode(0o700)).unwrap();
        let root = crate::entry_gate::physical_directory(&root_path).unwrap();
        let home = canonical_temp.join("home");
        Self {
            _temp: temp,
            root_path,
            root,
            home,
        }
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        use std::io::Write;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(self.root_path.join(name))
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn populated() -> Self {
        let fixture = Self::new();
        fixture.write(
            "servers.json",
            &serde_json::to_vec_pretty(&registry()).unwrap(),
        );
        fixture.write(
            "secrets.json",
            br#"{"schema_version":1,"revision":7,"values":{"token":"synthetic-value"}}"#,
        );
        fixture
    }
    fn contents(&self) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(&self.root_path)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_str().unwrap().to_owned(),
                    fs::read(path).unwrap(),
                )
            })
            .collect()
    }
}
fn registry() -> Registry {
    Registry {
        schema_version: 1,
        revision: 5,
        servers: vec![Server {
            server_id: "fixture".into(),
            enabled_projects: BTreeSet::new(),
            transport: Destination::StreamableHttp {
                url: "https://example.test/mcp".into(),
                secret_headers: BTreeMap::from([("authorization".into(), "token".into())]),
            },
            protocol_profile: ProtocolProfile::Discover2026,
            capabilities: Capabilities::default(),
            timeouts: Timeouts::default(),
        }],
    }
}
const MIGRATION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn borrowed_registry_rechecks_present_and_absent_source_without_copying_payload() {
    let fixture = Fixture::populated();
    let documents = SourceDocumentsV1::inspect(&fixture.root).unwrap();
    let borrowed = documents.verified_registry().unwrap().unwrap();
    assert_eq!(borrowed, &registry());
    assert!(std::ptr::eq(borrowed, documents.registry.as_ref().unwrap()));
    let absent = Fixture::new();
    let documents = SourceDocumentsV1::inspect(&absent.root).unwrap();
    assert!(documents.verified_registry().unwrap().is_none());
}

#[test]
fn borrowed_registry_refuses_changed_source_and_newly_present_document() {
    let fixture = Fixture::populated();
    let documents = SourceDocumentsV1::inspect(&fixture.root).unwrap();
    let mut changed = registry();
    changed.revision += 1;
    let changed_bytes = serde_json::to_vec(&changed).unwrap();
    fs::write(fixture.root_path.join("servers.json"), &changed_bytes).unwrap();
    assert!(documents.verified_registry().is_err());
    assert_eq!(
        fs::read(fixture.root_path.join("servers.json")).unwrap(),
        changed_bytes
    );

    let absent = Fixture::new();
    let documents = SourceDocumentsV1::inspect(&absent.root).unwrap();
    absent.write("servers.json", &changed_bytes);
    assert!(documents.verified_registry().is_err());
    assert!(absent.root_path.join("servers.json").is_file());
}

#[test]
fn absent_documents_are_readonly_and_keep_absent_baselines_with_fresh_tokens() {
    let fixture = Fixture::new();
    let source = SourceDocumentsV1::inspect(&fixture.root).unwrap();
    assert!(fixture.contents().is_empty());
    let first = source
        .baselines(MIGRATION, &fixture.home, &BTreeMap::new())
        .unwrap();
    let second = source
        .baselines(MIGRATION, &fixture.home, &BTreeMap::new())
        .unwrap();
    for metadata in [
        first.credential.metadata().unwrap(),
        first.registry.metadata().unwrap(),
    ] {
        assert_eq!(metadata.presence, DocumentPresenceV2::Absent);
        assert_eq!(metadata.revision, 0);
    }
    assert_ne!(
        first.credential.metadata().unwrap().token,
        second.credential.metadata().unwrap().token
    );
    assert!(first.mapped_paths.is_none());
    source
        .verify_credential_baseline(&first.credential)
        .unwrap();
    assert!(fixture.contents().is_empty());
}
#[test]
fn present_source_preserves_values_revisions_and_exact_raw_registry_hash_without_writes() {
    let fixture = Fixture::populated();
    let before = fixture.contents();
    let source = SourceDocumentsV1::inspect(&fixture.root).unwrap();
    let baselines = source
        .baselines(MIGRATION, &fixture.home, &BTreeMap::new())
        .unwrap();
    assert_eq!(baselines.credential.metadata().unwrap().revision, 7);
    assert_eq!(baselines.registry.metadata().unwrap().revision, 5);
    source
        .verify_credential_baseline(&baselines.credential)
        .unwrap();
    let expected_hash = format!("{:x}", Sha256::digest(&before["servers.json"]));
    match baselines.registry.receipt() {
        crate::store::documents_v2::DocumentReceiptV2::MigrationBaseline { receipt } => {
            assert_eq!(
                receipt
                    .registry_projection
                    .as_ref()
                    .unwrap()
                    .source_file_hash
                    .as_deref(),
                Some(expected_hash.as_str())
            );
        }
        _ => panic!("expected baseline"),
    }
    let receipt = serde_json::to_string(baselines.credential.receipt()).unwrap();
    assert!(!receipt.contains("synthetic-value"));
    assert!(!receipt.contains("source_file_hash"));
    assert_eq!(fixture.contents(), before);
    assert_eq!(
        baselines
            .credential
            .into_snapshot()
            .unwrap()
            .resolve("token")
            .unwrap(),
        "synthetic-value"
    );
}
#[test]
fn invalid_source_schema_fields_and_secret_types_are_retained() {
    for (name, bytes) in [
        (
            "servers.json",
            br#"{"schema_version":99,"revision":1,"servers":[]}"#.as_slice(),
        ),
        (
            "servers.json",
            br#"{"schema_version":1,"revision":1,"servers":[],"unknown":true}"#.as_slice(),
        ),
        (
            "secrets.json",
            br#"{"schema_version":99,"revision":1,"values":{}}"#.as_slice(),
        ),
        (
            "secrets.json",
            br#"{"schema_version":1,"revision":1,"values":{"token":12}}"#.as_slice(),
        ),
        (
            "secrets.json",
            br#"{"schema_version":1,"revision":1,"values":{},"unknown":true}"#.as_slice(),
        ),
    ] {
        let fixture = Fixture::new();
        fixture.write(name, bytes);
        let before = fixture.contents();
        assert!(SourceDocumentsV1::inspect(&fixture.root).is_err());
        assert_eq!(fixture.contents(), before);
    }
}
#[test]
fn missing_referenced_credential_refuses_baseline_without_source_changes() {
    let fixture = Fixture::new();
    fixture.write("servers.json", &serde_json::to_vec(&registry()).unwrap());
    let before = fixture.contents();
    assert!(matches!(
        SourceDocumentsV1::inspect(&fixture.root),
        Err(Error::Auth)
    ));
    assert_eq!(fixture.contents(), before);
}
#[test]
fn source_inode_replacement_and_same_inode_content_drift_break_pins() {
    for replace in [false, true] {
        let fixture = Fixture::populated();
        let source = SourceDocumentsV1::inspect(&fixture.root).unwrap();
        let path = fixture.root_path.join("secrets.json");
        if replace {
            let replacement = fixture._temp.path().join("replacement");
            let contents = fs::read(&path).unwrap();
            fs::write(&replacement, contents).unwrap();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
            fs::rename(replacement, &path).unwrap();
        } else {
            fs::write(
                &path,
                br#"{"schema_version":1,"revision":7,"values":{"token":"different-value"}}"#,
            )
            .unwrap();
        }
        let before = fixture.contents();
        assert!(source.verify_current().is_err());
        assert!(
            source
                .baselines(MIGRATION, &fixture.home, &BTreeMap::new())
                .is_err()
        );
        assert_eq!(fixture.contents(), before);
    }
}
#[test]
fn absent_document_created_after_inspection_and_source_mode_drift_are_not_absence() {
    let fixture = Fixture::new();
    let source = SourceDocumentsV1::inspect(&fixture.root).unwrap();
    fixture.write(
        "secrets.json",
        br#"{"schema_version":1,"revision":1,"values":{}}"#,
    );
    assert!(source.verify_current().is_err());
    assert!(
        source
            .baselines(MIGRATION, &fixture.home, &BTreeMap::new())
            .is_err()
    );
    let fixture = Fixture::populated();
    let source = SourceDocumentsV1::inspect(&fixture.root).unwrap();
    fs::set_permissions(
        fixture.root_path.join("secrets.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(source.verify_current().is_err());
    assert_eq!(
        fs::metadata(fixture.root_path.join("secrets.json"))
            .unwrap()
            .mode()
            & 0o777,
        0o644
    );
}
#[test]
fn symlinks_hardlinks_fifo_and_nonprivate_source_are_rejected_without_repair() {
    for kind in ["symlink", "hardlink", "fifo", "directory", "mode"] {
        let fixture = Fixture::new();
        let path = fixture.root_path.join("secrets.json");
        let outside = fixture._temp.path().join("outside");
        fs::write(
            &outside,
            br#"{"schema_version":1,"revision":1,"values":{}}"#,
        )
        .unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        match kind {
            "symlink" => std::os::unix::fs::symlink(&outside, &path).unwrap(),
            "hardlink" => fs::hard_link(&outside, &path).unwrap(),
            "fifo" => {
                use std::{ffi::CString, os::unix::ffi::OsStrExt};
                let path = CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            "directory" => fs::create_dir(&path).unwrap(),
            _ => {
                fs::set_permissions(&fixture.root_path, fs::Permissions::from_mode(0o755)).unwrap()
            }
        }
        let original = fs::symlink_metadata(&fixture.root_path).unwrap().mode();
        assert!(SourceDocumentsV1::inspect(&fixture.root).is_err(), "{kind}");
        assert_eq!(
            fs::symlink_metadata(&fixture.root_path).unwrap().mode(),
            original
        );
        if kind != "mode" {
            assert!(fs::symlink_metadata(path).is_ok());
        }
        assert!(fs::metadata(outside).is_ok());
    }
}
#[test]
fn oversized_source_documents_fail_before_whole_body_read_and_remain_unchanged() {
    for name in ["servers.json", "secrets.json"] {
        let fixture = Fixture::new();
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(fixture.root_path.join(name))
            .unwrap();
        file.set_len(MAX_STORE_BYTES as u64 + 1).unwrap();
        assert!(matches!(
            SourceDocumentsV1::inspect(&fixture.root),
            Err(Error::Limit)
        ));
        assert_eq!(file.metadata().unwrap().len(), MAX_STORE_BYTES as u64 + 1);
    }
}
#[test]
fn direct_comparison_rejects_different_secret_values_with_identical_public_references() {
    let fixture = Fixture::populated();
    let source = SourceDocumentsV1::inspect(&fixture.root).unwrap();
    let wrong = CredentialDocumentV2::baseline(
        MIGRATION,
        Some(
            Secrets::new(
                7,
                BTreeMap::from([("token".into(), "different-value".into())]),
            )
            .unwrap(),
        ),
    )
    .unwrap();
    let before = fixture.contents();
    assert_eq!(source.verify_credential_baseline(&wrong), Err(Error::Auth));
    assert_eq!(fixture.contents(), before);
}

#[test]
fn document_created_after_final_absence_read_cannot_return_an_absent_baseline() {
    let fixture = Fixture::new();
    let source = SourceDocumentsV1::inspect(&fixture.root).unwrap();
    let path = fixture.root_path.join("secrets.json");
    let late = path.clone();
    fn install(remaining: usize, late: PathBuf) {
        AFTER_ABSENT_DOCUMENT.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move |name| {
                let next = remaining - usize::from(name == "secrets.json");
                if next != 0 {
                    install(next, late);
                } else {
                    use std::io::Write;
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(late)
                        .unwrap();
                    file.write_all(br#"{"schema_version":1,"revision":1,"values":{}}"#)
                        .unwrap();
                    file.sync_all().unwrap();
                }
            }));
        });
    }
    // baselines verifies twice before preparation and twice before returning.
    // Inject after the last secrets.json ENOENT, leaving only closing metadata.
    install(4, late);
    assert!(matches!(
        source.baselines(MIGRATION, &fixture.home, &BTreeMap::new()),
        Err(Error::Protection)
    ));
    assert!(path.is_file(), "late absence hook was not reached");
    AFTER_ABSENT_DOCUMENT.with(|slot| assert!(slot.borrow().is_none()));
    assert_eq!(
        fs::read(path).unwrap(),
        br#"{"schema_version":1,"revision":1,"values":{}}"#
    );
}
