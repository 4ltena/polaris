//! Codex wire, usage, cache, retry and stream regression tests.
use super::*;
use crate::ToolCall;

fn frame(kind: &str, extra: Value) -> Vec<u8> {
    let mut v = serde_json::json!({ "type": kind });
    if let Some(o) = extra.as_object() {
        for (k, val) in o {
            v[k] = val.clone();
        }
    }
    format!("data: {v}\n\n").into_bytes()
}

#[test]
fn default_model_is_gpt_6_astra() {
    assert_eq!(DEFAULT_MODEL, "gpt-6-astra");
}

fn message_item(text: &str) -> Value {
    serde_json::json!({
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }]
        }
    })
}

fn function_call_item(id: &str, name: &str, args: &str) -> Value {
    serde_json::json!({
        "item": { "type": "function_call", "call_id": id, "name": name, "arguments": args }
    })
}

fn reasoning_item(id: &str, encrypted_content: &str) -> Value {
    serde_json::json!({
        "item": {
            "type": "reasoning",
            "id": id,
            "summary": [],
            "encrypted_content": encrypted_content,
        }
    })
}

#[derive(Default)]
struct ObserverFixture {
    deltas: std::sync::Mutex<Vec<String>>,
    finals: std::sync::Mutex<Vec<(String, Option<bool>)>>,
    reject_delta: bool,
    reject_final: bool,
}
impl crate::ResponseObserver for ObserverFixture {
    fn try_emit(&self, event: crate::ResponseEvent<'_>) -> Result<(), crate::ObserverError> {
        match event {
            crate::ResponseEvent::TextDelta { text, .. } => {
                if self.reject_delta {
                    return Err(crate::ObserverError::Full);
                }
                self.deltas.lock().unwrap().push(text.to_string());
            }
            crate::ResponseEvent::FinalText {
                text,
                delta_matches,
            } => {
                if self.reject_final {
                    return Err(crate::ObserverError::Closed);
                }
                self.finals
                    .lock()
                    .unwrap()
                    .push((text.to_string(), delta_matches));
            }
        }
        Ok(())
    }
}

fn observed_frames(delta: &str, final_text: &str) -> Vec<u8> {
    let mut done = message_item(final_text);
    done["output_index"] = serde_json::json!(0);
    [frame("response.output_text.delta", serde_json::json!({"output_index":0,"content_index":0,"delta":delta})),
            frame("response.output_item.done", done),
            frame("response.completed", serde_json::json!({"response":{"usage":{"input_tokens":3,"output_tokens":4,"total_tokens":7}}}))].concat()
}

#[test]
fn p43_delta_final_and_utf8_split_do_not_double_body() {
    for delta in ["日本語", "different"] {
        let bytes = observed_frames(delta, "日本語");
        for split in 0..=bytes.len() {
            let observer = ObserverFixture::default();
            let mut folder = Folder::new();
            folder
                .push_observed(&bytes[..split], Some(&observer))
                .unwrap();
            folder
                .push_observed(&bytes[split..], Some(&observer))
                .unwrap();
            let matches = folder.preview_matches();
            let response = folder.finish().unwrap();
            assert_eq!(response.text, "日本語");
            assert_eq!(response.usage.unwrap().total_tokens, 7);
            assert_eq!(matches, Some(delta == "日本語"));
            assert_eq!(*observer.deltas.lock().unwrap(), vec![delta.to_string()]);
        }
        let mut plain = Folder::new();
        plain.push(&bytes).unwrap();
        assert_eq!(plain.preview_matches(), None);
        assert_eq!(plain.finish().unwrap().text, "日本語");
    }
}

#[test]
fn p43_interleaved_preview_order_and_missing_delta_are_visible() {
    let observer = ObserverFixture::default();
    let mut folder = Folder::new();
    for (index, text) in [(1, "b"), (0, "a")] {
        folder
            .push_observed(
                &frame(
                    "response.output_text.delta",
                    serde_json::json!({"output_index":index,"content_index":0,"delta":text}),
                ),
                Some(&observer),
            )
            .unwrap();
    }
    for (index, text) in [(0, "a"), (1, "b")] {
        let mut item = message_item(text);
        item["output_index"] = serde_json::json!(index);
        folder
            .push_observed(&frame("response.output_item.done", item), Some(&observer))
            .unwrap();
    }
    assert_eq!(folder.preview_matches(), Some(false));
    folder
        .push(&frame("response.completed", serde_json::json!({})))
        .unwrap();
    assert_eq!(folder.finish().unwrap().text, "ab");
    let mut folder = Folder::new();
    folder
        .push_observed(
            &frame(
                "response.output_text.delta",
                serde_json::json!({"output_index":0,"content_index":0,"delta":"a"}),
            ),
            Some(&observer),
        )
        .unwrap();
    assert_eq!(folder.preview_matches(), Some(false));
}

#[test]
fn p43_text_toolcount_and_arguments_boundaries() {
    for size in [
        crate::MAX_TEXT_BYTES - 1,
        crate::MAX_TEXT_BYTES,
        crate::MAX_TEXT_BYTES + 1,
    ] {
        let mut folder = Folder::new();
        assert_eq!(
            folder
                .push(&frame(
                    "response.output_item.done",
                    message_item(&"x".repeat(size))
                ))
                .is_ok(),
            size <= crate::MAX_TEXT_BYTES
        );
    }
    for size in [
        crate::MAX_TEXT_BYTES - 1,
        crate::MAX_TEXT_BYTES,
        crate::MAX_TEXT_BYTES + 1,
    ] {
        let observer = ObserverFixture::default();
        let mut folder = Folder::new();
        let result = folder.push_observed(
            &frame(
                "response.output_text.delta",
                serde_json::json!({"output_index":0,"content_index":0,"delta":"x".repeat(size)}),
            ),
            Some(&observer),
        );
        assert_eq!(result.is_ok(), size <= crate::MAX_TEXT_BYTES);
    }
    for count in [
        crate::MAX_TOOL_CALLS - 1,
        crate::MAX_TOOL_CALLS,
        crate::MAX_TOOL_CALLS + 1,
    ] {
        let mut folder = Folder::new();
        let mut result = Ok(());
        for _ in 0..count {
            result = folder.push(&frame(
                "response.output_item.done",
                function_call_item("id", "read", "{}"),
            ));
        }
        assert_eq!(result.is_ok(), count <= crate::MAX_TOOL_CALLS);
    }
    for size in [
        crate::MAX_ARGUMENT_BYTES - 1,
        crate::MAX_ARGUMENT_BYTES,
        crate::MAX_ARGUMENT_BYTES + 1,
    ] {
        let raw = format!("\"{}\"", "x".repeat(size - 2));
        let mut folder = Folder::new();
        assert_eq!(
            folder
                .push(&frame(
                    "response.output_item.done",
                    function_call_item("id", "read", &raw)
                ))
                .is_ok(),
            size <= crate::MAX_ARGUMENT_BYTES
        );
    }
    let mut folder = Folder::new();
    folder.received_bytes = crate::MAX_RESPONSE_BYTES - 1;
    folder.push(b" ").unwrap();
    assert!(folder.push(b" ").is_err());
    assert!(folder.finish().is_err());
}

