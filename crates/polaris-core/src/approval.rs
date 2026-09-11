//! Approval boundary. `sandbox_mode` sets the technical boundary;
//! `approval_policy` sets the condition under which we stop and confirm.
//! The two are orthogonal.
//!
//! The party being asked is a trait so that tests don't need real terminal
//! input.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use polaris_sandbox::SandboxPolicy;
use polaris_tools::predicate::{Verdict, predict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Never ask. Anything out of scope is refused outright.
    Never,
    /// Ask only when out of scope.
    OnRequest,
    /// Ask before every mutating operation and every shell command.
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

pub trait Approver {
    fn ask(&mut self, reason: &str) -> Decision;

    /// Legacy implementations keep their synchronous behavior. UI approvers
    /// must override this method with a nonblocking future.
    fn ask_async<'a>(
        &'a mut self,
        reason: &'a str,
    ) -> Pin<Box<dyn Future<Output = Decision> + 'a>> {
        Box::pin(async move { self.ask(reason) })
    }
}

pub struct Gate {
    policy: ApprovalPolicy,
    approval_available: bool,
}

impl Gate {
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self {
            policy,
            approval_available: true,
        }
    }

    /// Preserve the parent's policy without granting unattended approval.
    pub(crate) fn for_subagent(&self) -> Self {
        Self {
            policy: self.policy,
            approval_available: false,
        }
    }

    /// A legacy background entry cannot carry this gate or relay approval.
    pub(crate) fn check_unattended(&self, operation: &str) -> Result<(), String> {
        if self.policy == ApprovalPolicy::Always {
            return Err(format!(
                "subagent approval relay is unavailable; operation refused: {operation}"
            ));
        }
        Ok(())
    }

    /// Shell commands are opaque: Always asks regardless of their contents.
    /// Approval does not change the sandbox policy passed to execution.
    pub fn check_command(
        &mut self,
        sandbox: &SandboxPolicy,
        command: &str,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        let Some(reason) = self.command_request(sandbox, command)? else {
            return Ok(());
        };
        Self::finish(&reason, approver.ask(&reason))
    }

    /// Async counterpart of `check_command`, using the same policy decision.
    pub(crate) async fn check_command_async(
        &mut self,
        sandbox: &SandboxPolicy,
        command: &str,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        let Some(reason) = self.command_request(sandbox, command)? else {
            return Ok(());
        };
        Self::finish(&reason, approver.ask_async(&reason).await)
    }

    /// Call before a mutating operation. Returns `Ok(())` if it may proceed,
    /// or a reason if it must stop.
    pub fn check(
        &mut self,
        sandbox: &SandboxPolicy,
        target: &Path,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        let Some(reason) = self.write_request(sandbox, target)? else {
            return Ok(());
        };
        Self::finish(&reason, approver.ask(&reason))
    }

    /// Async counterpart of `check`. A UI implementation must override
    /// `Approver::ask_async`; the default preserves legacy synchronous input.
    pub async fn check_async(
        &mut self,
        sandbox: &SandboxPolicy,
        target: &Path,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        let Some(reason) = self.write_request(sandbox, target)? else {
            return Ok(());
        };
        Self::finish(&reason, approver.ask_async(&reason).await)
    }

    /// Only for the internal execution port whose owner persists and resolves
    /// every request before spawning. Keep Never's static refusal; move the
    /// interactive question to that owner instead of asking twice.
    pub(crate) fn check_owned_write(
        &self,
        sandbox: &SandboxPolicy,
        target: &Path,
    ) -> Result<(), String> {
        Self {
            policy: self.policy,
            approval_available: true,
        }
        .write_request(sandbox, target)
        .map(|_| ())
    }

    pub(crate) fn check_owned_command(
        &self,
        sandbox: &SandboxPolicy,
        command: &str,
    ) -> Result<(), String> {
        Self {
            policy: self.policy,
            approval_available: true,
        }
        .command_request(sandbox, command)
        .map(|_| ())
    }

    fn command_request(
        &self,
        sandbox: &SandboxPolicy,
        command: &str,
    ) -> Result<Option<String>, String> {
        if self.policy != ApprovalPolicy::Always {
            return Ok(None);
        }
        self.approval_request(format!(
            "running command {command:?}. policy {}",
            sandbox.describe()
        ))
    }

    fn write_request(
        &self,
        sandbox: &SandboxPolicy,
        target: &Path,
    ) -> Result<Option<String>, String> {
        let reason = match (predict(sandbox, target), self.policy) {
            (Verdict::Allowed, ApprovalPolicy::Always) => {
                format!(
                    "writing to {}. policy {}",
                    target.display(),
                    sandbox.describe()
                )
            }
            (Verdict::Allowed, _) => return Ok(None),
            (Verdict::NeedsApproval { reason }, _) => reason,
        };
        self.approval_request(reason)
    }

    /// Shared preflight for both sync and async calls, before invoking an approver.
    fn approval_request(&self, reason: String) -> Result<Option<String>, String> {
        if self.policy == ApprovalPolicy::Never {
            return Err(reason);
        }
        if !self.approval_available {
            return Err(format!(
                "subagent approval relay is unavailable; operation refused: {reason}"
            ));
        }
        Ok(Some(reason))
    }

    fn finish(reason: &str, decision: Decision) -> Result<(), String> {
        match decision {
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

    #[tokio::test(flavor = "current_thread")]
    async fn p42_async_override_yields_to_ui_for_allow_and_deny() {
        struct UiApprover {
            entered: Option<tokio::sync::oneshot::Sender<String>>,
            answer: Option<tokio::sync::oneshot::Receiver<Decision>>,
        }
        impl Approver for UiApprover {
            fn ask(&mut self, _: &str) -> Decision {
                panic!("async gate called the blocking legacy method");
            }
            fn ask_async<'a>(
                &'a mut self,
                reason: &'a str,
            ) -> Pin<Box<dyn Future<Output = Decision> + 'a>> {
                Box::pin(async move {
                    self.entered
                        .take()
                        .unwrap()
                        .send(reason.to_string())
                        .unwrap();
                    self.answer.take().unwrap().await.unwrap()
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sandbox = workspace(dir.path());
        let target = sandbox.writable_roots()[0].join("normal.txt");
        for command in [false, true] {
            for decision in [Decision::Allow, Decision::Deny] {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
                let mut approver = UiApprover {
                    entered: Some(entered_tx),
                    answer: Some(answer_rx),
                };
                let mut gate = Gate::new(ApprovalPolicy::Always);
                let check = async {
                    let approver: &mut dyn Approver = &mut approver;
                    if command {
                        gate.check_command_async(&sandbox, "printf normal", approver)
                            .await
                    } else {
                        gate.check_async(&sandbox, &target, approver).await
                    }
                };
                let ui = async {
                    let reason = entered_rx.await.unwrap();
                    tokio::task::yield_now().await;
                    answer_tx.send(decision).unwrap();
                    reason
                };
                let (result, reason) =
                    tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        tokio::join!(check, ui)
                    })
                    .await
                    .unwrap();
                if command {
                    assert!(reason.contains("printf normal"));
                } else {
                    assert!(reason.contains(&target.display().to_string()));
                }
                match decision {
                    Decision::Allow => assert_eq!(result, Ok(())),
                    Decision::Deny => {
                        assert_eq!(result, Err(format!("the user did not approve: {reason}")))
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn p42_sync_async_policy_and_reason_parity_with_legacy_default() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sandbox = workspace(dir.path());
        for policy in [
            ApprovalPolicy::Never,
            ApprovalPolicy::OnRequest,
            ApprovalPolicy::Always,
        ] {
            for child in [false, true] {
                for decision in [Decision::Allow, Decision::Deny] {
                    for operation in 0..3 {
                        let target = if operation == 0 {
                            sandbox.writable_roots()[0].join("inside.txt")
                        } else {
                            outside.path().join("outside.txt")
                        };
                        let make_gate = || {
                            let gate = Gate::new(policy);
                            if child { gate.for_subagent() } else { gate }
                        };
                        let mut sync_gate = make_gate();
                        let mut async_gate = make_gate();
                        let mut sync_approver = Scripted {
                            answers: vec![decision],
                            asked: vec![],
                        };
                        let mut async_approver = Scripted {
                            answers: vec![decision],
                            asked: vec![],
                        };
                        let (sync_result, async_result) = if operation == 2 {
                            (
                                sync_gate.check_command(
                                    &sandbox,
                                    "printf normal",
                                    &mut sync_approver,
                                ),
                                async_gate
                                    .check_command_async(
                                        &sandbox,
                                        "printf normal",
                                        &mut async_approver,
                                    )
                                    .await,
                            )
                        } else {
                            (
                                sync_gate.check(&sandbox, &target, &mut sync_approver),
                                async_gate
                                    .check_async(&sandbox, &target, &mut async_approver)
                                    .await,
                            )
                        };
                        assert_eq!(
                            sync_result, async_result,
                            "{policy:?} child={child} operation={operation}"
                        );
                        assert_eq!(sync_approver.asked, async_approver.asked);
                        let needs_approval = policy == ApprovalPolicy::Always || operation == 1;
                        let should_ask =
                            needs_approval && policy != ApprovalPolicy::Never && !child;
                        assert_eq!(async_approver.asked.len(), usize::from(should_ask));
                        assert_eq!(
                            async_result.is_ok(),
                            !needs_approval || (should_ask && decision == Decision::Allow)
                        );
                        if child && needs_approval && policy != ApprovalPolicy::Never {
                            assert!(
                                async_result
                                    .unwrap_err()
                                    .contains("approval relay is unavailable")
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn d04_child_keeps_write_approval_and_refuses_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = workspace(dir.path());
        let mut approver = Scripted {
            answers: vec![],
            asked: vec![],
        };
        let mut child = Gate::new(ApprovalPolicy::Always).for_subagent();
        let reason = child
            .check(
                &sandbox,
                &sandbox.writable_roots()[0].join("normal.txt"),
                &mut approver,
            )
            .unwrap_err();
        assert!(reason.contains("approval relay is unavailable"));
        assert!(approver.asked.is_empty());
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
