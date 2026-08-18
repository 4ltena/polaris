//! エージェントループ。ツール呼び出しが無くなった時点の本文を返す。
//!
//! アシスタントのターンがツールを呼んだときは、そのターン自身を
//! `tool_calls` 付きでセッションへ記録してから各ツールを実行する。
//! OpenAI の往復規約は、続く `role: "tool"` メッセージの前にこの
//! アシスタントメッセージが履歴に存在することを要求するため、
//! 順序を守らないと次のターンの送信が API に拒否される。

use std::path::{Path, PathBuf};

use polaris_provider::{CompletionRequest, Provider};

use crate::audit::{AuditLog, Record};
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

/// 変更操作に要る文脈。`agent::run` が受け取って `dispatch` へ渡す。
///
/// `sandbox` と `helper` は `write` / `edit` / `bash` の全てが要る。`gate` と
/// `approver` は `write` / `edit` だけが使う（`bash` は述語を通さないため。
/// `crate::approval` のドキュメント、およびこのファイル冒頭のツール別の
/// 振る舞いの説明を参照）。
pub struct ToolContext<'a> {
    pub sandbox: &'a polaris_sandbox::SandboxPolicy,
    pub helper: &'a Path,
    pub gate: &'a mut crate::approval::Gate,
    pub approver: &'a mut dyn crate::approval::Approver,
}

