//! Linux の強制。landlock の ruleset を子の中で自分自身へ適用する。
//!
//! `restrict_self()` はスレッド単位で一方向であり、呼んだ後に作られた
//! スレッドと子へ継承される。走行中のハーネス本体で呼ぶと、そのスレッドが
//! 恒久的に制限され、以降の全ての作業が巻き添えになる。呼ぶ場所は
//! `Command::pre_exec` の内側だけである。

use crate::SandboxError;
use crate::policy::{SandboxMode, SandboxPolicy};

/// 実用上の下限。ABI 1（カーネル 5.13）ではディレクトリを跨ぐ rename と
/// link を表現できず、エディタや多くのツールが使う「一時ファイルへ書いて
/// rename で置き換える」保存が扱えない。ABI 2（5.19）を下限とする。
const REQUIRED_ABI: landlock::ABI = landlock::ABI::V2;

/// 現在のプロセス（＝ fork 済みの子）へ方針を適用する。
///
/// `full-access` では何も適用しない。制限しないことが方針だからである。
/// ただし呼び出し側は `full-access` でも子を起こす。試験する経路と本番の
/// 経路を同一に保つためである。
pub fn apply_to_current_process(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    use landlock::{
        Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };

    if policy.mode() == SandboxMode::FullAccess {
        return Ok(());
    }

    // ABI V2 未満（カーネル 5.19 未満）では `AccessFs::Refer`（ディレクトリを
    // 跨ぐ rename/link）が無く、既定の best-effort だとこの一項目だけが
    // 黙って落とされて `PartiallyEnforced` に倒れる。ここだけ
    // `HardRequirement` にして、要求の一部でも満たせないカーネルでは
    // `handle_access` の時点で即座に失敗させる。エラーは landlock 側が
    // 「どの権利が足りないか」を運ぶので、それをそのまま報告に混ぜる。
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(REQUIRED_ABI))
        .map_err(|e| {
            SandboxError::NotEnforced(format!(
                "カーネルが landlock {REQUIRED_ABI:?} の要求権利を満たさない: {e}"
            ))
        })?
        .set_compatibility(CompatLevel::BestEffort)
        .create()
        .map_err(|e| SandboxError::NotEnforced(format!("ruleset を作れない: {e}")))?;

    // 読み取りは全体に許す。read-only と workspace-write の違いは
    // 書き込み側にしかない。
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            PathFd::new("/")
                .map_err(|e| SandboxError::NotEnforced(format!("/ を開けない: {e}")))?,
            AccessFs::from_read(REQUIRED_ABI),
        ))
        .map_err(|e| SandboxError::NotEnforced(format!("読み取り規則を足せない: {e}")))?;

    // `/dev/null` への書き込みだけを開ける。macOS 側と同じ理由である
    // （`macos::build_profile` の長いコメントを参照）。landlock でも
    // `cmd > /dev/null` は
    // `/bin/sh: 1: cannot create /dev/null: Permission denied` となり、
    // リダイレクトが開けない時点でコマンド本体が一度も走らない。
    //
    // 与える権利は `AccessFs::WriteFile` の 1 つだけとする。`>` が要求する
    // のは既存ファイルを書き込みで開くことであり、それを司る権利がこれ
    // 一つである（`O_TRUNC` を司る `Truncate` は ABI V3 の権利で、この
    // ruleset は V2 の権利しか handle していないため関与しない）。
    // `AccessFs::from_all` を渡すと unlink（`RemoveFile`）や同じ場所への
    // 新規作成まで一緒に開くことになるので使わない。規則の対象は
    // `/dev/null` というファイル 1 個であり、`PathBeneath` を使っていても
    // ディレクトリではないため `/dev` 配下の他のノードには波及しない。
    //
    // read-only でも足すのは macOS と同じ理由による。書いた内容は捨てられ、
    // ファイルシステムの状態は変わらない。
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            PathFd::new("/dev/null")
                .map_err(|e| SandboxError::NotEnforced(format!("/dev/null を開けない: {e}")))?,
            AccessFs::WriteFile,
        ))
        .map_err(|e| SandboxError::NotEnforced(format!("/dev/null の規則を足せない: {e}")))?;

    for root in policy.writable_roots() {
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                PathFd::new(root).map_err(|e| {
                    SandboxError::NotEnforced(format!("{} を開けない: {e}", root.display()))
                })?,
                AccessFs::from_all(REQUIRED_ABI),
            ))
            .map_err(|e| SandboxError::NotEnforced(format!("書き込み規則を足せない: {e}")))?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| SandboxError::NotEnforced(format!("restrict_self に失敗: {e}")))?;

    // 適用されなかった状態、一部しか適用されなかった状態のどちらも拒否では
    // ない。ここを通してしまうと、守っていない（または一部しか守っていない）
    // 状態が完全に守っている状態と同じ見た目になる。`HardRequirement` は
    // ABI 側の不足を `handle_access` の時点で捕まえるが、それとは別の理由
    // （例えば restrict_self 自体が seccomp に塞がれて一部だけ効くような
    // 経路）で `PartiallyEnforced` に落ちる可能性を塞ぐため、
    // `FullyEnforced` 以外を丸ごと拒否する。
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(SandboxError::NotEnforced(format!(
            "カーネルが landlock {REQUIRED_ABI:?} を完全には強制しなかった（{:?}）。\
             カーネルが古いか、seccomp に塞がれているか、機能が部分的にしか使えない",
            status.ruleset
        )));
    }

    Ok(())
}

