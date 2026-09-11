//! Verify bounded local-runtime discovery and independent metadata observations.
use super::*;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, method, path},
};

#[test]
fn rejects_non_literal_or_non_origin_endpoints() {
    for endpoint in [
        "http://localhost:1234",
        "http://example.com",
        "http://192.168.1.2:1234",
        "http://0.0.0.0:1234",
        "http://[::]:1234",
        "http://127.1:1234",
        "http://2130706433:1234",
        "http://0x7f000001:1234",
        "http://user:secret@127.0.0.1",
        "http://@127.0.0.1",
        "http://127.0.0.1?",
        "http://127.0.0.1#",
        "http://127.0.0.1/v1",
        "http://127.0.0.1/../",
        "http://127.0.0.1\\@example.com",
        " http://127.0.0.1",
        "ftp://127.0.0.1",
    ] {
        assert!(Endpoint::parse(endpoint).is_err(), "accepted {endpoint}");
    }
    for endpoint in [
        "http://127.0.0.1:11434",
        "http://[::1]:1234/",
        "https://127.0.0.1",
    ] {
        assert!(Endpoint::parse(endpoint).is_ok(), "rejected {endpoint}");
    }
}

async fn adapter(runtime: Runtime) -> (MockServer, LocalAdapter) {
    let server = MockServer::start().await;
    let client = LocalAdapter::new(runtime, Endpoint::parse(&server.uri()).unwrap()).unwrap();
    (server, client)
}

