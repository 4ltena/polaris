//! Shared CLI/TUI preparation with synthetic stores and providers, no network.
use super::*;
use crate::mcp::preselection::Status;
use polaris_mcp::catalog::index::{IndexedTool, ServerIndex, summary_hash};
use polaris_mcp::registration::{Registry, Server};
use polaris_provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, Usage, UsageMeter,
};
use std::sync::{Arc, Mutex, MutexGuard};

// Protect process-wide MCP registration state from fixture teardown in
// concurrent local tests. Keep the temporary tree until the guard drops.
static LOCAL_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

struct LockedFixtureTemp {
    _temp: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
}

struct Selector {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    reply: &'static str,
    hold: bool,
}

#[async_trait::async_trait]
impl Provider for Selector {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        if self.hold {
            return std::future::pending().await;
        }
        Ok(CompletionResponse {
            text: self.reply.into(),
            usage: Some(Usage {
                input_tokens: 7,
                output_tokens: 11,
                total_tokens: 18,
                cached_tokens: 0,
            }),
            ..Default::default()
        })
    }
}

fn server(id: &str, project: &str) -> Server {
    serde_json::from_value(serde_json::json!({
        "server_id":id, "enabled_projects":[project],
        "transport":{"kind":"streamable_http","url":"http://127.0.0.1:12345/mcp","secret_headers":{}},
        "capabilities":{"catalog_metadata":true,"tool_call":true,"resource_read":false,"prompt_selection":false}
    })).unwrap()
}

fn save_index(store: &Store, server: &Server) {
    store
        .save_catalog_entry(
            &server.server_id,
            ServerIndex {
                destination_hash: server.destination_hash().unwrap(),
                partial: false,
                tools: vec![IndexedTool {
                    key: "lookup".into(),
                    name: "lookup".into(),
                    description: "look up data".into(),
                    summary_hash: summary_hash(
                        &server.server_id,
                        "lookup",
                        "lookup",
                        "look up data",
                    )
                    .unwrap(),
                    schema_hash: "a".repeat(64),
                }],
            },
        )
        .unwrap();
}

fn fixture() -> (LockedFixtureTemp, LocalHost, Registry) {
    let guard = LOCAL_FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let store = Store::open(&home).unwrap();
    let registry = Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![
            server("current", "project"),
            server("other", "other-project"),
        ],
    };
    store.save_registry(0, &registry).unwrap();
    for server in &registry.servers {
        save_index(&store, server);
    }
    (
        LockedFixtureTemp {
            _temp: temp,
            _guard: guard,
        },
        LocalHost {
            store: Some(store),
            home: home.clone(),
            root: home.join("project"),
            project: "project".into(),
            runtime_roots: vec![],
            excluded_roots: vec![],
        },
        registry,
    )
}

fn runtime(host: &LocalHost) -> RunRuntime {
    host.runtime("session", "run", ExternalCeiling::Denied)
        .unwrap()
        .unwrap()
        .0
}

fn selector(
    reply: &'static str,
    hold: bool,
) -> (Arc<dyn Provider>, Arc<Mutex<Vec<CompletionRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        Arc::new(Selector {
            requests: requests.clone(),
            reply,
            hold,
        }),
        requests,
    )
}

#[tokio::test]
async fn local_preselection_filters_projects_and_is_metered_once_inside_and_outside_the_turn() {
    let (_temp, host, _) = fixture();
    let mut run = runtime(&host);
    let (provider, requests) = selector(r#"{"tools":[0]}"#, false);
    let outer = UsageMeter::default();
    host.configure_preselection(&mut run, Some(Arc::new(outer.wrap(provider))), None);
    let inner = UsageMeter::default();
    assert_eq!(
        run.prepare_local_preselection("lookup this", "implement", &inner)
            .await,
        Some(Status::Selected)
    );
    assert_eq!(
        run.prepare_local_preselection("different task", "verify", &inner)
            .await,
        None
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&requests[0].messages[0].content).unwrap();
    assert_eq!(body["task"], "lookup this");
    assert_eq!(body["phase"], "implement");
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["tools"][0]["server"], "current");
    assert_eq!(inner.snapshot().usage.total_tokens, 18);
    assert_eq!(inner.snapshot().reported_responses, 1);
    assert_eq!(inner.snapshot(), outer.snapshot());
}

