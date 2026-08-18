//! 追記専用の監査ログ。署名は付けない。インプロセスでは署名する主体と
//! 行為する主体が同一であり、署名はログ以上のことを証明しないため。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::secret_screen::{FilterResult, screen_text};

pub struct AuditLog {
    file: File,
}

impl AuditLog {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    /// 1 行を追記する。**ここへ渡るあらゆる文字列は書く直前に伏字化を通る。**
    /// 欄を増やすときは必ず `screen` を通すこと。通し忘れは、そのまま
    /// 生の資格情報がログへ落ちる経路になる。`tool` はモデルのツール呼び出し
    /// からそのまま渡ってくる値であり、閉じた集合ではない（プロンプト
    /// インジェクションを受けたモデルが任意の文字列を出せる）ため、他の欄と
    /// 例外なく同じ経路を通す。
    pub fn record(&mut self, r: &Record<'_>) -> std::io::Result<()> {
        let mut line = serde_json::json!({
            "tool": screen(r.tool),
            "detail": screen(r.detail),
            "result": truncate_result(&screen(r.result)),
        });
        if let Some(p) = r.sandbox {
            line["sandbox"] = serde_json::Value::String(screen(&p.describe()));
        }
        if let Some(t) = r.target {
            line["target"] = serde_json::Value::String(screen(&t.display().to_string()));
        }
        writeln!(self.file, "{line}")?;
        self.file.flush()
    }
}

/// 監査ログ 1 行の内容。仕様が求める「型、解決後のサンドボックス方針、
/// 書込先、結果」をこの型が運ぶ。`sandbox` と `target` は「無い」ことと
/// 「空文字列だった」ことを区別するため `Option` で受け、`None` のときは
/// 欄ごと省く（[`AuditLog::record`] 側の仕事）。
pub struct Record<'a> {
    pub tool: &'a str,
    pub detail: &'a str,
    pub sandbox: Option<&'a polaris_sandbox::SandboxPolicy>,
    pub target: Option<&'a Path>,
    pub result: &'a str,
}

/// 監査ログへ書くあらゆる文字列が通る唯一の関門。`FilterResult::Drop`
/// （行全体が丸ごとシークレットだった場合）は元の文字列を一切書かず
/// `[DROPPED]` に置き換える。
fn screen(s: &str) -> String {
    match screen_text(s) {
        FilterResult::Keep(s) | FilterResult::Redacted(s) => s,
        FilterResult::Drop => "[DROPPED]".to_string(),
    }
}

/// `result` 欄だけの上限（バイト）。監査ログの役目は「何をして、どう
/// 終わったか」を再構成できることであり、成功した呼び出しの本文をまるごと
/// 複製する場所ではない。特に `read` は成功するとファイル全体を本文として
/// 返すため、上限を設けないと監査ログがワークスペースの複製先になる。
///
/// `polaris_tools::bash::MAX_OUTPUT_BYTES`（32 KiB）はモデルへ返す一次
/// チャンネルの上限であり、そちらは応答の実物である必要がある。ここは
/// 二次的な記録で、読む側が話を再構成し実物を見に行くための手掛かりが
/// 残ればよいため、より小さい 4 KiB を選ぶ。
const MAX_RESULT_BYTES: usize = 4 * 1024;

