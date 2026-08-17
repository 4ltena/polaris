//! OpenAI 互換のチャット補完。base_url を差し替えれば互換エンドポイントも叩ける。

use serde_json::Value;

use crate::{CompletionRequest, CompletionResponse, Provider, ProviderError, Role, ToolCall};
use polaris_tools::ToolSpec;

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            base_url,
            api_key,
            model,
            client: reqwest::Client::new(),
        }
    }
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// `ToolSpec` をワイヤ形式（`{"type":"function","function":{...}}`）へ
/// 変換する、この形の唯一の生成元。
///
/// 予算計測 (`polaris_core::budget::always_on_tokens`) もこの関数の出力を
/// 数える。かつては予算側が `ToolSpec` の `Serialize` 実装を直接数え、この
/// 関数がこことは別に同じ形を組み立てていた。二箇所が独立に「同じはず」の
/// ワイヤ形状を作っていたことが、実際に送信するバイト列と計測するバイト列
/// がずれる原因になったため、生成元をここへ一本化する。
pub fn tool_wire_shape(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let mut messages = vec![serde_json::json!({
            "role": "system",
            "content": req.system,
        })];
        for m in &req.messages {
            let mut entry = serde_json::json!({
                "role": role_str(m.role),
                "content": m.content,
            });
            // アシスタントのターンがツールを呼んだ場合は、その `tool_calls` を
            // このメッセージ自体に載せて送り返す。API はこれを見て、続く
            // `role: "tool"` メッセージの `tool_call_id` と突き合わせる。
            // `arguments` は受信時に一度パースした JSON 値を、送信時には
            // 対称的に JSON 文字列へ戻す（ワイヤ上はどちらも文字列）。
            if !m.tool_calls.is_empty() {
                let calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": c.arguments.to_string(),
                            }
                        })
                    })
                    .collect();
                entry["tool_calls"] = Value::Array(calls);
            }
            if let Some(id) = &m.tool_call_id {
                entry["tool_call_id"] = Value::String(id.clone());
            }
            messages.push(entry);
        }

        let tools = tool_wire_shape(&req.tools);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ProviderError::Http(format!("status {}", resp.status())));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(e.to_string()))?;

        // `choices` が無い/空、または choices[0].message が無いなら、応答を
        // 解釈できていない。ここで Decode にしないと text="" / tool_calls=[]
        // という「正常終了っぽい空応答」になり、上位のエージェントループが
        // 「ツール呼び出しなし＝完了」と誤読して黙って壊れる。
        let msg = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("message"))
            .ok_or_else(|| ProviderError::Decode("choices が空、または message が無い".into()))?;

        // content と tool_calls の両方が「実質的に無い」場合のみ Decode に
        // する。左右対称に「無い」を定義する必要がある — 片方だけ緩いと、
        // その緩い側の門から同じ黙った空成功が抜けてしまう。
        //
        // content が「無い」とは、フィールド自体が無いか、JSON null で
        // あること。missing と null はワイヤ上ここで区別する意味が無い。
        // 一方 content が空文字列で「存在する」場合は、モデルが何も
        // 言わなかっただけの正常応答として扱う（絶対に空≠不在にしない）。
        //
        // tool_calls が「無い」とは、フィールド自体が無いか、JSON null か、
        // 空配列であること。`{"content": null, "tool_calls": null}` や
        // `{"content": null, "tool_calls": []}` は、missing キーの場合と
        // 意味的に同一（呼び出しは一つも要求されていない）であり、区別
        // しないと text="" / tool_calls=[] の「正常終了っぽい空応答」が
        // このガードをすり抜けてしまう（ループはこれを「ツール呼び出し
        // 無し＝完了」と誤読し、空文字列を最終回答として返して黙って壊れる）。
        //
        // `content: null` かつ tool_calls が非空の組み合わせ（ツールだけを
        // 呼ぶターンの通常形）はこのガードに引っかからない。
        let content_field = msg.get("content");
        let tool_calls_field = msg.get("tool_calls");
        let content_is_absent = content_field.is_none_or(|c| c.is_null());
        let tool_calls_are_absent = tool_calls_field
            .is_none_or(|c| c.is_null() || c.as_array().is_some_and(|a| a.is_empty()));
        if content_is_absent && tool_calls_are_absent {
            return Err(ProviderError::Decode(
                "message に content も tool_calls も無い".into(),
            ));
        }
        let text = content_field
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();

        let mut tool_calls = Vec::new();
        if let Some(calls) = tool_calls_field.and_then(|tc| tc.as_array()) {
            for c in calls {
                // function.name が無い/空だと、ディスパッチャには「空名前の
                // 未知ツール」として届き、デコード失敗がツール選択の問題に
                // 見えてしまう。ここで止めて発生源で報告する。
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ProviderError::Decode("tool_calls[].function.name が無いか空".into())
                    })?;

                // id も同様に無い/空を Decode にする。name と同じ構造の
                // フィールドで、コストが同じなので揃えた。ただし空 id が
                // 下流（ツール結果の突き合わせ）で実際にどう壊れるかまでは
                // 確認していない。
                let id = c
                    .get("id")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ProviderError::Decode("tool_calls[].id が無いか空".into()))?;

                let raw = c["function"]["arguments"].as_str().unwrap_or("{}");
                let arguments: Value =
                    serde_json::from_str(raw).map_err(|e| ProviderError::Decode(e.to_string()))?;
                tool_calls.push(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                });
            }
        }

        Ok(CompletionResponse { text, tool_calls })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolCall};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn parses_tool_call_from_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "function": { "name": "read", "arguments": "{\"path\":\"a.rs\"}" }
                        }]
                    }
                }]
            })))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into());
        let res = p
            .complete(CompletionRequest {
                system: "s".into(),
                messages: vec![Message::user("go")],
                tools: vec![],
            })
            .await
            .expect("失敗した");

        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].name, "read");
        assert_eq!(res.tool_calls[0].arguments["path"], "a.rs");
    }

    #[tokio::test]
    async fn surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into());
        let err = p
            .complete(CompletionRequest {
                system: "s".into(),
                messages: vec![],
                tools: vec![],
            })
            .await
            .expect_err("エラーになるべき");
        assert!(matches!(err, ProviderError::Http(_)));
    }

    async fn complete_against(
        body: serde_json::Value,
    ) -> Result<CompletionResponse, ProviderError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into());
        p.complete(CompletionRequest {
            system: "s".into(),
            messages: vec![],
            tools: vec![],
        })
        .await
    }

    #[tokio::test]
    async fn errors_when_choices_is_missing() {
        let err = complete_against(serde_json::json!({}))
            .await
            .expect_err("choices が無いのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_choices_is_empty() {
        let err = complete_against(serde_json::json!({ "choices": [] }))
            .await
            .expect_err("choices が空なのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_message_has_neither_content_nor_tool_calls() {
        let err = complete_against(serde_json::json!({
            "choices": [{ "message": {} }]
        }))
        .await
        .expect_err("content も tool_calls も無いのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_content_is_null_and_tool_calls_missing() {
        // content: null は JSON 上「存在する」が、tool_calls も無ければ
        // 本文が実質何も無い応答であり、以前は "" として静かに Ok になって
        // いた。ループは「ツール呼び出し無し＝完了」と読むため、これは
        // 「空の答えで成功した」という誤った結果になる。
        let err = complete_against(serde_json::json!({
            "choices": [{ "message": { "content": null } }]
        }))
        .await
        .expect_err("content が null で tool_calls も無いのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_content_is_null_and_tool_calls_is_null() {
        // tool_calls: null は「フィールドが無い」場合と意味的に同一で
        // （呼び出しは一つも要求されていない）、区別しないと text="" /
        // tool_calls=[] の「正常終了っぽい空応答」がガードをすり抜ける。
        let err = complete_against(serde_json::json!({
            "choices": [{ "message": { "content": null, "tool_calls": null } }]
        }))
        .await
        .expect_err("content も tool_calls も null なのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_content_is_null_and_tool_calls_is_empty_array() {
        // tool_calls: [] も同様に「呼び出しなし」であり、missing/null と
        // 同じ扱いにしないと同じ抜け道になる。
        let err = complete_against(serde_json::json!({
            "choices": [{ "message": { "content": null, "tool_calls": [] } }]
        }))
        .await
        .expect_err("content が null で tool_calls も空配列なのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn empty_string_content_is_a_real_response_not_an_error() {
        let res = complete_against(serde_json::json!({
            "choices": [{ "message": { "content": "" } }]
        }))
        .await
        .expect("content が空文字列でも正常応答として扱うべき");
        assert_eq!(res.text, "");
        assert_eq!(res.tool_calls.len(), 0);
    }

    #[tokio::test]
    async fn errors_when_tool_call_missing_function_name() {
        let err = complete_against(serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "arguments": "{}" }
                    }]
                }
            }]
        }))
        .await
        .expect_err("function.name が無いのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_tool_call_has_empty_function_name() {
        let err = complete_against(serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "", "arguments": "{}" }
                    }]
                }
            }]
        }))
        .await
        .expect_err("function.name が空文字列なのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_tool_call_missing_id() {
        let err = complete_against(serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "function": { "name": "read", "arguments": "{}" }
                    }]
                }
            }]
        }))
        .await
        .expect_err("id が無いのでエラーになるべき");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn sends_tool_definitions_in_the_shape_tool_wire_shape_produces() {
        // 予算計測が数える形（`tool_wire_shape`）と、実際にワイヤへ乗る形が
        // 同じ関数から出ていることを、モックが受け取った生のボディで確かめる。
        // 型だけを見るテストでは、両者が独立に同じ形を再実装して食い違う
        // ことを検出できない。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let specs = polaris_tools::all_specs();
        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into());
        p.complete(CompletionRequest {
            system: "s".into(),
            messages: vec![],
            tools: specs.clone(),
        })
        .await
        .expect("失敗した");

        let received = server
            .received_requests()
            .await
            .expect("リクエストが記録されていない");
        let body: Value = received[0].body_json().expect("JSON として読めない");

        let expected = Value::Array(tool_wire_shape(&specs));
        assert_eq!(
            body["tools"], expected,
            "送信されたツール定義が tool_wire_shape の出力と一致しない"
        );
    }

    #[tokio::test]
    async fn serializes_tool_round_trip_to_the_wire_shape_openai_requires() {
        // 型だけを見るテストは、実際のワイヤ形式のズレを見逃す。ここでは
        // wiremock が受け取った生のリクエストボディを直接検証し、
        // (1) tool_calls を持つアシスタントのメッセージがそのまま履歴に
        //     残っていること、(2) arguments が JSON オブジェクトではなく
        //     JSON 文字列として送信されること、(3) 続く tool 結果が
        //     tool_call_id を持つこと、(4) 普通のユーザーメッセージには
        //     tool_calls / tool_call_id のどちらも乗らないこと、を確かめる。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "1 行だった" } }]
            })))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into());
        let history = vec![
            Message::user("a.txt は何行か"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "a.txt" }),
                }],
            ),
            Message::tool_result("c1", "1: hello"),
        ];

        p.complete(CompletionRequest {
            system: "s".into(),
            messages: history,
            tools: vec![],
        })
        .await
        .expect("失敗した");

        let received = server
            .received_requests()
            .await
            .expect("リクエストが記録されていない");
        assert_eq!(received.len(), 1);
        let body: Value = received[0].body_json().expect("JSON として読めない");
        let messages = body["messages"].as_array().expect("messages が無い");

        // 0: system, 1: user, 2: assistant(tool_calls), 3: tool
        let user = &messages[1];
        assert_eq!(user["role"], "user");
        assert!(
            user.get("tool_calls").is_none(),
            "普通のユーザーメッセージに tool_calls が乗っている: {user:?}"
        );
        assert!(
            user.get("tool_call_id").is_none(),
            "普通のユーザーメッセージに tool_call_id が乗っている: {user:?}"
        );

        let assistant = &messages[2];
        assert_eq!(assistant["role"], "assistant");
        let calls = assistant["tool_calls"]
            .as_array()
            .expect("assistant の tool_calls が無い");
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "read");
        let arguments = &calls[0]["function"]["arguments"];
        assert!(
            arguments.is_string(),
            "arguments は JSON 文字列であるべき: {arguments:?}"
        );
        let parsed: Value =
            serde_json::from_str(arguments.as_str().unwrap()).expect("パースできない");
        assert_eq!(parsed["path"], "a.txt");

        let tool_msg = &messages[3];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "c1");
        assert_eq!(tool_msg["content"], "1: hello");
    }
}