#[test]
fn p43_observer_failure_stops_before_done() {
    let observer = ObserverFixture {
        reject_delta: true,
        ..Default::default()
    };
    let mut folder = Folder::new();
    assert!(matches!(
        folder.push_observed(&observed_frames("a", "a"), Some(&observer)),
        Err(ProviderError::Observation {
            reason: crate::ObserverError::Full,
            completed: None
        })
    ));
    assert!(!folder.completed);
    assert!(folder.finish().is_err());
}

#[test]
fn a_text_only_stream_folds_into_text() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.output_item.done",
        message_item("42 lines"),
    ))
    .expect("push should succeed");
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    let r = f.finish().expect("should be complete");
    assert_eq!(r.text, "42 lines");
    assert!(r.tool_calls.is_empty());
}

#[test]
fn a_function_call_item_becomes_a_tool_call() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.output_item.done",
        function_call_item("call_9", "read", r#"{"path":"a.txt"}"#),
    ))
    .expect("push should succeed");
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    let r = f.finish().expect("should be complete");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].id, "call_9");
    assert_eq!(r.tool_calls[0].name, "read");
    assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
}

#[test]
fn a_reasoning_item_is_captured_with_its_encrypted_content() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.output_item.done",
        reasoning_item("r1", "opaque-blob"),
    ))
    .expect("push should succeed");
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    let r = f.finish().expect("should be complete");
    assert_eq!(r.reasoning.len(), 1);
    assert_eq!(r.reasoning[0].id, "r1");
    assert_eq!(r.reasoning[0].encrypted_content, "opaque-blob");
}

#[test]
fn a_reasoning_item_without_encrypted_content_is_skipped() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.output_item.done",
        serde_json::json!({ "item": { "type": "reasoning", "id": "r1", "summary": [] } }),
    ))
    .expect("push should succeed");
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    let r = f.finish().expect("should be complete");
    assert!(
        r.reasoning.is_empty(),
        "a reasoning item with no encrypted_content must not be kept"
    );
}

/// An empty string must be treated the same as absent, matching the
/// sibling `function_call` arm's `.filter(|s| !s.is_empty())` pattern.
/// Covers id-empty, encrypted_content-empty, and both-empty.
#[test]
fn a_reasoning_item_with_an_empty_id_or_encrypted_content_is_skipped() {
    for (id, encrypted_content) in [("", "blob"), ("r1", ""), ("", "")] {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_item.done",
            reasoning_item(id, encrypted_content),
        ))
        .expect("push should succeed");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");
        let r = f.finish().expect("should be complete");
        assert!(
            r.reasoning.is_empty(),
            "id={id:?} encrypted_content={encrypted_content:?} should have been skipped"
        );
    }
}

/// The result doesn't change no matter where the frame gets split.
/// Split tolerance is `sse.rs`'s responsibility, but whether this
/// path actually goes through it is confirmed separately here. If it
/// didn't, this would be redoing the reassembly on its own.
#[test]
fn a_stream_split_mid_frame_folds_the_same_way() {
    let whole: Vec<u8> = frame("response.output_item.done", message_item("split tolerance"))
        .into_iter()
        .chain(frame("response.completed", serde_json::json!({})))
        .collect();

    for cut in 1..whole.len() {
        let mut f = Folder::new();
        f.push(&whole[..cut]).expect("push should succeed");
        f.push(&whole[cut..]).expect("push should succeed");
        let r = f.finish().expect("should be complete");
        assert_eq!(r.text, "split tolerance", "broke when split at byte {cut}");
    }
}

/// A response that ended without ever seeing `response.completed`
/// must never be returned as a normal success. The agent loop reads
/// the absence of a tool call as "done," so it would silently return
/// an empty final answer.
#[test]
fn a_stream_that_never_completes_is_an_error() {
    let mut f = Folder::new();
    f.push(&frame("response.output_item.done", message_item("midway")))
        .expect("push should succeed");
    let err = f
        .finish()
        .expect_err("should fail since it never completed");
    assert!(
        matches!(err, ProviderError::Decode(_)),
        "not Decode: {err:?}"
    );
}

/// Even with an empty body, if it completed, it's a success. Don't
/// conflate an empty string with absence. This is the counterpart to
/// the test above — without it, an implementation that "always fails"
/// would pass.
#[test]
fn an_empty_but_completed_stream_is_a_success() {
    let mut f = Folder::new();
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    let r = f.finish().expect("should succeed since it completed");
    assert!(r.text.is_empty());
    assert!(r.tool_calls.is_empty());
}

#[test]
fn usage_is_captured_from_the_completed_event() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.completed",
        serde_json::json!({
            "response": {
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 8,
                    "total_tokens": 20
                }
            }
        }),
    ))
    .expect("push should succeed");

    let res = f.finish().expect("finish should succeed");
    let usage = res.usage.expect("usage should be present");
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 8);
    assert_eq!(usage.total_tokens, 20);
    assert_eq!(
        usage.cached_tokens, 0,
        "a completed event with no input_tokens_details must not fail to parse, just report 0"
    );
}

#[test]
fn cached_tokens_is_parsed_from_input_tokens_details() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.completed",
        serde_json::json!({
            "response": {
                "usage": {
                    "input_tokens": 9708,
                    "output_tokens": 167,
                    "total_tokens": 9875,
                    "input_tokens_details": {"cached_tokens": 5578}
                }
            }
        }),
    ))
    .expect("push should succeed");

    let res = f.finish().expect("finish should succeed");
    let usage = res.usage.expect("usage should be present");
    assert_eq!(usage.cached_tokens, 5578);
}

#[test]
fn a_completed_event_without_usage_yields_none_not_an_error() {
    let mut f = Folder::new();
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");

    let res = f.finish().expect("finish should succeed");
    assert!(res.usage.is_none());
}

#[test]
fn a_failed_response_carries_its_message() {
    let mut f = Folder::new();
    let err = f
        .push(&frame(
            "response.failed",
            serde_json::json!({ "response": { "error": { "message": "model overloaded" } } }),
        ))
        .expect_err("should fail");
    let ProviderError::Http(msg) = err else {
        panic!("not Http: {err:?}");
    };
    assert!(
        msg.contains("model overloaded"),
        "reason missing from message: {msg}"
    );
}

#[test]
fn a_cancelled_response_is_an_error() {
    let mut f = Folder::new();
    let err = f
        .push(&frame("response.cancelled", serde_json::json!({})))
        .expect_err("should fail");
    assert!(matches!(err, ProviderError::Http(_)), "not Http: {err:?}");
}

/// Delta events are skipped. Picking them up and piling them on
/// doubles the body text.
#[test]
fn delta_events_are_ignored() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.output_text.delta",
        serde_json::json!({ "delta": "duplicate" }),
    ))
    .expect("push should succeed");
    f.push(&frame(
        "response.output_item.done",
        message_item("duplicate"),
    ))
    .expect("push should succeed");
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    let r = f.finish().expect("should complete");
    assert_eq!(
        r.text, "duplicate",
        "picked up the delta and double-counted it"
    );
}

/// A call whose `arguments` is broken as JSON is stopped here, before
/// it reaches the dispatcher. Passing it through would look like "an
/// unknown argument," and the cause couldn't be traced back.
#[test]
fn a_function_call_with_broken_arguments_is_a_decode_error() {
    let mut f = Folder::new();
    let err = f
        .push(&frame(
            "response.output_item.done",
            function_call_item("c", "read", "{ not json"),
        ))
        .expect_err("should fail");
    assert!(
        matches!(err, ProviderError::Decode(_)),
        "not Decode: {err:?}"
    );
}

