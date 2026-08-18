//! `edit` ツール。`write` と同じ拘束経路を通る。

use std::path::Path;

use polaris_sandbox::{Mutation, SandboxPolicy};

use crate::ToolError;

pub fn edit(
    policy: &SandboxPolicy,
    helper: &Path,
    path: &Path,
    old: &str,
    new: &str,
) -> Result<String, ToolError> {
    let mutation = Mutation::Edit {
        path: path.to_path_buf(),
        old: old.to_string(),
        new: new.to_string(),
    };
    crate::write::run_mutation(policy, helper, &mutation, path)
}

#[cfg(test)]
mod tests {
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    use crate::edit::edit;

    /// `write.rs` の `success_helper` と同じ理由づけで `/bin/sh` だけを使う。
    /// stdin をそのまま `target` へ書き出し、`edited` と報告するだけの代役。
    fn success_helper(dir: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-edit-helper-success");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nset -e\ncat > {}\necho edited\n",
                target.display()
            ),
        )
        .expect("書けない");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p
    }

    /// stdin を捨てて（読み切ってデッドロックを避けつつ）固定の理由を
    /// 標準エラーへ書いて非0で終了するだけの代役。実際の一致件数の判定
    /// （0件・複数件・重なりの数え方）は Task 7 の `helper::apply` の単体
    /// テストがすでに押さえている。ここで確かめたいのは、子の標準エラーが
    /// ツールのエラーまで裸の終了コードに潰されず届くという配線だけである。
    fn failure_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-edit-helper-failure");
        std::fs::write(
            &p,
            "#!/bin/sh\ncat > /dev/null\necho '置換対象が 2 箇所' 1>&2\nexit 1\n",
        )
        .expect("書けない");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p
    }

    #[test]
    fn an_edit_inside_the_root_succeeds_through_the_confined_helper() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = policy.writable_roots()[0].join("f.txt");
        let helper = success_helper(helper_dir.path(), &target);

        let msg = edit(&policy, &helper, &target, "xxx", "yyy").expect("失敗した");

        // write 側と同じく、代役ヘルパは JSON を解釈しないので、ファイルに
        // 残るのは置換結果ではなく stdin に乗った変更操作の直列化結果。
        // old / new の双方が子まで届いたことを確かめる。
        let written = std::fs::read_to_string(&target).expect("読めない");
        assert!(written.contains("xxx"), "old が子へ届いていない: {written}");
        assert!(written.contains("yyy"), "new が子へ届いていない: {written}");
        assert!(!msg.trim().is_empty(), "結果の説明が空");
    }

    #[test]
    fn a_failing_edit_returns_the_child_s_reason_rather_than_a_bare_exit_code() {
        // 「exit 1」だけを返すと、モデルは何を直せばよいか分からず同じ
        // 失敗を繰り返す。往復とトークンの浪費になる。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");
        let helper = failure_helper(helper_dir.path());

        let target = policy.writable_roots()[0].join("f.txt");

        let err = edit(&policy, &helper, &target, "xxx", "yyy").expect_err("失敗ヘルパが通った");
        assert!(
            err.to_string().contains("2 箇所"),
            "理由が伝わらない: {err}"
        );
    }
}
