#![cfg(unix)]
//! Real owner/Writer; the grant is the only authority to save.
use super::*;
use crate::execution_owner::RunExecution;
use futures_util::FutureExt;
use polaris_desktop_protocol::{
    ids::*,
    request::{ApprovalResolve, Request as WireRequest, RequestBody},
};
use std::{future::pending, pin::Pin};

type Pending = Pin<Box<dyn Future<Output = Result<RegistrationGrant, RegistrationError>>>>;

fn approval(hash: &str) -> RegistrationApproval {
    RegistrationApproval {
        description: "名前: docs\n接続先: https://docs.example.com/mcp".into(),
        scope: "project".into(),
        payload_hash: hash.into(),
    }
}

fn authorize(port: &RegistrationPort, hash: &str) -> Pending {
    let port = port.clone();
    let approval = approval(hash);
    let mut future: Pending = Box::pin(async move { port.authorize(approval, pending()).await });
    assert!(future.as_mut().now_or_never().is_none());
    future
}

fn decide(writer: &mut Writer, owner: &mut RunExecution, decision: ApprovalDecision) {
    let pending = owner
        .registration
        .as_ref()
        .unwrap()
        .awaiting
        .as_ref()
        .unwrap()
        .approval
        .clone();
    writer
        .resolve_approval(
            &WireRequest {
                protocol_version: Default::default(),
                client_id: ClientId::new("registration-test").unwrap(),
                request_id: RequestId::new(format!(
                    "decision-{}",
                    &pending.approval_id.as_str()[pending.approval_id.as_str().len() - 16..]
                ))
                .unwrap(),
                body: RequestBody::ApprovalResolve(
                    SessionId::new("read-session").unwrap(),
                    ApprovalResolve {
                        approval_id: pending.approval_id.clone(),
                        run_id: pending.run_id,
                        attempt_id: pending.attempt_id,
                        policy_revision: pending.policy_revision,
                        decision,
                    },
                ),
            },
            now_ms().unwrap(),
        )
        .unwrap();
    owner.approval_resolved(&pending.approval_id, decision);
}

struct Refuse;
impl crate::approval::Approver for Refuse {
    fn ask(&mut self, _: &str) -> crate::approval::Decision {
        crate::approval::Decision::Deny
    }
}

fn refuse_gate() -> crate::approval::Gate {
    crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never)
}

async fn call_tool(
    tool: &crate::mcp::setup::SetupTool,
    args: &serde_json::Value,
) -> Result<String, String> {
    tool.call(args, pending(), &mut refuse_gate(), &mut Refuse)
        .await
}

const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn allowed_registration_gets_a_grant_only_after_the_durable_intent() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = owner
        .configure_registration(&target, ExternalCeiling::Allowed)
        .unwrap();
    let mut waiting = authorize(&port, HASH);
    owner.step(&mut writer, &target).unwrap();
    assert!(owner.awaiting_approval());
    let events = owner.take_approval_events();
    let [EventBody::ApprovalRequested(requested)] = events.as_slice() else {
        panic!("approval event");
    };
    assert_eq!(requested.operation, MCP_REGISTRATION_OPERATION);
    assert_eq!(requested.display.description, approval(HASH).description);
    assert!(waiting.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    assert!(
        writer.snapshot().unwrap().state.runs[0]
            .operations
            .is_empty()
    );

    decide(&mut writer, &mut owner, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    let grant = waiting.as_mut().now_or_never().unwrap().unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].operations.len(), 1);
    assert!(saved.state.runs[0].operations[0].result_id.is_none());
    assert!(!owner.quiescent());
    drop(grant);
    owner
        .record_finished_registrations(&mut writer, &target)
        .unwrap();
    assert!(
        writer.snapshot().unwrap().state.runs[0].operations[0]
            .result_id
            .is_some()
    );
    assert!(owner.quiescent());
}

