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

use crate::mcp::operation::{ExternalCeiling, McpApprovalDisplay, McpOperation};

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

/// A typed judge the gate may consult in the moment before it troubles the
/// person, and the request it judges against.
///
/// **Denial only.** It can turn a question into a refusal. It can never turn
/// a refusal into a grant, satisfy an `ExternalCeiling`, shorten the question
/// that is actually asked, or reopen something `ApprovalPolicy::Never`
/// already refused — that refusal happens in `approval_request`, before this
/// is consulted. A probability is not permission.
///
/// The question and the edge belong to [`crate::decision_bands`] and are not
/// restated here: the same "役に立つか" wording tool preselection uses, and a
/// refusal only in the **exclusion** band. A differently worded question, or
/// a new threshold to read the answer against, would be a second calibration
/// with nothing measured behind it.
pub struct Advisor {
    decider: std::sync::Arc<dyn polaris_provider::decision::Decider>,
    /// What the run was asked to do. Without it "is this useful" has no
    /// subject, so an advisor cannot be built from the operation alone.
    task: String,
}

impl Advisor {
    pub fn new(
        decider: std::sync::Arc<dyn polaris_provider::decision::Decider>,
        task: impl Into<String>,
    ) -> Self {
        Self {
            decider,
            task: task.into(),
        }
    }
}

pub struct Gate {
    policy: ApprovalPolicy,
    approval_available: bool,
    advisor: Option<Advisor>,
}

