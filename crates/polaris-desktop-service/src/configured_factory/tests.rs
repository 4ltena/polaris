//! Configured factory resource, source grant and runtime binding tests.
use super::*;
use polaris_core::desktop_store::{
    BootstrapIdentity, DesktopRoot, ExpectedBootstrapFile, ExpectedBootstrapStore, InitialState,
    OwnerBootstrapDocument, SourceApplyIdentity, Writer, read_owner_bootstrap,
};
use polaris_desktop_protocol::{
    ids::*,
    request::*,
    snapshot::{Configuration, Draft},
};
use polaris_provider::{ProviderError, Token};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Tokens(AtomicUsize);
#[async_trait::async_trait]
impl TokenSource for Tokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::Auth("unused fixture".into()))
    }
    async fn refreshed(&self) -> Result<Token, ProviderError> {
        self.token().await
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    source: PathBuf,
    helper: PathBuf,
    writer: Writer,
    tokens: Arc<Tokens>,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let source = base.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("ordinary"), "input").unwrap();
        let helper = base.join("helper");
        fs::write(&helper, "fixture helper never launched").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let store = base.join("store");
        fs::create_dir(&store).unwrap();
        fs::set_permissions(&store, fs::Permissions::from_mode(0o700)).unwrap();
        let writer = DesktopRoot::open_owned(&store)
            .unwrap()
            .create(
                ProjectId::new("p").unwrap(),
                SessionId::new("s").unwrap(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: "input".into(),
                        attachment_ids: vec![],
                    },
                    configuration: Configuration {
                        history_mode: Default::default(),
                        configuration_revision: DecimalU64::new(0),
                        provider: "codex".into(),
                        model: "gpt-6-astra".into(),
                        effort: "medium".into(),
                    },
                    policy_revision: DecimalU64::new(7),
                },
            )
            .unwrap();
        Self {
            _temp: temp,
            base,
            source,
            helper,
            writer,
            tokens: Arc::new(Tokens(AtomicUsize::new(0))),
        }
    }
    fn grant(&self) -> ConfirmedSourceGrant {
        let m = fs::metadata(&self.source).unwrap();
        ConfirmedSourceGrant {
            policy: CurrentSourcePolicy {
                source_path: self.source.clone(),
                source_identity: SourceApplyIdentity {
                    device: DecimalU64::new(m.dev()),
                    inode: DecimalU64::new(m.ino()),
                },
                policy_revision: DecimalU64::new(7),
                read_allowed: true,
                write_allowed: true,
            },
            tier: BootstrapTier::ReadCreate,
        }
    }
    fn bootstrap(&self) -> ValidatedOwnerBootstrap {
        let identity = |p: &std::path::Path| {
            let m = fs::metadata(p).unwrap();
            BootstrapIdentity {
                device: DecimalU64::new(m.dev()),
                inode: DecimalU64::new(m.ino()),
            }
        };
        let root = self.base.join("store");
        let saved = self.writer.snapshot().unwrap();
        let doc = OwnerBootstrapDocument {
            history_mode: Default::default(),
            schema_version: 1,
            project_id: saved.marker.project_id.clone(),
            session_id: saved.marker.session_id.clone(),
            store_identity: identity(&root),
            source_path: self.source.to_str().unwrap().into(),
            source_identity: identity(&self.source),
            tier: BootstrapTier::ReadCreate,
            policy_revision: DecimalU64::new(7),
            configuration_revision: saved.state.configuration.configuration_revision,
            provider: BootstrapProvider::Codex,
            model: None,
            effort: None,
            local_endpoint: None,
        };
        let bytes = serde_json::to_vec(&doc).unwrap();
        let path = root.join("owner.json");
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let hash = polaris_core::conversation_state::content_hash(&bytes);
        let mut sha256 = [0; 32];
        for (i, v) in sha256.iter_mut().enumerate() {
            *v = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16).unwrap();
        }
        read_owner_bootstrap(
            &path,
            &ExpectedBootstrapFile {
                identity: identity(&path),
                sha256,
            },
            &ExpectedBootstrapStore {
                path: &root,
                identity: identity(&root),
            },
            &saved,
        )
        .unwrap()
    }
    fn resources(&self) -> TrustedFactoryResources {
        TrustedFactoryResources {
            always_on: polaris_core::prompt::assemble_always_on("", "", &[]),
            audit: Arc::new(tokio::sync::Mutex::new(
                AuditLog::open(&self.base.join("audit")).unwrap(),
            )),
            skills: vec![],
            agent_types: vec![AgentType {
                name: "child".into(),
                description: "fixture".into(),
                body: "fixture".into(),
                path: self.base.clone(),
                allowed_tools: vec![],
                access: polaris_skills::AgentAccess::Read,
                tier: "medium".into(),
                wall_seconds: 5,
                max_turns: 2,
                workflow_phase: None,
                continuation: false,
                output_schema: self.base.join("schema"),
            }],
            max_turns: 2,
            spawn_concurrency: 1,
            spawn_write_concurrency: 1,
            tool_memory: None,
            history: None,
        }
    }
    fn factory(&self) -> ConfiguredRunFactory {
        ConfiguredRunFactory::new(
            self.bootstrap(),
            self.grant(),
            TrustedBackend::Codex {
                tokens: self.tokens.clone(),
            },
            self.resources(),
        )
        .unwrap()
    }
    fn workspace(&self) -> PreparedWorkspace {
        PreparedWorkspace::prepare(
            &self.source,
            &self.helper,
            SandboxMode::WorkspaceWrite,
            &[],
            polaris_core::isolated_workspace::Limits {
                max_entries: 16,
                max_files: 8,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 4,
            },
        )
        .unwrap()
    }
    fn accept(&mut self) -> RunTarget {
        self.accept_named("")
    }
    fn accept_named(&mut self, suffix: &str) -> RunTarget {
        let target = RunTarget {
            run_id: RunId::new(format!("r{suffix}")).unwrap(),
            attempt_id: AttemptId::new(format!("a{suffix}")).unwrap(),
        };
        self.writer
            .apply(
                &Request {
                    protocol_version: Default::default(),
                    client_id: ClientId::new("c").unwrap(),
                    request_id: RequestId::new(format!("start{suffix}")).unwrap(),
                    body: RequestBody::RunStart(
                        SessionId::new("s").unwrap(),
                        RunStart {
                            expected_draft_revision: self
                                .writer
                                .snapshot()
                                .unwrap()
                                .state
                                .draft
                                .draft_revision,
                            expected_configuration_revision: self
                                .writer
                                .snapshot()
                                .unwrap()
                                .state
                                .configuration
                                .configuration_revision,
                            expected_policy_revision: DecimalU64::new(7),
                        },
                    ),
                },
                Some(target.clone()),
            )
            .unwrap();
        self.writer
            .record_intent(&target, OperationId::new(format!("op{suffix}")).unwrap())
            .unwrap();
        target
    }
}
#[test]
fn role_revision_requires_refreshed_owner_bootstrap_before_next_run() {
    let mut f = Fixture::new();
    let old = f.factory();
    let catalog = f.resources().agent_types;
    f.writer
        .configure_role_bindings(
            DecimalU64::new(0),
            Some(polaris_core::desktop_store::SavedRoleBindings::new(vec![]).unwrap()),
            &catalog,
        )
        .unwrap();
    let target = f.accept_named("-roles");
    let saved = f.writer.snapshot().unwrap();
    old.stage_workspace(f.workspace()).unwrap();
    assert!(old.prepare(&target, &saved).is_err());

    let refreshed = f.factory();
    refreshed.stage_workspace(f.workspace()).unwrap();
    let inputs = refreshed.prepare(&target, &saved).unwrap();
    assert!(inputs.provider.resolve_role("child").is_err());
}
#[test]
fn frozen_local_role_is_revalidated_and_bound_without_primary_fallback() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for (path, body) in [
            (
                "GET /api/tags ",
                r#"{"models":[{"name":"qwen:small","digest":"sha256:observed"}]}"#,
            ),
            (
                "POST /api/show ",
                r#"{"capabilities":["completion"],"model_info":{"qwen.context_length":32768}}"#,
            ),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            assert!(
                std::str::from_utf8(&request[..count])
                    .unwrap()
                    .starts_with(path)
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });
    let mut f = Fixture::new();
    let catalog = f.resources().agent_types;
    f.writer
        .configure_role_bindings(
            DecimalU64::new(0),
            Some(
                polaris_core::desktop_store::SavedRoleBindings::new(vec![
                    polaris_core::desktop_store::SavedRoleBinding {
                        role: "child".into(),
                        runtime: polaris_core::desktop_store::SavedRuntime::Ollama,
                        endpoint,
                        model: "qwen:small".into(),
                        observed_tool_support: polaris_core::desktop_store::ToolSupport::Unknown,
                    },
                ])
                .unwrap(),
            ),
            &catalog,
        )
        .unwrap();
    let target = f.accept_named("-local-role");
    let saved = f.writer.snapshot().unwrap();
    let factory = f.factory();
    factory.stage_workspace(f.workspace()).unwrap();
    let inputs = factory.prepare(&target, &saved).unwrap();
    assert!(inputs.provider.resolve_role("child").unwrap().is_some());
    assert!(inputs.provider.resolve_role("missing").is_err());
    assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
    server.join().unwrap();
}
#[test]
fn configured_factory_stages_once_and_refuses_unconfigured_child_without_auth() {
    let mut f = Fixture::new();
    let factory = f.factory();
    let target = f.accept();
    let saved = f.writer.snapshot().unwrap();
    assert!(factory.prepare(&target, &saved).is_err());
    factory.stage_workspace(f.workspace()).unwrap();
    assert!(factory.stage_workspace(f.workspace()).is_err());
    let inputs = factory.prepare(&target, &saved).unwrap();
    assert!(inputs.mutation_policy.is_none());
    assert!(Arc::ptr_eq(&inputs.provider, &inputs.provider_pool));
    assert!(inputs.provider.resolve_role("child").is_err());
    assert!(inputs.provider_pool.resolve_role("child").is_err());
    assert!(inputs.provider.resolve_role("unknown").is_err());
    assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
    assert!(factory.prepare(&target, &saved).is_err());
    factory.stage_workspace(f.workspace()).unwrap();
    assert!(factory.prepare(&target, &saved).is_err());
    assert!(factory.staged.lock().unwrap().staged.is_some());
}
#[test]
fn configured_factory_rejects_grant_backend_and_catalog_mismatch() {
    let f = Fixture::new();
    let mut grant = f.grant();
    grant.policy.policy_revision = DecimalU64::new(8);
    assert!(
        ConfiguredRunFactory::new(
            f.bootstrap(),
            grant,
            TrustedBackend::Codex {
                tokens: f.tokens.clone()
            },
            f.resources()
        )
        .is_err()
    );
    assert!(
        ConfiguredRunFactory::new(
            f.bootstrap(),
            f.grant(),
            TrustedBackend::Openai {
                api_key: "synthetic-not-a-secret".into()
            },
            f.resources()
        )
        .is_err()
    );
    let mut resources = f.resources();
    resources.agent_types.push(resources.agent_types[0].clone());
    assert!(
        ConfiguredRunFactory::new(
            f.bootstrap(),
            f.grant(),
            TrustedBackend::Codex {
                tokens: f.tokens.clone()
            },
            resources
        )
        .is_err()
    );
    assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
}
#[test]
fn configured_factory_saved_checks_preserve_staged_workspace_on_rejection() {
    let mut f = Fixture::new();
    let factory = f.factory();
    factory.stage_workspace(f.workspace()).unwrap();
    let target = f.accept();
    let original = f.writer.snapshot().unwrap();
    for change in 0..5 {
        let mut saved = original.clone();
        match change {
            0 => saved.marker.session_id = SessionId::new("other").unwrap(),
            1 => saved.state.policy_revision = DecimalU64::new(8),
            2 => saved.state.configuration.model = "other".into(),
            3 => saved.state.runs[0].policy_revision = DecimalU64::new(8),
            _ => saved.state.runs[0].run.state = RunState::Cancelling,
        };
        assert!(factory.prepare(&target, &saved).is_err());
        assert!(factory.staged.lock().unwrap().staged.is_some());
    }
    assert!(factory.prepare(&target, &original).is_ok());
}
#[test]
fn configured_factory_mutation_is_explicit_and_source_identity_checked() {
    let mut f = Fixture::new();
    let factory = f.factory();
    let prepared = f.workspace();
    let scope = prepared.policy().clone();
    factory
        .stage_workspace_with_mutations(prepared, scope.clone())
        .unwrap();
    let target = f.accept();
    assert_eq!(
        factory
            .prepare(&target, &f.writer.snapshot().unwrap())
            .unwrap()
            .mutation_policy,
        Some(scope)
    );
    let moved = f.source.with_file_name("old-source");
    fs::rename(&f.source, &moved).unwrap();
    fs::create_dir(&f.source).unwrap();
    fs::write(f.source.join("ordinary"), "replacement").unwrap();
    assert!(factory.stage_workspace(f.workspace()).is_err());
}

