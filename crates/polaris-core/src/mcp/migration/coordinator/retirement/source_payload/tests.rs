//! Retired source payload, registry and package-pin inspection tests.
use super::*;
use polaris_mcp::{
    registration::{Capabilities, Destination, ProtocolProfile, Registry, Server, Timeouts},
    store::documents_v2::{CredentialDocumentV2, DocumentPresenceV2},
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
};

type SnapshotEntry = (PathBuf, u64, u32, u64, i64, i64, i64, i64, Option<Vec<u8>>);

struct Fixture {
    _root: tempfile::TempDir,
    _retained: Vec<tempfile::TempDir>,
    home: PathBuf,
    store: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let mut retained = Vec::new();
        for _ in 0..32 {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().canonicalize().unwrap().join("home");
            let store = home.join(".polaris/mcp");
            for path in [
                &home,
                &home.join(".polaris"),
                &store,
                &store.join("servers"),
            ] {
                fs::create_dir(path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            // Retired secret identities remain protected for the process lifetime.
            // Keep a colliding synthetic tree allocated while choosing a new one.
            let metadata = fs::metadata(store.join("servers")).unwrap();
            let collision = polaris_auth::protection::with_protected_paths(|registered| {
                registered
                    .identities()
                    .contains(&polaris_auth::protection::ProtectedIdentity {
                        dev: metadata.dev(),
                        ino: metadata.ino(),
                    })
            })
            .unwrap();
            if collision {
                retained.push(root);
                continue;
            }
            return Self {
                _root: root,
                _retained: retained,
                home,
                store,
            };
        }
        panic!("source payload fixture exhausted 32 private tree candidates")
    }
    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.store.join(path);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn package(&self, id: &str) -> PinnedPathsV1 {
        let server = self.store.join("servers").join(id);
        assert!(!server.exists());
        let pin = PinnedPathsV1 {
            schema_version: 1,
            package: format!("opaque-{}", self.store.display()),
            version: "1.0.0".into(),
            runtime: server.join("runtime"),
            entry: server.join("index.js"),
        };
        let pin_bytes = serde_json::to_vec_pretty(&pin).unwrap();
        for _ in 0..32 {
            let candidate = tempfile::tempdir_in(self._root.path()).unwrap();
            fs::set_permissions(candidate.path(), fs::Permissions::from_mode(0o750)).unwrap();
            for (name, bytes, mode) in [
                ("index.js", b"opaque package payload".as_slice(), 0o600),
                ("runtime", b"synthetic runtime bytes".as_slice(), 0o500),
                ("pinned.json", pin_bytes.as_slice(), 0o600),
            ] {
                let path = candidate.path().join(name);
                fs::write(&path, bytes).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            }
            let collision = polaris_auth::protection::with_protected_paths(|registered| {
                [
                    candidate.path().to_owned(),
                    candidate.path().join("index.js"),
                    candidate.path().join("runtime"),
                    candidate.path().join("pinned.json"),
                ]
                .iter()
                .any(|path| {
                    let metadata = fs::metadata(path).unwrap();
                    registered
                        .identities()
                        .contains(&polaris_auth::protection::ProtectedIdentity {
                            dev: metadata.dev(),
                            ino: metadata.ino(),
                        })
                })
            })
            .unwrap();
            if collision {
                // The outer fixture owns cleanup; do not release these inodes
                // into the allocator before the next candidate is prepared.
                let _ = candidate.keep();
                continue;
            }
            fs::rename(candidate.path(), &server).unwrap();
            return pin;
        }
        panic!("source payload package fixture exhausted 32 candidates")
    }
    fn registry(&self, servers: Vec<Server>) {
        self.write(
            "servers.json",
            &serde_json::to_vec(&Registry {
                schema_version: 1,
                revision: 7,
                servers,
            })
            .unwrap(),
        );
    }
    fn snapshot(&self) -> Vec<SnapshotEntry> {
        let mut pending = vec![self.store.clone()];
        let mut result = vec![];
        while let Some(path) = pending.pop() {
            let m = path.symlink_metadata().unwrap();
            if m.is_dir() {
                pending.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            }
            result.push((
                path.strip_prefix(&self.store).unwrap().to_owned(),
                m.ino(),
                m.mode(),
                m.nlink(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
                m.is_file().then(|| fs::read(path).unwrap()),
            ));
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }
    fn source(&self) -> SourcePreflightV2 {
        SourcePreflightV2::inspect(&self.home).unwrap()
    }
}
fn server(id: &str, transport: Destination) -> Server {
    Server {
        server_id: id.into(),
        transport,
        enabled_projects: BTreeSet::new(),
        protocol_profile: ProtocolProfile::Discover2026,
        capabilities: Capabilities::default(),
        timeouts: Timeouts::default(),
    }
}
fn stdio(pin: &PinnedPathsV1) -> Destination {
    Destination::Stdio {
        command: pin.runtime.clone(),
        cwd: pin.runtime.parent().unwrap().into(),
        args: vec![pin.entry.to_str().unwrap().into(), "opaque".into()],
        sandbox_profile: "read-only".into(),
        secret_env: BTreeMap::new(),
        readable_roots: vec![],
        executable_sha256: Some(format!("{:x}", Sha256::digest(b"synthetic runtime bytes"))),
        script: Some(pin.entry.clone()),
        script_sha256: Some(format!("{:x}", Sha256::digest(b"opaque package payload"))),
    }
}
const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn source_payload_is_read_only_complete_and_never_records_secret_size_or_hash() {
    let f = Fixture::new();
    f.package("unregistered");
    f.registry(vec![server(
        "http",
        Destination::StreamableHttp {
            url: "https://example.test/mcp".into(),
            secret_headers: BTreeMap::from([("authorization".into(), "owned_ref".into())]),
        },
    )]);
    f.write(
        "secrets.json",
        br#"{"schema_version":1,"revision":9,"values":{"owned_ref":"fixture-secret"}}"#,
    );
    f.write("catalog.json", br#"{"schema_version":1,"servers":{}}"#);
    f.write("owner.lock", b"");
    let before = f.snapshot();
    let source = f.source();
    let payload = SourcePayloadV2::inspect(&source, ID, &f.home).unwrap();
    payload.verify_current().unwrap();
    assert_eq!(before, f.snapshot());
    assert_eq!(payload.inventory.len(), before.len() - 1);
    let secret = payload
        .inventory
        .iter()
        .find(|entry| matches!(entry, SourceInventoryEntryV2::SecretDocument { .. }))
        .unwrap();
    let value = serde_json::to_value(secret).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 3);
    assert!(value.get("size").is_none());
    assert!(value.get("sha256").is_none());
    let record = serde_json::to_string(&payload.inventory).unwrap();
    assert!(!record.contains("fixture-secret"));
    assert!(!record.contains(&format!(
        "{:x}",
        Sha256::digest(fs::read(f.store.join("secrets.json")).unwrap())
    )));
    assert_eq!(payload.documents.credential.metadata().unwrap().revision, 9);
    assert_eq!(payload.documents.registry.metadata().unwrap().revision, 7);
    assert!(payload.catalog.empty_catalog.servers.is_empty());
    assert_eq!(payload.catalog.servers.len(), 1);
    assert_eq!(payload.catalog.servers[0].server_id, "http");
    assert!(!f.home.join(".polaris/mcp-v2").exists());
    assert!(
        !f.home
            .join(format!(".polaris/mcp-v2.staging-{ID}"))
            .exists()
    );
}

#[test]
fn source_payload_maps_registered_and_unregistered_pins_without_rewriting_opaque_text() {
    let f = Fixture::new();
    let registered = f.package("registered");
    let unregistered = f.package("unregistered");
    f.registry(vec![server("registered", stdio(&registered))]);
    let source = f.source();
    let payload = SourcePayloadV2::inspect(&source, ID, &f.home).unwrap();
    assert_eq!(payload.unregistered_pins.len(), 1);
    assert_eq!(payload.unregistered_pins[0].server_id, "unregistered");
    for (id, old) in [("registered", &registered), ("unregistered", &unregistered)] {
        let new: PinnedPathsV1 =
            serde_json::from_slice(&payload.staged_pin_bytes(id).unwrap()).unwrap();
        assert_eq!(new.package, old.package);
        assert_eq!(new.version, old.version);
        assert_eq!(
            new.runtime,
            f.home
                .join(".polaris/mcp-v2/servers")
                .join(id)
                .join("runtime")
        );
        assert_eq!(
            new.entry,
            f.home
                .join(".polaris/mcp-v2/servers")
                .join(id)
                .join("index.js")
        );
    }
    let Destination::Stdio {
        command,
        script,
        args,
        ..
    } = &payload.documents.registry.snapshot().unwrap().servers[0].transport
    else {
        panic!()
    };
    assert_eq!(
        command,
        &f.home.join(".polaris/mcp-v2/servers/registered/runtime")
    );
    assert_eq!(args[0], script.as_ref().unwrap().to_str().unwrap());
    assert_eq!(args[1], "opaque");
    payload.verify_current().unwrap();
}

#[test]
fn source_payload_preserves_absent_document_baselines_and_unregistered_packages() {
    let f = Fixture::new();
    f.package("orphan");
    let source = f.source();
    let payload = SourcePayloadV2::inspect(&source, ID, &f.home).unwrap();
    assert_eq!(
        payload.documents.credential.metadata().unwrap().presence,
        DocumentPresenceV2::Absent
    );
    assert_eq!(
        payload.documents.registry.metadata().unwrap().presence,
        DocumentPresenceV2::Absent
    );
    assert!(payload.documents.mapped_paths.is_none());
    assert_eq!(payload.unregistered_pins.len(), 1);
    assert!(payload.catalog.servers.is_empty());
}

#[test]
fn source_payload_rejects_unknown_cross_server_oversized_or_inconsistent_pins() {
    for case in [
        "unknown",
        "cross-server",
        "oversized",
        "inconsistent",
        "http-pin",
        "directory",
    ] {
        let f = Fixture::new();
        let mut pin = f.package("one");
        if case == "cross-server" {
            pin.runtime = f.store.join("servers/two/runtime");
        }
        if case == "inconsistent" {
            f.registry(vec![server("one", stdio(&pin))]);
            pin.entry = f.store.join("servers/one/other.js");
        }
        if case == "http-pin" {
            f.registry(vec![server(
                "one",
                Destination::StreamableHttp {
                    url: "https://example.test/mcp".into(),
                    secret_headers: BTreeMap::new(),
                },
            )]);
        }
        let mut bytes = serde_json::to_vec(&pin).unwrap();
        if case == "unknown" {
            let mut value = serde_json::to_value(&pin).unwrap();
            value["unknown"] = true.into();
            bytes = serde_json::to_vec(&value).unwrap();
        }
        if case == "oversized" {
            bytes = vec![b' '; MAX_JSON_BYTES + 1];
        }
        f.write("servers/one/pinned.json", &bytes);
        if case == "directory" {
            let path = f.store.join("servers/one/pinned.json");
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let source = f.source();
        let before = f.snapshot();
        assert!(
            SourcePayloadV2::inspect(&source, ID, &f.home).is_err(),
            "{case}"
        );
        assert_eq!(before, f.snapshot());
    }
}

#[test]
fn source_payload_rejects_same_byte_drift_replacement_or_late_membership_changes() {
    for case in [
        "secret",
        "registry",
        "package",
        "pin-replacement",
        "addition",
    ] {
        let f = Fixture::new();
        f.package("one");
        f.registry(vec![]);
        f.write(
            "secrets.json",
            br#"{"schema_version":1,"revision":9,"values":{"r":"value"}}"#,
        );
        let source = f.source();
        let store = f.store.clone();
        AFTER_PREPARATION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let path = store.join(match case {
                    "secret" => "secrets.json",
                    "registry" => "servers.json",
                    "package" => "servers/one/index.js",
                    "pin-replacement" => "servers/one/pinned.json",
                    _ => "servers/one/added.txt",
                });
                if case == "addition" {
                    fs::write(path, b"late package entry").unwrap();
                } else {
                    let bytes = fs::read(&path).unwrap();
                    if case == "pin-replacement" {
                        fs::remove_file(&path).unwrap();
                    }
                    fs::write(&path, bytes).unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                }
            }))
        });
        assert!(
            SourcePayloadV2::inspect(&source, ID, &f.home).is_err(),
            "{case}"
        );
    }
}

