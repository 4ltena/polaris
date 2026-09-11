//! Trusted strict10 resource validation and offline preflight failure tests.
use super::*;
use std::{
    cell::Cell,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    source: PathBuf,
    runtime: PathBuf,
    model: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let home = base.join("home");
        let store = base.join("store");
        let source = base.join("source");
        let model = base.join("model");
        for dir in [&home, &store, &source, &model, &home.join(".polaris")] {
            fs::create_dir_all(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let runtime = base.join("python");
        // Only magic recognition is exercised. The injected probe never executes it.
        fs::write(&runtime, b"\xcf\xfa\xed\xfefixture binary").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            model.join("config.json"),
            br#"{"model_type":"bert","architectures":["BertModel"],"hidden_size":384}"#,
        )
        .unwrap();
        fs::write(
            model.join("model.safetensors"),
            b"fixture weights, never loaded",
        )
        .unwrap();
        fs::write(model.join("tokenizer.json"), b"{}").unwrap();
        let fixture = Self {
            _temp: temp,
            home,
            store,
            source,
            runtime,
            model,
        };
        fixture.manifest();
        fixture.config();
        fixture
    }
    fn config(&self) {
        fs::write(self.home.join(".polaris/config.toml"), format!(
            "history_mode = \"legacy\"\n[embedding]\nruntime = {}\nmodel_path = {}\nrevision = \"fixed\"\n",
            serde_json::to_string(&self.runtime).unwrap(), serde_json::to_string(&self.model).unwrap()
        )).unwrap();
    }
    fn manifest(&self) {
        let files: BTreeMap<_, _> = ["config.json", "model.safetensors", "tokenizer.json"]
            .into_iter()
            .map(|p| {
                (
                    p.to_string(),
                    hex_hash(&fs::read(self.model.join(p)).unwrap()),
                )
            })
            .collect();
        fs::write(
            self.model.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({"revision":"fixed","files":files})).unwrap(),
        )
        .unwrap();
    }
    fn prepare(&self) -> Result<Option<TrustedMemoryIndex>, ResourceError> {
        prepare_with(
            HistoryMode::Strict10,
            BootstrapProvider::Codex,
            &self.home,
            &self.store,
            &self.source,
            |_| Ok(receipt()),
        )
    }
}
fn receipt() -> serde_json::Value {
    serde_json::json!({"model":MODEL,"dimension":384,"revision":"fixed",
        "packages":{"torch":"1","transformers":"2","tokenizers":"3","safetensors":"4"}})
}

#[test]
fn legacy_does_not_read_resources_or_invoke_probe() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing");
    assert!(
        prepare_with(
            HistoryMode::Legacy,
            BootstrapProvider::Ollama,
            &missing,
            &missing,
            &missing,
            |_| panic!("legacy invoked preflight")
        )
        .unwrap()
        .is_none()
    );
    assert!(!missing.exists());
}

#[test]
fn local_primary_is_refused_before_reading_config_or_probing() {
    let missing = Path::new("/missing");
    for provider in [BootstrapProvider::Ollama, BootstrapProvider::Lmstudio] {
        assert!(matches!(
            prepare_with(
                HistoryMode::Strict10,
                provider,
                missing,
                missing,
                missing,
                |_| panic!("local primary invoked preflight")
            ),
            Err(ResourceError::Provider)
        ));
    }
}

#[test]
fn trusted_home_config_is_required_and_project_config_is_not_consulted() {
    let f = Fixture::new();
    fs::create_dir(f.source.join(".polaris")).unwrap();
    fs::rename(
        f.home.join(".polaris/config.toml"),
        f.source.join(".polaris/config.toml"),
    )
    .unwrap();
    assert!(matches!(f.prepare(), Err(ResourceError::Configuration)));
    assert!(!f.store.join("embedding-helper").exists());
}

#[test]
fn malformed_incomplete_oversized_and_writable_configuration_is_rejected() {
    let f = Fixture::new();
    let path = f.home.join(".polaris/config.toml");
    for body in [
        "[embedding",
        "[embedding]\nrevision=\"fixed\"",
        &"x".repeat(MAX_CONFIG as usize + 1),
    ] {
        fs::write(&path, body).unwrap();
        assert!(matches!(f.prepare(), Err(ResourceError::Configuration)));
    }
    f.config();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(matches!(f.prepare(), Err(ResourceError::Configuration)));
}

#[test]
fn physical_paths_and_non_project_runtime_are_required() {
    let f = Fixture::new();
    assert!(matches!(
        verify(
            EmbeddingConfig {
                runtime: Some(f.runtime.clone()),
                model_path: Some(f.model.clone()),
                revision: Some("fixed".into())
            },
            f.runtime.parent().unwrap()
        ),
        Err(ResourceError::Path)
    ));
    for path in [
        "python",
        "/",
        "/tmp/../python",
        "/tmp//python",
        "/tmp/./python",
        "/tmp/python/",
    ] {
        assert!(protected_open(Path::new(path), false).is_err());
    }
    let alias = f.home.join("alias");
    symlink(&f.model, &alias).unwrap();
    assert!(protected_open(&alias.join("config.json"), false).is_err());
    let config_path = f.home.join(".polaris/config.toml");
    fs::rename(&config_path, f.home.join("config-copy")).unwrap();
    symlink(f.home.join("config-copy"), &config_path).unwrap();
    assert!(matches!(f.prepare(), Err(ResourceError::Configuration)));
}

