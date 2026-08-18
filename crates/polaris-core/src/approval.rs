//! 承認境界。`sandbox_mode` が技術的境界を、`approval_policy` が停止して
//! 確認する条件を定める。二つは直交する。
//!
//! 尋ねる相手を trait にするのは、テストが実際の端末入力を要らないように
//! するためである。

use std::path::Path;

use polaris_sandbox::SandboxPolicy;
use polaris_tools::predicate::{Verdict, predict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// 尋ねない。範囲外はそのまま断る。
    Never,
    /// 範囲外のときだけ尋ねる。
    OnRequest,
    /// 変更操作のたびに尋ねる。
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

pub trait Approver {
    fn ask(&mut self, reason: &str) -> Decision;
}

pub struct Gate {
    policy: ApprovalPolicy,
}

impl Gate {
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self { policy }
    }

    /// 変更操作の前に呼ぶ。通ってよければ `Ok(())`、止めるなら理由を返す。
    pub fn check(
        &mut self,
        sandbox: &SandboxPolicy,
        target: &Path,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        let verdict = predict(sandbox, target);

        let reason = match (&verdict, self.policy) {
            (Verdict::Allowed, ApprovalPolicy::Always) => {
                format!(
                    "{} へ書き込む。方針 {}",
                    target.display(),
                    sandbox.describe()
                )
            }
            (Verdict::Allowed, _) => return Ok(()),
            (Verdict::NeedsApproval { reason }, _) => reason.clone(),
        };

        // 尋ねる相手がいない設定では、尋ねずに断る。通してしまうと、
        // 無人であることがそのまま権限の拡大になる。
        if self.policy == ApprovalPolicy::Never {
            return Err(reason);
        }

        match approver.ask(&reason) {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(format!("利用者が承認しなかった: {reason}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    struct Scripted {
        answers: Vec<Decision>,
        asked: Vec<String>,
    }
    impl Approver for Scripted {
        fn ask(&mut self, reason: &str) -> Decision {
            self.asked.push(reason.to_string());
            self.answers.remove(0)
        }
    }

    fn workspace(root: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()]).expect("方針")
    }

    #[test]
    fn a_target_inside_the_root_is_never_asked_about() {
        // 範囲内の書き込みで毎回止まると、承認が意味を失う。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(dir.path());
        let mut approver = Scripted {
            answers: vec![],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);

        gate.check(
            &sandbox,
            &sandbox.writable_roots()[0].join("a.txt"),
            &mut approver,
        )
        .expect("範囲内が拒否された");
        assert!(
            approver.asked.is_empty(),
            "余計に尋ねた: {:?}",
            approver.asked
        );
    }

    #[test]
    fn a_target_outside_the_root_is_asked_about_with_the_reason() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(root.path());
        let mut approver = Scripted {
            answers: vec![Decision::Allow],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);

        let target = outside.path().join("b.txt");
        gate.check(&sandbox, &target, &mut approver)
            .expect("承認したのに拒否された");

        assert_eq!(approver.asked.len(), 1);
        assert!(
            approver.asked[0].contains(&target.display().to_string()),
            "理由にパスが無い: {}",
            approver.asked[0]
        );
    }

    #[test]
    fn a_denied_approval_stops_the_operation() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(root.path());
        let mut approver = Scripted {
            answers: vec![Decision::Deny],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);

        gate.check(&sandbox, &outside.path().join("b.txt"), &mut approver)
            .expect_err("拒否したのに通った");
    }

    #[test]
    fn never_refuses_without_asking() {
        // 無人実行では尋ねる相手がいない。尋ねずに通すのではなく、尋ねずに
        // 断る。通してしまうと、無人であることが権限の拡大になる。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(root.path());
        let mut approver = Scripted {
            answers: vec![Decision::Allow],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::Never);

        gate.check(&sandbox, &outside.path().join("b.txt"), &mut approver)
            .expect_err("Never なのに通った");
        assert!(approver.asked.is_empty(), "Never なのに尋ねた");
    }

    #[test]
    fn always_asks_even_inside_the_root() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(dir.path());
        let mut approver = Scripted {
            answers: vec![Decision::Allow],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::Always);

        gate.check(
            &sandbox,
            &sandbox.writable_roots()[0].join("a.txt"),
            &mut approver,
        )
        .expect("承認したのに拒否された");
        assert_eq!(approver.asked.len(), 1, "Always なのに尋ねなかった");
    }
}
