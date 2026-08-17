//! エージェントループ。ツール呼び出しが無くなった時点の本文を返す。
//!
//! アシスタントのターンがツールを呼んだときは、そのターン自身を
//! `tool_calls` 付きでセッションへ記録してから各ツールを実行する。
//! OpenAI の往復規約は、続く `role: "tool"` メッセージの前にこの
//! アシスタントメッセージが履歴に存在することを要求するため、
//! 順序を守らないと次のターンの送信が API に拒否される。

use std::path::Path;

use polaris_provider::{CompletionRequest, Provider};

use crate::audit::AuditLog;
use crate::session::Session;
use crate::stop::{StopReason, StopTracker};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("停止した: {0:?}")]
    Stopped(StopReason),
    #[error("プロバイダ: {0}")]
    Provider(#[from] polaris_provider::ProviderError),
    #[error("入出力: {0}")]
    Io(#[from] std::io::Error),
}

/// `system` は憲法ブロックと環境情報を含めて組み立て済みのものを渡す。
/// ループ内で組み立てないのは、毎ターン同じ文字列を送ってキャッシュ接頭辞を
/// 動かさないことを呼び出し側で保証させるため。
pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
    system: &str,
) -> Result<String, AgentError> {
    loop {
        // 毎ターン無条件に呼ぶ。エラー時にしか呼ばないと、エラーを一度も
        // 起こさない呼び出しパターンでは MaxTurns に一切引っかからず、
        // ループが無限に回りうる。
        if let Some(r) = stop.observe_turn() {
            return Err(AgentError::Stopped(r));
        }

        let res = provider
            .complete(CompletionRequest {
                system: system.to_string(),
                messages: session.messages.clone(),
                tools: polaris_tools::all_specs(),
            })
            .await?;

        if res.tool_calls.is_empty() {
            session.push_assistant(&res.text);
            return Ok(res.text);
        }

        // ツール結果を送る前に、このアシスタントのターン自身を tool_calls
        // 付きで履歴へ残す。ここを飛ばして tool 結果だけを積むと、次の
        // 送信で「どの呼び出しに対する結果か」を API 側が突き合わせられず、
        // 実際の OpenAI エンドポイントには拒否される（モック相手のテストは
        // ワイヤ形式を見ないため、この欠落を検出できない）。
        session.push_assistant_tool_calls(&res.text, res.tool_calls.clone());

        for call in &res.tool_calls {
            let outcome = dispatch(call);
            audit.record(&call.name, &call.arguments.to_string())?;
            match outcome {
                Ok(body) => session.push_tool_result(&call.id, &body),
                Err(msg) => {
                    if let Some(r) = stop.observe_error(&msg) {
                        return Err(AgentError::Stopped(r));
                    }
                    session.push_tool_result(&call.id, &msg);
                }
            }
        }
    }
}

/// ツール呼び出しを実際の実装へ振り分ける。失敗はモデルへ返す文字列にする。
fn dispatch(call: &polaris_provider::ToolCall) -> Result<String, String> {
    match call.name.as_str() {
        "read" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path が無い".to_string())?;
            let offset = call.arguments["offset"].as_u64().unwrap_or(0) as usize;
            let limit = call.arguments["limit"].as_u64().unwrap_or(2000) as usize;
            polaris_tools::read::read(Path::new(path), offset, limit).map_err(|e| e.to_string())
        }
        other => Err(format!("未知のツール: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::{CompletionResponse, ToolCall};
    use std::sync::Mutex;

    /// 1 回目はツール呼び出し、2 回目は本文を返すプロバイダ。
    struct Scripted {
        replies: Mutex<Vec<CompletionResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            let mut r = self.replies.lock().expect("lock");
            Ok(if r.is_empty() {
                CompletionResponse::default()
            } else {
                r.remove(0)
            })
        }
    }

    #[tokio::test]
    async fn runs_tool_then_returns_final_text() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("書けない");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
                    }],
                },
                CompletionResponse {
                    text: "1 行だった".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("a.txt は何行か");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);

        let system = crate::prompt::build_system("", "");
        let out = run(&p, &mut session, &mut audit, &mut stop, &system)
            .await
            .expect("失敗");
        assert_eq!(out, "1 行だった");

        let log = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("読めない");
        assert!(log.contains("\"tool\":\"read\""), "read が記録されていない");
    }

    #[tokio::test]
    async fn stops_when_tool_fails_three_times() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let call = || CompletionResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "/home/u/.ssh/id_rsa" }),
            }],
        };
        let p = Scripted {
            replies: Mutex::new(vec![call(), call(), call()]),
        };

        let mut session = Session::new();
        session.push_user("読んで");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(50);

        let system = crate::prompt::build_system("", "");
        let err = run(&p, &mut session, &mut audit, &mut stop, &system)
            .await
            .expect_err("止まるべき");
        assert!(matches!(
            err,
            AgentError::Stopped(StopReason::RepeatedError(_))
        ));
    }
}