/// The `[DONE]` sentinel is not JSON. Don't crash trying to interpret
/// it.
#[test]
fn the_done_sentinel_is_not_parsed_as_json() {
    let mut f = Folder::new();
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    f.push(b"data: [DONE]\n\n")
        .expect("must not fail on the sentinel");
    let r = f.finish().expect("should complete");
    assert!(r.text.is_empty());
}

#[test]
fn a_user_message_becomes_an_input_text_item() {
    let items = input_items(&[Message::user("hello")]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[0]["content"][0]["type"], "input_text");
    assert_eq!(items[0]["content"][0]["text"], "hello");
}

#[test]
fn disabled_web_search_keeps_the_existing_body_exact() {
    let request = req();
    assert_eq!(
        build_body("gpt-6-astra", &request, Some("medium")),
        build_body_with_web_search(
            "gpt-6-astra",
            &request,
            Some("medium"),
            WebSearchConfig::disabled(),
        )
        .expect("disabled is a no-op"),
    );
}

#[test]
fn live_web_search_is_rejected_before_a_request_body_can_be_used() {
    let error = build_body_with_web_search(
        "gpt-6-astra",
        &req(),
        Some("medium"),
        WebSearchConfig::live(crate::web_search::WebSearchRequestCaps::new(2, 4096).expect("caps")),
    )
    .expect_err("the Codex endpoint is unverified");
    assert!(matches!(error, ProviderError::Unsupported(_)));
}

#[test]
fn an_assistant_message_becomes_an_output_text_item() {
    let items = input_items(&[Message::assistant("yes")]);
    assert_eq!(items[0]["role"], "assistant");
    assert_eq!(items[0]["content"][0]["type"], "output_text");
    assert_eq!(items[0]["content"][0]["text"], "yes");
}

/// A tool call becomes a `function_call`, and `arguments` is a string
/// holding JSON, not JSON itself. Sending it as a raw Value here
/// makes the server say the type is wrong and return a 400.
#[test]
fn a_tool_call_becomes_a_function_call_with_stringified_arguments() {
    let items = input_items(&[Message::assistant_with_tool_calls(
        "",
        vec![ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "Cargo.toml" }),
        }],
    )]);
    assert_eq!(
        items.len(),
        1,
        "an empty message got added even though the body was empty"
    );
    assert_eq!(items[0]["type"], "function_call");
    assert_eq!(items[0]["call_id"], "call_1");
    assert_eq!(items[0]["name"], "read");
    let raw = items[0]["arguments"]
        .as_str()
        .expect("arguments is not a string");
    let parsed: Value = serde_json::from_str(raw).expect("arguments is not JSON");
    assert_eq!(parsed["path"], "Cargo.toml");
}

/// A turn carrying both body text and tool calls emits the message
/// first and the function_call after. If the order were reversed, the
/// model would see its own call before its own utterance.
#[test]
fn a_turn_with_both_text_and_calls_emits_the_message_first() {
    let items = input_items(&[Message::assistant_with_tool_calls(
        "I'll read it",
        vec![ToolCall {
            id: "c".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        }],
    )]);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[1]["type"], "function_call");
}

#[test]
fn a_turn_with_reasoning_emits_it_before_the_message_and_calls() {
    let items = input_items(&[Message::assistant_with_tool_calls(
        "I'll read it",
        vec![ToolCall {
            id: "c".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        }],
    )
    .with_reasoning(vec![crate::ReasoningItem {
        id: "r1".into(),
        encrypted_content: "opaque".into(),
    }])]);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["type"], "reasoning");
    assert!(
        items[0].get("id").is_none(),
        "id must be stripped from the replayed reasoning item, matching upstream's \
             behavior when store: false"
    );
    assert_eq!(items[0]["encrypted_content"], "opaque");
    assert_eq!(items[0]["summary"], serde_json::json!([]));
    assert_eq!(items[1]["type"], "message");
    assert_eq!(items[2]["type"], "function_call");
}

#[test]
fn a_turn_without_reasoning_emits_no_reasoning_item() {
    let items = input_items(&[Message::assistant("yes")]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
}

/// `agent::run_loop`'s no-tool-calls branch can push a `Message` with
/// non-empty `reasoning` but empty content and empty `tool_calls` (a
/// text-free final turn). Emitting the reasoning item alone would put
/// a reasoning item on the wire with nothing following it - a shape
/// the Responses API rejects when `store: false`. It must be dropped
/// entirely, not just left orphaned.
#[test]
fn a_turn_with_reasoning_but_no_content_or_tool_calls_emits_nothing() {
    let items = input_items(&[
        Message::assistant("").with_reasoning(vec![crate::ReasoningItem {
            id: "r1".into(),
            encrypted_content: "opaque".into(),
        }]),
    ]);
    assert!(
        items.is_empty(),
        "an orphaned reasoning item was emitted with nothing following it: {items:?}"
    );
}

#[test]
fn a_tool_result_becomes_a_function_call_output() {
    let items = input_items(&[Message::tool_result("call_1", "42 lines")]);
    assert_eq!(items[0]["type"], "function_call_output");
    assert_eq!(items[0]["call_id"], "call_1");
    assert_eq!(items[0]["output"], "42 lines");
}

/// The Responses API's tool definitions are flat. Sending
/// `/chat/completions`'s `{"type":"function","function":{…}}` gets
/// rejected.
#[test]
fn tool_definitions_are_flat_not_nested() {
    let specs = polaris_tools::all_specs();
    let wire = tool_wire_shape(&specs);
    assert_eq!(wire.len(), specs.len());
    for (w, s) in wire.iter().zip(specs.iter()) {
        assert_eq!(w["type"], "function");
        assert_eq!(w["name"], s.name, "name is not placed flat");
        assert!(
            w.get("function").is_none(),
            "a nested function is still present"
        );
        assert!(w["description"].is_string());
        assert_eq!(w["parameters"], s.parameters);
    }
}

#[test]
fn the_body_carries_instructions_and_never_stores_state() {
    let req = CompletionRequest {
        system: "system".into(),
        messages: vec![Message::user("go")],
        tools: polaris_tools::all_specs(),
    };
    let body = build_body("gpt-5.3-codex", &req, None);

    assert_eq!(body["model"], "gpt-5.3-codex");
    assert_eq!(body["instructions"], "system");
    assert_eq!(
        body["store"], false,
        "the server is being made to hold conversation state"
    );
    assert_eq!(body["stream"], true);
    assert!(
        body.get("previous_response_id").is_none(),
        "conversation reuse is being used"
    );
    assert_eq!(
        body["input"]
            .as_array()
            .expect("input is not an array")
            .len(),
        1
    );
    assert_eq!(
        body["tools"]
            .as_array()
            .expect("tools is not an array")
            .len(),
        req.tools.len()
    );
    assert!(
        body.get("reasoning").is_none(),
        "reasoning is being sent even though effort wasn't passed"
    );
}

#[test]
fn compact_profile_is_wire_identical_to_the_existing_body() {
    let req = CompletionRequest {
        system: "system".into(),
        messages: vec![Message::user("go")],
        tools: polaris_tools::all_specs(),
    };
    let existing = build_body("gpt-6-astra", &req, Some("medium"));
    let instructions = cache_prefix::CachePrefixProfile::Compact.instructions("gpt-6-astra", &req);
    let profiled = build_body_with_instructions("gpt-6-astra", &req, &instructions, Some("medium"));
    assert_eq!(profiled, existing, "compact changed the existing wire body");
    assert!(profiled.get("cache_prefix").is_none());
    assert!(profiled.get("cache_prefix_version").is_none());
}

#[test]
fn stable_uses_a_distinct_key_that_holds_while_history_grows() {
    let turn = |n| CompletionRequest {
        system: "system".into(),
        messages: std::iter::once(Message::assistant_with_tool_calls(
            "read first",
            vec![ToolCall {
                id: "call_preserved".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "Cargo.toml"}),
            }],
        ))
        .chain(std::iter::once(Message::tool_result(
            "call_preserved",
            "contents",
        )))
        .chain((0..n).map(|i| Message::user(format!("turn {i}"))))
        .collect(),
        tools: polaris_tools::all_specs(),
    };
    let first = turn(1);
    let appended = turn(2);
    let compact = build_body("gpt-6-astra", &first, Some("medium"));
    let stable_first_instructions =
        cache_prefix::CachePrefixProfile::Stable.instructions("gpt-6-astra", &first);
    let stable_appended_instructions =
        cache_prefix::CachePrefixProfile::Stable.instructions("gpt-6-astra", &appended);
    let stable_first = build_body_with_instructions(
        "gpt-6-astra",
        &first,
        &stable_first_instructions,
        Some("medium"),
    );
    let stable_appended = build_body_with_instructions(
        "gpt-6-astra",
        &appended,
        &stable_appended_instructions,
        Some("medium"),
    );
    assert_ne!(
        stable_first["prompt_cache_key"],
        compact["prompt_cache_key"]
    );
    assert_eq!(
        stable_first["prompt_cache_key"], stable_appended["prompt_cache_key"],
        "appending history moved the stable cache key"
    );
    assert_eq!(stable_first["input"], first_body_input(&first));
    assert_eq!(stable_appended["input"], first_body_input(&appended));
    assert_eq!(stable_first["input"][1]["call_id"], "call_preserved");
    assert_eq!(stable_first["tools"], compact["tools"]);
    for body in [&stable_first, &stable_appended] {
        assert!(body.get("cache_prefix").is_none());
        assert!(body.get("cache_prefix_version").is_none());
        assert!(body.get("cache_prefix_hash").is_none());
        assert!(body.get("target_applied").is_none());
    }
}

