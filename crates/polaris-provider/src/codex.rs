//! ChatGPT のサブスクリプション認証で Responses API を話すプロバイダ。
//!
//! `/chat/completions` とは形が違う。ツール定義は入れ子ではなく平坦で、
//! 履歴は `messages` ではなく `input` の要素列であり、`arguments` は
//! JSON ではなく JSON を収めた文字列である。`openai.rs` と関数を共有
//! しないのは、片方を直したときにもう片方が黙って壊れる形にしないため。

use serde_json::Value;

use crate::{CompletionRequest, CompletionResponse, Message, ProviderError, Role, ToolCall, sse};

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

/// SSE の意味論。フレーミングは `sse::SseDecoder` に任せ、ここは
/// イベントの解釈だけを持つ。HTTP から切り離してあるので、ネットワーク
/// 無しで試験できる。
pub struct Folder {
    decoder: sse::SseDecoder,
    text: String,
    tool_calls: Vec<ToolCall>,
    completed: bool,
}

impl Default for Folder {
    fn default() -> Self {
        Self::new()
    }
}

impl Folder {
    pub fn new() -> Self {
        Self {
            decoder: sse::SseDecoder::new(),
            text: String::new(),
            tool_calls: Vec::new(),
            completed: false,
        }
    }

    /// 受け取ったバイト片を押し込む。完成したイベントだけを解釈する。
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProviderError> {
        for ev in self.decoder.push(bytes) {
            let data = ev.data.trim();
            // 番兵。JSON ではないので解釈しない。
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let v: Value = serde_json::from_str(data)
                .map_err(|e| ProviderError::Decode(format!("SSE の data が JSON でない: {e}")))?;

            match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                "response.output_item.done" => self.take_item(&v)?,
                "response.completed" => self.completed = true,
                "response.failed" => {
                    let msg = v
                        .pointer("/response/error/message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("理由が示されていない");
                    return Err(ProviderError::Http(format!("応答が失敗した: {msg}")));
                }
                "response.cancelled" => {
                    return Err(ProviderError::Http("応答が取り消された".into()));
                }
                // 差分やその他は読み飛ばす。確定したアイテムだけを見れば
                // 同じ結果になり、再結合の失敗という壊れ方を持ち込まない。
                _ => {}
            }
        }
        Ok(())
    }