#[test]
fn source_payload_rejects_missing_secret_references_and_invalid_migration_id() {
    let f = Fixture::new();
    f.registry(vec![server(
        "http",
        Destination::StreamableHttp {
            url: "https://example.test/mcp".into(),
            secret_headers: BTreeMap::from([("authorization".into(), "missing".into())]),
        },
    )]);
    // The existing source preflight already rejects the missing reference.
    assert!(SourcePreflightV2::inspect(&f.home).is_err());
    f.write(
        "secrets.json",
        br#"{"schema_version":1,"revision":9,"values":{"missing":"value"}}"#,
    );
    let source = f.source();
    let mut payload = SourcePayloadV2::inspect(&source, ID, &f.home).unwrap();
    payload.documents.credential = CredentialDocumentV2::baseline(ID, None).unwrap();
    assert!(payload.documents.validate().is_err());
    assert!(payload.verify_current().is_err());
    let empty = Fixture::new();
    let source = empty.source();
    assert!(SourcePayloadV2::inspect(&source, "unknown", &empty.home).is_err());
}

#[test]
fn source_payload_rejects_launcher_partial_but_preserves_opaque_package_locks() {
    let f = Fixture::new();
    f.package("one");
    f.write("servers/one/package-lock.json", b"opaque package lock");
    let source = f.source();
    SourcePayloadV2::inspect(&source, ID, &f.home).unwrap();
    drop(source);
    f.write(
        "servers/one/pinned.json.partial",
        b"unfinished launcher control document",
    );
    let source = f.source();
    let before = f.snapshot();
    assert!(SourcePayloadV2::inspect(&source, ID, &f.home).is_err());
    assert_eq!(before, f.snapshot());
}