#[test]
fn stable_skips_non_target_models_and_tool_less_requests() {
    let tools = polaris_tools::all_specs();
    let target = CompletionRequest {
        system: "system".into(),
        messages: vec![Message::user("go")],
        tools: tools.clone(),
    };
    let tool_less = CompletionRequest {
        system: "system".into(),
        messages: vec![Message::user("go")],
        tools: vec![],
    };
    for (model, req) in [
        ("gpt-6-astra-preview", &target),
        ("gpt-6-astra", &tool_less),
    ] {
        let instructions = cache_prefix::CachePrefixProfile::Stable.instructions(model, req);
        let profiled = build_body_with_instructions(model, req, &instructions, Some("medium"));
        assert_eq!(profiled, build_body(model, req, Some("medium")));
        assert!(
            !cache_prefix::CachePrefixProfile::Stable
                .diagnostics(model, req)
                .target_applied
        );
    }
}

fn first_body_input(req: &CompletionRequest) -> Value {
    Value::Array(input_items(&req.messages))
}

/// The point of the key: it must not move as the conversation grows.
/// A key derived from the messages would change every turn and route
/// each request to a fresh shard, which is exactly the 0% hit rate
/// this was added to fix. Pinning turn 1 against turn 5 is what
/// catches that regression; asserting the key merely exists would
/// not.
#[test]
fn the_cache_key_holds_still_while_the_conversation_grows() {
    let turn = |n: usize| CompletionRequest {
        system: "system".into(),
        messages: (0..n).map(|i| Message::user(format!("turn {i}"))).collect(),
        tools: polaris_tools::all_specs(),
    };
    let first = build_body("m", &turn(1), Some("high"));
    let fifth = build_body("m", &turn(5), Some("high"));

    assert!(
        first["prompt_cache_key"].is_string(),
        "no prompt_cache_key is being sent"
    );
    assert_eq!(
        first["prompt_cache_key"], fifth["prompt_cache_key"],
        "the key moved as the conversation grew, so every turn routes elsewhere"
    );
}

/// The other half of the pair. Everything the key covers is something
/// that breaks prefix reuse, so a request that changes one of them
/// must not be pointed at the shard holding the old prefix — it would
/// only evict it. Without this, a constant key would pass the test
/// above.
#[test]
fn anything_that_breaks_the_prefix_changes_the_cache_key() {
    let req = |system: &str, tools: Vec<polaris_tools::ToolSpec>| CompletionRequest {
        system: system.into(),
        messages: vec![Message::user("go")],
        tools,
    };
    let all = polaris_tools::all_specs();
    let base =
        build_body("m", &req("system", all.clone()), Some("high"))["prompt_cache_key"].clone();

    for (what, other) in [
        (
            "the model",
            build_body("other", &req("system", all.clone()), Some("high")),
        ),
        (
            "the effort",
            build_body("m", &req("system", all.clone()), Some("low")),
        ),
        (
            "the system prompt",
            build_body("m", &req("other", all.clone()), Some("high")),
        ),
        (
            "the tool list",
            build_body("m", &req("system", all[1..].to_vec()), Some("high")),
        ),
    ] {
        assert_ne!(
            base, other["prompt_cache_key"],
            "changing {what} left the cache key alone"
        );
    }
}

/// Not a cosmetic request to be shown the model's reasoning. Without
/// it the backend writes nothing to its prompt cache for a reasoning
/// model, and every turn pays full price for a prefix it already
/// sent — which is what polaris did until it was measured.
#[test]
fn the_body_asks_for_encrypted_reasoning_so_the_turn_is_cacheable() {
    let req = CompletionRequest {
        system: "s".into(),
        messages: vec![Message::user("x")],
        tools: vec![],
    };
    let body = build_body("m", &req, None);
    assert_eq!(
        body["include"],
        serde_json::json!(["reasoning.encrypted_content"]),
        "the prompt cache is being left switched off"
    );
}

/// When there isn't a single tool, `tools` isn't sent at all. Sending
/// an empty array could be taken as an instruction to "use no tools."
#[test]
fn an_empty_tool_list_is_omitted_rather_than_sent_empty() {
    let req = CompletionRequest {
        system: "s".into(),
        messages: vec![Message::user("x")],
        tools: vec![],
    };
    let body = build_body("m", &req, None);
    assert!(body.get("tools").is_none(), "an empty tools is being sent");
}

/// When `effort` is passed, it rides as `reasoning.effort`. The
/// counterpart to the fact that the `reasoning` key itself is absent
/// when it isn't passed. With only one half of this pair, either an
/// implementation that always emits reasoning, or one that always
/// omits it, would pass.
#[test]
fn an_effort_becomes_the_reasoning_field() {
    let req = CompletionRequest {
        system: "s".into(),
        messages: vec![Message::user("x")],
        tools: vec![],
    };
    let body = build_body("m", &req, Some("xhigh"));
    assert_eq!(body["reasoning"]["effort"], "xhigh");
}

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Tokens {
    calls: AtomicUsize,
    refreshes: AtomicUsize,
}

impl Tokens {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            refreshes: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl crate::TokenSource for Tokens {
    async fn token(&self) -> Result<crate::Token, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(crate::Token {
            access_token: "first".into(),
            account_id: "acct-1".into(),
            effort: None,
        })
    }
    async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(crate::Token {
            access_token: "second".into(),
            account_id: "acct-1".into(),
            effort: None,
        })
    }
}

