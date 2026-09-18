//! Lexical narrowing, bounded light-model output and closed failure reasons.
use super::*;
use polaris_mcp::catalog::index::summary_hash;
use polaris_provider::{CompletionResponse, ProviderError};
use std::{
    future::pending,
    sync::{Arc, Mutex},
};

struct Scripted {
    reply: Result<&'static str, ()>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        self.reply
            .map(|text| CompletionResponse {
                text: text.into(),
                ..Default::default()
            })
            .map_err(|()| ProviderError::Http("fixture".into()))
    }
}

fn index(names: &[&str]) -> BTreeMap<String, ServerIndex> {
    let tools = names
        .iter()
        .map(|name| IndexedTool {
            key: (*name).into(),
            name: (*name).into(),
            description: format!("{name} operation"),
            summary_hash: summary_hash("fixture", name, name, &format!("{name} operation"))
                .unwrap(),
            schema_hash: "a".repeat(64),
        })
        .collect();
    [(
        "fixture".to_owned(),
        ServerIndex {
            destination_hash: "d".into(),
            partial: false,
            tools,
        },
    )]
    .into()
}

fn scripted(reply: Result<&'static str, ()>) -> (Scripted, Arc<Mutex<Vec<CompletionRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        Scripted {
            reply,
            requests: requests.clone(),
        },
        requests,
    )
}

