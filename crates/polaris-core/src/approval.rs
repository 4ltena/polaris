//! Approval boundary. `sandbox_mode` sets the technical boundary;
//! `approval_policy` sets the condition under which we stop and confirm.
//! The two are orthogonal.
//!
//! The party being asked is a trait so that tests don't need real terminal
//! input.

use std::path::Path;

use polaris_sandbox::SandboxPolicy;
use polaris_tools::predicate::{Verdict, predict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Never ask. Anything out of scope is refused outright.
    Never,
    /// Ask only when out of scope.
    OnRequest,
    /// Ask before every mutating operation.
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

    /// Call before a mutating operation. Returns `Ok(())` if it may proceed,
    /// or a reason if it must stop.
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
                    "writing to {}. policy {}",
                    target.display(),
                    sandbox.describe()
                )
            }
            (Verdict::Allowed, _) => return Ok(()),
            (Verdict::NeedsApproval { reason }, _) => reason.clone(),
        };

        // When there's no one configured to ask, refuse without asking.
        // Letting it through would mean running unattended is itself an
        // expansion of privilege.
        if self.policy == ApprovalPolicy::Never {
            return Err(reason);
        }

        match approver.ask(&reason) {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(format!("the user did not approve: {reason}")),
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
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()]).expect("policy")
    }

    #[test]
    fn a_target_inside_the_root_is_never_asked_about() {
        // If in-scope writes stopped every time, approval would lose its meaning.
        let dir = tempfile::tempdir().expect("temp directory");
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
        .expect("an in-scope target was refused");
        assert!(
            approver.asked.is_empty(),
            "asked unnecessarily: {:?}",
            approver.asked
        );
    }

    #[test]
    fn a_target_outside_the_root_is_asked_about_with_the_reason() {
        let root = tempfile::tempdir().expect("temp directory");
        let outside = tempfile::tempdir().expect("temp directory");
        let sandbox = workspace(root.path());
        let mut approver = Scripted {
            answers: vec![Decision::Allow],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);

        let target = outside.path().join("b.txt");
        gate.check(&sandbox, &target, &mut approver)
            .expect("was refused despite being approved");

        assert_eq!(approver.asked.len(), 1);
        assert!(
            approver.asked[0].contains(&target.display().to_string()),
            "reason is missing the path: {}",
            approver.asked[0]
        );
    }

    #[test]
    fn a_denied_approval_stops_the_operation() {
        let root = tempfile::tempdir().expect("temp directory");
        let outside = tempfile::tempdir().expect("temp directory");
        let sandbox = workspace(root.path());
        let mut approver = Scripted {
            answers: vec![Decision::Deny],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);

        gate.check(&sandbox, &outside.path().join("b.txt"), &mut approver)
            .expect_err("went through despite being denied");
    }

    #[test]
    fn never_refuses_without_asking() {
        // In an unattended run, there's no one to ask. Rather than letting
        // it through without asking, it must refuse without asking. Letting
        // it through would mean running unattended is itself an expansion
        // of privilege.
        let root = tempfile::tempdir().expect("temp directory");
        let outside = tempfile::tempdir().expect("temp directory");
        let sandbox = workspace(root.path());
        let mut approver = Scripted {
            answers: vec![Decision::Allow],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::Never);

        gate.check(&sandbox, &outside.path().join("b.txt"), &mut approver)
            .expect_err("went through despite being Never");
        assert!(approver.asked.is_empty(), "asked despite being Never");
    }

    #[test]
    fn always_asks_even_inside_the_root() {
        let dir = tempfile::tempdir().expect("temp directory");
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
        .expect("was refused despite being approved");
        assert_eq!(approver.asked.len(), 1, "did not ask despite being Always");
    }

    #[test]
    fn always_with_needs_approval_preserves_the_specific_reason() {
        // The most dangerous case of (Always, NeedsApproval). Even when the
        // policy is "always ask," the predicate has already found the actual
        // reason (e.g. out of path). That detailed reason must not be
        // overwritten by a generic "about to write" message. The party being
        // asked needs to know the detailed reason.
        let root = tempfile::tempdir().expect("temp directory");
        let outside = tempfile::tempdir().expect("temp directory");
        let sandbox = workspace(root.path());
        let mut approver = Scripted {
            answers: vec![Decision::Allow],
            asked: vec![],
        };
        let mut gate = Gate::new(ApprovalPolicy::Always);

        let target = outside.path().join("risky.txt");
        gate.check(&sandbox, &target, &mut approver)
            .expect("was refused despite being approved");

        assert_eq!(approver.asked.len(), 1);
        let reason = &approver.asked[0];
        // Confirm all 3 points the spec requires are present.
        assert!(
            reason.contains(&target.display().to_string()),
            "reason is missing the path: {reason}"
        );
        assert!(
            reason.contains("workspace-write"),
            "reason is missing the policy: {reason}"
        );
        assert!(
            reason.contains(&sandbox.writable_roots()[0].display().to_string()),
            "reason is missing the root: {reason}"
        );
    }
}