fn req() -> CompletionRequest {
    CompletionRequest {
        system: "s".into(),
        messages: vec![Message::user("go")],
        tools: vec![],
    }
}

fn sse_body(frames: &[Vec<u8>]) -> String {
    frames
        .iter()
        .map(|f| String::from_utf8_lossy(f).to_string())
        .collect()
}

#[tokio::test]
async fn p43_codex_http_error_limits_and_read_deadlines() {
    for status in [401, 429, 500] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string("x".repeat(crate::MAX_ERROR_BYTES + 1)),
            )
            .mount(&server)
            .await;
        let tokens = Tokens::new();
        let provider = CodexProvider::new(server.uri(), "fixture".into(), tokens).unwrap();
        let result = provider.complete(req()).await;
        assert!(
            matches!(result, Err(ref e) if e.to_string().contains("limit")),
            "status={status}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
    for status in [200, 401, 429, 500] {
        let (url, worker) = crate::tests::raw_reply(status, None).await;
        let mut provider = CodexProvider::with_idle_timeout(
            url,
            "fixture".into(),
            Tokens::new(),
            Duration::from_millis(30),
        )
        .unwrap();
        provider.body_deadline = Duration::from_millis(30);
        let result = tokio::time::timeout(Duration::from_secs(1), provider.complete(req()))
            .await
            .unwrap();
        assert!(
            matches!(result, Err(ProviderError::Http(_))),
            "status={status}"
        );
        worker.await.unwrap();
    }
}

#[tokio::test]
async fn p43_rejected_codex_retains_received_usage() {
    for tail in [
        "data: {broken".to_string(),
        "data: {broken\n\n".to_string(),
        "x".repeat(crate::sse::MAX_SSE_EVENT_BYTES + 1),
    ] {
        let server = MockServer::start().await;
        let mut body = observed_frames("normal", "normal");
        body.extend_from_slice(tail.as_bytes());
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let meter = crate::UsageMeter::default();
        let provider = meter.wrap(Arc::new(
            CodexProvider::new(server.uri(), "fixture".into(), Tokens::new()).unwrap(),
        ));
        let result = provider.complete(req()).await;
        assert!(
            matches!(result, Err(ProviderError::ReceivedUsage { source, usage })
            if matches!(*source, ProviderError::Decode(_)) && usage.total_tokens == 7)
        );
        assert_eq!(meter.snapshot().failed_requests, 0);
        assert_eq!(meter.snapshot().usage.total_tokens, 7);
        assert_eq!(meter.snapshot().reported_responses, 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn p43_completed_usage_survives_body_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let worker = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![];
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        let headers = String::from_utf8(request).unwrap();
        let len: usize = headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        socket.read_exact(&mut vec![0; len]).await.unwrap();
        let body = observed_frames("normal", "normal");
        let header = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            body.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(&body).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
        // Keep EOF pending until the client's total body deadline closes it.
        let _ = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
            .await
            .unwrap();
    });
    let mut provider = CodexProvider::new(endpoint, "fixture".into(), Tokens::new()).unwrap();
    provider.body_deadline = Duration::from_millis(50);
    let meter = crate::UsageMeter::default();
    let result = meter.wrap(Arc::new(provider)).complete(req()).await;
    assert!(
        matches!(result, Err(ProviderError::ReceivedUsage { source, usage })
            if matches!(*source, ProviderError::Http(_)) && usage.total_tokens == 7)
    );
    assert_eq!(meter.snapshot().usage.total_tokens, 7);
    assert_eq!(meter.snapshot().reported_responses, 1);
    assert_eq!(meter.snapshot().failed_requests, 0);
    worker.await.unwrap();
}

#[tokio::test]
async fn p43_codex_observer_meter_and_wire_compatibility() {
    for mode in [
        "none",
        "observe",
        "mismatch",
        "reject_delta",
        "reject_final",
    ] {
        let server = MockServer::start().await;
        let body = observed_frames(
            if mode == "mismatch" {
                "wrong"
            } else {
                "normal"
            },
            "normal",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let ledger = AttemptLedger::default();
        let provider = Arc::new(
            CodexProvider::new(server.uri(), "fixture".into(), Tokens::new())
                .unwrap()
                .with_attempt_ledger(ledger.clone()),
        );
        let meter = crate::UsageMeter::default();
        let metered = meter.wrap(provider);
        let observer = ObserverFixture {
            reject_delta: mode == "reject_delta",
            reject_final: mode == "reject_final",
            ..Default::default()
        };
        let hook: Option<&dyn crate::ResponseObserver> = (mode != "none").then_some(&observer);
        let result = metered
            .complete_in_turn_with_observer(req(), &TurnContext::new(), hook)
            .await;
        if mode == "reject_delta" {
            assert!(matches!(
                result,
                Err(ProviderError::Observation {
                    completed: None,
                    ..
                })
            ));
            assert_eq!(meter.snapshot().failed_requests, 1);
            assert_eq!(meter.snapshot().reported_responses, 0);
        } else {
            if mode == "reject_final" {
                assert!(
                    matches!(result, Err(ProviderError::Observation { completed: Some(ref response), .. }) if response.text == "normal" && response.usage.unwrap().total_tokens == 7)
                );
            } else {
                assert_eq!(result.unwrap().text, "normal");
            }
            assert_eq!(meter.snapshot().usage.total_tokens, 7);
            assert_eq!(meter.snapshot().reported_responses, 1);
            assert_eq!(meter.snapshot().failed_requests, 0);
        }
        if mode == "observe" || mode == "mismatch" {
            assert_eq!(
                *observer.finals.lock().unwrap(),
                vec![("normal".into(), Some(mode == "observe"))]
            );
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].body_json::<Value>().unwrap(),
            build_body("fixture", &req(), None)
        );
        let attempts = ledger.snapshot();
        assert_eq!(attempts.len(), 1);
        if mode == "reject_delta" {
            assert_eq!(attempts[0].status, crate::attempts::AttemptStatus::Failed);
            assert_eq!(
                attempts[0].usage,
                crate::attempts::UsageObservation::Missing
            );
        } else {
            assert_eq!(
                attempts[0].status,
                crate::attempts::AttemptStatus::Succeeded
            );
            assert_eq!(
                attempts[0].usage,
                crate::attempts::UsageObservation::Known {
                    input_tokens: 3,
                    output_tokens: 4,
                    total_tokens: 7,
                    cached_tokens: 0,
                }
            );
        }
    }
}

/// The headers and body sent out must match the spec. If this is
/// wrong, no matter how correctly the response gets interpreted, the
/// server won't even talk to us.
#[tokio::test]
async fn the_request_carries_the_bearer_and_the_account_id() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer first"))
        .and(header("chatgpt-account-id", "acct-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
            frame("response.output_item.done", message_item("ok")),
            frame("response.completed", serde_json::json!({})),
        ])))
        .mount(&s)
        .await;

    let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new()).expect("client");
    let r = p.complete(req()).await.expect("should succeed");
    assert_eq!(r.text, "ok");
}

