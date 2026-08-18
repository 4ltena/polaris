//! 拘束下でのプロセス起動。プラットフォームごとの実装をここで振り分ける。
//!
//! 返す `Outcome` は「子が走ったうえでの結果」である。子を拘束できなかった
//! 場合は `Outcome` ではなく `SandboxError::NotEnforced` を返す。両者を
//! 混ぜると、拘束されていない子が走ったことを呼び出し側が検出できない。

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::SandboxError;
use crate::policy::SandboxPolicy;

#[derive(Debug)]
pub struct Outcome {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 拘束下で `program` を起動する。`stdin` を渡すと子の標準入力へ流し込む。
pub fn run_confined(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
) -> Result<Outcome, SandboxError> {
    let mut cmd = build_command(policy, program, args)?;

    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    if let Some(s) = stdin {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| SandboxError::NotEnforced("標準入力を開けない".into()))?
            .write_all(s.as_bytes())?;
        // drop して EOF を送る。閉じないと `cat` のような子が待ち続ける。
        drop(child.stdin.take());
    }

    let out = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // 拘束できなかった場合はここで打ち切る。子が走ったかどうかに関わらず、
    // 走った子が拘束されていた保証が無いためである。
    if let Some(detail) = classify_apply_failure(&stderr) {
        return Err(SandboxError::NotEnforced(detail));
    }

    Ok(Outcome {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
    })
}

#[cfg(target_os = "macos")]
fn build_command(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
) -> Result<Command, SandboxError> {
    let mut cmd = Command::new(crate::macos::SANDBOX_EXEC);
    cmd.args(crate::macos::build_args(policy, program, args));
    Ok(cmd)
}

#[cfg(target_os = "linux")]
fn build_command(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
) -> Result<Command, SandboxError> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(program);
    cmd.args(args);

    let policy_for_child = policy.clone();
    // SAFETY: pre_exec は fork 後 exec 前の子でのみ走る。ここで呼ぶ
    // restrict_self はスレッド単位で一方向なので、親のスレッドには影響しない。
    // 呼び出す関数はメモリ確保を伴うが、この子は直後に exec するため
    // async-signal-safety の制約下にある区間は短く、landlock クレート自身が
    // この使い方を想定している。
    unsafe {
        cmd.pre_exec(move || {
            crate::linux::apply_to_current_process(&policy_for_child)
                .map_err(|e| std::io::Error::other(e.to_string()))
        });
    }
    Ok(cmd)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn build_command(
    _policy: &SandboxPolicy,
    _program: &Path,
    _args: &[String],
) -> Result<Command, SandboxError> {
    // 強制の委譲先が無い環境では、拘束されていない子を起こさない。
    // 「サンドボックスが無いので素通しで実行する」は、この設計では
    // 選択肢に入らない。
    Err(SandboxError::UnsupportedPlatform)
}

/// 標準エラーから「サンドボックスの適用そのものが失敗した」を検出する。
///
/// 方針違反による拒否とは別の事象である。macOS では `sandbox-exec` が
/// `sandbox_apply:` を含む行を出す。通常の拒否は子自身のエラーメッセージ
/// （`Operation not permitted` など）として現れるため、その文字列だけで
/// 判定すると両者を取り違える。
fn classify_apply_failure(stderr: &str) -> Option<String> {
    if stderr.contains("sandbox_apply") {
        return Some(format!("sandbox-exec が方針を適用できなかった: {stderr}"));
    }
    if stderr.contains("landlock") && stderr.contains("強制しなかった") {
        return Some(format!("landlock が強制されなかった: {stderr}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    #[test]
    fn a_command_inside_the_root_succeeds_and_its_output_comes_back() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = policy.writable_roots()[0].join("inside.txt");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), format!("echo ok > {}", target.display())],
            None,
        )
        .expect("拘束実行そのものが失敗した");

        assert_eq!(out.status, 0, "内側への書き込みが失敗した: {out:?}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("読めない").trim(),
            "ok"
        );
    }

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_sandbox() {
        // 受け入れ基準 3 の土台。モックを使わず、実際に書き込みを試みて
        // 拒否を観測する。ツール経由の確認は Task 8 が別に行う。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("nope.txt");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), format!("echo pwned > {}", target.display())],
            None,
        )
        .expect("拘束実行そのものが失敗した");

        assert_ne!(out.status, 0, "ルート外への書き込みが成功した: {out:?}");
        assert!(
            !target.exists(),
            "ファイルが作られている: {}",
            target.display()
        );
    }

    #[test]
    fn stdin_reaches_the_child() {
        // write / edit のヘルパは操作を標準入力から受け取る。ここが通らないと
        // 変更操作が一切成立しない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/cat"),
            &[],
            Some("あいうえお"),
        )
        .expect("拘束実行そのものが失敗した");

        assert_eq!(out.status, 0, "{out:?}");
        assert_eq!(out.stdout.trim(), "あいうえお");
    }

    #[test]
    fn full_access_still_crosses_the_boundary() {
        // 制限しない方針でも子を起こす。ここで直接実行へ分岐すると、
        // 試験している経路と本番の経路が別物になる。
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("方針");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo crossed".into()],
            None,
        )
        .expect("拘束実行そのものが失敗した");
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim(), "crossed");
    }

    #[test]
    fn a_sandbox_that_failed_to_apply_is_an_error_not_a_denial() {
        // 適用の失敗を Outcome として返すと、呼び出し側はそれを方針違反と
        // 区別できない。区別できなければ、拘束されていない子が走ったことに
        // 誰も気づけない。ここでは classify_apply_failure を直接試す。
        assert!(
            classify_apply_failure("sandbox-exec: sandbox_apply: Operation not permitted")
                .is_some(),
            "適用失敗を検出できていない"
        );
        assert!(
            classify_apply_failure("sh: /nope: Operation not permitted").is_none(),
            "通常の拒否を適用失敗と誤判定している"
        );
        assert!(classify_apply_failure("").is_none());
    }
}
