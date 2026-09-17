//! Trusted factory injection through the real reactor, with no live provider.
use super::*;
use polaris_core::mcp::{operation::ExternalCeiling, runtime::RuntimeConfig};
use polaris_desktop_protocol::request::McpPromptChoice;
use polaris_mcp::{
    catalog::index::{IndexedTool, ServerIndex, summary_hash},
    registration::{Registry, canonical_hash},
    store::{Secrets, Store},
};

use crate::engine::mcp_fixture as mcp_server;

struct WithMcp {
    inner: Arc<dyn TrustedRunFactory>,
    config: RuntimeConfig,
    selection_provider: Option<Arc<dyn Provider>>,
}
impl TrustedRunFactory for WithMcp {
    fn workflow_config(&self) -> Option<&polaris_core::workflow::WorkflowConfig> {
        self.inner.workflow_config()
    }
    fn workflow_skills(&self) -> &[polaris_skills::Skill] {
        self.inner.workflow_skills()
    }
    fn role_catalog(&self) -> &[polaris_skills::AgentType] {
        self.inner.role_catalog()
    }
    fn prepare(
        &self,
        target: &RunTarget,
        published: &Published,
    ) -> Result<TrustedRunInputs, ServiceError> {
        let mut input = self.inner.prepare(target, published)?;
        input.mcp = Some(TrustedMcpResources {
            config: self.config.clone(),
            selection_provider: self.selection_provider.clone(),
        });
        // Host-selected prompts are read capability, not tool authorization.
        input.external_ceiling = ExternalCeiling::Denied;
        Ok(input)
    }
    fn completed(&self, receipt: TrustedRunCompletion) {
        self.inner.completed(receipt);
    }
}

fn inject(engine: &mut Engine, project: &str, servers: Vec<polaris_mcp::registration::Server>) {
    inject_config(
        engine,
        RuntimeConfig::new(
            Registry {
                schema_version: 1,
                revision: 1,
                servers,
            },
            project.into(),
            Arc::new(Secrets::new(1, Default::default()).unwrap()),
            Default::default(),
        )
        .unwrap(),
    );
}

fn inject_config(engine: &mut Engine, config: RuntimeConfig) {
    inject_config_with_selector(engine, config, None);
}

fn inject_config_with_selector(
    engine: &mut Engine,
    config: RuntimeConfig,
    selection_provider: Option<Arc<dyn Provider>>,
) {
    let trusted = engine.trusted_runs.as_mut().unwrap();
    trusted.factory = Arc::new(WithMcp {
        inner: trusted.factory.clone(),
        config,
        selection_provider,
    });
}

fn attach_catalog_index(fixture: &mut Fixture, server: &polaris_mcp::registration::Server) {
    let home = fixture._temp.path().join("mcp");
    std::fs::create_dir(&home).unwrap();
    let home = home.canonicalize().unwrap();
    let store = Store::open(&home).unwrap();
    let registry = Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![server.clone()],
    };
    store.save_registry(0, &registry).unwrap();
    let description = "";
    let input_hash = canonical_hash(&serde_json::json!({"type":"object"})).unwrap();
    let schema_hash = canonical_hash(&serde_json::json!([input_hash])).unwrap();
    store
        .save_catalog_entry(
            "fixture",
            ServerIndex {
                destination_hash: server.destination_hash().unwrap(),
                partial: false,
                tools: vec![IndexedTool {
                    key: "fixture".into(),
                    name: "fixture".into(),
                    description: description.into(),
                    summary_hash: summary_hash("fixture", "fixture", "fixture", description)
                        .unwrap(),
                    schema_hash,
                }],
            },
        )
        .unwrap();
    fixture
        .service
        .attach_mcp_settings(crate::engine::McpSettings {
            store,
            home,
            active_registry_revision: 1,
            active_secrets_revision: 0,
            runtime_roots: vec![],
            excluded_roots: vec![],
            limits: polaris_core::isolated_workspace::Limits {
                max_entries: 8,
                max_files: 4,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 2,
            },
        });
}

struct SelectorProvider {
    calls: Arc<std::sync::Mutex<Vec<polaris_provider::CompletionRequest>>>,
}

fn fixture_tool_alias() -> String {
    polaris_mcp::catalog::tool_alias(&polaris_mcp::catalog::ItemId {
        server_id: "fixture".into(),
        kind: polaris_mcp::catalog::Kind::Tool,
        key: "fixture".into(),
    })
    .unwrap()
}

