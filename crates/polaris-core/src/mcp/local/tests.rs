//! Shared CLI/TUI preparation with synthetic stores and providers, no network.
use super::*;
use crate::mcp::preselection::Status;
use polaris_mcp::catalog::index::{IndexedTool, ServerIndex, summary_hash};
use polaris_mcp::registration::{Registry, Server};
use polaris_provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, Usage, UsageMeter,
};
use std::sync::{Arc, Mutex};

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

fn fixture() -> (tempfile::TempDir, LocalHost, Registry) {
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
        temp,
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