#[test]
fn source_payload_accepts_many_large_unregistered_pins_and_encodes_one_at_a_time() {
    let f = Fixture::new();
    // Unregistered materialized packages are not limited by MAX_SERVERS.
    let count = polaris_mcp::registration::MAX_SERVERS * 2 + 1;
    for index in 0..count {
        let id = format!("orphan-{index:03}");
        let mut pin = f.package(&id);
        pin.package = "opaque".repeat(36_000);
        let bytes = serde_json::to_vec(&pin).unwrap();
        assert!(bytes.len() > 200_000 && bytes.len() < MAX_JSON_BYTES);
        f.write(&format!("servers/{id}/pinned.json"), &bytes);
    }
    let source = f.source();
    let payload = SourcePayloadV2::inspect(&source, ID, &f.home).unwrap();
    assert_eq!(payload.pins.len(), count);
    assert_eq!(payload.unregistered_pins.len(), count);
    for id in ["orphan-000", "orphan-032", "orphan-064"] {
        let bytes = payload.staged_pin_bytes(id).unwrap();
        let pin: PinnedPathsV1 = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(pin.package.len(), 216_000);
        assert_eq!(
            pin.entry,
            f.home
                .join(".polaris/mcp-v2/servers")
                .join(id)
                .join("index.js")
        );
    }
}
