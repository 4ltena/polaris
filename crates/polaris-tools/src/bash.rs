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
    fn truncation_lands_on_a_character_boundary_of_the_input() {
        // 元の主張は `out.is_char_boundary(out.len())` だった。これは Rust の
        // どんな `String` に対しても真（`len()` は常に文字境界）であり、何も
        // 確かめていない。実際に効いていたのは「境界まで戻さずに切ると
        // `&s[..end]` が panic する」という副作用だけで、panic を避けるように
        // 書き換えた実装（`get()` で空を返す等）や境界への寄せ方を誤った実装は、
        // 「境界」と名乗るこのテストを緑のまま通り抜ける。
        //
        // 監査ログ側（`polaris_core::audit` の同名テスト）と同じ形にする。
        // 切り詰めた本文が入力の接頭辞であり、その切れ目が入力の文字境界に
        // 載っており、しかも上限のすぐ手前まで来ていることを見る。実コマンド
        // ではなく純粋関数へ直接あてるのは、これが `truncate` の性質であって
        // 子プロセスの性質ではないためである（子を通す経路は
        // `oversized_output_is_truncated_and_says_so` が別に見ている）。
        //
        // "あ" は 3 バイト。MAX_OUTPUT_BYTES は 3 の倍数ではないので、単純に
        // MAX_OUTPUT_BYTES バイト目で切ると必ず文字の途中を踏む。
        let input = "あ".repeat(MAX_OUTPUT_BYTES / 3 + 10);
        assert!(
            input.len() > MAX_OUTPUT_BYTES,
            "前提が崩れている: 上限を超えていない"
        );
        assert!(
            !input.is_char_boundary(MAX_OUTPUT_BYTES),
            "前提が崩れている: 上限がちょうど文字境界に載っており、境界戻しが効かない"
        );

        let out = truncate(&input);
        // 印は本文の次の行から始まる。入力に改行は無いので、最初の行が本文。
        let body = out.split('\n').next().expect("本文が無い");

        assert!(
            input.starts_with(body),
            "切り詰めた本文が入力の接頭辞になっていない（別の文字列を返している）"
        );
        assert!(
            input.is_char_boundary(body.len()),
            "切れ目が入力の文字境界に載っていない: {} バイト目",
            body.len()
        );
        assert!(
            body.len() <= MAX_OUTPUT_BYTES,
            "上限を超えて返している: {} バイト",
            body.len()
        );
        // UTF-8 の 1 文字は最大 4 バイトなので、境界戻しは高々 3 バイト。
        // これより手前で切る実装（空文字列を返す等）はここで落ちる。
        assert!(
            body.len() > MAX_OUTPUT_BYTES - 4,
            "上限よりかなり手前で切っている: {} バイト",
            body.len()
        );
        assert!(out.contains("切り詰め"), "切り詰めたことが本文に無い");
    }
}
