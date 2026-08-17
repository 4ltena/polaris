//! OpenAI 互換のチャット補完。base_url を差し替えれば互換エンドポイントも叩ける。

use serde_json::Value;

use crate::{CompletionRequest, CompletionResponse, Provider, ProviderError, Role, ToolCall};

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

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let mut messages = vec![serde_json::json!({
            "role": "system",
            "content": req.system,
        })];
        for m in &req.messages {
            messages.push(serde_json::json!({
                "role": role_str(m.role),
                "content": m.content,
            }));
        }

        let tools: Vec<Value> = req
            .tools
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
            .collect();

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

        // content と tool_calls の両方が「フィールド自体が無い」場合のみ
        // Decode にする。content が空文字列で「存在する」場合は、モデルが
        // 何も言わなかっただけの正常応答として扱う（絶対に空≠不在にしない）。
        let content_field = msg.get("content");
        let tool_calls_field = msg.get("tool_calls");
        if content_field.is_none() && tool_calls_field.is_none() {
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
    use crate::Message;
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
                messages: vec![Message {
                    role: Role::User,
                    content: "go".into(),
                }],
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
}