fn helper_hash(f: &Fixture) -> [u8; 32] {
    let hash = polaris_core::conversation_state::content_hash(&fs::read(&f.helper).unwrap());
    let mut bytes = [0; 32];
    for (i, v) in bytes.iter_mut().enumerate() {
        *v = u8::from_str_radix(&hash[2 * i..2 * i + 2], 16).unwrap();
    }
    bytes
}
fn recipe(f: &Fixture, mutations: bool) -> WorkspacePreparationRecipe {
    WorkspacePreparationRecipe::new(
        f.source.clone(),
        f.helper.clone(),
        helper_hash(f),
        polaris_core::isolated_workspace::Limits {
            max_entries: 16,
            max_files: 8,
            max_file_bytes: 1024,
            max_total_bytes: 4096,
            max_depth: 4,
        },
        vec![],
        None,
        mutations,
    )
    .unwrap()
}
fn recipe_factory(f: &Fixture, recipe: WorkspacePreparationRecipe) -> ConfiguredRunFactory {
    ConfiguredRunFactory::with_preparation_recipe(
        f.bootstrap(),
        f.grant(),
        TrustedBackend::Codex {
            tokens: f.tokens.clone(),
        },
        f.resources(),
        recipe,
    )
    .unwrap()
}
#[test]
fn configured_recipe_creates_two_fresh_copies_and_copy_only_grants() {
    let mut f = Fixture::new();
    let factory = recipe_factory(&f, recipe(&f, true));
    assert!(factory.stage_workspace(f.workspace()).is_err());
    let first_target = f.accept();
    let first = factory
        .prepare(&first_target, &f.writer.snapshot().unwrap())
        .unwrap();
    fs::write(
        first.prepared.snapshot().path().join("ordinary"),
        "first copy edit",
    )
    .unwrap();
    f.writer
        .finish_with_messages(
            &first_target,
            &OperationId::new("op").unwrap(),
            polaris_desktop_protocol::run_state::Observation::Succeeded,
            ResultId::new("finished").unwrap(),
            vec![polaris_provider::Message::assistant("done")],
        )
        .unwrap();
    let draft_revision = f.writer.snapshot().unwrap().state.draft.draft_revision;
    f.writer
        .apply(
            &Request {
                protocol_version: Default::default(),
                client_id: ClientId::new("c").unwrap(),
                request_id: RequestId::new("next-draft").unwrap(),
                body: RequestBody::DraftUpdate(
                    SessionId::new("s").unwrap(),
                    DraftUpdate {
                        expected_draft_revision: draft_revision,
                        text: "second input".into(),
                        attachment_ids: vec![],
                    },
                ),
            },
            None,
        )
        .unwrap();
    let second_target = f.accept_named("2");
    let second = factory
        .prepare(&second_target, &f.writer.snapshot().unwrap())
        .unwrap();
    assert_ne!(
        first.prepared.snapshot().path(),
        second.prepared.snapshot().path()
    );
    assert_eq!(
        fs::read_to_string(second.prepared.snapshot().path().join("ordinary")).unwrap(),
        "input"
    );
    assert_eq!(
        fs::read_to_string(f.source.join("ordinary")).unwrap(),
        "input"
    );
    for input in [&first, &second] {
        let scope = input.mutation_policy.as_ref().unwrap();
        assert!(
            scope
                .writable_roots()
                .iter()
                .all(|p| p.starts_with(input.prepared.snapshot().path()))
        );
        assert!(!scope.writable_roots().contains(&f.source));
    }
    assert!(Arc::ptr_eq(&first.provider, &second.provider));
    assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
}
#[test]
fn configured_recipe_refuses_changed_helper_and_source_before_returning_inputs() {
    for source_change in [false, true] {
        let mut f = Fixture::new();
        let factory = recipe_factory(&f, recipe(&f, false));
        let target = f.accept();
        if source_change {
            fs::rename(&f.source, f.source.with_file_name("old-source")).unwrap();
            fs::create_dir(&f.source).unwrap();
            fs::write(f.source.join("ordinary"), "replacement").unwrap();
        } else {
            let original = fs::read(&f.helper).unwrap();
            fs::rename(&f.helper, f.helper.with_file_name("old-helper")).unwrap();
            fs::write(&f.helper, original).unwrap();
            fs::set_permissions(&f.helper, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let saved = f.writer.snapshot().unwrap();
        assert!(factory.prepare(&target, &saved).is_err());
        assert!(factory.prepare(&target, &saved).is_err());
        assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn configured_recipe_manifest_hash_and_symlink_checks_are_real() {
    let f = Fixture::new();
    let limits = polaris_core::isolated_workspace::Limits {
        max_entries: 16,
        max_files: 8,
        max_file_bytes: 1024,
        max_total_bytes: 4096,
        max_depth: 4,
    };
    assert!(
        WorkspacePreparationRecipe::new(
            f.source.clone(),
            f.helper.clone(),
            [0; 32],
            limits,
            vec![],
            None,
            false
        )
        .is_err()
    );
    let alias = f.base.join("helper-alias");
    std::os::unix::fs::symlink(&f.helper, &alias).unwrap();
    assert!(
        WorkspacePreparationRecipe::new(
            f.source.clone(),
            alias,
            helper_hash(&f),
            limits,
            vec![],
            None,
            false
        )
        .is_err()
    );
    let recipe = recipe(&f, false);
    fs::write(&f.helper, "changed in same inode").unwrap();
    assert!(recipe.prepare(&f.grant()).is_err());
}

#[test]
fn configured_recipe_protected_helper_refuses_before_digest() {
    let f = Fixture::new();
    let hash = helper_hash(&f);
    let denied = f.base.join(".env");
    fs::rename(&f.helper, &denied).unwrap();
    let file = fs::File::open(&denied).unwrap();
    let called = std::cell::Cell::new(false);
    assert!(
        polaris_core::isolated_run::with_protected_helper_read(&denied, &file, |_| {
            called.set(true);
        })
        .is_err()
    );
    assert!(!called.get());
    assert!(
        WorkspacePreparationRecipe::new(
            f.source.clone(),
            denied,
            hash,
            polaris_core::isolated_workspace::Limits {
                max_entries: 16,
                max_files: 8,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 4,
            },
            vec![],
            None,
            false,
        )
        .is_err()
    );
}

#[test]
fn configured_recipe_registered_moved_inode_refuses_before_digest() {
    let mut f = Fixture::new();
    let registered = f.base.join("registered.json");
    polaris_auth::api_key::save_to(&registered, "synthetic-only").unwrap();
    let identity = fs::metadata(&registered).unwrap();
    let moved = f.base.join("ordinary-executable");
    fs::rename(&registered, &moved).unwrap();
    fs::set_permissions(&moved, fs::Permissions::from_mode(0o700)).unwrap();
    polaris_auth::api_key::save_to(&registered, "rotated-synthetic-only").unwrap();
    let metadata = fs::metadata(&moved).unwrap();
    assert_eq!(
        (metadata.dev(), metadata.ino()),
        (identity.dev(), identity.ino())
    );
    assert_eq!(metadata.nlink(), 1);
    f.helper = moved.clone();
    let hash = helper_hash(&f); // Only synthetic fixture bytes, never actual auth.
    let file = fs::File::open(&moved).unwrap();
    let called = std::cell::Cell::new(false);
    assert!(
        polaris_core::isolated_run::with_protected_helper_read(&moved, &file, |_| {
            called.set(true);
        })
        .is_err()
    );
    assert!(!called.get());
    assert!(
        WorkspacePreparationRecipe::new(
            f.source.clone(),
            moved,
            hash,
            polaris_core::isolated_workspace::Limits {
                max_entries: 16,
                max_files: 8,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 4,
            },
            vec![],
            None,
            false,
        )
        .is_err()
    );
    // Registry retains historical identities; retain this bounded dummy fixture
    // so concurrent tests cannot reuse its registered inodes.
    let _ = f._temp.keep();
}
