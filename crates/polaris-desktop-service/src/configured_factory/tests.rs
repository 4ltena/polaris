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
    /// What the fixture's one agent type declares it needs. Empty by
    /// default, because most tests do not care; a test that does sets it
    /// before building the resources.
    child_tools: Vec<String>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_mode(Default::default())
    }
    fn with_mode(mode: polaris_desktop_protocol::snapshot::HistoryMode) -> Self {
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
                        history_mode: mode,
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
            child_tools: Vec::new(),
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
        self.bootstrap_with_tier(BootstrapTier::ReadCreate)
    }
    fn bootstrap_with_tier(&self, tier: BootstrapTier) -> ValidatedOwnerBootstrap {
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
            history_mode: saved.state.configuration.history_mode,
            schema_version: 1,
            project_id: saved.marker.project_id.clone(),
            session_id: saved.marker.session_id.clone(),
            store_identity: identity(&root),
            source_path: self.source.to_str().unwrap().into(),
            source_identity: identity(&self.source),
            tier,
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
            decision_config: Default::default(),
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
                allowed_tools: self.child_tools.clone(),
                access: polaris_skills::AgentAccess::Read,
                tier: polaris_skills::Tier::Subgiant,
                tier_min: polaris_skills::Tier::Dwarf,
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
            mcp: None,
            workflow: polaris_core::workflow::WorkflowConfig {
                enabled: false,
                ..Default::default()
            },
            input_preparation: Default::default(),
            decision_local_only: false,
            decider: None,
            answer_finalizer: None,
        }
    }
    fn factory(&self) -> ConfiguredRunFactory {
        ConfiguredRunFactory::new(
            self.bootstrap(),
            self.grant(),
            TrustedBackend::Codex {
                accounts: None,
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
    /// An acceptance that fixed the run to one registration.
    fn accept_bound(&mut self, registration_id: &str) -> (RunTarget, Published) {
        let target = self.accept_named("");
        let mut saved = self.writer.snapshot().unwrap();
        saved.state.runs[0].account = Some(
            polaris_core::desktop_store::SavedAccount::new(registration_id.into(), 1).unwrap(),
        );
        (target, saved)
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
                            mcp_prompt: None,
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
fn mcp_configuration_preserves_project_and_external_ceiling_before_capability_collapse() {
    use polaris_core::mcp::{operation::ExternalCeiling, runtime::RuntimeConfig};
    for (tier, ceiling) in [
        (BootstrapTier::ReadCreate, ExternalCeiling::Denied),
        (BootstrapTier::ReadCreateBuild, ExternalCeiling::Denied),
        (
            BootstrapTier::ReadCreateBuildExternal,
            ExternalCeiling::Allowed,
        ),
    ] {
        for local_only in [false, true] {
            let mut f = Fixture::new();
            let mut grant = f.grant();
            grant.tier = tier;
            let mut resources = f.resources();
            resources.decision_local_only = local_only;
            resources.mcp = Some(
                RuntimeConfig::new(
                    polaris_mcp::registration::Registry {
                        schema_version: 1,
                        revision: 1,
                        servers: vec![],
                    },
                    "p".into(),
                    Arc::new(polaris_mcp::store::Secrets::new(1, Default::default()).unwrap()),
                    Default::default(),
                )
                .unwrap(),
            );
            let factory = ConfiguredRunFactory::new(
                f.bootstrap_with_tier(tier),
                grant,
                TrustedBackend::Codex {
                    accounts: None,
                    tokens: f.tokens.clone(),
                },
                resources,
            )
            .unwrap();
            let target = f.accept();
            assert_eq!(
                factory.account_models().contains(&MCP_SELECTION_MODEL),
                !local_only
            );
            assert!(
                factory
                    .account_models()
                    .contains(&factory.bootstrap.model())
            );
            factory.stage_workspace(f.workspace()).unwrap();
            let input = factory
                .prepare(&target, &f.writer.snapshot().unwrap())
                .unwrap();
            assert_eq!(input.external_ceiling, ceiling);
            let mcp = input.mcp.unwrap();
            assert_eq!(mcp.config.project_id(), "p");
            assert_eq!(
                mcp.selection_provider.is_some(),
                !local_only,
                "only explicit local-only suppresses the remote auxiliary selector"
            );
            if tier != BootstrapTier::ReadCreate {
                assert!(matches!(
                input.execution_capability,
                Some(
                    polaris_core::desktop_execution::ConfirmedExecutionCapability::ConfinedCode { .. }
                )
            ));
            }
            assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn mcp_configuration_for_another_project_is_rejected_before_authentication() {
    let f = Fixture::new();
    let mut resources = f.resources();
    resources.mcp = Some(
        polaris_core::mcp::runtime::RuntimeConfig::new(
            polaris_mcp::registration::Registry {
                schema_version: 1,
                revision: 1,
                servers: vec![],
            },
            "other-project".into(),
            Arc::new(polaris_mcp::store::Secrets::new(1, Default::default()).unwrap()),
            Default::default(),
        )
        .unwrap(),
    );
    assert!(matches!(
        ConfiguredRunFactory::new(
            f.bootstrap(),
            f.grant(),
            TrustedBackend::Codex {
                accounts: None,
                tokens: f.tokens.clone()
            },
            resources,
        ),
        Err(FactoryError::Binding)
    ));
    assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
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
            None,
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
                        endpoint: Some(endpoint),
                        effort: None,
                        model: "qwen:small".into(),
                        observed_tool_support: polaris_core::desktop_store::ToolSupport::Unknown,
                    },
                ])
                .unwrap(),
            ),
            None,
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
                accounts: None,
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
                accounts: None,
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
            accounts: None,
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

#[test]
fn configured_factory_structured_history_prepares_dedicated_summary_without_auth() {
    let mut f = Fixture::with_mode(polaris_desktop_protocol::snapshot::HistoryMode::Structured10);
    let mut resources = f.resources();
    resources.history = Some(TrustedMemoryIndex {
        embedder: polaris_core::conversation_memory::LocalStdioEmbedder::new(
            f.base.join("unused-python"),
            f.base.join("unused-helper"),
            f.base.join("unused-model"),
            f.base.join("unused-runtime"),
            "fixture-revision".into(),
            1,
            std::time::Duration::from_secs(1),
        )
        .unwrap(),
        database: f.base.join("unused-index.sqlite3"),
        identity: polaris_core::desktop_store::MemoryResources {
            embedding_model: "intfloat/multilingual-e5-small".into(),
            embedding_revision: "fixture-revision".into(),
            embedding_dimension: 384,
            fingerprint: "a".repeat(64),
        },
    });
    let factory = ConfiguredRunFactory::new(
        f.bootstrap(),
        f.grant(),
        TrustedBackend::Codex {
            accounts: None,
            tokens: f.tokens.clone(),
        },
        resources,
    )
    .unwrap();
    assert!(factory.summary_provider.is_some());
    factory.stage_workspace(f.workspace()).unwrap();
    let target = f.accept();
    let saved = f.writer.snapshot().unwrap();
    let inputs = factory.prepare(&target, &saved).unwrap();
    let history = inputs
        .history
        .as_ref()
        .expect("structured history resources must not be discarded by zip");
    assert!(!Arc::ptr_eq(&history.summary_provider, &inputs.provider));
    assert_eq!(history.database, f.base.join("unused-index.sqlite3"));
    assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
    assert!(!history.database.exists());
}

#[test]
fn a_run_bound_to_a_registration_never_falls_back_to_the_single_authentication() {
    let mut f = Fixture::new();
    let factory = ConfiguredRunFactory::new(
        f.bootstrap(),
        f.grant(),
        TrustedBackend::Codex {
            accounts: None,
            tokens: f.tokens.clone(),
        },
        f.resources(),
    )
    .unwrap();
    factory.stage_workspace(f.workspace()).unwrap();
    let (target, saved) = f.accept_bound("reg-a");
    // No accounts owner can resolve the binding, so the run cannot send at all.
    assert!(factory.prepare(&target, &saved).is_err());
    assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
    let _ = f._temp.keep();
}

#[test]
fn a_factory_without_accounts_binds_no_registration_at_acceptance() {
    let f = Fixture::new();
    let factory = ConfiguredRunFactory::new(
        f.bootstrap(),
        f.grant(),
        TrustedBackend::Codex {
            accounts: None,
            tokens: f.tokens.clone(),
        },
        f.resources(),
    )
    .unwrap();
    assert_eq!(factory.bind_account().unwrap(), None);
    let _ = f._temp.keep();
}

#[test]
fn account_backed_roles_and_weights_are_built_on_the_registration_the_run_holds() {
    use polaris_core::desktop_store::{
        SavedRoleBinding, SavedRoleBindings, SavedRuntime, SavedTier, SavedTierBinding,
        SavedTierBindings, ToolSupport,
    };
    let mut f = Fixture::new();
    let catalog = f.resources().agent_types;
    f.writer
        .configure_role_bindings(
            DecimalU64::new(0),
            Some(
                SavedRoleBindings::new(vec![SavedRoleBinding {
                    role: "child".into(),
                    runtime: SavedRuntime::Account,
                    // No endpoint: the account is reached through the
                    // registration, not through an origin the user gave.
                    endpoint: None,
                    effort: Some("high".into()),
                    model: "gpt-5.6-terra".into(),
                    observed_tool_support: ToolSupport::Supported,
                }])
                .unwrap(),
            ),
            Some(
                SavedTierBindings::new(vec![SavedTierBinding {
                    tier: SavedTier::Dwarf,
                    runtime: SavedRuntime::Account,
                    endpoint: None,
                    model: "gpt-5.4-mini".into(),
                    effort: Some("low".into()),
                }])
                .unwrap(),
            ),
            &catalog,
        )
        .unwrap();
    let target = f.accept_named("-account-role");
    let saved = f.writer.snapshot().unwrap();
    let factory = f.factory();
    factory.stage_workspace(f.workspace()).unwrap();
    let inputs = factory.prepare(&target, &saved).unwrap();
    assert!(inputs.provider.resolve_role("child").unwrap().is_some());
    assert!(inputs.provider.resolve_role("missing").is_err());
    assert!(inputs.provider.resolve_tier("dwarf").unwrap().is_some());
    assert!(
        inputs
            .provider
            .resolve_tier("supergiant")
            .unwrap()
            .is_none(),
        "an unpriced weight names nothing and the caller falls back"
    );
    // Building the providers must not have sent anything: a model name is
    // what the account is asked to answer under, and whether it will is
    // settled by the first real request.
    assert_eq!(f.tokens.0.load(Ordering::SeqCst), 0);
}

#[test]
fn a_weight_priced_without_any_role_still_routes() {
    use polaris_core::desktop_store::{
        SavedRuntime, SavedTier, SavedTierBinding, SavedTierBindings,
    };
    let mut f = Fixture::new();
    let catalog = f.resources().agent_types;
    f.writer
        .configure_role_bindings(
            DecimalU64::new(0),
            None,
            Some(
                SavedTierBindings::new(vec![SavedTierBinding {
                    tier: SavedTier::Supergiant,
                    runtime: SavedRuntime::Account,
                    endpoint: None,
                    model: "gpt-6-astra".into(),
                    effort: None,
                }])
                .unwrap(),
            ),
            &catalog,
        )
        .unwrap();
    let target = f.accept_named("-weight-only");
    let saved = f.writer.snapshot().unwrap();
    let factory = f.factory();
    factory.stage_workspace(f.workspace()).unwrap();
    let inputs = factory.prepare(&target, &saved).unwrap();
    assert!(
        inputs
            .provider
            .resolve_tier("supergiant")
            .unwrap()
            .is_some()
    );
    // Pricing a weight is not binding a role, so an unbound role is still
    // refused exactly as it was before.
    assert!(inputs.provider.resolve_role("child").is_err());
}

#[test]
fn a_type_that_needs_tools_is_not_bound_to_a_model_nobody_saw_supporting_them() {
    use polaris_core::desktop_store::{
        SavedRoleBinding, SavedRoleBindings, SavedRuntime, ToolSupport,
    };
    // One run per case: a fixture accepts a single run, and what is being
    // compared is what preparation does with each saved observation.
    let prepared = |support| {
        let mut f = Fixture::new();
        f.child_tools = vec!["read".into()];
        let catalog = f.resources().agent_types;
        f.writer
            .configure_role_bindings(
                DecimalU64::new(0),
                Some(
                    SavedRoleBindings::new(vec![SavedRoleBinding {
                        role: "child".into(),
                        runtime: SavedRuntime::Account,
                        endpoint: None,
                        effort: None,
                        model: "gpt-5.6-terra".into(),
                        observed_tool_support: support,
                    }])
                    .unwrap(),
                ),
                None,
                &catalog,
            )
            .unwrap();
        let target = f.accept_named("-tools");
        let saved = f.writer.snapshot().unwrap();
        let factory = f.factory();
        factory.stage_workspace(f.workspace()).unwrap();
        factory.prepare(&target, &saved).map(|_| ())
    };
    // Unknown is not the same as supported. The observation is the
    // settings screen's, recorded when the binding was saved, and a run
    // must not assume what nobody looked at.
    for refused in [ToolSupport::Unknown, ToolSupport::Unsupported] {
        assert!(
            prepared(refused).is_err(),
            "{refused:?} must not bind a type that needs tools"
        );
    }
    assert!(prepared(ToolSupport::Supported).is_ok());
}

#[tokio::test]
async fn decision_status_preserves_unavailable_configuration_and_observed_capacity_without_auth() {
    use polaris_provider::decision::{
        Decider, Decision, DecisionError, DecisionMeter, DecisionObservation, Question,
    };
    struct CapacityJudge;
    #[async_trait::async_trait]
    impl Decider for CapacityJudge {
        async fn decide(
            &self,
            _: &serde_json::Value,
            _: &std::collections::BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            Err(DecisionError::Unavailable)
        }
        async fn observe(
            &self,
            _: &serde_json::Value,
            _: &std::collections::BTreeMap<String, Question>,
        ) -> DecisionObservation {
            let mut observed = DecisionObservation::single(
                self.identity(),
                self.capabilities(),
                Err(DecisionError::Unavailable),
                std::time::Duration::ZERO,
            );
            observed.attempts[0].invoked = false;
            observed.attempts[0].capacity = Some(polaris_provider::decision::DecisionCapacity {
                status: "unverified",
                reason: Some("ColocatedModels".into()),
                budget_bytes: None,
                required_bytes: None,
                colocated_bytes: None,
            });
            observed
        }
    }
    let fixture = Fixture::new();
    let mut factory = fixture.factory();
    factory.resources.decision_config.enabled = true;
    factory.resources.decision_config.backend = polaris_core::config::DecisionBackend::Clef;
    factory.resources.decision_config.model = Some("synthetic-uninstalled-profile".into());
    let unavailable = factory.decision_status().unwrap().join("\n");
    assert!(unavailable.contains("synthetic-uninstalled-profile"));
    assert!(unavailable.contains("構築不可"));
    let meter = DecisionMeter::default();
    let judge = meter.wrap(Arc::new(CapacityJudge));
    factory.resources.decider = Some(judge.clone());
    let before = factory.decision_status().unwrap().join("\n");
    assert!(before.contains("観測なし"));
    assert!(meter.recent().is_empty());
    judge
        .observe(&serde_json::Value::Null, &Default::default())
        .await;
    let after = factory.decision_status().unwrap().join("\n");
    assert!(after.contains("会話別の使用量ではありません"));
    assert!(after.contains("ColocatedModels"));
    assert!(after.contains("呼出=false"));
    assert_eq!(meter.recent().len(), 1);
    assert_eq!(fixture.tokens.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn outside_run_decider_binds_startup_resources_without_masking_prepared_run_context() {
    use polaris_provider::decision::{
        Decider, Decision, DecisionContext, DecisionError, DecisionObservation, Question,
    };
    struct ContextJudge(Mutex<Vec<Option<u64>>>);
    #[async_trait::async_trait]
    impl Decider for ContextJudge {
        async fn decide(
            &self,
            _: &serde_json::Value,
            _: &std::collections::BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            panic!("unbound context path")
        }
        async fn observe_with_context(
            &self,
            _: &serde_json::Value,
            _: &std::collections::BTreeMap<String, Question>,
            context: DecisionContext,
        ) -> DecisionObservation {
            self.0
                .lock()
                .unwrap()
                .push(context.planned_local_model_bytes);
            DecisionObservation::single(
                self.identity(),
                self.capabilities(),
                Err(DecisionError::Unavailable),
                std::time::Duration::ZERO,
            )
        }
    }
    struct Model(Option<u64>);
    #[async_trait::async_trait]
    impl Provider for Model {
        fn planned_local_model_bytes(&self) -> Option<u64> {
            self.0
        }
        async fn complete(
            &self,
            _: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, ProviderError> {
            panic!("capacity snapshot dispatched a provider")
        }
    }
    let mut fixture = Fixture::new();
    let mut factory = fixture.factory();
    let published = fixture.writer.snapshot().unwrap();
    let judge = Arc::new(ContextJudge(Mutex::new(Vec::new())));
    factory.resources.decider = Some(judge.clone());
    let ask = |bytes| DecisionContext {
        planned_local_model_bytes: bytes,
    };
    factory
        .outside_run_decider(&published)
        .unwrap()
        .observe_with_context(
            &serde_json::Value::Null,
            &Default::default(),
            ask(Some(999)),
        )
        .await;
    assert_eq!(judge.0.lock().unwrap().last(), Some(&Some(0)));
    factory.selection_provider = Some(Arc::new(Model(Some(7))));
    factory
        .outside_run_decider(&published)
        .unwrap()
        .observe(&serde_json::Value::Null, &Default::default())
        .await;
    assert_eq!(judge.0.lock().unwrap().last(), Some(&Some(7)));
    use polaris_core::desktop_store::{
        SavedRoleBinding, SavedRoleBindings, SavedRuntime, ToolSupport,
    };
    let roles = |runtime| {
        SavedRoleBindings::new(vec![SavedRoleBinding {
            role: "child".into(),
            runtime,
            endpoint: if runtime == SavedRuntime::Account {
                None
            } else {
                Some("http://127.0.0.1:11434".into())
            },
            model: "fixture".into(),
            effort: None,
            observed_tool_support: ToolSupport::Supported,
        }])
        .unwrap()
    };
    let mut role_snapshot = published.clone();
    role_snapshot.state.role_bindings = Some(roles(SavedRuntime::Account));
    factory
        .outside_run_decider(&role_snapshot)
        .unwrap()
        .observe(&serde_json::Value::Null, &Default::default())
        .await;
    assert_eq!(judge.0.lock().unwrap().last(), Some(&Some(7)));
    role_snapshot.state.role_bindings = Some(roles(SavedRuntime::Ollama));
    factory
        .outside_run_decider(&role_snapshot)
        .unwrap()
        .observe(&serde_json::Value::Null, &Default::default())
        .await;
    assert_eq!(
        judge.0.lock().unwrap().last(),
        Some(&None),
        "planned local role was treated as zero"
    );
    fixture.accept();
    let mut past = fixture.writer.snapshot().unwrap();
    past.state.runs[0].role_bindings = Some(roles(SavedRuntime::Ollama));
    factory
        .outside_run_decider(&past)
        .unwrap()
        .observe(&serde_json::Value::Null, &Default::default())
        .await;
    assert_eq!(
        judge.0.lock().unwrap().last(),
        Some(&None),
        "past local role was assumed unloaded"
    );
    let mut changed = published.clone();
    changed.state.configuration.model = "different".into();
    assert!(factory.outside_run_decider(&changed).is_none());
    factory.resources.history = Some(TrustedMemoryIndex {
        embedder: polaris_core::conversation_memory::LocalStdioEmbedder::new(
            fixture.base.join("unused-python"),
            fixture.base.join("unused-helper"),
            fixture.base.join("unused-model"),
            fixture.base.join("unused-runtime"),
            "fixture-revision".into(),
            1,
            std::time::Duration::from_secs(1),
        )
        .unwrap(),
        database: fixture.base.join("unused-index.sqlite3"),
        identity: polaris_core::desktop_store::MemoryResources {
            embedding_model: "intfloat/multilingual-e5-small".into(),
            embedding_revision: "fixture-revision".into(),
            embedding_dimension: 384,
            fingerprint: "a".repeat(64),
        },
    });
    factory
        .outside_run_decider(&published)
        .unwrap()
        .observe(&serde_json::Value::Null, &Default::default())
        .await;
    assert_eq!(
        judge.0.lock().unwrap().last(),
        Some(&None),
        "unknown embedding peak was treated as zero"
    );
    assert!(!fixture.base.join("unused-runtime").exists());
    factory.resources.history = None;
    factory.primary_provider = Arc::new(Model(None));
    factory
        .outside_run_decider(&published)
        .unwrap()
        .observe(&serde_json::Value::Null, &Default::default())
        .await;
    assert_eq!(judge.0.lock().unwrap().last(), Some(&None));
    factory
        .decider()
        .unwrap()
        .observe_with_context(&serde_json::Value::Null, &Default::default(), ask(Some(37)))
        .await;
    assert_eq!(
        judge.0.lock().unwrap().last(),
        Some(&Some(37)),
        "outside-run sources replaced the prepared run plan"
    );
    assert_eq!(fixture.tokens.0.load(Ordering::SeqCst), 0);
}
