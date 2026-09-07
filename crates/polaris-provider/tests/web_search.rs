//! Hosted Web request boundaries and anonymous response fixtures.
#[path = "../src/web_search.rs"]
mod web_search;

use serde_json::json;
use web_search::{
    HostedItemStatus, WebSearchAction, WebSearchConfig, WebSearchError, WebSearchPolicy,
    WebSearchRequestCaps, parse_hosted_web_search_item, parse_url_citations,
};

fn caps() -> WebSearchRequestCaps {
    WebSearchRequestCaps::new(2, 4096).expect("valid caps")
}

#[test]
fn disabled_is_a_wire_noop() {
    let mut body = json!({
        "model": "gpt-6-astra",
        "include": ["reasoning.encrypted_content"],
        "tools": [{"type": "function", "name": "read"}],
    });
    let original = body.clone();

    WebSearchConfig::disabled()
        .apply_to_body(&mut body)
        .expect("disabled must be accepted");

    assert_eq!(body, original);
}

#[test]
fn unverified_endpoint_is_rejected_without_mutating_the_wire() {
    let mut body = json!({"model": "gpt-6-astra"});
    let original = body.clone();

    let error = WebSearchConfig::live(caps())
        .apply_to_body(&mut body)
        .expect_err("unverified backend must not receive public API fields");

    assert_eq!(error, WebSearchError::UnsupportedEndpoint);
    assert_eq!(body, original);
}

#[test]
fn verified_cached_and_live_requests_have_explicit_distinct_access() {
    for (policy, external_web_access) in [
        (WebSearchPolicy::Cached, false),
        (WebSearchPolicy::Live, true),
    ] {
        let mut body = json!({"tools": [{"type": "function", "name": "read"}]});
        let config = match policy {
            WebSearchPolicy::Cached => WebSearchConfig::cached(caps()),
            WebSearchPolicy::Live => WebSearchConfig::live(caps()),
            WebSearchPolicy::Disabled => unreachable!(),
        }
        .with_verified_responses_contract();

        config.apply_to_body(&mut body).expect("verified contract");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(
            body["tools"][1],
            json!({
                "type": "web_search",
                "external_web_access": external_web_access,
            })
        );
        assert_eq!(body["include"], json!(["web_search_call.action.sources"]));
        assert_eq!(body["max_tool_calls"], 2);
        assert_eq!(body["max_output_tokens"], 4096);
    }
}

#[test]
fn rejects_partial_or_zero_caps() {
    assert!(WebSearchRequestCaps::new(0, 4096).is_err());
    assert!(WebSearchRequestCaps::new(2, 0).is_err());
}

#[test]
fn parses_anonymous_hosted_search_fixture_without_creating_a_function_call() {
    let fixture = json!({
        "type": "response.output_item.done",
        "output_index": 3,
        "item": {
            "id": "ws_anon_1",
            "type": "web_search_call",
            "status": "completed",
            "action": {
                "type": "search",
                "queries": ["Polaris Rust"],
                "sources": [
                    {"type": "url", "url": "https://example.test/a"},
                    {"type": "url", "url": "https://example.test/b"}
                ]
            }
        }
    });

    let item = parse_hosted_web_search_item(&fixture)
        .expect("fixture is valid")
        .expect("hosted item is retained");
    assert_eq!(item.output_index, 3);
    assert_eq!(item.id, "ws_anon_1");
    assert_eq!(item.status, HostedItemStatus::Completed);
    assert_eq!(
        item.action,
        WebSearchAction::Search {
            queries: vec!["Polaris Rust".into()]
        }
    );
    assert_eq!(item.sources.len(), 2);
    assert!(
        parse_hosted_web_search_item(&json!({
            "item": {"type": "function_call", "call_id": "call_1"}
        }))
        .expect("function calls are unrelated")
        .is_none()
    );
}

#[test]
fn preserves_citations_and_marks_unknown_offsets_unsafe() {
    let citations = parse_url_citations(&json!({
        "type": "output_text",
        "text": "A source-backed answer",
        "annotations": [
            {
                "type": "url_citation",
                "url": "https://example.test/a",
                "title": "Example A",
                "start_index": 2,
                "end_index": 8
            },
            {
                "type": "url_citation",
                "url": "https://example.test/b",
                "title": "Example B"
            }
        ]
    }));

    assert_eq!(citations.len(), 2);
    assert_eq!(citations[0].url, "https://example.test/a");
    assert!(citations[0].has_safe_offsets());
    assert!(!citations[1].has_safe_offsets());
}

#[test]
fn preserves_open_and_find_actions_and_rejects_missing_contract_fields() {
    let open = parse_hosted_web_search_item(&json!({
        "output_index": 1,
        "item": {
            "id": "ws_1", "type": "web_search_call", "status": "incomplete",
            "action": {"type": "open_page", "url": "https://example.test/open"}
        }
    }))
    .expect("open page is valid")
    .expect("hosted item");
    assert_eq!(open.status, HostedItemStatus::Incomplete);
    assert_eq!(
        open.action,
        WebSearchAction::OpenPage {
            url: "https://example.test/open".into()
        }
    );

    let find = parse_hosted_web_search_item(&json!({
        "output_index": 2,
        "item": {
            "id": "ws_2", "type": "web_search_call", "status": "completed",
            "action": {"type": "find_in_page", "url": "https://example.test/find", "pattern": "Polaris"}
        }
    }))
    .expect("find is valid")
    .expect("hosted item");
    assert_eq!(
        find.action,
        WebSearchAction::FindInPage {
            url: "https://example.test/find".into(),
            pattern: "Polaris".into(),
        }
    );

    let error = parse_hosted_web_search_item(&json!({
        "item": {"id": "ws_missing_index", "type": "web_search_call", "status": "completed", "action": {"type": "search"}}
    }))
    .expect_err("missing order cannot be guessed");
    assert!(matches!(error, WebSearchError::Malformed(_)));
}
