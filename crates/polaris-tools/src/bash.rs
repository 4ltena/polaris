//! `bash` ツール。拘束された `/bin/sh` の中でコマンドを走らせる。
//!
//! 述語を通さない。任意のコードを実行するため、何に触れるかを事前に
//! 決定できないからである。試行し、拒否されたら理由をモデルへ返す。

use std::path::Path;

use polaris_sandbox::{SandboxPolicy, run_confined};

use crate::ToolError;

/// 返す出力の上限。`grep` と `find` がここを通るため、大きな出力は
/// 例外ではなく通常の事象である。
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;

pub fn run(policy: &SandboxPolicy, command: &str) -> Result<String, ToolError> {
    let outcome = run_confined(
        policy,
        Path::new("/bin/sh"),
        &["-c".to_string(), command.to_string()],
        None,
    )?;

    let combined = if outcome.stderr.trim().is_empty() {
        outcome.stdout
    } else {
        format!("{}{}", outcome.stdout, outcome.stderr)
    };
    let body = truncate(&combined);

    if outcome.status == 0 {
        return Ok(body);
    }

    Err(ToolError::CommandFailed {
        status: outcome.status,
        policy: policy.describe(),
        detail: body,
    })
}

/// 上限を超えたら文字境界で切り、切り詰めたことを本文で述べる。
/// 印の無い部分的な結果は、完全な答えとして提示された誤った答えである。
fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[出力が {} バイトを超えたのでここで切り詰めた。必要なら範囲を絞って再実行すること]",
        &s[..end],
        MAX_OUTPUT_BYTES
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    fn workspace(root: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()]).expect("方針")
    }

    #[test]
    fn stdout_comes_back() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let out = run(&workspace(dir.path()), "echo こんにちは").expect("失敗");
        assert!(out.contains("こんにちは"), "{out}");
    }

    #[test]
    fn a_failing_command_returns_its_stderr_and_its_exit_code() {
        // 終了コードだけを返すと、モデルは何が悪かったのか分からず同じ
        // コマンドを繰り返す。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let err = run(&workspace(dir.path()), "echo 失敗の理由 >&2; exit 3")
            .expect_err("失敗が成功として返った");
        let msg = err.to_string();
        assert!(msg.contains("失敗の理由"), "標準エラーが無い: {msg}");
        assert!(msg.contains('3'), "終了コードが無い: {msg}");
    }

    #[test]
    fn a_write_outside_the_root_is_denied_and_the_message_names_the_policy() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");

        let err = run(
            &workspace(root.path()),
            &format!("echo pwned > {}", target.display()),
        )
        .expect_err("ルート外への書き込みが成功した");

        assert!(!target.exists(), "ファイルが作られている");
        assert!(
            err.to_string().contains("workspace-write"),
            "方針が伝わらない: {err}"
        );
    }

    #[test]
    fn a_write_inside_the_root_succeeds() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());
        let target = policy.writable_roots()[0].join("ok.txt");
        run(&policy, &format!("echo ok > {}", target.display())).expect("内側への書き込みが失敗");
        assert!(target.exists());
    }

    #[test]
    fn oversized_output_is_truncated_and_says_so() {
        // 上限そのものを試す。短い入力で「切り詰めなかった」ことだけを見る
        // テストは、上限を何桁変えても通ってしまう。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let out = run(
            &workspace(dir.path()),
            &format!("head -c {} /dev/zero | tr '\\0' 'a'", MAX_OUTPUT_BYTES * 2),
        )
        .expect("失敗");

        assert!(out.len() < MAX_OUTPUT_BYTES * 2, "切り詰められていない");
        assert!(out.contains("切り詰め"), "切り詰めたことが本文に無い");
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // バイト単位で切ると多バイト文字の途中で割れる。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let n = MAX_OUTPUT_BYTES / 3 + 10;
        let out = run(
            &workspace(dir.path()),
            &format!("for i in $(seq {n}); do printf 'あ'; done"),
        )
        .expect("失敗");
        assert!(out.is_char_boundary(out.len()), "文字境界で切れていない");
    }
}