#[tokio::test]
async fn visible_request_limit_rejects_oversize_before_attempt_or_send() {
    let server = MockServer::start().await;
    let ledger = crate::attempts::AttemptLedger::default();
    let provider = CodexProvider::new(server.uri(), "m".into(), Tokens::new())
        .expect("client")
        .with_attempt_ledger(ledger.clone())
        .with_visible_request_byte_limit(1);

    let error = provider
        .complete(req())
        .await
        .expect_err("body exceeds one byte");
    assert!(matches!(error, ProviderError::Budget(_)));
    assert!(
        ledger.snapshot().is_empty(),
        "oversize body creates no attempt"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "oversize body sends nothing"
    );
}

#[tokio::test]
async fn visible_request_limit_leaves_a_tiny_valid_body_unchanged() {
    let baseline_server = MockServer::start().await;
    let limited_server = MockServer::start().await;
    for server in [&baseline_server, &limited_server] {
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
                frame("response.output_item.done", message_item("ok")),
                frame("response.completed", serde_json::json!({})),
            ])))
            .mount(server)
            .await;
    }

    CodexProvider::new(baseline_server.uri(), "m".into(), Tokens::new())
        .expect("client")
        .complete(req())
        .await
        .expect("baseline request succeeds");
    CodexProvider::new(limited_server.uri(), "m".into(), Tokens::new())
        .expect("client")
        .with_visible_request_byte_limit(32_000)
        .complete(req())
        .await
        .expect("limited request succeeds");

    let baseline = baseline_server.received_requests().await.expect("recorded");
    let limited = limited_server.received_requests().await.expect("recorded");
    assert_eq!(baseline.len(), 1);
    assert_eq!(limited.len(), 1);
    assert_eq!(baseline[0].body, limited[0].body);
}

/// `set_effort` must override the account-plan-derived effort
/// (`Tokens` here always returns `effort: None`, so any effort seen
/// on the wire had to come from the override, not the token).
#[tokio::test]
async fn set_effort_overrides_the_plan_derived_effort() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
            frame("response.output_item.done", message_item("ok")),
            frame("response.completed", serde_json::json!({})),
        ])))
        .mount(&s)
        .await;

    let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new()).expect("client");
    p.set_effort(Some("xhigh"));
    p.complete(req()).await.expect("should succeed");

    let received = s.received_requests().await.expect("recorded");
    let body: Value = received[0].body_json().expect("json");
    assert_eq!(body["reasoning"]["effort"], "xhigh");
}

/// On receiving a 401, refresh and retry exactly once, and succeed.
#[tokio::test]
async fn a_401_is_retried_once_with_a_refreshed_token() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer first"))
        .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer second"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
            frame(
                "response.output_item.done",
                message_item("succeeded on retry"),
            ),
            frame("response.completed", serde_json::json!({})),
        ])))
        .mount(&s)
        .await;

    let t = Tokens::new();
    let ledger = crate::attempts::AttemptLedger::default();
    let p = CodexProvider::new(s.uri(), "m".into(), t.clone())
        .expect("client")
        .with_attempt_ledger(ledger.clone());
    let r = p.complete(req()).await.expect("should succeed on retry");
    assert_eq!(r.text, "succeeded on retry");
    assert_eq!(
        t.refreshes.load(Ordering::SeqCst),
        1,
        "the number of refreshes isn't 1"
    );
    let attempts = ledger.snapshot();
    assert_eq!(attempts.len(), 2, "401 retry is two physical sends");
    assert_eq!(attempts[0].logical_id, attempts[1].logical_id);
    assert_eq!(attempts[0].status, crate::attempts::AttemptStatus::Failed);
    assert_eq!(
        attempts[1].status,
        crate::attempts::AttemptStatus::Succeeded
    );
    assert_eq!(
        attempts[1].retry_reason.as_deref(),
        Some("401 unauthorized")
    );
    assert_eq!(
        attempts[1].usage,
        crate::attempts::UsageObservation::Missing
    );
}

#[tokio::test]
async fn ledger_marks_a_sent_future_cancelled_and_ignores_an_unpolled_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(200))
                .set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
        )
        .mount(&server)
        .await;

    let ledger = crate::attempts::AttemptLedger::default();
    let provider = CodexProvider::new(server.uri(), "m".into(), Tokens::new())
        .expect("client")
        .with_attempt_ledger(ledger.clone());
    let never_polled = provider.complete(req());
    drop(never_polled);
    assert!(ledger.snapshot().is_empty(), "unpolled futures do not send");

    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            provider.complete(req())
        )
        .await
        .is_err()
    );
    let attempts = ledger.snapshot();
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].status,
        crate::attempts::AttemptStatus::Cancelled
    );
    assert_eq!(
        attempts[0].usage,
        crate::attempts::UsageObservation::Missing
    );
}

#[tokio::test]
async fn cache_pacing_preserves_wire_body_and_cancelled_wait_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
            frame("response.output_item.done", message_item("ok")),
            frame("response.completed", serde_json::json!({})),
        ])))
        .mount(&server)
        .await;
    let mut off = CodexProvider::new(server.uri(), "gpt-6-astra".into(), Tokens::new()).unwrap();
    off.pacing = PacingGate::new(PacingMode::Off);
    off.set_effort(Some("medium"));
    let mut on = CodexProvider::new(server.uri(), "gpt-6-astra".into(), Tokens::new()).unwrap();
    on.pacing = PacingGate::with_interval(PacingMode::On, std::time::Duration::from_millis(100));
    on.set_effort(Some("medium"));
    off.complete(req()).await.unwrap();
    on.complete(req()).await.unwrap();
    // Timeout owns and drops the actual request future, releasing its gate guard.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(5), on.complete(req()))
            .await
            .is_err()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    tokio::time::timeout(std::time::Duration::from_secs(1), on.complete(req()))
        .await
        .unwrap()
        .unwrap();
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 3);
    assert!(received.windows(2).all(|pair| pair[0].body == pair[1].body));
    assert!(
        received
            .iter()
            .all(|r| !r.headers.contains_key("x-codex-turn-state"))
    );
}

#[tokio::test]
async fn cache_pacing_applies_again_to_the_single_auth_retry() {
    let server = MockServer::start().await;
    let starts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = starts.clone();
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(move |request: &wiremock::Request| {
            observed.lock().unwrap().push(std::time::Instant::now());
            if request.headers.get("authorization").unwrap() == "Bearer first" {
                ResponseTemplate::new(401)
            } else {
                ResponseTemplate::new(200).set_body_string(sse_body(&[
                    frame("response.output_item.done", message_item("ok")),
                    frame("response.completed", serde_json::json!({})),
                ]))
            }
        })
        .mount(&server)
        .await;
    let tokens = Tokens::new();
    let mut provider = CodexProvider::new(server.uri(), "m".into(), tokens.clone()).unwrap();
    provider.pacing =
        PacingGate::with_interval(PacingMode::On, std::time::Duration::from_millis(100));
    provider.complete(req()).await.unwrap();
    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
    let starts = starts.lock().unwrap();
    assert_eq!(starts.len(), 2);
    // Allow local transport scheduling jitter; gate unit tests check the exact boundary.
    assert!(starts[1].duration_since(starts[0]) >= std::time::Duration::from_millis(80));
}

