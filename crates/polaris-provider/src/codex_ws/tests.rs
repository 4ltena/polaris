//! The two decisions that decide whether a continuation is safe to use.
use super::*;
use serde_json::json;

/// The server holds what it was sent **and what it produced**. A delta taken
/// against the sent items alone would resend the model's own answers on every
/// turn — correct output, silently paid for twice.
#[test]
fn the_delta_is_measured_against_what_the_server_already_holds() {
    let baseline = vec![
        json!({"role": "user", "text": "one"}),
        json!({"type": "output"}),
    ];
    let input = vec![
        json!({"role": "user", "text": "one"}),
        json!({"type": "output"}),
        json!({"role": "user", "text": "two"}),
    ];
    assert_eq!(
        delta(&baseline, &input),
        Some(vec![json!({"role": "user", "text": "two"})])
    );

    // Nothing new is legal: the first request after a warm-up carries no
    // items and relies entirely on the continuation.
    assert_eq!(delta(&baseline, &baseline), Some(vec![]));
}

/// A history that was rewritten, reordered or trimmed is not the history the
/// server holds. Sending a delta against it would leave the model reasoning
/// from a conversation that never happened.
#[test]
fn a_rewritten_history_refuses_the_continuation() {
    let baseline = vec![json!({"a": 1}), json!({"b": 2})];
    // Rewritten in place.
    assert_eq!(delta(&baseline, &[json!({"a": 1}), json!({"b": 99})]), None);
    // Shorter than the baseline: compaction, or a dropped item.
    assert_eq!(delta(&baseline, &[json!({"a": 1})]), None);
    // Reordered.
    assert_eq!(delta(&baseline, &[json!({"b": 2}), json!({"a": 1})]), None);
}

/// The continuation stands for a context the server built under one set of
/// instructions, tools and weights. Continuing from it under another is
/// splicing two conversations together.
#[test]
fn a_different_request_shape_is_a_different_conversation() {
    let body = json!({
        "model": "gpt-6-astra",
        "instructions": "be careful",
        "tools": [{"name": "read"}],
        "reasoning": {"effort": "medium"},
        "prompt_cache_key": "polaris-abcd",
        "input": [json!({"x": 1})],
    });
    let held = Shape::of(&body);
    assert_eq!(held, Shape::of(&body.clone()));

    for (field, value) in [
        ("model", json!("gpt-5.6-luna")),
        ("instructions", json!("be quick")),
        ("tools", json!([{"name": "write"}])),
        ("reasoning", json!({"effort": "xhigh"})),
        ("prompt_cache_key", json!("polaris-efgh")),
    ] {
        let mut other = body.clone();
        other[field] = value;
        assert_ne!(
            held,
            Shape::of(&other),
            "{field} must break the continuation"
        );
    }

    // `input` grows on every request and is compared as a prefix instead.
    // Including it here would refuse every continuation ever offered.
    let mut grown = body.clone();
    grown["input"] = json!([{"x": 1}, {"x": 2}]);
    assert_eq!(held, Shape::of(&grown));
}

/// The server records an `id` it issued and a `status` it decided; polaris
/// renders its history from its own types and sends neither. Comparing the
/// raw items finds a difference on the first tool call every time, so the
/// continuation is refused on every request — correct, and silently
/// expensive. Observed on 2026-09-18 on the first real socket run.
#[test]
fn the_fields_only_the_server_carries_do_not_refuse_the_continuation() {
    let from_server = json!({
        "type": "function_call",
        "name": "read",
        "call_id": "call_abc",
        "arguments": "{\"path\":\"a\"}",
        "id": "fc_0123",
        "status": "completed",
    });
    let from_polaris = json!({
        "type": "function_call",
        "name": "read",
        "call_id": "call_abc",
        "arguments": "{\"path\":\"a\"}",
    });
    assert_eq!(
        delta(
            std::slice::from_ref(&from_server),
            &[from_polaris.clone(), json!({"next": 1})]
        ),
        Some(vec![json!({"next": 1})])
    );

    // Everything else still refuses. Dropping the two fields must not turn
    // into "close enough".
    let mut other_call = from_polaris.clone();
    other_call["call_id"] = json!("call_xyz");
    assert_eq!(
        delta(std::slice::from_ref(&from_server), &[other_call]),
        None
    );
    let mut other_args = from_polaris;
    other_args["arguments"] = json!("{\"path\":\"b\"}");
    assert_eq!(delta(&[from_server], &[other_args]), None);
}