/// `result` を上限まで切り詰める。**バイト単位で単純に切ると多バイト文字の
/// 途中で割れて panic する** ため、文字境界まで戻ってから切る
/// （`polaris_tools::bash::truncate` と同じ考え方）。切り詰めたことを
/// 本文へ明記するのは、印の無い部分的な結果は完全な答えとして提示された
/// 誤った答えになるため。
fn truncate_result(s: &str) -> String {
    if s.len() <= MAX_RESULT_BYTES {
        return s.to_string();
    }
    let mut end = MAX_RESULT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[監査ログでは {} バイトを超えたのでここで切り詰めた。全文はツール呼び出しの結果側にある]",
        &s[..end],
        MAX_RESULT_BYTES
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_before_writing() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        log.record(&Record {
            tool: "bash",
            detail: "export API_TOKEN=sk-abcdefghijklmnopqrstuvwxyz012345",
            sandbox: None,
            target: None,
            result: "ok",
        })
        .expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert!(
            !body.contains("sk-abcdefghijklmnopqrstuvwxyz012345"),
            "生の値が残っている"
        );
        assert!(body.contains("[REDACTED]"), "伏字化されていない");
        assert!(body.contains("\"tool\":\"bash\""), "ツール名が無い");
    }

    #[test]
    fn appends_one_line_per_record() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        log.record(&Record {
            tool: "read",
            detail: "src/main.rs",
            sandbox: None,
            target: None,
            result: "ok",
        })
        .expect("書けない");
        log.record(&Record {
            tool: "read",
            detail: "src/lib.rs",
            sandbox: None,
            target: None,
            result: "ok",
        })
        .expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert_eq!(body.lines().count(), 2);
    }

    #[test]
    fn redacts_secrets_in_tool_field_too() {
        // tool はモデルのツール呼び出しからそのまま渡ってくる値であり、
        // プロンプトインジェクションを受けたモデルが任意の文字列を出しうる。
        // detail と同じ経路で伏字化されることを確認する。
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        log.record(&Record {
            tool: "export API_TOKEN=sk-abcdefghijklmnopqrstuvwxyz012345",
            detail: "harmless detail",
            sandbox: None,
            target: None,
            result: "ok",
        })
        .expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert!(
            !body.contains("sk-abcdefghijklmnopqrstuvwxyz012345"),
            "tool 内の生の値が残っている"
        );
        assert!(body.contains("[REDACTED]"), "tool が伏字化されていない");
    }

    #[test]
    fn a_record_carries_the_policy_the_target_and_the_result() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");

        let policy = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");

        log.record(&Record {
            tool: "write",
            detail: "{\"path\":\"a.txt\"}",
            sandbox: Some(&policy),
            target: Some(std::path::Path::new("/w/a.txt")),
            result: "ok",
        })
        .expect("書けない");

        let line = std::fs::read_to_string(&path).expect("読めない");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON でない");
        assert_eq!(v["tool"], "write");
        assert_eq!(v["result"], "ok");
        assert_eq!(v["target"], "/w/a.txt");
        assert!(
            v["sandbox"]
                .as_str()
                .expect("sandbox が無い")
                .contains("workspace-write"),
            "方針が記録されていない: {v}"
        );
    }

    #[test]
    fn every_new_field_passes_through_the_secret_screen() {
        // 欄を増やすたびに伏字化を通し忘れる穴が開く。M1 では tool 欄が
        // 通っていない時期があった。ここでは target と result の双方に
        // 秘密らしき文字列を入れ、そのまま落ちないことを見る。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");

        let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        log.record(&Record {
            tool: "bash",
            detail: "echo x",
            sandbox: None,
            target: Some(std::path::Path::new(secret)),
            result: secret,
        })
        .expect("書けない");

        let line = std::fs::read_to_string(&path).expect("読めない");
        assert!(
            !line.contains(secret),
            "秘密が生のまま監査ログに落ちている: {line}"
        );
    }

    #[test]
    fn an_absent_policy_and_target_are_omitted_rather_than_written_as_empty() {
        // 空文字列を書くと、「方針が無い」と「方針が空文字列だった」が
        // 区別できなくなる。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");

        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: "ok",
        })
        .expect("書けない");

        let line = std::fs::read_to_string(&path).expect("読めない");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON でない");
        assert!(v.get("sandbox").is_none() || v["sandbox"].is_null(), "{v}");
        assert!(v.get("target").is_none() || v["target"].is_null(), "{v}");
    }

    #[test]
    fn writes_dropped_marker_when_entire_detail_is_secret() {
        // 行全体が裸のシークレットのみで、周囲の文脈が無い場合
        // screen_text は FilterResult::Drop を返す（secret_screen::tests::
        // drops_bare_high_entropy_secret_with_no_surrounding_context と同じ
        // 入力）。record はこの分岐で元の文字列を一切書かず [DROPPED] を書く。
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        let bare_secret = "sk-abc123DEF456ghi789XYZ000aaa111";
        assert!(matches!(screen_text(bare_secret), FilterResult::Drop));

        log.record(&Record {
            tool: "bash",
            detail: bare_secret,
            sandbox: None,
            target: None,
            result: "ok",
        })
        .expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert!(!body.contains(bare_secret), "生の値が残っている");
        assert!(
            body.contains("[DROPPED]"),
            "Drop 分岐でマーカーが書かれていない"
        );
    }

    #[test]
    fn a_result_over_the_ceiling_is_truncated_and_says_so() {
        // read が成功するとファイル全体を result として渡してくる。
        // 上限を設けないと監査ログがワークスペースの複製先になる。
        //
        // 空白無しで 20 文字以上続く塊は screen() の高エントロピー判定に
        // 掛かって丸ごと [DROPPED] になってしまう（それ自体は正しい挙動）ため、
        // 単語を空白で区切った、ファイル本文らしい内容を使う。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");

        let long = "lorem ipsum dolor sit amet ".repeat(MAX_RESULT_BYTES / 10);
        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: &long,
        })
        .expect("書けない");

        let line = std::fs::read_to_string(&path).expect("読めない");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON でない");
        let recorded = v["result"].as_str().expect("result が無い");
        assert!(
            recorded.len() < long.len(),
            "切り詰められていない: {} バイト",
            recorded.len()
        );
        assert!(
            recorded.contains("切り詰め"),
            "切り詰めたことが本文に無い: {recorded}"
        );
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // バイト単位で単純に切ると多バイト文字の途中で割れて panic する。
        // "あ" は 3 バイトなので、MAX_RESULT_BYTES（4096、3 の倍数でない）を
        // 単純に切ると境界に当たらない位置を必ず踏む。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");

        let long = "あ".repeat(MAX_RESULT_BYTES / 3 + 10);
        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: &long,
        })
        .expect("書けない（境界の途中で切って panic した可能性がある）");

        let line = std::fs::read_to_string(&path).expect("読めない");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON でない");
        let recorded = v["result"].as_str().expect("result が無い");
        assert!(
            recorded.len() <= MAX_RESULT_BYTES + 200,
            "上限近辺に収まっていない: {} バイト",
            recorded.len()
        );
    }
}