#[cfg(test)]
mod linux_enforcement {
    use crate::policy::{SandboxMode, SandboxPolicy};

    /// `/bin/sh -c script` を policy の下（fork 後・exec 前に
    /// `apply_to_current_process` を通した子）で起動し、終了ステータスを返す。
    /// 適用そのものが失敗した場合（`pre_exec` が `Err` を返す）は `Command`
    /// の起動自体が失敗として親に返るため、ここで panic する。各テストの
    /// 診断としては起動失敗より「書けた／書けなかった」の方が本題なので、
    /// 起動失敗はテスト側の意図と無関係な壊れ方として扱う。
    fn run_under_policy(policy: &SandboxPolicy, script: &str) -> std::process::ExitStatus {
        let policy_for_child = policy.clone();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                crate::linux::apply_to_current_process(&policy_for_child)
                    .map_err(|e| std::io::Error::other(e.to_string()))
            });
        }
        cmd.status().expect("起動できない")
    }

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_kernel() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = outside.path().join("should-not-exist.txt");
        let status = run_under_policy(&policy, &format!("echo pwned > {}", target.display()));

        assert!(!status.success(), "ルート外への書き込みが成功した");
        assert!(
            !target.exists(),
            "ファイルが作られている: {}",
            target.display()
        );
    }

    #[test]
    fn a_write_inside_the_writable_root_succeeds() {
        // ルート外への拒否だけを見るテストは「全部拒否する」壊れ方を見逃す
        // （書き込みルールを丸ごと削っても、read-access に弱めても、この
        // テストは通り続けてしまう）。ルート内への書き込みが実際に通ること
        // を対にして確認し、「denies everything」を検出可能にする。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = root.path().join("should-exist.txt");
        let status = run_under_policy(&policy, &format!("echo ok > {}", target.display()));

        assert!(status.success(), "ルート内への書き込みが拒否された");
        assert!(
            target.exists(),
            "ファイルが作られていない: {}",
            target.display()
        );
        let content = std::fs::read_to_string(&target).expect("書けたはずのファイルを読めない");
        assert_eq!(content.trim(), "ok", "書いた内容と読めた内容が違う");
    }

    #[test]
    fn read_only_denies_any_write() {
        // read-only は書込可能ルートを 1 件も持てない（Task 1 の制約）。
        // ここで見るのは、その状態で「どこにも書けない」がそのまま
        // 強制側にも反映されていること。
        let scratch = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("方針");

        let target = scratch.path().join("should-not-exist.txt");
        let status = run_under_policy(&policy, &format!("echo pwned > {}", target.display()));

        assert!(!status.success(), "read-only なのに書き込みが成功した");
        assert!(
            !target.exists(),
            "ファイルが作られている: {}",
            target.display()
        );
    }
}
