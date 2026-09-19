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