#[test]
fn scripts_cannot_be_selected_as_the_python_runtime() {
    let f = Fixture::new();
    fs::write(&f.runtime, b"#!/bin/sh\necho forbidden").unwrap();
    assert!(matches!(f.prepare(), Err(ResourceError::Runtime)));
    assert!(!f.store.join("embedding-helper").exists());
}

#[test]
fn missing_or_unprotected_runtime_and_missing_model_stop_before_readiness() {
    let f = Fixture::new();
    for mode in [0o600, 0o777] {
        fs::set_permissions(&f.runtime, fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(f.prepare(), Err(ResourceError::Runtime)));
    }
    fs::set_permissions(&f.runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(&f.model, f.home.join("moved-model")).unwrap();
    assert!(matches!(f.prepare(), Err(ResourceError::Path)));
    fs::rename(&f.runtime, f.home.join("moved-runtime")).unwrap();
    assert!(matches!(f.prepare(), Err(ResourceError::Runtime)));
    assert!(!f.store.join("embedding-helper").exists());
}

#[test]
fn manifest_hash_revision_inventory_and_symlink_must_match_before_probe() {
    for mutation in 0..4 {
        let f = Fixture::new();
        match mutation {
            0 => fs::write(f.model.join("model.safetensors"), b"changed").unwrap(),
            1 => {
                let p = f.model.join("manifest.json");
                let body = fs::read_to_string(&p).unwrap().replace("fixed", "other");
                fs::write(p, body).unwrap();
            }
            2 => fs::write(f.model.join("extra.json"), b"unlisted").unwrap(),
            _ => symlink(&f.runtime, f.model.join("linked")).unwrap(),
        }
        let calls = Cell::new(0);
        assert!(
            prepare_with(
                HistoryMode::Strict10,
                BootstrapProvider::Codex,
                &f.home,
                &f.store,
                &f.source,
                |_| {
                    calls.set(calls.get() + 1);
                    Ok(receipt())
                }
            )
            .is_err()
        );
        assert_eq!(calls.get(), 0);
        assert!(!f.store.join("embedding-helper").exists());
    }
}

#[test]
fn different_architecture_or_dimension_cannot_be_accepted_by_valid_manifest() {
    let f = Fixture::new();
    for body in [
        br#"{"hidden_size":768,"model_type":"bert","architectures":["BertModel"]}"#.as_slice(),
        br#"{"hidden_size":384,"model_type":"xlm-roberta","architectures":["XLMRobertaModel"]}"#
            .as_slice(),
    ] {
        fs::write(f.model.join("config.json"), body).unwrap();
        f.manifest();
        assert!(matches!(f.prepare(), Err(ResourceError::Architecture)));
    }
}

#[test]
fn prepared_identity_binds_verified_resources_and_uses_only_owner_database() {
    let f = Fixture::new();
    let first = f.prepare().unwrap().unwrap();
    assert_eq!(first.database, f.store.join("conversation-memory.sqlite3"));
    assert!(
        !first.database.exists(),
        "preflight must not open the index"
    );
    assert_eq!(first.identity.embedding_model, MODEL);
    assert_eq!(first.identity.embedding_dimension, 384);
    assert_eq!(first.identity.embedding_revision, "fixed");
    first.identity.validate().unwrap();
    assert_eq!(first.identity, f.prepare().unwrap().unwrap().identity);
    fs::write(&f.runtime, b"\xcf\xfa\xed\xfechanged binary").unwrap();
    assert_ne!(
        first.identity.fingerprint,
        f.prepare().unwrap().unwrap().identity.fingerprint
    );
    assert!(
        first
            .embedder
            .helper
            .starts_with(f.store.join("embedding-helper"))
    );
}

#[test]
fn resource_mutation_or_bad_readiness_receipt_prevents_publication() {
    let f = Fixture::new();
    assert!(matches!(
        prepare_with(
            HistoryMode::Strict10,
            BootstrapProvider::Codex,
            &f.home,
            &f.store,
            &f.source,
            |_| {
                fs::write(&f.runtime, b"\xcf\xfa\xed\xfechanged during readiness").unwrap();
                Ok(receipt())
            }
        ),
        Err(ResourceError::Changed)
    ));
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"model":MODEL,"dimension":384,"revision":"other","packages":{}}),
    ] {
        assert!(matches!(
            prepare_with(
                HistoryMode::Strict10,
                BootstrapProvider::Codex,
                &f.home,
                &f.store,
                &f.source,
                |_| Ok(bad)
            ),
            Err(ResourceError::Readiness)
        ));
    }
}

#[test]
fn existing_database_alias_cannot_redirect_the_memory_index() {
    let f = Fixture::new();
    symlink(&f.runtime, f.store.join("conversation-memory.sqlite3")).unwrap();
    assert!(matches!(f.prepare(), Err(ResourceError::Path)));
    assert!(!f.store.join("embedding-helper").exists());
}