#[test]
fn a_confined_run_dials_the_proxy_it_was_given() {
    // HTTPS_PROXY wins over ALL_PROXY, the scheme is stripped, and a blank
    // value is not a proxy.
    assert_eq!(
        super::proxy_for("chatgpt.com", Some("http://127.0.0.1:8080"), None, None).as_deref(),
        Some("127.0.0.1:8080")
    );
    assert_eq!(
        super::proxy_for("chatgpt.com", None, Some("127.0.0.1:9/"), None).as_deref(),
        Some("127.0.0.1:9")
    );
    assert_eq!(
        super::proxy_for("chatgpt.com", Some("  "), Some("127.0.0.1:9"), None).as_deref(),
        Some("127.0.0.1:9")
    );
    assert_eq!(super::proxy_for("chatgpt.com", None, None, None), None);
}

#[test]
fn no_proxy_keeps_a_named_host_direct() {
    let proxy = Some("127.0.0.1:8080");
    assert_eq!(
        super::proxy_for("chatgpt.com", proxy, None, Some("chatgpt.com")),
        None
    );
    assert_eq!(
        super::proxy_for("api.chatgpt.com", proxy, None, Some("chatgpt.com")),
        None
    );
    assert_eq!(
        super::proxy_for("chatgpt.com", proxy, None, Some("*")),
        None
    );
    // A different host is still proxied, and an empty list exempts nothing.
    assert!(super::proxy_for("example.com", proxy, None, Some("chatgpt.com")).is_some());
    assert!(super::proxy_for("chatgpt.com", proxy, None, Some("")).is_some());
    // Not a suffix match on a bare substring.
    assert!(super::proxy_for("notchatgpt.com", proxy, None, Some("chatgpt.com")).is_some());
}

/// Parent and subagents share one provider handle, and a tier-routed turn
/// asks a different model, so several shapes arrive on one socket. A single
/// slot made each one evict the last: the parent's next turn then found the
/// child's continuation, failed the shape check, and re-sent everything.
#[test]
fn a_second_shape_does_not_evict_the_first() {
    let shape = |model: &str| {
        Shape::of(&json!({
            "model": model,
            "instructions": "be careful",
            "tools": [{"name": "read"}],
            "reasoning": {"effort": "medium"},
            "prompt_cache_key": "polaris-abcd",
        }))
    };
    let entry = |model: &str, id: &str| super::Continuation {
        response_id: id.to_owned(),
        baseline: vec![json!({"m": model})],
        shape: shape(model),
    };
    let mut held = super::Continuations::default();
    assert_eq!(held.len(), 0);
    held.remember(entry("gpt-6-astra", "resp-parent"));
    held.remember(entry("gpt-5.6-luna", "resp-child"));

    // The child's arrival must leave the parent's continuation in place.
    assert_eq!(
        held.get(&shape("gpt-6-astra"))
            .map(|c| c.response_id.as_str()),
        Some("resp-parent")
    );
    assert_eq!(
        held.get(&shape("gpt-5.6-luna"))
            .map(|c| c.response_id.as_str()),
        Some("resp-child")
    );
    assert_eq!(held.len(), 2);

    // The same shape again replaces rather than accumulates.
    held.remember(entry("gpt-6-astra", "resp-parent-2"));
    assert_eq!(held.len(), 2);
    assert_eq!(
        held.get(&shape("gpt-6-astra"))
            .map(|c| c.response_id.as_str()),
        Some("resp-parent-2")
    );
}

#[test]
fn the_store_is_bounded_and_drops_the_oldest() {
    let shape = |n: usize| Shape::of(&json!({"model": format!("m{n}")}));
    let mut held = super::Continuations::default();
    for n in 0..super::CONTINUATIONS + 2 {
        held.remember(super::Continuation {
            response_id: format!("resp-{n}"),
            baseline: Vec::new(),
            shape: shape(n),
        });
    }
    assert_eq!(held.len(), super::CONTINUATIONS);
    assert!(
        held.get(&shape(0)).is_none(),
        "the oldest shape should be gone"
    );
    assert!(held.get(&shape(super::CONTINUATIONS + 1)).is_some());
}

