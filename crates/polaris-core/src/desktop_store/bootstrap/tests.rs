//! Owner bootstrap metadata, identity, configuration and provider validation tests.
use super::*;
use crate::desktop_store::{DesktopRoot, InitialState, Writer};
use polaris_desktop_protocol::snapshot::{Configuration, Draft};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::PathBuf,
};

struct Fixture {
    _temp: tempfile::TempDir,
    path: PathBuf,
    root: PathBuf,
    writer: Writer,
    document: OwnerBootstrapDocument,
}
fn identity(metadata: &fs::Metadata) -> BootstrapIdentity {
    BootstrapIdentity {
        device: DecimalU64::new(metadata.dev()),
        inode: DecimalU64::new(metadata.ino()),
    }
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("store");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let source = base.join("source");
        fs::create_dir(&source).unwrap();
        let project = ProjectId::new("project").unwrap();
        let session = SessionId::new("session").unwrap();
        let writer = DesktopRoot::open_owned(&root)
            .unwrap()
            .create(
                project.clone(),
                session.clone(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: String::new(),
                        attachment_ids: vec![],
                    },
                    configuration: Configuration {
                        history_mode: Default::default(),
                        configuration_revision: DecimalU64::new(3),
                        provider: "codex".into(),
                        model: "gpt-6-astra".into(),
                        effort: "medium".into(),
                    },
                    policy_revision: DecimalU64::new(7),
                },
            )
            .unwrap();
        let document = OwnerBootstrapDocument {
            history_mode: Default::default(),
            schema_version: 1,
            project_id: project,
            session_id: session,
            store_identity: identity(&fs::metadata(&root).unwrap()),
            source_path: source.to_str().unwrap().into(),
            source_identity: identity(&fs::metadata(&source).unwrap()),
            tier: BootstrapTier::ReadCreateBuild,
            policy_revision: DecimalU64::new(7),
            configuration_revision: DecimalU64::new(3),
            provider: BootstrapProvider::Codex,
            model: None,
            effort: None,
            local_endpoint: None,
        };
        Self {
            _temp: temp,
            path: root.join("owner.json"),
            root,
            writer,
            document,
        }
    }
    fn write(&self, bytes: &[u8]) -> ExpectedBootstrapFile {
        fs::write(&self.path, bytes).unwrap();
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600)).unwrap();
        ExpectedBootstrapFile {
            identity: identity(&fs::metadata(&self.path).unwrap()),
            sha256: Sha256::digest(bytes).into(),
        }
    }
    fn expected_store(&self) -> ExpectedBootstrapStore<'_> {
        ExpectedBootstrapStore {
            path: &self.root,
            identity: self.document.store_identity,
        }
    }
    fn read(&self, expected: &ExpectedBootstrapFile) -> StoreResult<ValidatedOwnerBootstrap> {
        read_owner_bootstrap(
            &self.path,
            expected,
            &self.expected_store(),
            &self.writer.snapshot().unwrap(),
        )
    }
    fn publish(&self) -> ExpectedBootstrapFile {
        self.write(&serde_json::to_vec(&self.document).unwrap())
    }
}