impl Gate {
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self {
            policy,
            approval_available: true,
            advisor: None,
        }
    }

    /// Registering one is optional, like every other use of the judge.
    /// Without it the gate behaves exactly as it did before.
    pub fn advise(&mut self, advisor: Advisor) {
        self.advisor = Some(advisor);
    }

    /// Preserve the parent's policy without granting unattended approval.
    pub(crate) fn for_subagent(&self) -> Self {
        Self {
            policy: self.policy,
            approval_available: false,
            // A subagent cannot reach an approver at all, so there is no
            // question here for an advisor to pre-empt.
            advisor: None,
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
            // A preview builds the request without asking anyone, so there
            // is no question for an advisor to pre-empt.
            advisor: None,
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
            // A preview builds the request without asking anyone, so there
            // is no question for an advisor to pre-empt.
            advisor: None,
        }
        .command_request(sandbox, command)
        .map(|_| ())
    }

    /// Generic MCP tool calls are external and unknown-impact.  An explicit
    /// host ceiling is required before the normal Gate policy may ask once.
    /// Server annotations and the local sandbox are deliberately absent from
    /// this decision: neither can establish remote/app permission.
    pub fn check_mcp_operation(
        &mut self,
        ceiling: ExternalCeiling,
        operation: &McpOperation,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        let Some(reason) = self.mcp_operation_request(ceiling, operation)? else {
            return Ok(());
        };
        Self::finish(&reason, approver.ask(&reason))
    }

    /// Nonblocking counterpart for hosts whose approver implements async UI.
    ///
    /// A registered [`Advisor`] is consulted here and nowhere else. The order
    /// is the point: the ceiling and the policy decide first, so a judge is
    /// never asked about something already refused, and is never what lets
    /// something through. It runs only once a question was going to be asked,
    /// so a run that approves nothing pays nothing for it.
    pub async fn check_mcp_operation_async(
        &mut self,
        ceiling: ExternalCeiling,
        operation: &McpOperation,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        let Some(reason) = self.mcp_operation_request(ceiling, operation)? else {
            return Ok(());
        };
        if let Some(refusal) = self.plainly_useless(operation).await {
            return Err(refusal);
        }
        Self::finish(&reason, approver.ask_async(&reason).await)
    }

    /// `Some` only when a judge answered, and answered in the exclusion band.
    ///
    /// Everything else — no advisor, a judge that could not answer, an answer
    /// anywhere above the exclusion edge — leaves the person to decide, which
    /// is what happens today. Its only effect is an early `Err`: it is
    /// consulted after the ceiling and the policy, so nothing it returns can
    /// grant anything.
    async fn plainly_useless(&self, operation: &McpOperation) -> Option<String> {
        let advisor = self.advisor.as_ref()?;
        let display = operation.approval_display();
        let bands = crate::decision_bands::judge(
            advisor.decider.as_ref(),
            &serde_json::json!({ "task": advisor.task }),
            &advisor.task,
            "操作",
            &[(display.metadata.item.as_str(), display.description.as_str())],
            std::future::pending(),
        )
        .await
        .ok()?;
        // One candidate was asked about, so exactly one band holds it. Only
        // the exclusion band is acted on; the other two keep the question.
        (bands.excluded == 1).then(|| {
            format!(
                "判定器が依頼に対して役に立たないと判断したため実行しなかった: {}",
                display.description
            )
        })
    }

    /// Registration from a CLI or TUI run: the explicit external permission
    /// first, then one question unless the policy forbids asking.
    pub async fn check_mcp_registration_async(
        &mut self,
        ceiling: ExternalCeiling,
        description: &str,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        if !ceiling.permits_external() {
            return Err("mcp_external_ceiling_absent".into());
        }
        let Some(reason) = self.approval_request(format!("MCPサーバの登録\n{description}"))?
        else {
            return Ok(());
        };
        Self::finish(&reason, approver.ask_async(&reason).await)
    }

    /// Desktop's existing owner calls this before it persists an intent and
    /// owns the actual approval.  No local question or grant is created here.
    /// `Some` tells that owner to use the returned, secret-safe display.
    pub fn owned_mcp_approval(
        &self,
        ceiling: ExternalCeiling,
        operation: &McpOperation,
    ) -> Result<Option<McpApprovalDisplay>, String> {
        Self {
            policy: self.policy,
            approval_available: true,
            // A preview builds the request without asking anyone, so there
            // is no question for an advisor to pre-empt.
            advisor: None,
        }
        .mcp_operation_request(ceiling, operation)
        .map(|request| request.map(|_| operation.approval_display()))
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

    fn mcp_operation_request(
        &self,
        ceiling: ExternalCeiling,
        operation: &McpOperation,
    ) -> Result<Option<String>, String> {
        if operation.is_resource_read() {
            return Err("mcp_operation_not_tool".into());
        }
        if !ceiling.permits_external() {
            return Err("mcp_external_ceiling_absent".into());
        }
        let display = operation.approval_display();
        self.approval_request(format!(
            "{}; ペイロード {}",
            display.description, display.metadata.payload_hash
        ))
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
    use crate::mcp::operation::HostOperationIdentity;
    use polaris_mcp::{
        OperationBinding,
        catalog::{ItemId, Kind},
    };
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

    fn mcp_operation() -> McpOperation {
        McpOperation::from_binding(
            HostOperationIdentity {
                run_id: "run-1".into(),
                attempt_id: "attempt-1".into(),
                model_call_id: "call-1".into(),
                operation_id: "operation-1".into(),
                policy_revision: 3,
                expires_at_ms: 1234,
            },
            "fixture project",
            OperationBinding {
                item: ItemId {
                    server_id: "fixture-server".into(),
                    kind: Kind::Tool,
                    key: "modify-project".into(),
                },
                registry_revision: 2,
                destination_hash: "destination".into(),
                connection_generation: "connection".into(),
                catalog_generation: "catalog".into(),
                schema_hash: Some("schema".into()),
                arguments_hash: "arguments".into(),
                selection_id: None,
                resource_uri: None,
            },
        )
        .unwrap()
    }

    /// Answers every question with the same probability, and counts how
    /// many times it was asked. `None` makes it fail instead.
    struct Judge {
        yes: Option<f64>,
        asked: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl polaris_provider::decision::Decider for Judge {
        async fn decide(
            &self,
            _state: &serde_json::Value,
            questions: &std::collections::BTreeMap<String, polaris_provider::decision::Question>,
        ) -> Result<polaris_provider::decision::Decision, polaris_provider::decision::DecisionError>
        {
            *self.asked.lock().unwrap() += 1;
            let Some(yes) = self.yes else {
                return Err(polaris_provider::decision::DecisionError::Unavailable);
            };
            Ok(polaris_provider::decision::Decision {
                answers: questions
                    .keys()
                    .map(|key| {
                        (
                            key.clone(),
                            polaris_provider::decision::Answer {
                                noul: Some(yes),
                                ..Default::default()
                            },
                        )
                    })
                    .collect(),
                input_tokens: None,
                output_tokens: None,
            })
        }
    }

    fn advised(yes: Option<f64>) -> (Gate, std::sync::Arc<Judge>) {
        let judge = std::sync::Arc::new(Judge {
            yes,
            asked: std::sync::Mutex::new(0),
        });
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);
        gate.advise(Advisor::new(judge.clone(), "画像を小さくしたい"));
        (gate, judge)
    }

    /// The exclusion edge is the only answer that acts. Below it the person
    /// is not troubled at all; at it and above, the question is asked exactly
    /// as it would have been without a judge.
    #[tokio::test(flavor = "current_thread")]
    async fn an_advisor_refuses_only_in_the_exclusion_band() {
        for (yes, asks) in [
            (crate::decision_bands::EXCLUDE - 0.1, false),
            (crate::decision_bands::EXCLUDE, false),
            (crate::decision_bands::EXCLUDE + 0.1, true),
            (crate::decision_bands::ADOPT, true),
        ] {
            let (mut gate, judge) = advised(Some(yes));
            let mut approver = Scripted {
                answers: vec![Decision::Allow],
                asked: vec![],
            };
            let held = gate
                .check_mcp_operation_async(
                    ExternalCeiling::Allowed,
                    &mcp_operation(),
                    &mut approver,
                )
                .await;
            assert_eq!(*judge.asked.lock().unwrap(), 1, "{yes}");
            assert_eq!(approver.asked.is_empty(), !asks, "{yes}");
            assert_eq!(held.is_ok(), asks, "{yes}");
            if !asks {
                assert!(held.unwrap_err().contains("役に立たない"), "{yes}");
            }
        }
    }

    /// A judge that could not answer is not a judge that said no.
    #[tokio::test(flavor = "current_thread")]
    async fn a_judge_that_cannot_answer_leaves_the_person_to_decide() {
        let (mut gate, judge) = advised(None);
        let mut approver = Scripted {
            answers: vec![Decision::Allow],
            asked: vec![],
        };
        assert!(
            gate.check_mcp_operation_async(
                ExternalCeiling::Allowed,
                &mcp_operation(),
                &mut approver
            )
            .await
            .is_ok()
        );
        assert_eq!(*judge.asked.lock().unwrap(), 1);
        assert_eq!(approver.asked.len(), 1);
    }

    /// The ceiling and the policy decide first, so the judge is never asked
    /// about something already refused — and can never be what reopens it.
    #[tokio::test(flavor = "current_thread")]
    async fn an_advisor_is_not_consulted_about_what_was_already_refused() {
        for (ceiling, policy) in [
            (ExternalCeiling::Denied, ApprovalPolicy::OnRequest),
            (ExternalCeiling::Allowed, ApprovalPolicy::Never),
        ] {
            let judge = std::sync::Arc::new(Judge {
                // Would adopt, if it were ever asked.
                yes: Some(1.0),
                asked: std::sync::Mutex::new(0),
            });
            let mut gate = Gate::new(policy);
            gate.advise(Advisor::new(judge.clone(), "画像を小さくしたい"));
            let mut approver = Scripted {
                answers: vec![Decision::Allow],
                asked: vec![],
            };
            assert!(
                gate.check_mcp_operation_async(ceiling, &mcp_operation(), &mut approver)
                    .await
                    .is_err()
            );
            assert_eq!(*judge.asked.lock().unwrap(), 0);
            assert!(approver.asked.is_empty());
        }
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

    #[test]
    fn generic_mcp_tool_requires_one_question_and_denial_stops_it() {
        for policy in [ApprovalPolicy::OnRequest, ApprovalPolicy::Always] {
            for decision in [Decision::Allow, Decision::Deny] {
                let mut gate = Gate::new(policy);
                let mut approver = Scripted {
                    answers: vec![decision],
                    asked: vec![],
                };
                let result = gate.check_mcp_operation(
                    ExternalCeiling::Allowed,
                    &mcp_operation(),
                    &mut approver,
                );
                assert_eq!(approver.asked.len(), 1, "{policy:?} must ask once");
                assert!(approver.asked[0].contains("影響: 未判定"));
                assert_eq!(result.is_ok(), decision == Decision::Allow);
            }
        }
    }

    #[test]
    fn mcp_tool_denies_for_never_missing_ceiling_or_subagent_without_relay() {
        for policy in [
            ApprovalPolicy::Never,
            ApprovalPolicy::OnRequest,
            ApprovalPolicy::Always,
        ] {
            let mut gate = Gate::new(policy);
            let mut approver = Scripted {
                answers: vec![Decision::Allow],
                asked: vec![],
            };
            let missing = gate
                .check_mcp_operation(ExternalCeiling::Denied, &mcp_operation(), &mut approver)
                .unwrap_err();
            assert_eq!(missing, "mcp_external_ceiling_absent");
            assert!(approver.asked.is_empty());
        }

        let mut never = Gate::new(ApprovalPolicy::Never);
        let mut approver = Scripted {
            answers: vec![Decision::Allow],
            asked: vec![],
        };
        assert!(
            never
                .check_mcp_operation(ExternalCeiling::Allowed, &mcp_operation(), &mut approver)
                .is_err()
        );
        assert!(approver.asked.is_empty());

        let mut child = Gate::new(ApprovalPolicy::OnRequest).for_subagent();
        let error = child
            .check_mcp_operation(ExternalCeiling::Allowed, &mcp_operation(), &mut approver)
            .unwrap_err();
        assert!(error.contains("approval relay is unavailable"));
        assert!(approver.asked.is_empty());
    }

    #[test]
    fn desktop_owner_receives_display_without_local_question_or_grant() {
        let gate = Gate::new(ApprovalPolicy::OnRequest);
        let display = gate
            .owned_mcp_approval(ExternalCeiling::Allowed, &mcp_operation())
            .unwrap()
            .expect("owner must approve unknown tool");
        assert_eq!(display.title, "MCP ツール実行");
        assert_eq!(
            display.metadata.impact,
            crate::mcp::operation::McpImpact::Unknown
        );
        assert!(display.description.contains("fixture-server"));

        assert!(
            Gate::new(ApprovalPolicy::Never)
                .owned_mcp_approval(ExternalCeiling::Allowed, &mcp_operation())
                .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_async_approval_uses_the_nonblocking_ui_boundary_once() {
        struct AsyncOnly {
            asked: usize,
            decision: Decision,
        }
        impl Approver for AsyncOnly {
            fn ask(&mut self, _: &str) -> Decision {
                panic!("synchronous UI must not run")
            }
            fn ask_async<'a>(
                &'a mut self,
                _: &'a str,
            ) -> Pin<Box<dyn Future<Output = Decision> + 'a>> {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    self.asked += 1;
                    self.decision
                })
            }
        }
        for decision in [Decision::Allow, Decision::Deny] {
            let mut approver = AsyncOnly { asked: 0, decision };
            let result = Gate::new(ApprovalPolicy::OnRequest)
                .check_mcp_operation_async(
                    ExternalCeiling::Allowed,
                    &mcp_operation(),
                    &mut approver,
                )
                .await;
            assert_eq!(result.is_ok(), decision == Decision::Allow);
            assert_eq!(approver.asked, 1);
        }
    }

    #[test]
    fn server_annotations_cannot_relax_generic_mcp_tool_approval() {
        // Catalog annotations are intentionally not an input to McpOperation
        // or Gate.  Even a server's claimed read-only annotation cannot turn
        // a generic tool into an allowed impact class.
        let _claimed_annotation = "readOnly=true; destructive=false";
        let operation = mcp_operation();
        assert_eq!(
            operation.impact(),
            crate::mcp::operation::McpImpact::Unknown
        );
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);
        let mut approver = Scripted {
            answers: vec![Decision::Deny],
            asked: vec![],
        };
        assert!(
            gate.check_mcp_operation(ExternalCeiling::Allowed, &operation, &mut approver)
                .is_err()
        );
        assert_eq!(approver.asked.len(), 1);
    }
}
