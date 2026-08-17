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
    skills: &[polaris_skills::Skill],
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
            let outcome = dispatch(call, skills);
            audit.record(&call.name, &call.arguments.to_string())?;
            match outcome {
                Ok(body) => {
                    // 成功したので連続エラーのストリークをリセットする。ここを
                    // 呼ばないと、成功を挟んだ同一エラーの繰り返しが「連続」と
                    // 誤判定されて止まる（stop.rs のコメント参照）。
                    stop.observe_success();
                    session.push_tool_result(&call.id, &body);
                }
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
fn dispatch(
    call: &polaris_provider::ToolCall,
    skills: &[polaris_skills::Skill],
) -> Result<String, String> {
    match call.name.as_str() {
        "read" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path が無い".to_string())?;
            let offset = call.arguments["offset"].as_u64().unwrap_or(0) as usize;
            let limit = call.arguments["limit"].as_u64().unwrap_or(2000) as usize;
            polaris_tools::read::read(Path::new(path), offset, limit).map_err(|e| e.to_string())
        }
        "skill" => {
            let q = call.arguments["q"]
                .as_str()
                .ok_or_else(|| "q が無い".to_string())?;
            Ok(polaris_tools::skill::lookup(skills, q))
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
        let out = run(&p, &mut session, &mut audit, &mut stop, &system, &[])
            .await
            .expect("失敗");
        assert_eq!(out, "1 行だった");

        let log = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("読めない");
        assert!(log.contains("\"tool\":\"read\""), "read が記録されていない");
    }

    #[tokio::test]
    async fn interleaved_success_does_not_trip_the_repeated_error_stop() {
        // stop.rs 側の単体テストと対になる、ループ経由の回帰テスト。
        // 「同一エラー3回」を、間に成功を挟んだ5回のエラーとして再現する。
        // observe_success がループの成功経路から呼ばれていなければ、
        // 3回目のエラーで（成功を挟んでいるにもかかわらず）止まってしまう。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("書けない");

        let fail_call = || CompletionResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "/home/u/.ssh/id_rsa" }),
            }],
        };
        let ok_call = || CompletionResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
            }],
        };

        let p = Scripted {
            replies: Mutex::new(vec![
                fail_call(),
                ok_call(),
                fail_call(),
                ok_call(),
                fail_call(),
                CompletionResponse {
                    text: "終わった".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("読んで");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(50);

        let system = crate::prompt::build_system("", "");
        let out = run(&p, &mut session, &mut audit, &mut stop, &system, &[])
            .await
            .expect("成功を挟んでいるので3回連続扱いにならず止まらないはず");
        assert_eq!(out, "終わった");
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
        let err = run(&p, &mut session, &mut audit, &mut stop, &system, &[])
            .await
            .expect_err("止まるべき");
        assert!(matches!(
            err,
            AgentError::Stopped(StopReason::RepeatedError(_))
        ));
    }

    #[tokio::test]
    async fn dispatches_the_skill_tool() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let skills = vec![polaris_skills::Skill {
            name: "demo".into(),
            description: "説明".into(),
            body: "デモ本文".into(),
            path: "/x/demo/SKILL.md".into(),
        }];

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({ "q": "demo" }),
                    }],
                },
                CompletionResponse { text: "読んだ".into(), tool_calls: vec![] },
            ]),
        };

        let mut session = Session::new();
        session.push_user("demo の本文を読んで");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let system = crate::prompt::build_system("", "");

        let out = run(&p, &mut session, &mut audit, &mut stop, &system, &skills)
            .await
            .expect("失敗");
        assert_eq!(out, "読んだ");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        assert!(tool_msg.content.contains("デモ本文"), "本文が渡っていない");

        let log = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("読めない");
        let skill_lines = log
            .lines()
            .filter(|l| l.contains("\"tool\":\"skill\""))
            .count();
        assert_eq!(skill_lines, 1, "skill の呼び出しが記録されていない: {log}");
    }

    #[tokio::test]
    async fn the_skill_tool_reports_when_no_skills_are_loaded() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({ "q": "何か" }),
                    }],
                },
                CompletionResponse { text: "了解".into(), tool_calls: vec![] },
            ]),
        };
        let mut session = Session::new();
        session.push_user("skill を探して");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let system = crate::prompt::build_system("", "");

        run(&p, &mut session, &mut audit, &mut stop, &system, &[])
            .await
            .expect("失敗");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        assert!(!tool_msg.content.is_empty(), "空の結果を返してはいけない");
    }
}