#[tokio::test]
async fn denied_tier_denial_invalid_and_departed_callers_record_no_intent() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = owner
        .configure_registration(&target, ExternalCeiling::Denied)
        .unwrap();
    let mut waiting = authorize(&port, HASH);
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(
        waiting.as_mut().now_or_never().unwrap().err(),
        Some(RegistrationError::Denied)
    );
    assert!(
        writer
            .snapshot()
            .unwrap()
            .state
            .unresolved_approvals
            .is_empty()
    );

    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = owner
        .configure_registration(&target, ExternalCeiling::Allowed)
        .unwrap();
    let mut invalid = authorize(&port, "not-a-hash");
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(
        invalid.as_mut().now_or_never().unwrap().err(),
        Some(RegistrationError::Invalid)
    );

    let mut denied = authorize(&port, HASH);
    owner.step(&mut writer, &target).unwrap();
    decide(&mut writer, &mut owner, ApprovalDecision::Deny);
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(
        denied.as_mut().now_or_never().unwrap().err(),
        Some(RegistrationError::Denied)
    );

    let departed = authorize(&port, HASH);
    owner.step(&mut writer, &target).unwrap();
    owner.take_approval_events();
    drop(departed);
    owner.step(&mut writer, &target).unwrap();
    assert!(matches!(
        owner.take_approval_events().as_slice(),
        [EventBody::ApprovalExpired(ApprovalExpired {
            reason: ExpiryReason::Cancelled,
            ..
        })]
    ));
    let saved = writer.snapshot().unwrap();
    assert!(saved.state.unresolved_approvals.is_empty());
    assert!(saved.state.runs[0].operations.is_empty());
    assert!(owner.quiescent());
}

#[tokio::test]
async fn setup_tool_saves_detected_servers_only_after_allow_and_rebinds_later_candidates() {
    use crate::mcp::setup::SetupTool;
    use serde_json::{Value, json};
    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = owner
        .configure_registration(&target, ExternalCeiling::Allowed)
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        home.join(".codex/config.toml"),
        "[mcp_servers.docs]\nurl = \"https://docs.example.com/mcp\"\nhttp_headers = { authorization = \"Bearer synthetic-token\" }\n\n[mcp_servers.search]\nurl = \"https://search.example.com/mcp\"\n",
    )
    .unwrap();
    let tool = SetupTool::new(
        home.clone(),
        vec![],
        vec![],
        crate::isolated_workspace::Limits {
            max_entries: 8,
            max_files: 4,
            max_file_bytes: 1024,
            max_total_bytes: 4096,
            max_depth: 2,
        },
        "project".into(),
        crate::mcp::setup::Authorization::Owner(port),
    );
    let detected = tool
        .call(
            &json!({"op":"detect"}),
            pending(),
            &mut refuse_gate(),
            &mut Refuse,
        )
        .now_or_never()
        .unwrap()
        .unwrap();
    assert!(!detected.contains("docs.example.com") && !detected.contains("synthetic-token"));
    let detected: Value = serde_json::from_str(&detected).unwrap();
    assert_eq!(detected["candidates"][0]["name"], "docs");
    let args = json!({"op":"register","candidate":detected["candidates"][0]["candidate"]});
    let mut register = Box::pin(call_tool(&tool, &args));
    assert!(register.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    assert!(register.as_mut().now_or_never().is_none());
    assert!(!home.join(".polaris/mcp/servers.json").exists());
    decide(&mut writer, &mut owner, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    let saved: Value =
        serde_json::from_str(&register.as_mut().now_or_never().unwrap().unwrap()).unwrap();
    assert_eq!(saved["registry_revision"], "1");
    assert!(home.join(".polaris/mcp/servers.json").exists());
    owner.step(&mut writer, &target).unwrap();
    assert!(
        writer.snapshot().unwrap().state.runs[0].operations[0]
            .result_id
            .is_some()
    );
    assert_eq!(
        call_tool(&tool, &args).now_or_never().unwrap(),
        Err(json!({"error":"unknown_candidate"}).to_string())
    );
    // A candidate from the same detection is rebound before the user is asked.
    let args = json!({"op":"register","candidate":detected["candidates"][1]["candidate"]});
    let mut second = Box::pin(call_tool(&tool, &args));
    assert!(second.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    decide(&mut writer, &mut owner, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    let saved: Value =
        serde_json::from_str(&second.as_mut().now_or_never().unwrap().unwrap()).unwrap();
    assert_eq!(saved["registry_revision"], "2");
}
