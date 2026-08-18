//! 拘束下でのプロセス起動。プラットフォームごとの実装をここで振り分ける。
//!
//! 返す `Outcome` は「子が走ったうえでの結果」である。子を拘束できなかった
//! 場合は `Outcome` ではなく `SandboxError::NotEnforced` を返す。両者を
//! 混ぜると、拘束されていない子が走ったことを呼び出し側が検出できない。
//!
//! 二つの設計上の注意点（fix round 1 で追加）:
//!
//! - 標準入力は別スレッドで書く。子の標準入力を親スレッドのまま同期的に
//!   書き切ってから `wait_with_output` で標準出力を読み始めると、子が
//!   読みながら書き返す（`cat` 等）場合に双方のパイプが満杯になり、
//!   互いに相手の排出待ちで固まる。書き込みスレッドが `BrokenPipe` を
//!   受け取っても、それは子が入力を読み切る前に終了しただけであり、
//!   子の本当の `Outcome`（終了コードや出力）を捨てる理由にはならない。
//! - 「適用に失敗した」の検出はプラットフォームで手段が異なる。macOS は
//!   `sandbox-exec` の終了状態（`classify_apply_failure`）、Linux は
//!   `pre_exec` クロージャが返すエラーに積んだ番兵 errno
//!   （`classify_spawn_error`）で見分ける。両者の詳細は各関数のコメントへ。
//!
//! fix round 2 で直した点: `wait_with_output` がエラーを返す経路では、
//! 書き込みスレッドの `JoinHandle` を合流させずに `?` で早期 return して
//! いた。合流しないまま drop すると、スレッドは検知できないまま生き
//! 残る（detach）。`wait_with_output` の結果は `?` を使わずいったん
//! 変数で受け、書き込みスレッドを必ず合流させたあとで、その結果を
//! 見て何を返すか決める。
//!
//! 既知の限界（未解決、意図的に残している）:
//!
//! - Linux の番兵 errno は、`pre_exec` が失敗した「という事実」だけを
//!   親へ運べる。`apply_to_current_process` が返す本当のエラー内容
//!   （カーネルが古い／ruleset を作れない／ルートが消えている、等）は
//!   fork の通知経路の制約上、親には届かない。`NotEnforced` の文言は
//!   「なぜ」ではなく「起きた」までしか言えない。
//! - `classify_apply_failure`（macOS）が拾えるのは、`sandbox_apply` の
//!   文字列と、SIGABRT かつ出力が両方とも空という既知の様態の 2 つだけ
//!   である。これ以外の経路で `sandbox-exec` が静かに適用へ失敗する
//!   様態が存在すれば、それは今のところ `None`（＝ `Ok(Outcome)`）を
//!   返してしまう。実機で確認できた範囲の網羅であり、完全性の主張では
//!   ない。

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

    let mut child = cmd.spawn().map_err(classify_spawn_error)?;

    // 標準入力は別スレッドで書き、親スレッドは wait_with_output で
    // 標準出力・標準エラーの排出に専念する。書き込みと排出を同じ
    // スレッドで順番にやると、子が「読みながら書き返す」種類の
    // プログラム（cat 等）に対してデッドロックする。
    let writer = stdin.map(|s| {
        let mut child_stdin = child
            .stdin
            .take()
            .expect("stdin は Stdio::piped() で開いたので取れるはず");
        let payload = s.to_owned();
        std::thread::spawn(move || -> std::io::Result<()> {
            match child_stdin.write_all(payload.as_bytes()) {
                Ok(()) => Ok(()),
                // 子が入力を読み切る前に終了するのは、こちら側の失敗では
                // ない。子の本当の Outcome は wait_with_output 側が別途
                // 持ってくる。ここで Err にすると、その本当の Outcome を
                // 握り潰してしまう。
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                Err(e) => Err(e),
            }
            // child_stdin はここで drop され、書き込み端を閉じて子へ EOF
            // を伝える。
        })
    });

    // wait_with_output は内部で self.stdin を drop するが、piped にした
    // 場合はすでに上で take() 済みなので、ここでの drop は no-op になる。
    //
    // ここで `?` を使わないのが要点。`wait_with_output` 自体がエラーを
    // 返す経路（稀だが起こりうる）で早期 return すると、下の join に
    // 辿り着けないまま `writer` の `JoinHandle` が drop され、書き込み
    // スレッドが合流されずに検知不能なまま生き残る（detach）。結果を
    // いったん変数で受け、書き込みスレッドを必ず合流させたあとで、
    // 何を返すか決める。
    let wait_result = child.wait_with_output();

    // 呼び出しを抜ける前に、wait_with_output の成否に関わらず書き込み
    // スレッドを必ず合流させる。
    let writer_join_result = writer.map(|handle| handle.join());

    let out = wait_result?;

    if let Some(join_result) = writer_join_result {
        match join_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(SandboxError::Io(e)),
            Err(_) => {
                return Err(SandboxError::Io(std::io::Error::other(
                    "標準入力を書き込むスレッドが panic した",
                )));
            }
        }
    }

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // 拘束できなかった場合はここで打ち切る。子が走ったかどうかに関わらず、
    // 走った子が拘束されていた保証が無いためである。
    if let Some(detail) = classify_apply_failure(&out.status, &stdout, &stderr) {
        return Err(SandboxError::NotEnforced(detail));
    }

    Ok(Outcome {
        status: out.status.code().unwrap_or(-1),
        stdout,
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

/// pre_exec 内で拘束の適用に失敗したことを示す番兵の errno。
///
/// 実在する Linux errno の範囲（拡張分を含めても `ERFKILL`/`EHWPOISON`
/// あたりの 133 程度まで）を大きく超えており、本物の exec 失敗の errno
/// とは衝突しない。値そのものに意味は無い（`b"pola"` を数値化しただけ）。
///
/// この値を `from_raw_os_error` に積む設計にした理由: `rust:1.96` の
/// Linux コンテナで実証した通り、`pre_exec` クロージャが返す `io::Error`
/// のうち `raw_os_error()` を持たないもの（`io::Error::other(msg)` 等で
/// 組み立てた独自エラー）は、fork した子から親への通知経路（自己パイプ
/// 越しの errno 転送）でメッセージが完全に失われ、親側では独自エラーの
/// 種類に関わらず一律 `EINVAL`（22, os error 22）に丸められることを
/// 確認した（`io::Error::other("...")` を含む 5 種類の独自エラーすべてで
/// 再現、3 回の繰り返しでも同じ）。EINVAL は execve 自体が返しうる本物の
/// errno でもあるため、これを適用失敗の判定に使うと本物の exec 失敗と
/// 衝突しうる。一方 `from_raw_os_error(n)` に積んだ値は、実在する
/// errno かどうかに関わらず改変されずに親まで運ばれることも同じ検証で
/// 確認した（200, 999, 65536, i32::MAX, -1 のいずれも往復して一致）。
#[cfg(target_os = "linux")]
const SANDBOX_APPLY_FAILURE_ERRNO: i32 = 0x706f_6c61;

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
    //
    // ここで返すエラーは `SANDBOX_APPLY_FAILURE_ERRNO` の doc が説明する
    // 理由により、元のメッセージを保持できない。番兵 errno だけを積む。
    unsafe {
        cmd.pre_exec(move || {
            crate::linux::apply_to_current_process(&policy_for_child)
                .map_err(|_| std::io::Error::from_raw_os_error(SANDBOX_APPLY_FAILURE_ERRNO))
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

/// `Command::spawn()` が返した `io::Error` を、拘束できなかったことによる
/// 失敗（`NotEnforced`）と、それ以外の起動失敗（`Io`。バイナリが無い等）
/// とに分ける。
///
/// Linux でだけ意味のある分岐である。macOS では `sandbox-exec` の起動
/// 失敗はバイナリ自体が無い等の通常の spawn 失敗であり、適用の成否は
/// 子が実際に走ったあと `classify_apply_failure` が終了状態から判定する。
#[cfg(target_os = "linux")]
fn classify_spawn_error(e: std::io::Error) -> SandboxError {
    if e.raw_os_error() == Some(SANDBOX_APPLY_FAILURE_ERRNO) {
        SandboxError::NotEnforced(format!(
            "pre_exec 内で landlock の適用に失敗した（番兵 errno {SANDBOX_APPLY_FAILURE_ERRNO} \
             で検出）。元のエラー内容は fork の通知経路を通らないため失われる。"
        ))
    } else {
        SandboxError::Io(e)
    }
}

#[cfg(not(target_os = "linux"))]
fn classify_spawn_error(e: std::io::Error) -> SandboxError {
    SandboxError::Io(e)
}

/// 終了状態と出力から「サンドボックスの適用そのものが失敗した」を検出する。
///
/// 方針違反による拒否とは別の事象である。macOS では二重の判定を行う:
///
/// 1. `sandbox-exec` はプロファイルの適用に失敗すると `sandbox_apply:` を
///    含む行を stderr に出すことがある。ただしこれは undocumented かつ
///    英語決め打ちの文字列であり、常に出るとは限らない（2 を参照）。
/// 2. 入れ子の `sandbox-exec` を、`file-read*` を許可しない外側の方針の
///    下で走らせると、適用そのものの失敗として `SIGABRT`（signal 6）で
///    落ち、stdout・stderr がともに空になることを実機で 3/3 回確認した
///    （`/usr/bin/sandbox-exec -f <file-read* を許さない外側プロファイル>
///    /usr/bin/sandbox-exec -p <内側プロファイル> -- ...`）。1 の文字列
///    一致に頼れない場合の保険として、終了状態そのものを見る。
///
/// SIGABRT 単独ではなく「出力が両方とも空であること」も条件に加えるのは、
/// legitimate に abort するプログラム（例えば panic=abort でビルドした
/// バイナリがメッセージを書いてから落ちる場合）を適用失敗と誤判定しない
/// ためである。通常の失敗コマンド（`false` 相当）は通常終了であり
/// `ExitStatusExt::signal()` は `None` のままなので、ここには一切触れない。
#[cfg(target_os = "macos")]
fn classify_apply_failure(
    status: &std::process::ExitStatus,
    stdout: &str,
    stderr: &str,
) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;

    if stderr.contains("sandbox_apply") {
        return Some(format!("sandbox-exec が方針を適用できなかった: {stderr}"));
    }

    const SIGABRT: i32 = 6;
    if status.signal() == Some(SIGABRT) && stdout.is_empty() && stderr.is_empty() {
        return Some(
            "sandbox-exec が SIGABRT で終了し、出力も一切無かった。入れ子の sandbox-exec が \
             適用に失敗した既知の様態（file-read* を許さない外側方針の下での適用失敗）と一致する"
                .to_string(),
        );
    }

    None
}

/// Linux では適用失敗を `classify_spawn_error` がすでに捕捉している
/// （この関数まで来る時点で `pre_exec` は成功済み、つまり landlock は
/// `FullyEnforced` が確定している）。以降に子が何をしても、それは拘束の
/// 失敗ではなく子自身の結果である。
#[cfg(not(target_os = "macos"))]
fn classify_apply_failure(
    _status: &std::process::ExitStatus,
    _stdout: &str,
    _stderr: &str,
) -> Option<String> {
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

    /// `/dev/null` へのリダイレクトが実サンドボックスで通ること。
    ///
    /// プロファイル本文（macOS）や ruleset の組み立て（Linux）を文字列や
    /// 構造として見るテストでは、構文としては正しいが実際には効かない許可を
    /// そのまま通してしまう。ここは本物の `/bin/sh` を本物の拘束下で走らせ、
    /// リダイレクトが開けたかどうかを終了コードで見る。
    ///
    /// `&& echo SURVIVED` を付けているのが要点である。リダイレクトが開けない
    /// とシェルは本体を一度も実行せずに落ちるので、「コマンドが走ったか」を
    /// 標準出力で確かめないと、拒否と「実行はしたが何も出さなかった」を
    /// 区別できない。
    #[test]
    fn a_redirect_to_dev_null_is_allowed_and_the_command_itself_still_runs() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo hi > /dev/null && echo SURVIVED".into()],
            None,
        )
        .expect("拘束実行そのものが失敗した");

        assert_eq!(
            out.status, 0,
            "`> /dev/null` が拒否された。シェルの常套句が方針の拒否として \
             モデルへ届く: {out:?}"
        );
        assert_eq!(out.stdout.trim(), "SURVIVED", "本体が走っていない: {out:?}");

        // `2>` 側も同じ経路であることを確かめる。片方だけ通る状態は無い
        // はずだが、モデルが実際に書くのは両方である。
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo hi 2>/dev/null && echo SURVIVED".into()],
            None,
        )
        .expect("拘束実行そのものが失敗した");
        assert_eq!(out.status, 0, "`2>/dev/null` が拒否された: {out:?}");
    }

    /// read-only でも `/dev/null` は開ける。同時に、それが read-only の
    /// 性質を減らしていないこと——普通のファイルへの書き込みは拒否された
    /// まま——を同じテストの中で対にして見る。片方だけのテストは、
    /// 「`/dev/null` を開けるようにしたつもりで書き込み全体を開けた」
    /// 壊れ方を見逃す。
    #[test]
    fn read_only_can_discard_output_but_still_cannot_write_a_real_file() {
        let scratch = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("方針");

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo hi > /dev/null && echo SURVIVED".into()],
            None,
        )
        .expect("拘束実行そのものが失敗した");
        assert_eq!(
            out.status, 0,
            "read-only で `> /dev/null` が拒否された: {out:?}"
        );
        assert_eq!(out.stdout.trim(), "SURVIVED", "本体が走っていない: {out:?}");

        let target = scratch
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("should-not-exist.txt");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), format!("echo pwned > {}", target.display())],
            None,
        )
        .expect("拘束実行そのものが失敗した");

        assert_ne!(out.status, 0, "read-only なのに書き込みが成功した: {out:?}");
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

    /// finding 1: 5MB は OS のパイプバッファ（通常 64KB 程度）を大きく
    /// 超える。`/bin/cat` は読んだ端から標準出力へ書き戻すので、親が
    /// 標準入力を同期的に書き切ってから標準出力を読み始める実装だと、
    /// 双方のパイプが満杯になり、互いに相手の排出待ちで永久に固まる。
    /// タイムアウトで包み、固まった場合でもテストプロセス自体は
    /// `recv_timeout` で先に進めるようにしてある。
    #[test]
    fn a_large_stdin_payload_does_not_deadlock() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");
        let payload = "x".repeat(5 * 1024 * 1024);

        let (tx, rx) = std::sync::mpsc::channel();
        let payload_for_thread = payload.clone();
        std::thread::spawn(move || {
            let result = run_confined(
                &policy,
                std::path::Path::new("/bin/cat"),
                &[],
                Some(&payload_for_thread),
            );
            // 受け手がタイムアウトで先に諦めていても構わない。
            let _ = tx.send(result);
        });

        let out = rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("run_confined が固まった（デッドロックの疑い）")
            .expect("拘束実行そのものが失敗した");

        assert_eq!(out.status, 0, "{}", out.status);
        assert_eq!(out.stdout.len(), payload.len(), "受け取った量が違う");
        assert_eq!(out.stdout, payload);
    }

    /// finding 2: 子が入力を読み切る前に終了すると、書き込み側は
    /// `BrokenPipe` を受け取ることがある。これは呼び出し側の失敗ではなく、
    /// 子の本当の `Outcome`（終了コードや出力）はそれでも呼び出し側へ
    /// 届かなければならない。
    #[test]
    fn a_child_that_exits_without_reading_stdin_still_returns_its_real_outcome() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");
        let payload = "x".repeat(5 * 1024 * 1024);

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "sleep 0.2; exit 7".into()],
            Some(&payload),
        )
        .expect("拘束実行そのものが失敗した（BrokenPipe を Outcome ではなく Err にしている）");

        assert_eq!(out.status, 7, "{out:?}");
    }

    /// 横断的な制約の確認: `false` 相当（通常終了・シグナルなし・
    /// 出力なし）は、この後 finding 4 で入る SIGABRT 判定にも、
    /// finding 3 の番兵 errno にも触れてはならない。bash ツールは実
    /// コマンドをここへ通す。
    #[test]
    fn an_ordinary_failing_command_stays_an_outcome_not_a_sandbox_failure() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let out = run_confined(&policy, std::path::Path::new("/usr/bin/false"), &[], None)
            .expect("通常の失敗コマンドが拘束失敗扱いされた");

        assert_ne!(out.status, 0, "{out:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandbox_that_failed_to_apply_is_an_error_not_a_denial() {
        // 適用の失敗を Outcome として返すと、呼び出し側はそれを方針違反と
        // 区別できない。区別できなければ、拘束されていない子が走ったことに
        // 誰も気づけない。ここでは classify_apply_failure を直接試す。
        // 「通常終了」を実プロセスから取り、from_raw の手組みに頼らない。
        let ordinary = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 1"])
            .status()
            .expect("起動できない");

        assert!(
            classify_apply_failure(
                &ordinary,
                "",
                "sandbox-exec: sandbox_apply: Operation not permitted"
            )
            .is_some(),
            "適用失敗を検出できていない"
        );
        assert!(
            classify_apply_failure(&ordinary, "", "sh: /nope: Operation not permitted").is_none(),
            "通常の拒否を適用失敗と誤判定している"
        );
        assert!(classify_apply_failure(&ordinary, "", "").is_none());
    }

    /// finding 4: `sandbox_apply` の文字列に頼れない実例。外側の方針が
    /// `file-read*` を許可しないと、入れ子の `sandbox-exec` は SIGABRT
    /// (signal 6) で落ち、stdout・stderr はどちらも空になる
    /// （実機で 3/3 回再現）。文字列一致だけの判定はここを `Ok(Outcome)`
    /// として素通しし、拘束されていない子が走ったかどうか誰も気づけない
    /// 「危険な方向」に倒れる。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_nested_sandbox_apply_failure_with_empty_output_is_classified_via_signal() {
        let outer = tempfile::NamedTempFile::new().expect("一時ファイル");
        std::fs::write(
            outer.path(),
            "(version 1)\n(deny default)\n(allow process-fork)\n(allow process-exec)\n",
        )
        .expect("書けない");

        let out = std::process::Command::new(crate::macos::SANDBOX_EXEC)
            .args([
                "-f",
                outer.path().to_str().expect("パス"),
                crate::macos::SANDBOX_EXEC,
                "-p",
                "(version 1)(allow default)",
                "--",
                "/bin/echo",
                "hi",
            ])
            .output()
            .expect("起動そのものが失敗した");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stdout.is_empty(), "想定外の標準出力: {stdout}");
        assert!(
            stderr.is_empty(),
            "想定外の標準エラー: {stderr}（文字列一致に頼れないという事例そのものが崩れている）"
        );

        assert!(
            classify_apply_failure(&out.status, &stdout, &stderr).is_some(),
            "空の stderr のまま SIGABRT で終了した適用失敗を検出できていない"
        );
    }

    /// finding 4 の精度確認: SIGABRT だけで判定すると、正規に abort する
    /// （かつ何か書き出す）プログラムまで適用失敗と誤判定しかねない。
    /// 出力が空でないなら SIGABRT でも適用失敗とは判定しないことを確認する。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_child_that_aborts_after_writing_output_is_not_misclassified_as_apply_failure() {
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", "echo real-crash 1>&2; kill -ABRT $$"])
            .output()
            .expect("起動できない");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.is_empty(), "前提が崩れている: stderr が空だった");

        assert!(
            classify_apply_failure(&out.status, &stdout, &stderr).is_none(),
            "出力のある本物の SIGABRT を適用失敗と誤判定した"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pre_exec_apply_failure_is_not_enforced_not_io() {
        // ルートが方針構築後に消えると、pre_exec 内の
        // apply_to_current_process が本物の理由（PathFd::new の ENOENT）
        // で失敗する。ここで SandboxError::Io ではなく NotEnforced が
        // 返ることを確認する。Io のままだと「拘束できなかった」が
        // 「バイナリが見つからなかった」と見分けが付かなくなる。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");
        std::fs::remove_dir_all(root.path()).expect("削除できない");

        let err = run_confined(
            &policy,
            std::path::Path::new("/bin/echo"),
            &["hi".to_string()],
            None,
        )
        .expect_err("ルートが消えているのに拘束実行が成功してしまった");

        assert!(
            matches!(err, SandboxError::NotEnforced(_)),
            "適用の失敗が NotEnforced ではなく別の型で返ってきた: {err:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_binary_on_linux_is_io_not_not_enforced() {
        // 対照実験。バイナリが存在しない場合の spawn 失敗は「拘束でき
        // なかった」ではない。番兵 errno と衝突して NotEnforced に化けて
        // はならない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let err = run_confined(
            &policy,
            std::path::Path::new("/definitely/does/not/exist/polaris-test-binary"),
            &[],
            None,
        )
        .expect_err("存在しないバイナリの起動が成功してしまった");

        assert!(
            matches!(err, SandboxError::Io(_)),
            "存在しないバイナリの失敗が Io ではなく別の型で返ってきた: {err:?}"
        );
    }
}