/// A readonly auth source rejects the refresh step after a 401. The
/// provider must return that error directly: a second Responses request
/// would reuse credentials that were already refused. The auth crate
/// separately verifies that its readonly source makes no token POST or
/// credential-store write before returning this error.
#[tokio::test]
async fn a_401_with_a_rejected_refresh_does_not_repeat_the_model_request() {
    struct ReadOnlyTokens {
        refreshes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::TokenSource for ReadOnlyTokens {
        async fn token(&self) -> Result<crate::Token, ProviderError> {
            Ok(crate::Token {
                access_token: "first".into(),
                account_id: "acct-1".into(),
                effort: None,
            })
        }

        async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::Auth(
                "authentication refresh is disabled".into(),
            ))
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
        .mount(&server)
        .await;

    let tokens = Arc::new(ReadOnlyTokens {
        refreshes: AtomicUsize::new(0),
    });
    let provider = CodexProvider::new(server.uri(), "m".into(), tokens.clone()).expect("client");
    let error = provider
        .complete(req())
        .await
        .expect_err("readonly refresh rejection must stop the request");

    assert!(
        matches!(error, ProviderError::Auth(_)),
        "not Auth: {error:?}"
    );
    assert_eq!(
        tokens.refreshes.load(Ordering::SeqCst),
        1,
        "the provider must ask the auth source exactly once"
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("request recording")
            .len(),
        1,
        "a rejected refresh must not repeat the model request"
    );
}

/// If 401 happens twice in a row, give up. Never retry indefinitely.
/// The kind is Auth, not Http.
#[tokio::test]
async fn a_second_401_gives_up_as_an_auth_error() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
        .mount(&s)
        .await;

    let t = Tokens::new();
    let p = CodexProvider::new(s.uri(), "m".into(), t.clone()).expect("client");
    let err = p.complete(req()).await.expect_err("should fail");
    assert!(matches!(err, ProviderError::Auth(_)), "not Auth: {err:?}");
    assert_eq!(
        t.refreshes.load(Ordering::SeqCst),
        1,
        "the retry didn't stop at 1"
    );
}

/// What the test above confirms is only that "the result is Auth,"
/// not that "it finishes quickly." A mutation that degrades
/// `complete`'s retry into a `loop` still compiles and would just
/// hang the test above forever, never showing up as a red X. Here we
/// bound it by time and additionally count how many requests the
/// server actually received, to detect a degradation into a loop both
/// fast and reliably. If it were looping, more than 2 requests should
/// arrive within 500ms.
#[tokio::test]
async fn a_second_401_stops_retrying_within_a_time_bound() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
        .mount(&s)
        .await;

    let t = Tokens::new();
    let p = CodexProvider::new(s.uri(), "m".into(), t.clone()).expect("client");

    let bound = Duration::from_millis(500);
    let outcome = tokio::time::timeout(bound, p.complete(req())).await;

    // wiremock has request recording on by default. Even if the
    // timeout fired, the count received up to that point is still
    // meaningful (if it were looping, it should exceed 2).
    let received = s
        .received_requests()
        .await
        .expect("request recording should be on by default")
        .len();

    let mut failures = Vec::new();
    if outcome.is_err() {
        failures.push(format!(
            "did not finish within {bound:?} ({received} received so far). the retry may be looping"
        ));
    }
    if received != 2 {
        failures.push(format!(
            "did not stop at exactly 2 (initial + 1 retry) ({received} received)"
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

/// A 500 is not Auth. Keep the retry entry point exclusive to 401. If
/// this breaks down, every transient server failure would call
/// `refreshed()` and burn a token.
#[tokio::test]
async fn a_500_response_does_not_trigger_the_refresh_and_retry_path() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&s)
        .await;

    let t = Tokens::new();
    let p = CodexProvider::new(s.uri(), "m".into(), t.clone()).expect("client");
    let err = p.complete(req()).await.expect_err("should fail");
    assert!(matches!(err, ProviderError::Http(_)), "not Http: {err:?}");
    assert_eq!(
        t.refreshes.load(Ordering::SeqCst),
        0,
        "a refresh (retry) happened even though it was a 500"
    );
}

/// A 429 includes the reset information in its wording. A rejection
/// you can't act on just makes the same failure repeat.
#[tokio::test]
async fn a_429_surfaces_the_retry_hint() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "37")
                .set_body_string("rate limited"),
        )
        .mount(&s)
        .await;

    let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new()).expect("client");
    let err = p.complete(req()).await.expect_err("should fail");
    let ProviderError::Http(msg) = err else {
        panic!("not Http: {err:?}");
    };
    assert!(
        msg.contains("37"),
        "retry-after missing from message: {msg}"
    );
}

/// A stream that was cut off without ever seeing completion is a
/// failure. HTTP is 200, so letting this through would return an
/// empty final answer.
#[tokio::test]
async fn a_truncated_stream_is_an_error_even_on_200() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
            "response.output_item.done",
            message_item("cut off midway"),
        )])))
        .mount(&s)
        .await;

    let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new()).expect("client");
    let err = p.complete(req()).await.expect_err("should fail");
    assert!(
        matches!(err, ProviderError::Decode(_)),
        "not Decode: {err:?}"
    );
}

/// The moment a token can't be obtained, it's Auth. Never reaches the
/// network.
#[tokio::test]
async fn a_token_source_failure_is_an_auth_error() {
    struct NoTokens;
    #[async_trait::async_trait]
    impl crate::TokenSource for NoTokens {
        async fn token(&self) -> Result<crate::Token, ProviderError> {
            Err(ProviderError::Auth("not logged in".into()))
        }
        async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
            Err(ProviderError::Auth("not logged in".into()))
        }
    }

    let p = CodexProvider::new(
        "http://127.0.0.1:1/unreachable".into(),
        "m".into(),
        Arc::new(NoTokens),
    )
    .expect("client");
    let err = p.complete(req()).await.expect_err("should fail");
    assert!(matches!(err, ProviderError::Auth(_)), "not Auth: {err:?}");
}

fn affinity_provider(base: String, mode: TurnAffinityMode) -> CodexProvider {
    CodexProvider::with_idle_timeout_and_affinity(
        base,
        "m".into(),
        Tokens::new(),
        Duration::from_secs(1),
        mode,
    )
    .expect("client")
}

/// A state first observed after `response.completed` stays within its
/// context. A fresh context, even on the same shared provider, has none.
#[tokio::test]
async fn turn_affinity_is_shared_only_within_one_context() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "opaque-state")
                .set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
        )
        .mount(&server)
        .await;
    let provider = affinity_provider(server.uri(), TurnAffinityMode::On);
    let turn = TurnContext::new();
    provider
        .complete_in_turn(req(), &turn)
        .await
        .expect("first response");
    provider
        .complete_in_turn(req(), &turn)
        .await
        .expect("continuation");
    provider
        .complete_in_turn(req(), &TurnContext::new())
        .await
        .expect("new turn");

    let requests = server.received_requests().await.expect("recorded");
    assert!(requests[0].headers.get("x-codex-turn-state").is_none());
    assert_eq!(
        requests[1]
            .headers
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok()),
        Some("opaque-state")
    );
    assert!(requests[2].headers.get("x-codex-turn-state").is_none());
}

/// Absence is ordinary success, but must never manufacture a value for a
/// later tool continuation.
#[tokio::test]
async fn missing_turn_state_is_never_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
            "response.completed",
            serde_json::json!({}),
        )])))
        .mount(&server)
        .await;
    let provider = affinity_provider(server.uri(), TurnAffinityMode::On);
    let turn = TurnContext::new();
    provider
        .complete_in_turn(req(), &turn)
        .await
        .expect("first response");
    provider
        .complete_in_turn(req(), &turn)
        .await
        .expect("continuation");
    for request in server.received_requests().await.expect("recorded") {
        assert!(request.headers.get("x-codex-turn-state").is_none());
    }
}