#[async_trait::async_trait]
impl Provider for SelectorProvider {
    async fn complete(
        &self,
        request: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        self.calls.lock().unwrap().push(request);
        Ok(polaris_provider::CompletionResponse {
            text: r#"{"tools":[0]}"#.into(),
            usage: Some(polaris_provider::Usage {
                input_tokens: 2,
                output_tokens: 3,
                total_tokens: 5,
                cached_tokens: 0,
            }),
            ..Default::default()
        })
    }
}

fn choice() -> McpPromptChoice {
    McpPromptChoice {
        server: "fixture".into(),
        prompt: "prompt".into(),
        arguments: Default::default(),
    }
}

async fn finish(engine: &mut Engine) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            engine.step_real(None, false).unwrap();
            if engine.active.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("MCP reactor did not finish");
}

#[tokio::test]
async fn selected_prompt_reaches_provider_once_and_saved_raw_survives_reopen() {
    let (mut server, methods, server_task) = mcp_server::server_fixture("").await;
    server.enabled_projects = [project_id().as_str().to_owned()].into();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut f = fixture_with_history(
        Behavior::Complete,
        false,
        None,
        Some(Arc::new(RecordingProvider {
            calls: requests.clone(),
            summary: false,
            fail: false,
            hold: false,
            read_source: false,
        })),
    );
    let engine = f.service.engine.as_mut().unwrap();
    inject(engine, project_id().as_str(), vec![server]);
    let accepted = start_with_prompt(engine, Some(choice()));
    finish(engine).await;
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    assert_eq!(saved.raw.len(), 3);
    assert_eq!(saved.raw[0].message.content, "fixture input");
    assert!(saved.raw[1].message.content.contains("selected prompt"));
    assert_eq!(saved.raw[2].message.content, "strict raw completion");
    assert_eq!(saved.raw[2].message.role, Role::Assistant);
    let prompt = saved.state.runs[0]
        .operations
        .iter()
        .find(|o| o.prompt.is_some())
        .unwrap();
    assert!(prompt.result_id.is_some());
    assert_eq!(f.completions.lock().unwrap().len(), 1);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages.len(), 1);
        assert_eq!(requests[0].messages[0].role, Role::User);
        assert!(requests[0].messages[0].content.contains("fixture input"));
        assert!(requests[0].messages[0].content.contains("selected prompt"));
    }
    let observed = methods.lock().unwrap().clone();
    assert_eq!(
        observed
            .iter()
            .filter(|m| m.as_str() == "prompts/get")
            .count(),
        1
    );
    assert!(!observed.iter().any(|m| m == "tools/call"));
    engine.handle(&accepted, &mut vec![]).unwrap();
    finish(engine).await;
    assert_eq!(engine.store.snapshot().unwrap().marker, saved.marker);
    assert_eq!(f.preparations.load(Ordering::SeqCst), 1);
    drop(f.service);
    let reopened = DesktopService::open(f.config).unwrap();
    let after = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
    assert_eq!(after.marker, saved.marker);
    assert_eq!(
        serde_json::to_vec(&after.raw).unwrap(),
        serde_json::to_vec(&saved.raw).unwrap()
    );
    assert_eq!(*methods.lock().unwrap(), observed);
    assert_eq!(requests.lock().unwrap().len(), 1);
    server_task.abort();
    assert!(server_task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn indexed_selector_is_metered_once_audited_and_exposes_its_tool_to_main() {
    let (mut server, _methods, server_task) = mcp_server::server_fixture("").await;
    server.enabled_projects = [project_id().as_str().to_owned()].into();
    let main_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let selector_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut f = fixture_with_history(
        Behavior::Complete,
        false,
        None,
        Some(Arc::new(RecordingProvider {
            calls: main_calls.clone(),
            summary: false,
            fail: false,
            hold: false,
            read_source: false,
        })),
    );
    attach_catalog_index(&mut f, &server);
    let saved = {
        let engine = f.service.engine.as_mut().unwrap();
        inject_config_with_selector(
            engine,
            RuntimeConfig::new(
                Registry {
                    schema_version: 1,
                    revision: 1,
                    servers: vec![server],
                },
                project_id().as_str().into(),
                Arc::new(Secrets::new(1, Default::default()).unwrap()),
                Default::default(),
            )
            .unwrap(),
            Some(Arc::new(SelectorProvider {
                calls: selector_calls.clone(),
            })),
        );
        start(engine);
        finish(engine).await;
        engine.store.snapshot().unwrap()
    };
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    assert_eq!(selector_calls.lock().unwrap().len(), 1);
    {
        let main = main_calls.lock().unwrap();
        assert_eq!(main.len(), 1);
        assert_eq!(
            main[0]
                .tools
                .iter()
                .filter(|tool| tool.name == fixture_tool_alias())
                .count(),
            1,
            "selected MCP tool reaches main provider"
        );
    }
    let usage = saved.state.runs[0].usage.unwrap();
    assert_eq!(usage.reported_responses, 2);
    assert_eq!(usage.usage.input_tokens, 13);
    assert_eq!(usage.usage.output_tokens, 7);
    assert_eq!(usage.usage.total_tokens, 20);
    assert_eq!(usage.usage.cached_tokens, 0);
    let audit = std::fs::read_to_string(f._temp.path().join("audit.jsonl")).unwrap();
    assert!(audit.contains("mcp-preselection"));
    assert!(audit.contains("selected"));
    server_task.abort();
    assert!(server_task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn local_backend_keeps_a_valid_index_inactive_and_audits_unsupported_backend() {
    let (mut server, methods, server_task) = mcp_server::server_fixture("").await;
    server.enabled_projects = [project_id().as_str().to_owned()].into();
    let main_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut f = fixture_with_history(
        Behavior::Complete,
        false,
        None,
        Some(Arc::new(RecordingProvider {
            calls: main_calls.clone(),
            summary: false,
            fail: false,
            hold: false,
            read_source: false,
        })),
    );
    attach_catalog_index(&mut f, &server);
    let saved = {
        let engine = f.service.engine.as_mut().unwrap();
        inject(engine, project_id().as_str(), vec![server]);
        start(engine);
        finish(engine).await;
        engine.store.snapshot().unwrap()
    };
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    {
        let main = main_calls.lock().unwrap();
        assert_eq!(main.len(), 1);
        assert!(
            !main[0]
                .tools
                .iter()
                .any(|tool| tool.name == fixture_tool_alias()),
            "local backend receives no automatic tool"
        );
    }
    assert!(methods.lock().unwrap().is_empty());
    let usage = saved.state.runs[0].usage.unwrap();
    assert_eq!(usage.reported_responses, 1);
    assert_eq!(usage.usage.total_tokens, 15);
    let audit = std::fs::read_to_string(f._temp.path().join("audit.jsonl")).unwrap();
    assert!(audit.contains("mcp-preselection"));
    assert!(audit.contains("unsupported_backend"));
    server_task.abort();
    assert!(server_task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn configured_server_without_selection_stays_lazy_and_preserves_plain_input() {
    let (mut server, methods, server_task) = mcp_server::server_fixture("").await;
    server.enabled_projects = [project_id().as_str().to_owned()].into();
    let mut f = fixture(Behavior::Complete, false);
    let engine = f.service.engine.as_mut().unwrap();
    inject(engine, project_id().as_str(), vec![server]);
    start(engine);
    finish(engine).await;
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Succeeded
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert!(methods.lock().unwrap().is_empty());
    server_task.abort();
    assert!(server_task.await.unwrap_err().is_cancelled());
}

#[test]
fn mismatched_project_configuration_fails_before_provider_and_never_replays() {
    let mut f = fixture(Behavior::Complete, false);
    let engine = f.service.engine.as_mut().unwrap();
    inject(engine, "another-project", vec![]);
    let accepted = start(engine);
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].run.state, RunState::Failed);
    assert_eq!(saved.raw.len(), 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    engine.handle(&accepted, &mut vec![]).unwrap();
    engine.step_real(None, false).unwrap();
    assert!(engine.active.is_none());
    assert_eq!(engine.store.snapshot().unwrap().marker, saved.marker);
    assert_eq!(f.preparations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stdio_scratch_outlives_its_child_until_the_disconnected_owner_finishes() {
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let mut f = fixture(Behavior::Complete, false);
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap().join("server");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o700)).unwrap();
    let registry = Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![polaris_mcp::registration::Server {
            server_id: "fixture".into(),
            enabled_projects: [project_id().as_str().to_owned()].into(),
            transport: polaris_mcp::registration::Destination::Stdio {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "printf ok > \"$HOME/marker\"; printf ok > \"$TMPDIR/marker\"; printf '%s\\n%s\\n' \"$HOME\" \"$$\" > started; exec /bin/sleep 60".into()],
                cwd: cwd.clone(),
                sandbox_profile: "workspace-write".into(),
                secret_env: Default::default(),
                readable_roots: vec![],
                executable_sha256: None,
                script: None,
                script_sha256: None,
            },
            protocol_profile: Default::default(),
            capabilities: polaris_mcp::registration::Capabilities {
                catalog_metadata: true,
                prompt_selection: true,
                ..Default::default()
            },
            timeouts: polaris_mcp::registration::Timeouts {
                connect_seconds: 60,
                connect_retries: 0,
                ..Default::default()
            },
        }],
    };
    let config = RuntimeConfig::prepare_registered(
        polaris_mcp::store::Snapshot {
            registry: Some(registry),
            secrets: None,
        },
        project_id().as_str().into(),
        std::path::Path::new("/var/empty/polaris-test-home"),
        &["/bin", "/usr/bin", "/usr/lib", "/System/Library"].map(std::path::PathBuf::from),
        std::slice::from_ref(&f.config.store_root),
        polaris_core::isolated_workspace::Limits {
            max_entries: 8,
            max_files: 4,
            max_file_bytes: 1024,
            max_total_bytes: 4096,
            max_depth: 2,
        },
    )
    .unwrap()
    .unwrap();
    let engine = f.service.engine.as_mut().unwrap();
    inject_config(engine, config);
    start_with_prompt(engine, Some(choice()));
    until(engine, |_| {
        std::fs::read_to_string(cwd.join("started")).is_ok_and(|s| s.lines().count() == 2)
    });
    let started = std::fs::read_to_string(cwd.join("started")).unwrap();
    let (home, pid) = started.split_once('\n').unwrap();
    let scratch = std::path::Path::new(home).parent().unwrap().to_owned();
    let pid: libc::pid_t = pid.trim().parse().unwrap();

    let active = engine.active.as_mut().unwrap();
    active.execution.invalidate_after_intent = Some(false);
    let _read = active
        .execution
        .port
        .submit_confined_read(
            &polaris_core::desktop_execution::ConfinedReadRequest::DiffBefore {
                path: "dummy".into(),
            },
        )
        .unwrap();
    until(engine, |e| {
        e.active
            .as_ref()
            .unwrap()
            .execution
            .cleanup_complete_for_test()
    });
    // Synthetic unconfirmed cleanup; the MCP child itself is a real native process.
    let hold = Release(
        engine
            .active
            .as_mut()
            .unwrap()
            .execution
            .inject_unconfirmed_cleanup(),
    );

    engine.failed = true;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while engine
        .active
        .as_ref()
        .unwrap()
        .real
        .as_ref()
        .unwrap()
        .worker
        .is_some()
    {
        assert!(engine.settle_without_output().is_err());
        assert!(
            std::time::Instant::now() < deadline,
            "run worker did not join"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(engine.settle_without_output().is_err());
    assert!(!engine.ready && engine.active.is_some());
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert!(scratch.join("home/marker").is_file() && scratch.join("tmp/marker").is_file());
    engine.failed = false;

    let (_client, reader) = tokio::io::duplex(1024);
    let mut serving = Box::pin(f.service.serve(reader, tokio::io::sink()));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut serving)
            .await
            .is_err()
    );
    drop(serving);
    assert!(f.service.engine.is_none());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(matches!(
        DesktopService::open(f.config.clone()),
        Err(ServiceError::Store(StoreError::Busy))
    ));
    assert!(scratch.join("home/marker").is_file() && scratch.join("tmp/marker").is_file());

    drop(hold);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let reopened = loop {
        match DesktopService::open(f.config.clone()) {
            Ok(service) => break service,
            Err(ServiceError::Store(StoreError::Busy)) => {
                assert!(std::time::Instant::now() < deadline, "owner did not finish");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("unexpected fixture reopen: {error}"),
        }
    };
    while scratch.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "scratch was not reclaimed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let saved = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
    assert!(saved.state.runs[0].run.state.is_terminal());
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}