#[tokio::test]
async fn small_index_skips_ranking_and_the_model_sees_summaries_only() {
    let index = index(&["render_still", "list_scenes"]);
    let (provider, requests) = scripted(Ok(r#"{"tools":[1]}"#));
    let query = format!("{}末尾", "あ".repeat(QUERY_BYTES));
    let (picks, status) = select(
        Some(&provider),
        Ok(Some(&index)),
        &query,
        "implement",
        pending(),
    )
    .await;
    assert_eq!(status, Status::Selected);
    assert_eq!(picks.len(), 1);
    assert_eq!(picks[0].key, "list_scenes");
    assert_eq!(
        picks[0].summary_hash,
        index["fixture"].tools[1].summary_hash
    );
    let request = &requests.lock().unwrap()[0];
    assert!(request.tools.is_empty());
    let body: serde_json::Value = serde_json::from_str(&request.messages[0].content).unwrap();
    assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    assert!(body["task"].as_str().unwrap().len() <= QUERY_BYTES);
    assert!(!request.messages[0].content.contains("末尾"));
    assert!(!request.messages[0].content.contains(&"a".repeat(64)));
}

#[tokio::test]
async fn large_index_fills_the_request_from_bm25_then_names_then_the_catalogue() {
    let mut names: Vec<String> = (0..30).map(|i| format!("unrelated_{i}")).collect();
    names.push("render_still".into());
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let index = index(&names);
    let (provider, requests) = scripted(Ok(r#"{"tools":[]}"#));
    let (_, status) = select(
        Some(&provider),
        Ok(Some(&index)),
        "render a still image",
        "",
        pending(),
    )
    .await;
    assert_eq!(status, Status::NoneSelected);
    let body: serde_json::Value =
        serde_json::from_str(&requests.lock().unwrap()[0].messages[0].content).unwrap();
    let offered = body["tools"].as_array().unwrap();
    assert!(offered.len() <= CANDIDATES);
    assert!(offered.iter().any(|t| t["name"] == "render_still"));
    let (_, status) = select(
        Some(&provider),
        Ok(Some(&index)),
        "render_stillで静止画を出して",
        "",
        pending(),
    )
    .await;
    assert_eq!(status, Status::NoneSelected);
    // No word is shared with any description, which is the ordinary case
    // for a Japanese request against English descriptions: a 162-tool
    // catalogue produced no non-zero BM25 score on any of five tasks when
    // this was measured on 2026-09-18. The run is not refused. The
    // catalogue's head goes to the light model, which is free to choose
    // nothing from it, and here does.
    requests.lock().unwrap().clear();
    let (_, status) = select(
        Some(&provider),
        Ok(Some(&index)),
        "静止画を出して",
        "",
        pending(),
    )
    .await;
    assert_eq!(status, Status::NoneSelected);
    let body: serde_json::Value =
        serde_json::from_str(&requests.lock().unwrap()[0].messages[0].content).unwrap();
    assert_eq!(body["tools"].as_array().unwrap().len(), CANDIDATES);

    // A request with no words in it is a different thing, and the head of
    // the catalogue answers no question it could have asked.
    let (_, status) = select(Some(&provider), Ok(Some(&index)), "   ", "", pending()).await;
    assert_eq!(status, Status::NoLexicalMatch);
}

#[tokio::test]
async fn cjk_instruction_inside_a_description_reaches_the_selector_with_a_phase() {
    let mut names: Vec<String> = (0..25).map(|i| format!("tool_{i}")).collect();
    names.push("render".into());
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut index = index(&names);
    let tools = &mut index.get_mut("fixture").unwrap().tools;
    let render = tools.last_mut().unwrap();
    render.description = "静止画を書き出す".into();
    let (provider, requests) = scripted(Ok(r#"{"tools":[0]}"#));
    let (picks, status) = select(
        Some(&provider),
        Ok(Some(&index)),
        "静止画",
        "implement",
        pending(),
    )
    .await;
    assert_eq!(status, Status::Selected);
    assert_eq!(picks[0].key, "render");
    let body: serde_json::Value =
        serde_json::from_str(&requests.lock().unwrap()[0].messages[0].content).unwrap();
    // The one tool whose description the request names leads the list; the
    // rest of the budget is spent on the catalogue rather than left unused.
    let offered = body["tools"].as_array().unwrap();
    assert_eq!(offered.len(), CANDIDATES);
    assert_eq!(offered[0]["name"], "render");
}

/// The failure this fills the request against. `verify_card` is the only
/// tool in a 162-entry catalogue that BM25 scores for a request in the
/// `verify` phase, and it scores on the phase word alone. Ranking it first
/// is right; sending it *instead of* the catalogue is not.
#[tokio::test]
async fn one_weak_ranking_hit_does_not_take_the_catalogue_away() {
    let mut names: Vec<String> = (0..30).map(|i| format!("tool_{i}")).collect();
    names.push("verify_card".into());
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut index = index(&names);
    let tools = &mut index.get_mut("fixture").unwrap().tools;
    tools.last_mut().unwrap().description = "verify a card without charging it".into();
    let (provider, requests) = scripted(Ok(r#"{"tools":[]}"#));
    let (_, status) = select(
        Some(&provider),
        Ok(Some(&index)),
        "テストが失敗した原因を調べたい",
        "verify",
        pending(),
    )
    .await;
    assert_eq!(status, Status::NoneSelected);
    let body: serde_json::Value =
        serde_json::from_str(&requests.lock().unwrap()[0].messages[0].content).unwrap();
    let offered = body["tools"].as_array().unwrap();
    assert_eq!(offered.len(), CANDIDATES);
    // Ranked first, because it is the only thing the scorer saw. Not sent
    // alone, because what it was scored on was the phase and not the task.
    assert_eq!(offered[0]["name"], "verify_card");
}

#[tokio::test]
async fn invalid_output_failures_and_missing_inputs_preselect_nothing() {
    let index = index(&["a_tool", "b_tool", "c_tool", "d_tool"]);
    for reply in [
        "not json",
        r#"{"tools":[4]}"#,
        r#"{"tools":[0,0]}"#,
        r#"{"tools":[0,1,2,3]}"#,
        r#"{"tools":[0],"extra":true}"#,
    ] {
        let (provider, _) = scripted(Ok(reply));
        assert_eq!(
            select(Some(&provider), Ok(Some(&index)), "tool", "", pending()).await,
            (vec![], Status::InvalidOutput),
            "{reply}"
        );
    }
    let (provider, _) = scripted(Err(()));
    assert_eq!(
        select(Some(&provider), Ok(Some(&index)), "tool", "", pending())
            .await
            .1,
        Status::ProviderFailed
    );
    assert_eq!(
        select(None, Ok(Some(&index)), "tool", "", pending())
            .await
            .1,
        Status::UnsupportedBackend
    );
    assert_eq!(
        select(None, Ok(None), "tool", "", pending()).await.1,
        Status::NoIndex
    );
    assert_eq!(
        select(None, Err(Status::IndexBusy), "tool", "", pending())
            .await
            .1,
        Status::IndexBusy
    );
    assert_eq!(
        select(None, Ok(Some(&BTreeMap::new())), "tool", "", pending())
            .await
            .1,
        Status::NoIndex
    );
    assert_eq!(
        select(None, Ok(Some(&index)), "  ", "", pending()).await.1,
        Status::NoLexicalMatch
    );
    let (provider, requests) = scripted(Ok(r#"{"tools":[0]}"#));
    assert_eq!(
        select(
            Some(&provider),
            Ok(Some(&index)),
            "tool",
            "",
            std::future::ready(())
        )
        .await
        .1,
        Status::Cancelled
    );
    assert!(requests.lock().unwrap().is_empty());
}