/// Header transport is the only on/off difference. The JSON body still
/// contains the real six-tool catalog, encrypted reasoning and a tool
/// output byte-for-byte unchanged.
#[tokio::test]
async fn turn_affinity_does_not_change_outbound_body_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "opaque-state")
                .set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
        )
        .mount(&server)
        .await;
    let request = || CompletionRequest {
        system: "s".into(),
        messages: vec![
            Message::assistant("thinking").with_reasoning(vec![crate::ReasoningItem {
                id: "reasoning-id".into(),
                encrypted_content: "encrypted-reasoning".into(),
            }]),
            Message::tool_result("call-1", "tool-output"),
        ],
        tools: polaris_tools::all_specs(),
    };
    assert_eq!(
        request().tools.len(),
        6,
        "the actual always-on catalog has six tools"
    );
    let off_turn = TurnContext::new();
    let off = affinity_provider(server.uri(), TurnAffinityMode::Off);
    off.complete_in_turn(request(), &off_turn)
        .await
        .expect("off initial response");
    off.complete_in_turn(request(), &off_turn)
        .await
        .expect("off continuation");
    let on_turn = TurnContext::new();
    let on = affinity_provider(server.uri(), TurnAffinityMode::On);
    on.complete_in_turn(request(), &on_turn)
        .await
        .expect("on initial response");
    on.complete_in_turn(request(), &on_turn)
        .await
        .expect("on continuation");
    let requests = server.received_requests().await.expect("recorded");
    assert!(requests[1].headers.get("x-codex-turn-state").is_none());
    assert!(requests[3].headers.get("x-codex-turn-state").is_some());
    assert_eq!(requests[1].body, requests[3].body);
}

fn sequence_responder(
    responses: Vec<ResponseTemplate>,
) -> impl Fn(&wiremock::Request) -> ResponseTemplate + Send + Sync {
    let responses = Arc::new(std::sync::Mutex::new(responses));
    move |_| responses.lock().expect("responses").remove(0)
}

#[tokio::test]
async fn concurrent_contexts_keep_distinct_server_states_on_one_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sequence_responder(vec![
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "state-a")
                .set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "state-b")
                .set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
            ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                "response.completed",
                serde_json::json!({}),
            )])),
            ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                "response.completed",
                serde_json::json!({}),
            )])),
        ]))
        .mount(&server)
        .await;
    let provider = Arc::new(affinity_provider(server.uri(), TurnAffinityMode::On));
    let a = Arc::new(TurnContext::new());
    let b = Arc::new(TurnContext::new());
    let (ra, rb) = tokio::join!(
        provider.complete_in_turn(req(), &a),
        provider.complete_in_turn(req(), &b)
    );
    ra.expect("a initial");
    rb.expect("b initial");
    let (ra, rb) = tokio::join!(
        provider.complete_in_turn(req(), &a),
        provider.complete_in_turn(req(), &b)
    );
    ra.expect("a continuation");
    rb.expect("b continuation");
    let requests = server.received_requests().await.expect("recorded");
    let sent: std::collections::BTreeSet<_> = requests[2..]
        .iter()
        .map(|r| {
            r.headers
                .get("x-codex-turn-state")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        sent,
        ["state-a".to_string(), "state-b".to_string()]
            .into_iter()
            .collect()
    );
}

#[tokio::test]
async fn failed_cancelled_decode_and_empty_headers_never_replace_state() {
    let cases = vec![
        ResponseTemplate::new(200)
            .insert_header("x-codex-turn-state", "new")
            .set_body_string(sse_body(&[frame("response.failed", serde_json::json!({}))])),
        ResponseTemplate::new(200)
            .insert_header("x-codex-turn-state", "new")
            .set_body_string(sse_body(&[frame(
                "response.cancelled",
                serde_json::json!({}),
            )])),
        ResponseTemplate::new(200)
            .insert_header("x-codex-turn-state", "new")
            .set_body_string("data: not-json\n\n"),
        ResponseTemplate::new(200)
            .insert_header("x-codex-turn-state", "")
            .set_body_string(sse_body(&[frame(
                "response.completed",
                serde_json::json!({}),
            )])),
    ];
    for bad in cases {
        // First-state-wins alone would mask a bug that saves a failed
        // response's header. Exercise an initially empty context too.
        let fresh_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sequence_responder(vec![
                bad.clone(),
                ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
            ]))
            .mount(&fresh_server)
            .await;
        let fresh_provider = affinity_provider(fresh_server.uri(), TurnAffinityMode::On);
        let fresh_turn = TurnContext::new();
        let _ = fresh_provider.complete_in_turn(req(), &fresh_turn).await;
        fresh_provider
            .complete_in_turn(req(), &fresh_turn)
            .await
            .expect("after initial bad response");
        let fresh_requests = fresh_server.received_requests().await.expect("recorded");
        assert!(
            fresh_requests[1]
                .headers
                .get("x-codex-turn-state")
                .is_none()
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sequence_responder(vec![
                ResponseTemplate::new(200)
                    .insert_header("x-codex-turn-state", "old")
                    .set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
                bad,
                ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
            ]))
            .mount(&server)
            .await;
        let provider = affinity_provider(server.uri(), TurnAffinityMode::On);
        let turn = TurnContext::new();
        provider
            .complete_in_turn(req(), &turn)
            .await
            .expect("initial");
        let _ = provider.complete_in_turn(req(), &turn).await;
        provider
            .complete_in_turn(req(), &turn)
            .await
            .expect("after bad response");
        let requests = server.received_requests().await.expect("recorded");
        assert_eq!(
            requests[2]
                .headers
                .get("x-codex-turn-state")
                .and_then(|v| v.to_str().ok()),
            Some("old")
        );
    }
}

#[tokio::test]
async fn refresh_keeps_state_only_when_the_account_is_unchanged() {
    struct RefreshTokens {
        changed: bool,
    }
    #[async_trait::async_trait]
    impl crate::TokenSource for RefreshTokens {
        async fn token(&self) -> Result<crate::Token, ProviderError> {
            Ok(crate::Token {
                access_token: "first".into(),
                account_id: "a".into(),
                effort: None,
            })
        }
        async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
            Ok(crate::Token {
                access_token: "second".into(),
                account_id: if self.changed { "b" } else { "a" }.into(),
                effort: None,
            })
        }
    }
    for changed in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sequence_responder(vec![
                ResponseTemplate::new(200)
                    .insert_header("x-codex-turn-state", "old")
                    .set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
                ResponseTemplate::new(401),
                ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
            ]))
            .mount(&server)
            .await;
        let provider = CodexProvider::with_idle_timeout_and_affinity(
            server.uri(),
            "m".into(),
            Arc::new(RefreshTokens { changed }),
            Duration::from_secs(1),
            TurnAffinityMode::On,
        )
        .expect("client");
        let turn = TurnContext::new();
        provider
            .complete_in_turn(req(), &turn)
            .await
            .expect("initial");
        provider
            .complete_in_turn(req(), &turn)
            .await
            .expect("refresh retry");
        let requests = server.received_requests().await.expect("recorded");
        assert_eq!(
            requests[1]
                .headers
                .get("x-codex-turn-state")
                .and_then(|v| v.to_str().ok()),
            Some("old")
        );
        assert_eq!(
            requests[2].headers.get("x-codex-turn-state").is_some(),
            !changed
        );
    }
}