#[tokio::test]
async fn ollama_inventory_and_show_preserve_unknown_and_remote() {
    let (server, client) = adapter(Runtime::Ollama).await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[
            {"name":"local-name", "digest":"abc"},
            {"name":"alias", "remote_host":"https://ollama.com", "remote_model":"cloud"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let models = client.inventory().await.unwrap();
    assert_eq!(models[0].execution_location(), ExecutionLocation::Unknown);
    assert_eq!(models[0].capabilities().tools, Capability::Unknown);
    assert_eq!(models[1].execution_location(), ExecutionLocation::Remote);
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .and(body_json(json!({"model":"local-name"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "capabilities":["completion","tools","future-feature"],
            "remote_model":"remote-alias", "model_info":{"x.context_length":8192}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let selected = client.select(&models[0]).await.unwrap();
    assert_eq!(selected.model().capabilities().tools, Capability::Supported);
    assert_eq!(
        selected.model().capabilities().vision,
        Capability::Unsupported
    );
    assert_eq!(
        selected.model().execution_location(),
        ExecutionLocation::Remote
    );
    assert_eq!(models[0].capabilities().tools, Capability::Unknown);
}

#[tokio::test]
async fn ollama_running_models_distinguish_loaded_and_unloaded_without_loading() {
    let (server, client) = adapter(Runtime::Ollama).await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[
            {"name":"resident"}, {"name":"downloaded"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[
            {"name":"resident", "model":"resident"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let models = client.inventory_with_load_state().await.unwrap();
    assert_eq!(models[0].load_state(), LoadState::Loaded);
    assert_eq!(models[1].load_state(), LoadState::Unloaded);
}

#[tokio::test]
async fn failed_or_malformed_ollama_running_probe_stays_unknown() {
    for running in [json!({}), json!({"models":[{"name":null}]})] {
        let (server, client) = adapter(Runtime::Ollama).await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"models":[{"name":"a"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(running))
            .mount(&server)
            .await;
        assert_eq!(
            client.inventory_with_load_state().await.unwrap()[0].load_state(),
            LoadState::Unknown
        );
    }
}

#[tokio::test]
async fn lm_metadata_never_proves_local_and_selection_is_independent() {
    let (server, client) = adapter(Runtime::LmStudio).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[
            {"key":"a", "type":"llm", "max_context_length":8192,
             "loaded_instances":[{"id":"a", "config":{"context_length":8192}}],
             "capabilities":{"vision":false,"trained_for_tool_use":true}},
            {"key":"b", "type":"embedding", "loaded_instances":[]}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let models = client.inventory_with_load_state().await.unwrap();
    let a = client.select(&models[0]).await.unwrap();
    let b = client.select(&models[1]).await.unwrap();
    assert_eq!(a.model().id(), "a");
    assert_eq!(b.model().id(), "b");
    assert_eq!(a.model().execution_location(), ExecutionLocation::Unknown);
    assert_eq!(a.model().capabilities().tools, Capability::Supported);
    assert_eq!(a.model().capabilities().vision, Capability::Unsupported);
    assert_eq!(b.model().capabilities().completion, Capability::Unsupported);
    assert_eq!(b.model().capabilities().tools, Capability::Unknown);
    assert_eq!(a.model().max_context_length(), Some(8192));
    assert_eq!(a.model().load_state(), LoadState::Loaded);
    assert_eq!(b.model().load_state(), LoadState::Unloaded);
}

#[tokio::test]
async fn lm_missing_or_malformed_loaded_instances_stays_unknown() {
    let (server, client) = adapter(Runtime::LmStudio).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[
            {"key":"old"}, {"key":"malformed", "loaded_instances":true},
            {"key":"null", "loaded_instances":[null]},
            {"key":"bad-config", "loaded_instances":[{"id":"x", "config":{}}]}
        ]})))
        .mount(&server)
        .await;
    let models = client.inventory_with_load_state().await.unwrap();
    assert!(
        models
            .iter()
            .all(|model| model.load_state() == LoadState::Unknown)
    );
}

#[tokio::test]
async fn redirect_is_not_followed() {
    let target = MockServer::start().await;
    let (server, client) = adapter(Runtime::Ollama).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target)
        .await;
    Mock::given(path("/api/tags"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", target.uri()))
        .mount(&server)
        .await;
    assert!(matches!(
        client.inventory().await,
        Err(LocalError::HttpStatus(302))
    ));
}

#[tokio::test]
async fn invalid_metadata_fails_without_echoing_body() {
    for body in [
        json!({}),
        json!({"models":null}),
        json!({"models":[{}]}),
        json!({"models":[{"key":"a","capabilities":{"vision":"yes"}}]}),
        json!({"models":[{"key":"a"},{"key":"a"}]}),
        json!({"models":[{"key":"a","max_context_length":-1}]}),
    ] {
        let (server, client) = adapter(Runtime::LmStudio).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        assert!(matches!(
            client.inventory().await,
            Err(LocalError::InvalidMetadata)
        ));
    }
}

#[tokio::test]
async fn response_size_and_model_count_are_bounded() {
    let (server, client) = adapter(Runtime::Ollama).await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("x".repeat(MAX_INVENTORY_BYTES + 1)),
        )
        .mount(&server)
        .await;
    assert!(matches!(
        client.inventory().await,
        Err(LocalError::ResponseTooLarge)
    ));
    let (server, client) = adapter(Runtime::Ollama).await;
    let models: Vec<_> = (0..=MAX_MODELS)
        .map(|i| json!({"name":i.to_string()}))
        .collect();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":models})))
        .mount(&server)
        .await;
    assert!(matches!(
        client.inventory().await,
        Err(LocalError::InvalidMetadata)
    ));
}

#[tokio::test]
async fn selection_rejects_another_endpoint_before_sending() {
    let (server, client) = adapter(Runtime::LmStudio).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{"key":"a"}]})))
        .mount(&server)
        .await;
    let models = client.inventory().await.unwrap();
    let (other, other_client) = adapter(Runtime::Ollama).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&other)
        .await;
    assert!(matches!(
        other_client.select(&models[0]).await,
        Err(LocalError::SelectionMismatch)
    ));
}

#[tokio::test]
async fn size_limit_applies_without_content_length() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            assert!(request.len() < 4096);
            let mut byte = [0; 1];
            socket.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        // Connection-close framing exercises the incremental cap, not a header check.
        let _ = socket.write_all(&vec![b'x'; MAX_INVENTORY_BYTES + 1]).await;
    });
    let client = LocalAdapter::new(Runtime::Ollama, endpoint).unwrap();
    assert!(matches!(
        client.inventory().await,
        Err(LocalError::ResponseTooLarge)
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn stalled_metadata_request_has_a_finite_deadline() {
    let (server, client) = adapter(Runtime::Ollama).await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_delay(REQUEST_TIMEOUT + Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    assert!(matches!(client.inventory().await, Err(LocalError::Timeout)));
}

#[tokio::test]
async fn empty_inventory_and_http_errors_are_distinct() {
    for (status, expected_empty) in [(200, true), (401, false), (404, false), (503, false)] {
        let (server, client) = adapter(Runtime::LmStudio).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({"models":[]})))
            .mount(&server)
            .await;
        let result = client.inventory().await;
        if expected_empty {
            assert!(result.unwrap().is_empty());
        } else {
            assert!(matches!(result, Err(LocalError::HttpStatus(s)) if s == status));
        }
    }
}

#[tokio::test]
async fn malformed_show_and_remote_fields_are_rejected() {
    for body in [
        json!({}),
        json!({"capabilities":true}),
        json!({"capabilities":[1]}),
        json!({"capabilities":[], "remote_host":false}),
        json!({"model_info":{"x.context_length":"8192"}}),
    ] {
        let (server, client) = adapter(Runtime::Ollama).await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"models":[{"name":"a"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let models = client.inventory().await.unwrap();
        assert!(matches!(
            client.select(&models[0]).await,
            Err(LocalError::InvalidMetadata)
        ));
    }
}

#[tokio::test]
async fn show_snapshot_does_not_merge_inventory_identity_or_capabilities() {
    for show in [
        json!({"capabilities":["vision"], "model_info":{"b.context_length":8192}}),
        json!({"details":{}}),
    ] {
        let (server, client) = adapter(Runtime::Ollama).await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{
                "name":"nameX", "digest":"digestA", "selected_variant":"variantA",
                "capabilities":["completion","tools"],
                "model_info":{"a.context_length":1024}, "remote_host":"https://remote.example"
            }]})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(show.clone()))
            .mount(&server)
            .await;
        let models = client.inventory().await.unwrap();
        assert_eq!(models[0].digest(), Some("digestA"));
        let selected = client.select(&models[0]).await.unwrap();
        assert_eq!(selected.model().id(), "nameX");
        assert_eq!(selected.model().digest(), None);
        assert_eq!(selected.model().variant(), None);
        assert_eq!(
            selected.model().execution_location(),
            ExecutionLocation::Remote
        );
        if show.get("capabilities").is_some() {
            assert_eq!(
                selected.model().capabilities().vision,
                Capability::Supported
            );
            assert_eq!(
                selected.model().capabilities().tools,
                Capability::Unsupported
            );
            assert_eq!(selected.model().max_context_length(), Some(8192));
        } else {
            assert_eq!(selected.model().capabilities(), &Capabilities::default());
            assert_eq!(selected.model().max_context_length(), None);
        }
    }
}