#[test]
fn bootstrap_defaults_read_only_and_exact_size_limit() {
    let f = Fixture::new();
    let before = f.writer.snapshot().unwrap();
    let mut bytes = serde_json::to_vec(&f.document).unwrap();
    bytes.resize(MAX_OWNER_BOOTSTRAP_BYTES, b' ');
    let expected = f.write(&bytes);
    let read = f.read(&expected).unwrap();
    assert_eq!(read.model(), "gpt-6-astra");
    assert_eq!(read.stored_effort(), "medium");
    assert_eq!(read.effective_effort(), Some("medium"));
    assert_eq!(read.document(), &f.document);
    let after = f.writer.snapshot().unwrap();
    assert_eq!(after.marker, before.marker);
    assert_eq!(after.state, before.state);
    bytes.push(b' ');
    let expected = f.write(&bytes);
    assert!(f.read(&expected).is_err());
}
#[test]
fn bootstrap_rejects_unknown_duplicate_and_unbounded_or_wrongly_typed_fields() {
    let f = Fixture::new();
    let original = serde_json::to_string(&f.document).unwrap();
    let mut cases = vec![
        format!(
            "{},\"auth_path\":\"/secret\"}}",
            &original[..original.len() - 1]
        ),
        format!("{},\"schema_version\":1}}", &original[..original.len() - 1]),
    ];
    for (key, value) in [
        ("schema_version", serde_json::json!(2)),
        ("source_path", serde_json::json!("x".repeat(4097))),
        ("policy_revision", serde_json::json!(7)),
        ("policy_revision", serde_json::json!("18446744073709551616")),
        ("source_path", serde_json::json!("/a/../b")),
    ] {
        let mut v = serde_json::to_value(&f.document).unwrap();
        v[key] = value;
        cases.push(v.to_string());
    }
    for bytes in cases {
        let expected = f.write(bytes.as_bytes());
        assert!(f.read(&expected).is_err());
    }
}
#[test]
fn bootstrap_binds_file_digest_store_and_saved_generations() {
    let mut f = Fixture::new();
    let mut expected = f.publish();
    expected.sha256[0] ^= 1;
    assert!(f.read(&expected).is_err());
    let mut expected = f.publish();
    expected.identity.inode = DecimalU64::new(0);
    assert!(f.read(&expected).is_err());
    let original = f.document.clone();
    for change in 0..7 {
        f.document = original.clone();
        match change {
            0 => f.document.project_id = ProjectId::new("other").unwrap(),
            1 => f.document.session_id = SessionId::new("other").unwrap(),
            2 => f.document.policy_revision = DecimalU64::new(8),
            3 => f.document.configuration_revision = DecimalU64::new(4),
            4 => f.document.model = Some("other".into()),
            5 => f.document.effort = Some("high".into()),
            _ => f.document.store_identity.inode = DecimalU64::new(0),
        };
        let expected = f.publish();
        assert!(f.read(&expected).is_err());
    }
}
#[test]
fn bootstrap_rejects_symlinks_hardlinks_and_nonprivate_files() {
    let f = Fixture::new();
    let expected = f.publish();
    fs::hard_link(&f.path, f.root.join("alias")).unwrap();
    assert!(f.read(&expected).is_err());
    fs::remove_file(f.root.join("alias")).unwrap();
    fs::set_permissions(&f.path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(f.read(&expected).is_err());
    let expected = f.publish();
    let moved = f.root.join("moved");
    fs::rename(&f.path, &moved).unwrap();
    symlink(&moved, &f.path).unwrap();
    assert!(f.read(&expected).is_err());
    let alias = f.root.with_file_name("store-alias");
    symlink(&f.root, &alias).unwrap();
    assert!(
        read_owner_bootstrap(
            &alias.join("owner.json"),
            &expected,
            &ExpectedBootstrapStore {
                path: &alias,
                identity: f.document.store_identity
            },
            &f.writer.snapshot().unwrap()
        )
        .is_err()
    );
}
#[test]
fn bootstrap_rejects_same_inode_changes_and_parent_rebinding_during_read() {
    for change in 0..2 {
        let f = Fixture::new();
        let expected = f.publish();
        let saved = f.writer.snapshot().unwrap();
        assert!(
            read_checked(&f.path, &expected, &f.expected_store(), &saved, || {
                if change == 0 {
                    fs::write(&f.path, b"{}").unwrap();
                } else {
                    fs::rename(&f.root, f.root.with_file_name("old-store")).unwrap();
                    fs::create_dir(&f.root).unwrap();
                    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o700)).unwrap();
                }
            })
            .is_err()
        );
    }
}
#[test]
fn bootstrap_local_requires_explicit_model_and_loopback_without_cloud_fallback() {
    let mut f = Fixture::new();
    f.document.provider = BootstrapProvider::Ollama;
    let mut saved = f.writer.snapshot().unwrap();
    saved.state.configuration.provider = "ollama".into();
    saved.state.configuration.model = "chosen-local".into();
    f.document.local_endpoint = Some("http://127.0.0.1:11434/".into());
    let expected = f.publish();
    assert!(read_owner_bootstrap(&f.path, &expected, &f.expected_store(), &saved).is_err());
    f.document.model = Some("chosen-local".into());
    let expected = f.publish();
    assert_eq!(
        read_owner_bootstrap(&f.path, &expected, &f.expected_store(), &saved)
            .unwrap()
            .model(),
        "chosen-local"
    );
    for endpoint in [
        None,
        Some("http://example.com/"),
        Some("http://localhost:11434/"),
        Some("http://key@127.0.0.1:11434/"),
    ] {
        f.document.local_endpoint = endpoint.map(String::from);
        let expected = f.publish();
        assert!(read_owner_bootstrap(&f.path, &expected, &f.expected_store(), &saved).is_err());
    }
}

#[test]
fn bootstrap_local_effort_is_storage_only_and_explicit_effort_is_rejected() {
    for provider in [BootstrapProvider::Ollama, BootstrapProvider::Lmstudio] {
        let mut f = Fixture::new();
        f.document.provider = provider;
        f.document.model = Some("chosen-local".into());
        f.document.local_endpoint = Some("http://127.0.0.1:11434/".into());
        let mut saved = f.writer.snapshot().unwrap();
        saved.state.configuration.provider = provider.as_str().into();
        saved.state.configuration.model = "chosen-local".into();
        let expected = f.publish();
        let read = read_owner_bootstrap(&f.path, &expected, &f.expected_store(), &saved).unwrap();
        assert_eq!(read.stored_effort(), "medium");
        assert_eq!(read.effective_effort(), None);
        for effort in ["medium", "high"] {
            f.document.effort = Some(effort.into());
            let expected = f.publish();
            assert!(read_owner_bootstrap(&f.path, &expected, &f.expected_store(), &saved).is_err());
        }
    }
}

#[test]
fn owner_configuration_shared_validator_preserves_cloud_and_local_semantics() {
    for provider in ["codex", "openai"] {
        for effort in ["low", "medium", "high", "xhigh", "max", "ultra"] {
            validate_owner_configuration(provider, "model", effort).unwrap();
        }
    }
    for provider in ["ollama", "lmstudio"] {
        validate_owner_configuration(provider, "local-model", "medium").unwrap();
        for effort in ["", "none", "low", "high", "ultra"] {
            assert!(validate_owner_configuration(provider, "model", effort).is_err());
        }
    }
    for provider in ["fake", "", "OpenAI", "unknown"] {
        assert!(validate_owner_configuration(provider, "model", "medium").is_err());
    }
    for model in ["".to_owned(), "x".repeat(513), "model\n".into()] {
        assert!(validate_owner_configuration("openai", &model, "medium").is_err());
    }
    assert!(validate_owner_configuration("openai", &"x".repeat(512), "medium").is_ok());
    assert!(validate_owner_configuration("codex", "model", "none").is_err());
}