    fn take_item(&mut self, v: &Value) -> Result<(), ProviderError> {
        let Some(item) = v.get("item") else {
            return Ok(());
        };
        match item
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
        {
            "message" => {
                if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                            self.text.push_str(t);
                        }
                    }
                }
            }
            "function_call" => {
                let id = item
                    .get("call_id")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ProviderError::Decode("function_call に call_id が無い".into())
                    })?;
                let name = item
                    .get("name")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ProviderError::Decode("function_call に name が無い".into()))?;
                let raw = item
                    .get("arguments")
                    .and_then(|s| s.as_str())
                    .unwrap_or("{}");
                let arguments: Value = serde_json::from_str(raw).map_err(|e| {
                    ProviderError::Decode(format!("function_call の arguments が JSON でない: {e}"))
                })?;
                self.tool_calls.push(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// 畳んだ結果を返す。完了を見ていなければ硬い失敗にする。
    pub fn finish(self) -> Result<CompletionResponse, ProviderError> {
        if !self.completed {
            return Err(ProviderError::Decode(
                "response.completed を見ないままストリームが終わった".into(),
            ));
        }
        Ok(CompletionResponse {
            text: self.text,
            tool_calls: self.tool_calls,
        })
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn a_text_only_stream_folds_into_text() {
        let mut f = Folder::new();
        f.push(&frame("response.output_item.done", message_item("42 行")))
            .expect("押せる");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("押せる");
        let r = f.finish().expect("完了しているべき");
        assert_eq!(r.text, "42 行");
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn a_function_call_item_becomes_a_tool_call() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_item.done",
            function_call_item("call_9", "read", r#"{"path":"a.txt"}"#),
        ))
        .expect("押せる");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("押せる");
        let r = f.finish().expect("完了しているべき");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "call_9");
        assert_eq!(r.tool_calls[0].name, "read");
        assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
    }

    /// フレームがどこで分割されても結果が変わらない。分割耐性は
    /// `sse.rs` の責任だが、この経路が実際にそれを通っていることは
    /// 別に確かめる。通っていなければ、ここで結合をやり直している。
    #[test]
    fn a_stream_split_mid_frame_folds_the_same_way() {
        let whole: Vec<u8> = frame("response.output_item.done", message_item("分割耐性"))
            .into_iter()
            .chain(frame("response.completed", serde_json::json!({})))
            .collect();

        for cut in 1..whole.len() {
            let mut f = Folder::new();
            f.push(&whole[..cut]).expect("押せる");
            f.push(&whole[cut..]).expect("押せる");
            let r = f.finish().expect("完了しているべき");
            assert_eq!(r.text, "分割耐性", "{cut} バイト目で分割したときに壊れた");
        }
    }

    /// `response.completed` を見ないまま終わった応答を、正常終了として
    /// 返してはならない。エージェントループはツール呼び出しが無いことを
    /// 「完了」と読むため、黙って空の最終回答を返す。
    #[test]
    fn a_stream_that_never_completes_is_an_error() {
        let mut f = Folder::new();
        f.push(&frame("response.output_item.done", message_item("途中")))
            .expect("押せる");
        let err = f.finish().expect_err("完了していないので失敗すべき");
        assert!(
            matches!(err, ProviderError::Decode(_)),
            "Decode 以外: {err:?}"
        );
    }

    /// 中身が空でも、完了していれば成功である。空文字列と不在を
    /// 取り違えない。上のテストの対であり、これが無いと「常に失敗」の
    /// 実装が通る。
    #[test]
    fn an_empty_but_completed_stream_is_a_success() {
        let mut f = Folder::new();
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("押せる");
        let r = f.finish().expect("完了しているので成功すべき");
        assert!(r.text.is_empty());
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn a_failed_response_carries_its_message() {
        let mut f = Folder::new();
        let err = f
            .push(&frame(
                "response.failed",
                serde_json::json!({ "response": { "error": { "message": "model overloaded" } } }),
            ))
            .expect_err("失敗すべき");
        let ProviderError::Http(msg) = err else {
            panic!("Http 以外: {err:?}");
        };
        assert!(msg.contains("model overloaded"), "理由が文面に無い: {msg}");
    }

    #[test]
    fn a_cancelled_response_is_an_error() {
        let mut f = Folder::new();
        let err = f
            .push(&frame("response.cancelled", serde_json::json!({})))
            .expect_err("失敗すべき");
        assert!(matches!(err, ProviderError::Http(_)), "Http 以外: {err:?}");
    }

    /// 差分イベントは読み飛ばす。拾って二重に積むと本文が重複する。
    #[test]
    fn delta_events_are_ignored() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_text.delta",
            serde_json::json!({ "delta": "重複" }),
        ))
        .expect("押せる");
        f.push(&frame("response.output_item.done", message_item("重複")))
            .expect("押せる");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("押せる");
        let r = f.finish().expect("完了");
        assert_eq!(r.text, "重複", "差分を拾って二重に積んでいる");
    }

    /// `arguments` が JSON として壊れている呼び出しは、ディスパッチャへ
    /// 渡す前にここで止める。渡すと「未知の引数」に見え、原因が遡れない。
    #[test]
    fn a_function_call_with_broken_arguments_is_a_decode_error() {
        let mut f = Folder::new();
        let err = f
            .push(&frame(
                "response.output_item.done",
                function_call_item("c", "read", "{ not json"),
            ))
            .expect_err("失敗すべき");
        assert!(
            matches!(err, ProviderError::Decode(_)),
            "Decode 以外: {err:?}"
        );
    }

    /// `[DONE]` という番兵は JSON ではない。解釈しようとして落ちない。
    #[test]
    fn the_done_sentinel_is_not_parsed_as_json() {
        let mut f = Folder::new();
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("押せる");
        f.push(b"data: [DONE]\n\n").expect("番兵で落ちてはいけない");
        let r = f.finish().expect("完了");
        assert!(r.text.is_empty());
    }

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
