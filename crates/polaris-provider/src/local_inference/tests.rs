//! Wire-only inference contracts; no real runtime or credentials.
use super::*;
use crate::local::{Endpoint, ExecutionLocation, LocalAdapter, Runtime};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn request() -> CompletionRequest {
    CompletionRequest {
        system: "system".into(),
        messages: vec![crate::Message::user("hello")],
        tools: vec![],
    }
}

async fn selections(server: &MockServer, models: Value) -> Vec<ModelSelection> {
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":models})))
        .mount(server)
        .await;
    let adapter =
        LocalAdapter::new(Runtime::LmStudio, Endpoint::parse(&server.uri()).unwrap()).unwrap();
    let mut selected = vec![];
    for model in adapter.inventory().await.unwrap() {
        selected.push(adapter.select(&model).await.unwrap());
    }
    server.reset().await;
    selected
}

async fn selection(server: &MockServer) -> ModelSelection {
    selections(
        server,
        json!([{"key":"alpha","type":"llm","capabilities":{"trained_for_tool_use":true}}]),
    )
    .await
    .remove(0)
}

async fn reply(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test]
async fn parallel_models_are_fixed_and_usage_and_tool_wire_are_shared() {
    let server = MockServer::start().await;
    let selected = selections(
        &server,
        json!([
            {"key":"alpha","type":"llm","capabilities":{"trained_for_tool_use":true}},
            {"key":"beta","type":"llm","remote_host":"cloud"}
        ]),
    )
    .await;
    let a = LocalInferenceProvider::new(selected[0].clone()).unwrap();
    let b = LocalInferenceProvider::new(selected[1].clone()).unwrap();
    assert_eq!(
        a.selection().model().execution_location(),
        ExecutionLocation::Unknown
    );
    assert_eq!(
        b.selection().model().execution_location(),
        ExecutionLocation::Remote
    );
    reply(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"content":"ok"}}],
            "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}
        })),
    )
    .await;
    a.set_model("wrong");
    a.set_effort(Some("high"));
    b.set_model("alpha");
    let mut tool_request = request();
    tool_request.tools = polaris_tools::all_specs();
    let expected_tools = crate::openai::tool_wire_shape(&tool_request.tools);
    let (ra, rb) = tokio::join!(a.complete(tool_request), b.complete(request()));
    assert_eq!(ra.unwrap().usage.unwrap().total_tokens, 13);
    assert_eq!(rb.unwrap().text, "ok");
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    for r in &received {
        assert!(!r.headers.contains_key("authorization"));
        let body: Value = r.body_json().unwrap();
        assert!(body.get("reasoning_effort").is_none());
        match body["model"].as_str().unwrap() {
            "alpha" => assert_eq!(body["tools"], json!(expected_tools)),
            "beta" => assert!(body.get("tools").is_none()),
            other => panic!("unexpected model {other}"),
        }
    }
}

#[tokio::test]
async fn unsupported_and_unknown_tools_and_hosted_search_send_nothing() {
    let server = MockServer::start().await;
    let selected = selections(
        &server,
        json!([
            {"key":"unknown"},
            {"key":"no-tools","type":"llm","capabilities":{"trained_for_tool_use":false}},
            {"key":"embedding","type":"embedding"}
        ]),
    )
    .await;
    assert!(matches!(
        LocalInferenceProvider::new(selected[2].clone()),
        Err(ProviderError::Unsupported(_))
    ));
    for model in &selected[..2] {
        let provider = LocalInferenceProvider::new(model.clone()).unwrap();
        let mut req = request();
        req.tools = polaris_tools::all_specs();
        assert!(matches!(
            provider.complete(req).await,
            Err(ProviderError::Unsupported(_))
        ));
        for config in [
            WebSearchConfig::cached(crate::web_search::WebSearchRequestCaps::new(1, 1).unwrap()),
            WebSearchConfig::live(crate::web_search::WebSearchRequestCaps::new(1, 1).unwrap()),
        ] {
            assert!(matches!(
                LocalInferenceProvider::with_web_search(model.clone(), config),
                Err(ProviderError::Unsupported(_))
            ));
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirect_is_not_followed() {
    let server = MockServer::start().await;
    let destination = MockServer::start().await;
    let provider = LocalInferenceProvider::new(selection(&server).await).unwrap();
    reply(
        &server,
        ResponseTemplate::new(307).insert_header(
            "Location",
            format!("{}/v1/chat/completions", destination.uri()),
        ),
    )
    .await;
    assert!(provider.complete(request()).await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn bounded_errors_and_success_and_rejected_usage() {
    let server = MockServer::start().await;
    let provider = LocalInferenceProvider::new(selection(&server).await).unwrap();
    for (status, size) in [
        (500, crate::MAX_ERROR_BYTES + 1),
        (200, crate::MAX_RESPONSE_BYTES + 1),
    ] {
        reply(
            &server,
            ResponseTemplate::new(status).set_body_string("x".repeat(size)),
        )
        .await;
        let error = provider.complete(request()).await.unwrap_err();
        assert!(error.to_string().len() < 1024);
        assert!(error.to_string().contains("limit"));
        server.reset().await;
    }
    reply(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices":[], "usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}
        })),
    )
    .await;
    assert!(
        matches!(provider.complete(request()).await, Err(ProviderError::ReceivedUsage { usage, .. }) if usage.total_tokens == 9)
    );
}

#[tokio::test]
async fn ollama_selected_name_uses_compatible_route() {
    let server = MockServer::start().await;
    Mock::given(path("/api/tags"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models":[{"name":"ollama-name"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/api/show"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"capabilities":["completion"]})),
        )
        .mount(&server)
        .await;
    let adapter =
        LocalAdapter::new(Runtime::Ollama, Endpoint::parse(&server.uri()).unwrap()).unwrap();
    let selected = adapter
        .select(&adapter.inventory().await.unwrap()[0])
        .await
        .unwrap();
    server.reset().await;
    let provider = LocalInferenceProvider::new(selected).unwrap();
    reply(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"choices":[{"message":{"content":"ok"}}]})),
    )
    .await;
    assert!(provider.complete(request()).await.unwrap().usage.is_none());
    assert_eq!(
        server.received_requests().await.unwrap()[0]
            .body_json::<Value>()
            .unwrap()["model"],
        "ollama-name"
    );
}

#[test]
fn environment_proxy_and_ca_are_ignored_in_isolated_process() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "local_inference::tests::environment_child",
        ])
        .env("POLARIS_LOCAL_INFERENCE_FIXTURE", "1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("http_proxy", "http://127.0.0.1:1")
        .env("https_proxy", "http://127.0.0.1:1")
        .env("all_proxy", "http://127.0.0.1:1")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .env("SSL_CERT_FILE", "/nonexistent/polaris-local-fixture.pem")
        .env("OPENAI_API_KEY", "fixture-not-a-credential")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "isolated environment fixture invoked by parent test"]
fn environment_child() {
    if std::env::var("POLARIS_LOCAL_INFERENCE_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    parallel_models_are_fixed_and_usage_and_tool_wire_are_shared();
}
