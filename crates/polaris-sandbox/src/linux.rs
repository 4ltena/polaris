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
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus,
    };

    if policy.mode() == SandboxMode::FullAccess {
        return Ok(());
    }

    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(REQUIRED_ABI))
        .map_err(|e| SandboxError::NotEnforced(format!("ruleset を作れない: {e}")))?
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

    // 適用されなかった状態は拒否ではない。ここを通してしまうと、
    // 守っていない状態が守っている状態と同じ見た目になる。
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(SandboxError::NotEnforced(
            "カーネルが landlock を強制しなかった（カーネルが古いか、seccomp に塞がれている）"
                .into(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod linux_enforcement {
    use crate::policy::{SandboxMode, SandboxPolicy};

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_kernel() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = outside.path().join("should-not-exist.txt");
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("echo pwned > {}", target.display()));

        let policy_for_child = policy.clone();
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                crate::linux::apply_to_current_process(&policy_for_child)
                    .map_err(|e| std::io::Error::other(e.to_string()))
            });
        }

        let status = cmd.status().expect("起動できない");
        assert!(!status.success(), "ルート外への書き込みが成功した");
        assert!(
            !target.exists(),
            "ファイルが作られている: {}",
            target.display()
        );
    }
}