/// `always_on` は [`crate::prompt::assemble_always_on`] が組み立てたものを渡す。
/// ループ内で組み立てないのは、毎ターン同じ文字列を送ってキャッシュ接頭辞を
/// 動かさないことを呼び出し側で保証させるため。
///
/// 文字列と `Vec<ToolSpec>` を別々に受けず [`crate::prompt::AlwaysOn`] で受ける
/// のは、毎ターン載るものをこの型の外側で作れないようにするため。呼び出し側が
/// 組み立て済みの文字列へ継ぎ足せると、常時コンテキストの上限は呼び出し側の
/// 書き方に委ねられてしまう。
///
/// `skills` は常時コンテキストには載らない。`skill` ツールが引かれたときだけ
/// 参照する。
pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
    always_on: &crate::prompt::AlwaysOn,
    skills: &[polaris_skills::Skill],
    ctx: &mut ToolContext<'_>,
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
                system: always_on.system().to_string(),
                messages: session.messages.clone(),
                tools: always_on.tools().to_vec(),
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
            let outcome = dispatch(call, skills, ctx);
            // `result` はモデルへ実際に返す本文（成功時）かエラー文言
            // （失敗時）そのもの。
            let result: &str = match &outcome {
                Ok(body) => body.as_str(),
                Err(msg) => msg.as_str(),
            };
            // `sandbox` と `target` は `write` / `edit` のときだけ埋める。
            // どちらも対象パスがちょうど1つに定まるツールで、その引数は
            // 各々の公開スキーマが "path" という名前で宣言している
            // （`polaris_tools::write_spec` / `edit_spec`）。埋めるのは
            // 成功・失敗いずれの記録でも同じ —— 拒否された試みが「何に
            // 触れようとしたか」を残せなければ、監査ログの目的である
            // 再構成可能性が失敗時にだけ欠ける。
            let is_mutation = call.name == "write" || call.name == "edit";
            let target: Option<PathBuf> = if is_mutation {
                call.arguments["path"].as_str().map(PathBuf::from)
            } else {
                None
            };
            audit.record(&Record {
                tool: &call.name,
                detail: &call.arguments.to_string(),
                sandbox: is_mutation.then_some(ctx.sandbox),
                target: target.as_deref(),
                result,
            })?;
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
///
/// `write` と `edit` は対象パスが1つに定まるため、実行前に `ctx.gate.check`
/// で述語による予測を通す。越えるなら実行前に停止して理由を返す —— 断りは
/// 例外ではなく通常のツール結果であり、モデルは別の場所へ書き直す機会を
/// 得る（`agent::tests::a_denied_write_comes_back_as_a_tool_result_not_a_loop_failure`
/// が固定している）。
///
/// `bash` はこの経路を通さない。任意のコードを実行するため、何に触れるかを
/// 事前に決定できないからである（`polaris_tools::bash` のドキュメント参照）。
/// 拘束下で試行し、拒否されたら子の出力に載る理由をそのままモデルへ返す。
fn dispatch(
    call: &polaris_provider::ToolCall,
    skills: &[polaris_skills::Skill],
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    match call.name.as_str() {
        "read" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path が無い".to_string())?;
            let offset = call.arguments["offset"].as_u64().unwrap_or(0) as usize;
            let limit = call.arguments["limit"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(polaris_tools::read::DEFAULT_LIMIT);
            polaris_tools::read::read(Path::new(path), offset, limit).map_err(|e| e.to_string())
        }
        "write" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path が無い".to_string())?;
            let content = call.arguments["content"]
                .as_str()
                .ok_or_else(|| "content が無い".to_string())?;
            let path = Path::new(path);
            ctx.gate.check(ctx.sandbox, path, ctx.approver)?;
            polaris_tools::write::write(ctx.sandbox, ctx.helper, path, content)
                .map_err(|e| e.to_string())
        }
        "edit" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path が無い".to_string())?;
            let old = call.arguments["old"]
                .as_str()
                .ok_or_else(|| "old が無い".to_string())?;
            let new = call.arguments["new"]
                .as_str()
                .ok_or_else(|| "new が無い".to_string())?;
            let path = Path::new(path);
            ctx.gate.check(ctx.sandbox, path, ctx.approver)?;
            polaris_tools::edit::edit(ctx.sandbox, ctx.helper, path, old, new)
                .map_err(|e| e.to_string())
        }
        "bash" => {
            let command = call.arguments["command"]
                .as_str()
                .ok_or_else(|| "command が無い".to_string())?;
            polaris_tools::bash::run(ctx.sandbox, command).map_err(|e| e.to_string())
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

    /// テスト用の承認者。常に許可し、尋ねられた回数を数える。
    struct AlwaysAllow {
        asked: usize,
    }
    impl crate::approval::Approver for AlwaysAllow {
        fn ask(&mut self, _reason: &str) -> crate::approval::Decision {
            self.asked += 1;
            crate::approval::Decision::Allow
        }
    }

    /// テスト用の承認者。尋ねられた回数と最後の理由を記録しつつ、指定した
    /// 決定を返す。`Gate::check` が実際に呼ばれたか（＝実行前に相談したか）
    /// を配線レベルで観測するための唯一の手掛かり。`asked == 0` は
    /// 「述語を一切通していない」ことの直接の証拠になる —— サンドボックス
    /// 自身の拒否メッセージは `write`/`edit` の事前チェックと見分けが付かない
    /// ことがある（Fix round 1 の指摘）ため、拒否の文面だけでは不十分。
    struct RecordingApprover {
        decision: crate::approval::Decision,
        asked: usize,
        last_reason: Option<String>,
    }
    impl crate::approval::Approver for RecordingApprover {
        fn ask(&mut self, reason: &str) -> crate::approval::Decision {
            self.asked += 1;
            self.last_reason = Some(reason.to_string());
            self.decision
        }
    }

    /// `write` / `edit` / `bash` を呼ばないテストのための、使われない文脈一式。
    /// `ToolContext` は参照しか持たないので、借用元をここで所有したまま
    /// 呼び出し側へ返し、各テストの中で `ToolContext` を組み立ててもらう。
    fn dummy_tool_parts() -> (
        polaris_sandbox::SandboxPolicy,
        std::path::PathBuf,
        crate::approval::Gate,
        AlwaysAllow,
    ) {
        (
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::FullAccess, &[])
                .expect("方針"),
            std::path::PathBuf::from("/bin/true"),
            crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never),
            AlwaysAllow { asked: 0 },
        )
    }

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

    /// 公開したツール定義そのものから、必須と宣言されている引数名を取り出す。
    ///
    /// テスト側に `"q"` と書いてしまうと、スキーマの宣言と `dispatch` の
    /// 読み出しは互いに独立した2つの主張のまま残る。片方だけが変わっても
    /// 両方の主張はそれぞれ自分の中では正しいので、どのテストも落ちない
    /// —— モデルには `query` を送れと伝え、ハーネスは `q` を探し、
    /// skill 呼び出しが全滅する状態で緑になる。
    fn declared_required_param(tool: &str) -> String {
        let specs = polaris_tools::all_specs();
        let spec = specs
            .iter()
            .find(|s| s.name == tool)
            .unwrap_or_else(|| panic!("{tool} のツール定義が無い"));
        let json = serde_json::to_value(spec).expect("直列化できない");
        json["parameters"]["required"][0]
            .as_str()
            .unwrap_or_else(|| panic!("{tool} のスキーマが必須引数を宣言していない"))
            .to_string()
    }

    fn call_with(tool: &str, param: &str, value: &str) -> ToolCall {
        let mut args = serde_json::Map::new();
        args.insert(param.to_string(), serde_json::Value::String(value.into()));
        ToolCall {
            id: "c1".into(),
            name: tool.into(),
            arguments: serde_json::Value::Object(args),
        }
    }

    #[test]
    fn the_skill_tool_reads_the_argument_name_its_schema_declares() {
        // 引数名をリテラルではなく公開スキーマから取る。スキーマ側の `q` を
        // `query` へ変えると（dispatch は `q` を読んだまま）このテストだけで
        // 食い違いが露見する。
        let skills = vec![polaris_skills::Skill {
            name: "demo".into(),
            description: "説明".into(),
            body: "デモ本文".into(),
            path: "/x/demo/SKILL.md".into(),
        }];
        let param = declared_required_param("skill");
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out =
            dispatch(&call_with("skill", &param, "demo"), &skills, &mut ctx).unwrap_or_else(|e| {
                panic!("公開スキーマが宣言する引数名 {param} を dispatch が読んでいない: {e}")
            });
        assert!(out.contains("デモ本文"), "本文が返っていない: {out}");
    }

    #[test]
    fn the_read_tool_reads_the_argument_name_its_schema_declares() {
        // skill 側と同じ束ね方を read にも掛ける。read のスキーマは M1 から
        // あるが、宣言と読み出しを結ぶものは同じく無かった。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("書けない");

        let param = declared_required_param("read");
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out = dispatch(
            &call_with("read", &param, target.to_str().expect("パス")),
            &[],
            &mut ctx,
        )
        .unwrap_or_else(|e| {
            panic!("公開スキーマが宣言する引数名 {param} を dispatch が読んでいない: {e}")
        });
        assert!(out.contains("hello"), "本文が返っていない: {out}");
    }

    #[test]
    fn a_read_without_an_explicit_limit_says_it_stopped_early() {
        // limit を省いた呼び出しには dispatch が既定値を補う。補われた側は
        // 自分が切り詰められたことを知らないため、`(全 N 行中 …)` の断り書き
        // だけが「ファイルはここで終わっていない」を伝える唯一の手段になる。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("long.txt");
        let total = polaris_tools::read::DEFAULT_LIMIT + 3;
        let body: String = (0..total).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&target, body).expect("書けない");

        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out = dispatch(
            &call_with("read", "path", target.to_str().expect("パス")),
            &[],
            &mut ctx,
        )
        .expect("読めるべき");

        assert!(
            out.contains(&format!(
                "続きは offset={} で読む",
                polaris_tools::read::DEFAULT_LIMIT
            )),
            "既定の limit で打ち切ったことが結果に出ていない: {}",
            &out[out.len().saturating_sub(160)..]
        );
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

        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
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

        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
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

        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let err = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
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
                CompletionResponse {
                    text: "読んだ".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("demo の本文を読んで");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &skills,
            &mut ctx,
        )
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
                CompletionResponse {
                    text: "了解".into(),
                    tool_calls: vec![],
                },
            ]),
        };
        let mut session = Session::new();
        session.push_user("skill を探して");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("失敗");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        // 「非空であること」だけを見ると、どんな置き換えでも通ってしまう
        // —— skill が1件も無いことを伝える分岐を丸ごと削っても、検索が外れた
        // ときの候補一覧（この場合は空の一覧）が返るだけで、このテストは
        // 緑のままだった。skill.rs 側の
        // `an_empty_skill_set_is_distinguishable_from_a_query_matching_nothing`
        // が固定しているのと同じ固有の文言を、ループを通した経路でも見る。
        assert!(
            tool_msg.content.contains("1 件も見つからない"),
            "skill が1件も無いことがモデルへ届いていない: {}",
            tool_msg.content
        );
    }

    /// `write` を 1 回呼んでから本文を返すプロバイダと、その周辺一式。
    /// ヘルパは polaris 本体ではなく最小のシェルスクリプトを使う。ここで
    /// 見たいのはループの配線であって、ヘルパ自身の正しさではない。
    fn write_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("helper.sh");
        std::fs::write(
            &p,
            r#"#!/bin/sh
python3 -c '
import json,sys,os
m=json.load(sys.stdin)
os.makedirs(os.path.dirname(m["path"]), exist_ok=True)
open(m["path"],"w").write(m["content"])
print("wrote")
'
"#,
        )
        .expect("書けない");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    #[tokio::test]
    async fn the_write_tool_reads_the_argument_names_its_schema_declares() {
        // 公開スキーマが宣言する引数名と dispatch が読む名前を束ねる。
        // スキーマ側だけを改名しても全テストが通る状態を残さない
        // （M3a の B1 と同じ欠陥クラス）。引数名はスキーマから取り出し、
        // このテストの中に literal で書かない。
        let specs = polaris_tools::all_specs();
        let spec = specs
            .iter()
            .find(|s| s.name == "write")
            .expect("write が無い");
        let json = serde_json::to_value(spec).expect("直列化");
        let required: Vec<String> = json["parameters"]["required"]
            .as_array()
            .expect("required が無い")
            .iter()
            .map(|v| {
                v.as_str()
                    .expect("required の要素が文字列でない")
                    .to_string()
            })
            .collect();
        assert_eq!(
            required.len(),
            2,
            "write の必須引数が 2 個でない: {required:?}"
        );

        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");
        let target = sandbox.writable_roots()[0].join("out.txt");

        // スキーマが宣言する名前だけを使って引数を組み立てる。片方が
        // "path" 以外へ改名されていれば、それが content 用の値を受け取り、
        // 書き込み先が食い違って下の assert が落ちる。
        let mut args = serde_json::Map::new();
        for name in &required {
            let value = if name.contains("path") {
                target.display().to_string()
            } else {
                "本文".to_string()
            };
            args.insert(name.clone(), serde_json::Value::String(value));
        }

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::Value::Object(args),
                    }],
                },
                CompletionResponse {
                    text: "書いた".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let mut session = Session::new();
        session.push_user("a.txt を作って");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::OnRequest);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("ループが失敗した");

        assert_eq!(out, "書いた");
        assert_eq!(
            std::fs::read_to_string(&target).expect("書かれていない"),
            "本文",
            "スキーマが宣言する引数名を dispatch が読んでいない"
        );
    }

    #[tokio::test]
    async fn a_denied_write_comes_back_as_a_tool_result_not_a_loop_failure() {
        // サンドボックス拒否は例外ではない。モデルが理解できる形で返し、
        // ループは続く。ここで Err にすると、モデルは別の場所へ書き直す
        // 機会を失い、拒否がそのまま実行の失敗になる。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");

        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "本文"
                        }),
                    }],
                },
                CompletionResponse {
                    text: "別の場所へ書き直す".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let mut session = Session::new();
        session.push_user("外へ書いて");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        // Never にして、承認で通り抜ける経路を塞ぐ。ここで見たいのは
        // 「拒否がツール結果として返る」ことである。
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("拒否でループごと失敗した");

        assert_eq!(
            out, "別の場所へ書き直す",
            "ループが 2 ターン目へ進んでいない"
        );
        assert!(!target.exists(), "ファイルが作られている");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        assert!(
            tool_msg.content.contains("workspace-write"),
            "拒否の理由に方針が無い: {}",
            tool_msg.content
        );
        assert!(
            tool_msg.content.contains(&target.display().to_string()),
            "拒否の理由にパスが無い: {}",
            tool_msg.content
        );
    }

    #[tokio::test]
    async fn a_write_to_a_hardlinked_path_inside_the_root_is_stopped_by_the_gate_before_anything_runs()
     {
        // Fix round 1 の指摘: 上の `a_denied_write_comes_back_...` はルート外
        // への書き込みを使っており、`Gate::check` を外しても実サンドボックス
        // 自身が同じ形の拒否メッセージ（パスと方針を含む）を返すため、
        // 「述語が実行前に止めた」ことと「試行してサンドボックスに拒否
        // された」ことを区別できない。ハードリンクはこの区別を作れる —— 対象
        // は書込可能ルートの内側（`<root>/hardlink.txt`）にあり、実サンド
        // ボックスはパスだけを見て許可してしまう
        // （`polaris_tools::predicate::tests::an_existing_hardlink_is_surfaced_for_approval`
        // 参照）。`Gate::check` が実行前に止めていることを、(1) 承認者へ
        // 実際に尋ねたこと、(2) 尋ねた理由にハードリンクの説明が含まれる
        // こと、(3) ヘルパが一度も走らずファイルの中身が変わっていない
        // こと、(4) ツール結果の文面がサンドボックス側の拒否
        // （`ToolError::WriteDenied` の「子の出力:」）ではなく `Gate` 自身の
        // 拒否文面であること、の4点で確認する。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = write_helper(helper_dir.path());

        let real = outside.path().join("real.txt");
        std::fs::write(&real, "original").expect("書けない");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");
        let linked = sandbox.writable_roots()[0].join("hardlink.txt");
        std::fs::hard_link(&real, &linked).expect("hard_link");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": linked.display().to_string(),
                            "content": "改竄"
                        }),
                    }],
                },
                CompletionResponse {
                    text: "別の場所へ".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let mut session = Session::new();
        session.push_user("hardlink.txt を書き換えて");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::OnRequest);
        let mut approver = RecordingApprover {
            decision: crate::approval::Decision::Deny,
            asked: 0,
            last_reason: None,
        };
        {
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };

            let out = run(
                &p,
                &mut session,
                &mut audit,
                &mut stop,
                &always_on,
                &[],
                &mut ctx,
            )
            .await
            .expect("拒否でループごと失敗した");
            assert_eq!(out, "別の場所へ");
        }

        assert_eq!(approver.asked, 1, "承認者へ尋ねていない");
        assert!(
            approver
                .last_reason
                .as_deref()
                .unwrap_or("")
                .contains("ハードリンク"),
            "尋ねた理由にハードリンクの説明が無い: {:?}",
            approver.last_reason
        );

        assert_eq!(
            std::fs::read_to_string(&real).expect("読めない"),
            "original",
            "述語より先にヘルパが実行され、ハードリンクの実体が書き換わっている \
             （実サンドボックスはこの書き込みをパス上は許可してしまう）"
        );

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        assert!(
            tool_msg.content.starts_with("利用者が承認しなかった"),
            "拒否の文面が Gate 自身のものになっていない \
             （サンドボックス側の拒否と混同している可能性がある）: {}",
            tool_msg.content
        );
        assert!(
            !tool_msg.content.contains("子の出力"),
            "サンドボックス側（ToolError::WriteDenied）の拒否文面が混ざっている: {}",
            tool_msg.content
        );
    }

    #[tokio::test]
    async fn bash_attempts_and_reports_without_ever_consulting_the_approver() {
        // Fix round 1 の指摘: `bash` は述語を通さないはずだが、それを配線
        // レベルで確認するテストが無かった。`ApprovalPolicy::Always`
        // （`Verdict` に関わらず必ず尋ねる）の下で `bash` を1回走らせ、
        // 承認者が一度も尋ねられていないことを確認する。Always を選ぶのは、
        // `bash` の腕へ誤って `Gate::check` が混入した場合、対象パスが
        // ルートの内側だろうと外だろうと必ず検出できるようにするため
        // （`OnRequest` だと、混入したチェックの対象パスの選び方次第では
        // 見逃しうる）。
        //
        // Fix round 2 の指摘: 上記だけでは「尋ねない」半分しか固定できて
        // いなかった。仕様が要求するのは「試行し、結果を返す」との連言
        // （何に触れるか事前に決められないため、事前に断るのではなく実際に
        // 触ってみて結果を報告する）であり、`bash` の腕を丸ごと no-op に
        // 置き換えても（＝一切実行しなくても）このテストは緑のままだった
        // （254件全通過で検出できず）。実際に副作用のあるコマンドを走らせ、
        // (1) 副作用が本当に起きたこと（拘束された子が実際に走った証拠）と
        // (2) その出力がツール結果として往路へ戻ってきたこと（試行の結果を
        // 報告している証拠）の両方を確認する。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");
        let proof = sandbox.writable_roots()[0].join("proof.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({
                            "command": format!(
                                "echo bash-really-ran > {} && cat {}",
                                proof.display(),
                                proof.display()
                            )
                        }),
                    }],
                },
                CompletionResponse {
                    text: "終わった".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let mut session = Session::new();
        session.push_user("proof.txt を作って中身を教えて");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
        let mut approver = RecordingApprover {
            decision: crate::approval::Decision::Allow,
            asked: 0,
            last_reason: None,
        };
        {
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: std::path::Path::new("/bin/true"), // bash はヘルパを使わない
                gate: &mut gate,
                approver: &mut approver,
            };

            let out = run(
                &p,
                &mut session,
                &mut audit,
                &mut stop,
                &always_on,
                &[],
                &mut ctx,
            )
            .await
            .expect("失敗");
            assert_eq!(out, "終わった");
        }

        // (1) 副作用が実際に起きたこと。`bash` の腕を no-op に置き換えても
        // 上の `run` 自体は成功しうるので、ここが唯一 no-op を検出できる点。
        assert_eq!(
            std::fs::read_to_string(&proof)
                .expect("proof.txt が無い（bash が実際には実行されていない）")
                .trim(),
            "bash-really-ran",
            "bash が実際に走った形跡が無い"
        );

        // (2) その出力がツール結果としてモデルへ戻っていること（試行の結果を
        // 報告している証拠）。
        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        assert!(
            tool_msg.content.contains("bash-really-ran"),
            "コマンドの出力がツール結果として返っていない: {}",
            tool_msg.content
        );

        // (3) 述語を一切通していないこと（Fix round 1 の元の主張）。
        assert_eq!(
            approver.asked, 0,
            "bash が承認者へ尋ねている（述語を通さないはずの経路に触れている）"
        );
    }

    #[tokio::test]
    async fn a_successful_write_s_audit_record_carries_the_policy_and_the_target() {
        // Fix round 1 の指摘: `write`/`edit` のときに監査記録の `sandbox`/
        // `target` を埋めるという Task 11〜12 の主張を、`agent.rs` の呼び出し
        // 経路を通したところで検査するテストが無かった（`audit.rs` 側は
        // `Record` 型そのものを検査するだけで、呼び出し元の組み立てロジック
        // は対象外）。実際にループを1周させ、書き出された監査行を読み戻して
        // 両欄を確認する。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");
        let target = sandbox.writable_roots()[0].join("audited.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "本文"
                        }),
                    }],
                },
                CompletionResponse {
                    text: "書いた".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let audit_path = dir.path().join("audit.jsonl");
        let mut session = Session::new();
        session.push_user("audited.txt を作って");
        let mut audit = AuditLog::open(&audit_path).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::OnRequest);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("失敗");

        let log = std::fs::read_to_string(&audit_path).expect("読めない");
        let line = log
            .lines()
            .find(|l| l.contains("\"tool\":\"write\""))
            .expect("write の監査行が無い");
        let v: serde_json::Value = serde_json::from_str(line).expect("JSON でない");
        // 完全一致ではなくファイル名の含有で見る。tmpdir の乱数接頭辞
        // （macOS では `/private/var/folders/<hash>/T/.tmpXXXXXX/...`）は
        // それ自体が高エントロピーな文字列に見えるため、`screen()` が
        // `[REDACTED]` へ部分的に書き換えることがある（実測で確認した。
        // secret_screen 自身の「見逃しより過検出を避ける」という設計方針
        // どおりの挙動であり、ここでの欠陥ではない）。完全一致で見ると
        // テストが実行環境の一時ディレクトリ名に左右されて壊れるため、
        // 「target 欄が存在し、対象ファイル名を運んでいる」ことだけを見る。
        assert!(
            v["target"]
                .as_str()
                .expect("target 欄が無い（または null）")
                .contains("audited.txt"),
            "監査記録の target が書込先を運んでいない: {v}"
        );
        assert!(
            v["sandbox"]
                .as_str()
                .expect("sandbox 欄が無い（または null）")
                .contains("workspace-write"),
            "監査記録の sandbox に方針が無い: {v}"
        );
    }
}