/// Only what polaris sends takes part. The server records more than it was
/// given, and each extra cost a measurement on 2026-09-20 before it was
/// named: `content: []` on a reasoning item, `annotations` and `logprobs`
/// inside every message part, and the `phase` it assigns each reply. Every
/// one refused the continuation on every request, so the parent re-sent its
/// whole history each turn.
#[test]
fn what_the_server_keeps_beside_an_item_does_not_refuse_the_continuation() {
    let reasoning = json!({"encrypted_content": "gAAAA", "type": "reasoning"});
    assert!(super::same_item(
        &json!({
            "content": [], "encrypted_content": "gAAAA", "type": "reasoning",
            "id": "rs_1", "status": "completed",
        }),
        &reasoning
    ));

    let message = json!({
        "type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": "ledger.py は残高を持つ。"}],
    });
    assert!(super::same_item(
        &json!({
            "type": "message", "role": "assistant", "id": "msg_01",
            "status": "completed", "phase": "final_answer",
            "content": [{
                "type": "output_text", "text": "ledger.py は残高を持つ。",
                "annotations": [], "logprobs": [],
            }],
        }),
        &message
    ));

    // A value polaris sends is still weighed.
    assert!(!super::same_item(
        &json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "report.py は表を組む。"}],
        }),
        &message
    ));

    // So is a field polaris sends that the server does not hold,
    let bare = json!({"type": "message", "role": "assistant"});
    assert!(!super::same_item(&bare, &message));

    // a part the server never recorded,
    let two_parts = json!({
        "type": "message", "role": "assistant",
        "content": [
            {"type": "output_text", "text": "ledger.py は残高を持つ。"},
            {"type": "output_text", "text": "report.py は表を組む。"},
        ],
    });
    assert!(!super::same_item(&message, &two_parts));

    // and a reasoning item the model actually filled in.
    assert!(!super::same_item(
        &reasoning,
        &json!({
            "content": [{"text": "something"}], "encrypted_content": "gAAAA",
            "type": "reasoning",
        })
    ));
}

/// The refusal message has to weigh items the way `delta` does. The server
/// records an empty `content` polaris never sends, so a raw scan blames the
/// first reasoning item on every refusal — including refusals that item has
/// nothing to do with.
#[test]
fn the_refusal_does_not_blame_a_difference_delta_accepts() {
    let recorded = json!({
        "content": [], "encrypted_content": "gAAAA", "type": "reasoning",
        "id": "rs_1", "status": "completed",
    });
    let sent = json!({"encrypted_content": "gAAAA", "type": "reasoning"});
    let held = super::Continuation {
        response_id: "resp-1".to_owned(),
        baseline: vec![recorded, json!({"type": "message", "role": "user"})],
        shape: Shape::of(&json!({"model": "gpt-6-astra"})),
    };

    // Shorter than the baseline: `delta` cannot continue, and the reason is
    // the length, not the reasoning item.
    let reason = super::why_refused(Some(&held), 1, std::slice::from_ref(&sent));
    assert!(
        reason.starts_with("短い"),
        "継続なしの理由が長さになっていない: {reason}"
    );

    // A real difference still gets named, with its position.
    let other = vec![sent, json!({"type": "message", "role": "assistant"})];
    let reason = super::why_refused(Some(&held), 1, &other);
    assert!(
        reason.starts_with("1件目の.roleが違う"),
        "本当の差を指していない: {reason}"
    );

    assert_eq!(super::why_refused(None, 0, &other), "前の応答がない");
    assert_eq!(
        super::why_refused(None, 3, &other),
        "この形の継続がない（保持 3形）"
    );
}

/// The path has to reach a field inside a long item, because that is where
/// every cause so far has been.
#[test]
fn the_refusal_names_the_field_that_differs() {
    let held = super::Continuation {
        response_id: "resp-1".to_owned(),
        baseline: vec![json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "ledger.py は残高を持つ。"}],
        })],
        shape: Shape::of(&json!({"model": "gpt-6-astra"})),
    };
    let sent = vec![json!({
        "type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": "report.py は表を組む。"}],
    })];
    let reason = super::why_refused(Some(&held), 1, &sent);
    assert!(
        reason.starts_with("0件目の.content[0].textが違う"),
        "差のある項目を指していない: {reason}"
    );
    assert!(reason.contains("ledger.py"), "保持の中身がない: {reason}");
    assert!(reason.contains("report.py"), "今回の中身がない: {reason}");
}
