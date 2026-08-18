//! ChatGPT のサブスクリプション認証で Responses API を話すプロバイダ。
//!
//! `/chat/completions` とは形が違う。ツール定義は入れ子ではなく平坦で、
//! 履歴は `messages` ではなく `input` の要素列であり、`arguments` は
//! JSON ではなく JSON を収めた文字列である。`openai.rs` と関数を共有
//! しないのは、片方を直したときにもう片方が黙って壊れる形にしないため。

use serde_json::Value;

use crate::{CompletionRequest, Message, Role};

/// 要求先。`store` を使わないので、この 1 本しか叩かない。
pub const ENDPOINT_BASE: &str = "https://chatgpt.com/backend-api/codex";
/// `POLARIS_MODEL` を省いたときの既定。
pub const DEFAULT_MODEL: &str = "gpt-5.3-codex";

/// 履歴を Responses の `input` 要素列へ変換する。
pub fn input_items(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::User => out.push(serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": m.content }],
            })),
            Role::Assistant => {
                // 本文が空でツール呼び出しだけのターンは珍しくない。
                // 空の message を足すと、内容の無い発話が履歴に増える。
                if !m.content.is_empty() {
                    out.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": m.content }],
                    }));
                }
                for c in &m.tool_calls {
                    out.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": c.id,
                        "name": c.name,
                        // JSON そのものではなく、JSON を収めた文字列。
                        "arguments": c.arguments.to_string(),
                    }));
                }
            }
            Role::Tool => out.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": m.tool_call_id.clone().unwrap_or_default(),
                "output": m.content,
            })),
        }
    }
    out
}

/// ツール定義を Responses の平坦な形へ変換する。
pub fn tool_wire_shape(tools: &[polaris_tools::ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect()
}

/// 要求本文を組み立てる。
pub fn build_body(model: &str, req: &CompletionRequest) -> Value {
    let mut body = serde_json::json!({
        "model": model,
        "instructions": req.system,
        "input": input_items(&req.messages),
        // サーバに会話状態を持たせない。毎ターン全文を送る。送るものと
        // 測るものが一致し、接頭辞も動かない。
        "store": false,
        "stream": true,
    });
    let tools = tool_wire_shape(&req.tools);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCall;

    #[test]
    fn a_user_message_becomes_an_input_text_item() {
        let items = input_items(&[Message::user("こんにちは")]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "こんにちは");
    }

    #[test]
    fn an_assistant_message_becomes_an_output_text_item() {
        let items = input_items(&[Message::assistant("はい")]);
        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[0]["content"][0]["type"], "output_text");
        assert_eq!(items[0]["content"][0]["text"], "はい");
    }

    /// ツール呼び出しは `function_call` になり、`arguments` は JSON では
    /// なく JSON を収めた文字列である。ここを Value のまま送ると、
    /// サーバは型が違うと言って 400 を返す。
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
        assert_eq!(items.len(), 1, "本文が空のときに空の message を足している");
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["name"], "read");
        let raw = items[0]["arguments"]
            .as_str()
            .expect("arguments が文字列でない");
        let parsed: Value = serde_json::from_str(raw).expect("arguments が JSON でない");
        assert_eq!(parsed["path"], "Cargo.toml");
    }

    /// 本文とツール呼び出しの両方を持つターンは、message を先に、
    /// function_call を後に並べる。順序が逆だと、モデルは自分の発話より
    /// 先に自分の呼び出しを見ることになる。
    #[test]
    fn a_turn_with_both_text_and_calls_emits_the_message_first() {
        let items = input_items(&[Message::assistant_with_tool_calls(
            "読みます",
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
    fn a_tool_result_becomes_a_function_call_output() {
        let items = input_items(&[Message::tool_result("call_1", "42 行")]);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["output"], "42 行");
    }

    /// Responses のツール定義は平坦である。`/chat/completions` の
    /// `{"type":"function","function":{…}}` を送ると受け付けられない。
    #[test]
    fn tool_definitions_are_flat_not_nested() {
        let specs = polaris_tools::all_specs();
        let wire = tool_wire_shape(&specs);
        assert_eq!(wire.len(), specs.len());
        for (w, s) in wire.iter().zip(specs.iter()) {
            assert_eq!(w["type"], "function");
            assert_eq!(w["name"], s.name, "name が平坦に置かれていない");
            assert!(
                w.get("function").is_none(),
                "入れ子の function が残っている"
            );
            assert!(w["description"].is_string());
            assert_eq!(w["parameters"], s.parameters);
        }
    }

    #[test]
    fn the_body_carries_instructions_and_never_stores_state() {
        let req = CompletionRequest {
            system: "システム".into(),
            messages: vec![Message::user("やって")],
            tools: polaris_tools::all_specs(),
        };
        let body = build_body("gpt-5.3-codex", &req);

        assert_eq!(body["model"], "gpt-5.3-codex");
        assert_eq!(body["instructions"], "システム");
        assert_eq!(body["store"], false, "サーバに会話状態を持たせている");
        assert_eq!(body["stream"], true);
        assert!(
            body.get("previous_response_id").is_none(),
            "会話の再利用を使っている"
        );
        assert_eq!(
            body["input"].as_array().expect("input が配列でない").len(),
            1
        );
        assert_eq!(
            body["tools"].as_array().expect("tools が配列でない").len(),
            req.tools.len()
        );
    }

    /// ツールが 1 本も無いときは `tools` を送らない。空配列を送ると、
    /// 「ツールを使うな」の指示と受け取られうる。
    #[test]
    fn an_empty_tool_list_is_omitted_rather_than_sent_empty() {
        let req = CompletionRequest {
            system: "s".into(),
            messages: vec![Message::user("x")],
            tools: vec![],
        };
        let body = build_body("m", &req);
        assert!(body.get("tools").is_none(), "空の tools を送っている");
    }
}