#[tokio::test]
async fn missing_selector_and_invalid_reply_keep_closed_reasons_and_known_usage() {
    let (_temp, host, _) = fixture();
    let mut run = runtime(&host);
    host.configure_preselection(&mut run, None, None);
    let meter = UsageMeter::default();
    assert_eq!(
        run.prepare_local_preselection("lookup", "", &meter).await,
        Some(Status::UnsupportedBackend)
    );
    assert_eq!(meter.snapshot(), Default::default());
    // The primary request can still be built without automatic tool schemas.
    assert!(run.freeze().unwrap().specs().is_empty());

    let mut run = runtime(&host);
    let (provider, requests) = selector("not json", false);
    host.configure_preselection(&mut run, Some(provider), None);
    assert_eq!(
        run.prepare_local_preselection("lookup", "", &meter).await,
        Some(Status::InvalidOutput)
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(meter.snapshot().usage.total_tokens, 18);
    assert_eq!(meter.snapshot().reported_responses, 1);
    assert!(run.freeze().unwrap().specs().is_empty());
}

#[tokio::test]
async fn selector_drop_records_unknown_cost_and_never_repeats_in_the_same_runtime() {
    let (_temp, host, _) = fixture();
    let mut run = runtime(&host);
    let (provider, requests) = selector("unused", true);
    let outer = UsageMeter::default();
    host.configure_preselection(&mut run, Some(Arc::new(outer.wrap(provider))), None);
    let inner = UsageMeter::default();
    let mut preparation = Box::pin(run.prepare_local_preselection("lookup", "", &inner));
    assert!(futures_util::poll!(&mut preparation).is_pending());
    assert_eq!(requests.lock().unwrap().len(), 1);
    drop(preparation);
    assert_eq!(inner.snapshot().failed_requests, 1);
    assert_eq!(outer.snapshot(), inner.snapshot());
    assert_eq!(
        run.prepare_local_preselection("lookup", "", &inner).await,
        None
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn index_remains_bound_to_the_startup_registry_and_frozen_requests_do_not_select() {
    let (_temp, host, mut registry) = fixture();
    let mut run = runtime(&host);
    let (provider, requests) = selector(r#"{"tools":[0]}"#, false);
    let polaris_mcp::registration::Destination::StreamableHttp { url, .. } =
        &mut registry.servers[0].transport
    else {
        panic!()
    };
    *url = "http://127.0.0.1:12346/changed".into();
    registry.revision += 1;
    let store = host.store.as_ref().unwrap();
    store.save_registry(1, &registry).unwrap();
    save_index(store, &registry.servers[0]);
    host.configure_preselection(&mut run, Some(provider.clone()), None);
    assert_eq!(
        run.prepare_local_preselection("lookup", "", &UsageMeter::default())
            .await,
        Some(Status::NoIndex)
    );
    assert!(requests.lock().unwrap().is_empty());

    let mut run = runtime(&host);
    run.freeze().unwrap();
    host.configure_preselection(&mut run, Some(provider), None);
    assert_eq!(
        run.prepare_local_preselection("lookup", "", &UsageMeter::default())
            .await,
        None
    );
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_store_created_after_host_startup_is_used_on_the_next_turn() {
    let (_temp, mut host, _) = fixture();
    host.store = None;
    let mut run = runtime(&host);
    let (provider, requests) = selector(r#"{"tools":[0]}"#, false);
    host.configure_preselection(&mut run, Some(provider), None);
    assert_eq!(
        run.prepare_local_preselection("lookup", "", &UsageMeter::default())
            .await,
        Some(Status::Selected)
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// Answers every question with one fixed probability.
struct FlatJudge(f64);

#[async_trait::async_trait]
impl polaris_provider::decision::Decider for FlatJudge {
    async fn decide(
        &self,
        _: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, polaris_provider::decision::Question>,
    ) -> Result<polaris_provider::decision::Decision, polaris_provider::decision::DecisionError>
    {
        Ok(polaris_provider::decision::Decision {
            answers: questions
                .keys()
                .map(|key| {
                    (
                        key.clone(),
                        polaris_provider::decision::Answer {
                            noul: Some(self.0),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            input_tokens: Some(1),
            output_tokens: Some(0),
        })
    }
}

#[tokio::test]
async fn registering_a_judge_is_what_decides_which_path_a_run_takes() {
    // Confident enough to adopt on the typed answer alone, so the light
    // model has nothing left to settle and is never asked. Without a judge
    // the same run must still reach it.
    let (_temp, host, _) = fixture();
    let meter = UsageMeter::default();

    let mut judged = runtime(&host);
    let (provider, requests) = selector(r#"{"tools":[0]}"#, false);
    host.configure_preselection(&mut judged, Some(provider), Some(Arc::new(FlatJudge(0.95))));
    assert_eq!(
        judged
            .prepare_local_preselection("lookup this", "implement", &meter)
            .await,
        Some(Status::Selected)
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        0,
        "the typed answers settled it, so the light model costs nothing"
    );

    let mut unjudged = runtime(&host);
    let (provider, requests) = selector(r#"{"tools":[0]}"#, false);
    host.configure_preselection(&mut unjudged, Some(provider), None);
    assert_eq!(
        unjudged
            .prepare_local_preselection("lookup this", "implement", &meter)
            .await,
        Some(Status::Selected)
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "without a judge the light model is the only thing that chooses"
    );
}

#[test]
fn every_registered_server_appears_with_why_it_is_or_is_not_usable() {
    // The fixture registers two: one enabled for this project, one enabled
    // only elsewhere. A server missing from the list for an unstated
    // reason is exactly what this report exists to prevent, so the one
    // this project has not enabled must still be shown.
    let (_temp, host, registry) = fixture();
    let report = host
        .state_report(ExternalCeiling::host_external(false))
        .expect("a report");
    assert_eq!(report.len(), registry.servers.len());
    let current = report
        .iter()
        .find(|held| held.server_id == "current")
        .expect("the enabled server");
    assert!(current.enabled_here);
    assert_eq!(current.quarantine, None);
    let other = report
        .iter()
        .find(|held| held.server_id == "other")
        .expect("the server enabled for another project");
    assert!(
        !other.enabled_here,
        "registered elsewhere is the common reason a server's tools never appear"
    );
}

#[cfg(target_os = "linux")]
use crate::approval::{ApprovalPolicy, Approver, Decision, Gate};
#[cfg(target_os = "linux")]
use crate::mcp::setup::{Detector, HOST_PATH};
#[cfg(target_os = "linux")]
use polaris_mcp::store::Snapshot;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[cfg(target_os = "linux")]
const LINUX_RUNTIME_ROOTS: &[&str] = &["/bin", "/usr/bin", "/usr/lib"];
#[cfg(target_os = "linux")]
const STDIO_NAME: &str = "local_probe";

#[cfg(target_os = "linux")]
struct RuntimeRootFixture {
    _temp: tempfile::TempDir,
    // home, project, server install tree, and data are separate siblings.
    paths: [PathBuf; 4],
    host: LocalHost,
}

#[cfg(target_os = "linux")]
fn runtime_root_fixture(roots: &[&str], stdio: bool) -> RuntimeRootFixture {
    let temp = tempfile::tempdir().unwrap();
    let paths = ["home", "project", "server", "data"].map(|name| {
        let path = temp.path().join(name);
        std::fs::create_dir(&path).unwrap();
        path.canonicalize().unwrap()
    });
    for left in 0..paths.len() {
        for right in left + 1..paths.len() {
            assert_ne!(paths[left], paths[right]);
            assert!(!paths[left].starts_with(&paths[right]));
            assert!(!paths[right].starts_with(&paths[left]));
        }
    }
    std::fs::create_dir(paths[1].join(".polaris")).unwrap();
    let executable = paths[2].join("server");
    std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    if stdio {
        std::fs::create_dir(paths[0].join(".codex")).unwrap();
        std::fs::write(
            paths[0].join(".codex/config.toml"),
            format!(
                "[mcp_servers.{STDIO_NAME}]\ncommand = \"{}\"\n",
                executable.display()
            ),
        )
        .unwrap();
    }
    let host = LocalHost {
        store: None,
        home: paths[0].clone(),
        root: paths[1].clone(),
        project: crate::project::project_id(&paths[1]),
        runtime_roots: roots.iter().map(PathBuf::from).collect(),
        excluded_roots: vec![
            paths[1].clone(),
            paths[3].join("state"),
            paths[3].join("sessions"),
            paths[3].join("sessions-v2"),
        ],
    };
    assert!(!Store::exists(&paths[0]));
    RuntimeRootFixture {
        _temp: temp,
        paths,
        host,
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct RegistrationApprover {
    reasons: Vec<String>,
}

#[cfg(target_os = "linux")]
impl Approver for RegistrationApprover {
    fn ask(&mut self, reason: &str) -> Decision {
        self.reasons.push(reason.to_owned());
        Decision::Allow
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadRootClass {
    Ok,
    Protection,
    Limit,
    Other,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct InstallRootInputs {
    structure_ok: bool,
    snapshot_ok: bool,
    protected_path_overlap: bool,
    install_dir_identity_collision: bool,
    script_identity_collision: bool,
    read_root: ReadRootClass,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallRootDecision {
    Accept,
    RetryProtectedIdentity,
    FatalUnsafeFixtureStructure,
    FatalProtectionSnapshot,
    FatalProtectedPathOverlap,
    FatalInconsistentSnapshot,
    FatalOtherReadRootDenial,
    FatalNonProtectionLimit,
    FatalNonProtectionOther,
}

#[cfg(target_os = "linux")]
fn classify_install_root(input: InstallRootInputs) -> InstallRootDecision {
    use InstallRootDecision::*;
    if !input.structure_ok {
        return FatalUnsafeFixtureStructure;
    }
    if !input.snapshot_ok {
        return FatalProtectionSnapshot;
    }
    if input.protected_path_overlap {
        return FatalProtectedPathOverlap;
    }
    let collision = input.install_dir_identity_collision || input.script_identity_collision;
    match (input.read_root, collision) {
        (ReadRootClass::Ok, false) => Accept,
        (ReadRootClass::Ok, true) => FatalInconsistentSnapshot,
        (ReadRootClass::Protection, true) => RetryProtectedIdentity,
        (ReadRootClass::Protection, false) => FatalOtherReadRootDenial,
        (ReadRootClass::Limit, _) => FatalNonProtectionLimit,
        (ReadRootClass::Other, _) => FatalNonProtectionOther,
    }
}

#[cfg(target_os = "linux")]
fn read_root_class(result: Result<(), polaris_mcp::Error>) -> ReadRootClass {
    match result {
        Ok(()) => ReadRootClass::Ok,
        Err(polaris_mcp::Error::Protection) => ReadRootClass::Protection,
        Err(polaris_mcp::Error::Limit) => ReadRootClass::Limit,
        Err(_) => ReadRootClass::Other,
    }
}

#[cfg(target_os = "linux")]
fn preflight_stdio_install_root(
    fixture: &RuntimeRootFixture,
    role: &str,
    generation: usize,
    discarded_install: usize,
    discarded_cwd: usize,
) -> InstallRootDecision {
    let home = &fixture.paths[0];
    let install_dir = &fixture.paths[2];
    let script = install_dir.join("server");
    // admit_read_root uses cwd only for path relationships; this path is never created.
    let diagnostic_cwd = home.join(".polaris/mcp/servers/install-root-diagnostic-absent");
    let dir_meta = std::fs::symlink_metadata(install_dir).ok();
    let script_meta = std::fs::symlink_metadata(&script).ok();
    let entries = std::fs::read_dir(install_dir).ok().and_then(|iter| {
        iter.map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .ok()
    });
    // SAFETY: geteuid has no arguments or memory effects.
    let euid = unsafe { libc::geteuid() };
    let separate = fixture
        .paths
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 2)
        .map(|(_, path)| path)
        .chain(std::iter::once(&diagnostic_cwd))
        .chain(fixture.host.excluded_roots.iter())
        .all(|other| {
            !other.starts_with(install_dir)
                && !install_dir.starts_with(other)
                && !other.starts_with(&script)
                && !script.starts_with(other)
        });
    let structure_ok = install_dir.canonicalize().ok().as_ref() == Some(install_dir)
        && script.canonicalize().ok().as_ref() == Some(&script)
        && dir_meta.as_ref().is_some_and(|meta| {
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.uid() == euid
                && meta.mode() & 0o022 == 0
        })
        && script_meta.as_ref().is_some_and(|meta| {
            meta.is_file()
                && !meta.file_type().is_symlink()
                && meta.nlink() == 1
                && meta.uid() == euid
                && meta.mode() & 0o022 == 0
                && meta.mode() & 0o100 != 0
        })
        && entries
            .as_ref()
            .is_some_and(|entries| entries.len() == 1 && entries[0] == script)
        && !polaris_tools::path_policy::is_denied(install_dir)
        && !polaris_tools::path_policy::is_denied(&script)
        && diagnostic_cwd.starts_with(home)
        && !diagnostic_cwd.exists()
        && separate;
    let snapshot = polaris_auth::protection::with_protected_paths(|registered| {
        let identity_matches = |meta: &std::fs::Metadata| {
            registered
                .identities()
                .contains(&polaris_auth::protection::ProtectedIdentity {
                    dev: meta.dev(),
                    ino: meta.ino(),
                })
        };
        (
            registered
                .paths()
                .iter()
                .any(|path| path.starts_with(install_dir)),
            dir_meta.as_ref().is_some_and(identity_matches),
            script_meta.as_ref().is_some_and(identity_matches),
        )
    });
    let snapshot_ok = snapshot.is_ok();
    let (protected_path_overlap, install_dir_identity_collision, script_identity_collision) =
        snapshot.unwrap_or((false, false, false));
    let read_root = read_root_class(crate::mcp::runtime::resources::admit_read_root(
        install_dir,
        home,
        &diagnostic_cwd,
        &fixture.host.excluded_roots,
    ));
    let decision = classify_install_root(InstallRootInputs {
        structure_ok,
        snapshot_ok,
        protected_path_overlap,
        install_dir_identity_collision,
        script_identity_collision,
        read_root,
    });
    eprintln!(
        "stdio install preflight role={role} generation={generation} decision={decision:?} \
         install_dir_identity_collision={install_dir_identity_collision} \
         script_identity_collision={script_identity_collision} \
         protected_path_overlap={protected_path_overlap} read_root={read_root:?} \
         discarded_install={discarded_install} discarded_cwd={discarded_cwd}"
    );
    decision
}

#[cfg(target_os = "linux")]
fn assert_install_root_decision_table() {
    use InstallRootDecision::*;
    let safe = InstallRootInputs {
        structure_ok: true,
        snapshot_ok: true,
        protected_path_overlap: false,
        install_dir_identity_collision: false,
        script_identity_collision: false,
        read_root: ReadRootClass::Ok,
    };
    assert_eq!(classify_install_root(safe), Accept);
    let protection = InstallRootInputs {
        read_root: ReadRootClass::Protection,
        ..safe
    };
    assert_eq!(
        classify_install_root(InstallRootInputs {
            install_dir_identity_collision: true,
            ..protection
        }),
        RetryProtectedIdentity
    );
    assert_eq!(
        classify_install_root(InstallRootInputs {
            script_identity_collision: true,
            ..protection
        }),
        RetryProtectedIdentity
    );
    assert_eq!(classify_install_root(protection), FatalOtherReadRootDenial);
    let collided = InstallRootInputs {
        install_dir_identity_collision: true,
        ..protection
    };
    assert_eq!(
        classify_install_root(InstallRootInputs {
            protected_path_overlap: true,
            ..collided
        }),
        FatalProtectedPathOverlap
    );
    assert_eq!(
        classify_install_root(InstallRootInputs {
            structure_ok: false,
            ..collided
        }),
        FatalUnsafeFixtureStructure
    );
    assert_eq!(
        classify_install_root(InstallRootInputs {
            structure_ok: false,
            protected_path_overlap: true,
            ..collided
        }),
        FatalUnsafeFixtureStructure
    );
    assert_eq!(
        classify_install_root(InstallRootInputs {
            read_root: ReadRootClass::Ok,
            ..collided
        }),
        FatalInconsistentSnapshot
    );
    assert_eq!(
        classify_install_root(InstallRootInputs {
            read_root: ReadRootClass::Limit,
            ..collided
        }),
        FatalNonProtectionLimit
    );
    assert_eq!(
        classify_install_root(InstallRootInputs {
            read_root: ReadRootClass::Other,
            ..collided
        }),
        FatalNonProtectionOther
    );
    assert_eq!(
        classify_install_root(InstallRootInputs {
            snapshot_ok: false,
            ..safe
        }),
        FatalProtectionSnapshot
    );
}

#[cfg(target_os = "linux")]
async fn register_stdio_fixture(
    fixture: &RuntimeRootFixture,
    detector: &Detector,
) -> Result<String, (String, String)> {
    let host = &fixture.host;
    let home = &fixture.paths[0];
    assert!(!Store::exists(home));
    // The direct oracle and SetupTool use the same product-owned host PATH.
    let found = detector.detect(
        &host.project,
        &Snapshot {
            registry: None,
            secrets: None,
        },
    );
    let matching: Vec<_> = found
        .iter()
        .filter(|entry| entry.name == STDIO_NAME)
        .collect();
    assert_eq!(matching.len(), 1);
    let detected = matching[0];
    assert_eq!(detected.transport, "stdio");
    assert_eq!(detected.reason, None);
    let proposal = detected.proposal.as_ref().unwrap();
    let polaris_mcp::registration::Destination::Stdio { readable_roots, .. } =
        &proposal.server.transport
    else {
        panic!("the direct stdio oracle changed transport");
    };
    assert_eq!(readable_roots, &vec![fixture.paths[2].clone()]);
    let server_id = proposal.server.server_id.clone();
    let payload_hash = proposal.payload_hash().unwrap();
    assert_eq!(payload_hash.len(), 64);
    assert!(payload_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));

    let tool = host.setup_tool(ExternalCeiling::Allowed);
    let mut gate = Gate::new(ApprovalPolicy::Always);
    let mut approver = RegistrationApprover::default();
    let listed: serde_json::Value = serde_json::from_str(
        &tool
            .call(
                &serde_json::json!({"op":"detect"}),
                std::future::pending(),
                &mut gate,
                &mut approver,
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let entries = listed["candidates"].as_array().unwrap();
    let matching: Vec<_> = entries
        .iter()
        .filter(|entry| entry["name"] == STDIO_NAME)
        .collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0]["transport"], "stdio");
    assert!(approver.reasons.is_empty());
    assert!(!Store::exists(home));
    let detected_name = matching[0]["name"].as_str().unwrap();
    assert_eq!(detected_name, detected.name);
    let candidate = matching[0]["candidate"].as_str().unwrap();
    assert!(!candidate.is_empty());
    let request = serde_json::json!({"op":"register","candidate":candidate});
    assert_eq!(request.as_object().unwrap().len(), 2);
    eprintln!(
        "stdio detect config_name={STDIO_NAME} direct_name={} setup_name={detected_name} \
         server_id={server_id} payload_hash={payload_hash} candidate={candidate} \
         register_request={request}",
        detected.name,
    );
    let result = tool
        .call(&request, std::future::pending(), &mut gate, &mut approver)
        .await;
    assert_eq!(approver.reasons.len(), 1);
    assert_eq!(
        approver.reasons[0],
        format!("MCPサーバの登録\n{}", proposal.description(detected_name))
    );
    eprintln!(
        "stdio callback_count={} approval_reason={} decision=Allow result={result:?}",
        approver.reasons.len(),
        serde_json::to_string(&approver.reasons[0]).unwrap(),
    );
    match result {
        Ok(saved) => {
            let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
            assert_eq!(saved["registered"], server_id);
            assert_eq!(saved["registry_revision"], "1");
            let store = Store::open(home).unwrap();
            let registry = store.load_registry().unwrap().unwrap();
            assert_eq!(registry.revision, 1);
            assert_eq!(registry.servers.len(), 1);
            assert_eq!(registry.servers[0].server_id, server_id);
            let (runtime, quarantined) = host
                .runtime("session", "run", ExternalCeiling::Allowed)
                .unwrap()
                .unwrap();
            assert!(quarantined.is_empty());
            assert_eq!(runtime.config().registry().servers.len(), 1);
            assert_eq!(runtime.config().registry().servers[0].server_id, server_id);
            eprintln!(
                "stdio registered server_id={server_id} registry_revision=1 runtime_id={server_id} quarantine=empty"
            );
            Ok(server_id)
        }
        Err(error) => {
            let registry = Store::open(home).unwrap().load_registry().unwrap();
            assert!(
                registry.is_none(),
                "registration failure must not advance revision"
            );
            eprintln!(
                "stdio refused server_id={server_id} registry_revision_absent=true error={error}"
            );
            Err((server_id, error))
        }
    }
}

#[cfg(target_os = "linux")]
fn reused_protected_cwd(
    fixture: &RuntimeRootFixture,
    result: &Result<String, (String, String)>,
) -> bool {
    let Err((server_id, error)) = result else {
        return false;
    };
    if error != &serde_json::json!({"error":"unsafe_location"}).to_string() {
        return false;
    }
    let cwd = fixture.paths[0]
        .join(".polaris/mcp/servers")
        .join(server_id);
    let Ok(metadata) = std::fs::symlink_metadata(&cwd) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    let identity = polaris_auth::protection::ProtectedIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    let stale_identity = polaris_auth::protection::with_protected_paths(|registered| {
        registered.identities().contains(&identity)
            && !registered
                .paths()
                .iter()
                .any(|path| path.starts_with(&cwd) || cwd.starts_with(path))
    })
    .unwrap_or(false);
    stale_identity
        && crate::mcp::runtime::resources::admit_read_root(
            &fixture.paths[2],
            &fixture.paths[0],
            &cwd,
            &fixture.host.excluded_roots,
        )
        .is_ok()
        && matches!(
            crate::isolated_workspace::RegisteredDirectory::open(&cwd, LIMITS),
            Err(error) if error.kind == crate::isolated_workspace::ErrorKind::InvalidPath
        )
}

#[cfg(target_os = "linux")]
async fn register_stdio_without_reused_cwd(
    mut fixture: RuntimeRootFixture,
    roots: &[&str],
    role: &str,
    discarded: &mut Vec<RuntimeRootFixture>,
) -> (RuntimeRootFixture, Result<String, (String, String)>) {
    let mut discarded_install = 0;
    let mut discarded_cwd = 0;
    for attempt in 0..32 {
        let detector = Detector::new(
            fixture.paths[0].clone(),
            HOST_PATH.iter().map(PathBuf::from).collect(),
        );
        let decision = preflight_stdio_install_root(
            &fixture,
            role,
            attempt + 1,
            discarded_install,
            discarded_cwd,
        );
        match decision {
            InstallRootDecision::Accept => {}
            InstallRootDecision::RetryProtectedIdentity => {
                if attempt == 31 {
                    panic!(
                        "stdio fixture role={role} exhausted 32 generations after install-root identity collision"
                    );
                }
                discarded_install += 1;
                discarded.push(fixture);
                fixture = runtime_root_fixture(roots, true);
                continue;
            }
            fatal => {
                panic!(
                    "stdio fixture role={role} generation={} install-root preflight failed: {fatal:?}",
                    attempt + 1
                );
            }
        }
        let result = register_stdio_fixture(&fixture, &detector).await;
        if !reused_protected_cwd(&fixture, &result) {
            eprintln!(
                "stdio fixture accepted role={role} discarded_install={discarded_install} discarded_cwd={discarded_cwd}"
            );
            return (fixture, result);
        }
        eprintln!(
            "stdio fixture cwd reused a protected inode; role={role} generation={}",
            attempt + 1
        );
        if attempt == 31 {
            panic!(
                "stdio fixture role={role} exhausted 32 generations after cwd identity collision"
            );
        }
        discarded_cwd += 1;
        discarded.push(fixture);
        fixture = runtime_root_fixture(roots, true);
    }
    unreachable!("the last attempt must return or panic");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_runtime_root_setup_tool_saves_stdio_and_runtime_keeps_it_registered() {
    let _guard = LOCAL_FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_install_root_decision_table();
    let constructor = runtime_root_fixture(RUNTIME_ROOTS, false);
    let constructor_home = &constructor.paths[0];
    assert_eq!(std::fs::read_dir(constructor_home).unwrap().count(), 0);
    assert!(!Store::exists(constructor_home));
    assert!(!constructor_home.join(".polaris/mcp/servers.json").exists());
    let constructor_host =
        LocalHost::open_with_home(&constructor.paths[1], constructor_home, false).unwrap();
    assert!(constructor_host.store.is_none());
    assert_eq!(
        constructor_host.runtime_roots,
        RUNTIME_ROOTS.iter().map(PathBuf::from).collect::<Vec<_>>()
    );
    assert_eq!(std::fs::read_dir(constructor_home).unwrap().count(), 0);
    assert!(!Store::exists(constructor_home));
    assert!(!constructor_home.join(".polaris/mcp/servers.json").exists());

    let control = runtime_root_fixture(LINUX_RUNTIME_ROOTS, true);
    let product = runtime_root_fixture(RUNTIME_ROOTS, true);
    assert_eq!(product.host.runtime_roots, constructor_host.runtime_roots);
    for control_path in &control.paths {
        for product_path in &product.paths {
            assert_ne!(control_path, product_path);
            assert!(!control_path.starts_with(product_path));
            assert!(!product_path.starts_with(control_path));
        }
    }
    let mut discarded = Vec::new();
    let (control, control_result) =
        register_stdio_without_reused_cwd(control, LINUX_RUNTIME_ROOTS, "control", &mut discarded)
            .await;
    let control_id = control_result.expect("control registration");
    assert!(!control_id.is_empty());
    let (product, product_result) =
        register_stdio_without_reused_cwd(product, RUNTIME_ROOTS, "product", &mut discarded).await;
    for control_path in &control.paths {
        for product_path in &product.paths {
            assert_ne!(control_path, product_path);
            assert!(!control_path.starts_with(product_path));
            assert!(!product_path.starts_with(control_path));
        }
    }
    let product_id = match &product_result {
        Ok(id) | Err((id, _)) => id,
    };
    assert_ne!(&control_id, product_id);
    eprintln!("stdio fixture IDs differ control_id={control_id} product_id={product_id}");
    // Before the product root fix this is exactly unsafe_location, with no
    // registry revision. The same assertion succeeds after the root fix.
    if let Err((_, error)) = &product_result {
        assert_eq!(
            error,
            &serde_json::json!({"error":"unsafe_location"}).to_string()
        );
        panic!("control succeeded; product registration refused the old runtime root: {error}");
    }
    assert!(product_result.is_ok());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_runtime_root_missing_path_returns_validate_error_and_runtime_quarantine() {
    let _guard = LOCAL_FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut discarded = Vec::new();
    let (mut fixture, result) = register_stdio_without_reused_cwd(
        runtime_root_fixture(LINUX_RUNTIME_ROOTS, true),
        LINUX_RUNTIME_ROOTS,
        "missing-root",
        &mut discarded,
    )
    .await;
    let server_id = result.unwrap();
    let store = Store::open(&fixture.paths[0]).unwrap();
    let mut registry = store.load_registry().unwrap().unwrap();
    let registered = registry.servers.remove(0);
    let nonce = fixture._temp.path().file_name().unwrap().to_string_lossy();
    let missing = fixture
        ._temp
        .path()
        .join(format!("missing-runtime-root-{nonce}"));
    assert!(!missing.exists());
    assert!(
        !fixture
            .host
            .runtime_roots
            .contains(&PathBuf::from("/System/Library"))
    );
    fixture.host.runtime_roots.push(missing);
    assert_eq!(fixture.host.validate(&registered), Err(Error::Protection));
    let (runtime, quarantined) = fixture
        .host
        .runtime("session", "run", ExternalCeiling::Allowed)
        .unwrap()
        .unwrap();
    assert_eq!(quarantined.get(&server_id), Some(&Error::Protection));
    assert!(
        runtime
            .config()
            .registry()
            .servers
            .iter()
            .all(|s| s.server_id != server_id)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_runtime_root_http_is_unchanged() {
    let _guard = LOCAL_FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = runtime_root_fixture(RUNTIME_ROOTS, false);
    let host = &fixture.host;
    let store = Store::open(&fixture.paths[0]).unwrap();
    assert!(
        host.runtime("session", "run", ExternalCeiling::Denied)
            .unwrap()
            .is_none()
    );
    assert!(
        host.state_report(ExternalCeiling::Denied)
            .unwrap()
            .is_empty()
    );
    let id = "http-runtime-root";
    let registry = Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![server(id, &host.project)],
    };
    store.save_registry(0, &registry).unwrap();
    let (runtime, quarantined) = host
        .runtime("session", "run", ExternalCeiling::Denied)
        .unwrap()
        .unwrap();
    assert!(quarantined.is_empty());
    assert_eq!(runtime.config().registry().servers.len(), 1);
    assert_eq!(runtime.config().registry().servers[0].server_id, id);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_runtime_roots_keep_system_library() {
    assert_eq!(
        RUNTIME_ROOTS,
        &["/bin", "/usr/bin", "/usr/lib", "/System/Library"]
    );
}
