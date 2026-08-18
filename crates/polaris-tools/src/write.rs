//! `write` ツール。実際の書き込みは拘束された子の中で起きる。
//!
//! プロセス内で `std::fs::write` を呼ばないのは、OS の強制がプロセス境界で
//! しか効かないためである。プロセス内で書けば、守っているのはこのクレートの
//! パス判定だけになり、判定の誤りがそのまま範囲外への書き込みになる。

use std::path::Path;

use polaris_sandbox::{Mutation, SandboxPolicy, run_confined};

use crate::ToolError;

pub fn write(
    policy: &SandboxPolicy,
    helper: &Path,
    path: &Path,
    content: &str,
) -> Result<String, ToolError> {
    let mutation = Mutation::Write {
        path: path.to_path_buf(),
        content: content.to_string(),
    };
    run_mutation(policy, helper, &mutation, path)
}

/// `write` と `edit` が共有する起動と結果の解釈。
pub(crate) fn run_mutation(
    policy: &SandboxPolicy,
    helper: &Path,
    mutation: &Mutation,
    path: &Path,
) -> Result<String, ToolError> {
    let payload = serde_json::to_string(mutation)
        .map_err(|e| ToolError::Io(std::io::Error::other(format!("操作を直列化できない: {e}"))))?;

    let outcome = run_confined(
        policy,
        helper,
        &["--confined-apply".to_string()],
        Some(&payload),
    )?;

    if outcome.status == 0 {
        return Ok(outcome.stdout.trim().to_string());
    }

    Err(ToolError::WriteDenied {
        path: path.display().to_string(),
        policy: policy.describe(),
        detail: outcome.stderr.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    use crate::write::write;

    /// 標準入力をそのまま `target` へ書き出すだけの、最小のヘルパ代役。
    ///
    /// 実際のヘルパ（`--confined-apply` を受けて JSON を `Mutation` として
    /// 解釈する）とは違う。ここで確かめたいのはツール層が「変更操作を
    /// stdin に乗せる」「拘束された子として起動する」「子の終了状態と
    /// 標準出力・標準エラーをどう解釈するか」の3点であり、JSON の意味論
    /// （一致件数の判定など）は Task 7 の `helper::apply` の単体テストが
    /// すでに押さえている。ここへ python3 を挟むと、このマシンでは
    /// `/usr/bin/python3` が存在せず `/Library/Frameworks` 配下のユーザ
    /// 導入 3.11 に解決されるため、サンドボックスを主題とするテストが
    /// インタプリタの所在という無関係な依存を抱え込む。`/bin/sh` と
    /// `cat` だけで足りる。
    ///
    /// `set -e` が要る。redirection の失敗（サンドボックスによる拒否）は
    /// `cat` 自体を起動する前にシェルが検出するが、それだけでは非特殊
    /// 組込みコマンドの1行が失敗しただけとして扱われ、スクリプトは次の
    /// `echo wrote` へ進んでしまう。それだと拒否されたはずの試行が
    /// 「成功して wrote と言った」ことになり、受け入れ基準を確かめる
    /// つもりのテストが誤った理由で通る。`set -e` を付けて、拒否が
    /// スクリプト自体の非0終了として最後まで伝わるようにする。
    fn success_helper(dir: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-helper-success");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nset -e\ncat > {}\necho wrote\n",
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

    #[test]
    fn a_write_inside_the_root_succeeds_through_the_confined_helper() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = policy.writable_roots()[0].join("out.txt");
        let helper = success_helper(helper_dir.path(), &target);

        let msg = write(&policy, &helper, &target, "本文").expect("失敗した");

        // 代役ヘルパは JSON を解釈しないので、ファイルに残るのは `content`
        // そのものではなく、標準入力に乗った変更操作の直列化結果である。
        // 見たいのは「stdin に本当に乗って子まで届いた」という配線であり、
        // ヘルパの意味論ではない（ブリーフ訂正の通り、弱い主張で足りる）。
        let written = std::fs::read_to_string(&target).expect("読めない");
        assert!(
            written.contains("本文"),
            "内容が stdin に乗って子まで届いていない: {written}"
        );
        assert!(!msg.trim().is_empty(), "結果の説明が空");
    }

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_sandbox_through_the_tool() {
        // 受け入れ基準 3。モックを使わず、`write` ツールを通して実際に
        // 書き込みを試み、拒否を観測する。共有の起動ヘルパ（run_confined）
        // を直接叩く経路では基準を満たさない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");
        let helper = success_helper(helper_dir.path(), &target);

        let err =
            write(&policy, &helper, &target, "本文").expect_err("ルート外への書き込みが成功した");

        assert!(
            !target.exists(),
            "ファイルが作られている: {}",
            target.display()
        );
        // 拒否メッセージは、拒否されたパスと方針と書込可能ルートを含む。
        let msg = err.to_string();
        assert!(
            msg.contains(&target.display().to_string()),
            "パスが無い: {msg}"
        );
        assert!(msg.contains("workspace-write"), "方針が無い: {msg}");
    }

    #[test]
    fn a_helper_that_cannot_be_confined_is_an_error_not_a_silent_success() {
        // 拘束できなかったのに書けてしまう状態を作らない。
        // macOS では起動するのは `/usr/bin/sandbox-exec` であり、存在しない
        // ヘルパは拘束された子の中での exec 失敗として現れる。したがって
        // `run_confined` 自体は `Err` ではなく非0の `status` を持つ
        // `Ok(Outcome)` を返し、それを非0終了として解釈する `write` 側の
        // 通常の拒否経路がここを捕まえる（実測: `sandbox-exec: execvp() ...
        // failed: No such file or directory` が `detail` に載る）。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");
        let missing = root.path().join("no-such-helper");
        let target = policy.writable_roots()[0].join("x.txt");

        assert!(write(&policy, &missing, &target, "本文").is_err());
        assert!(!target.exists());
    }
}
