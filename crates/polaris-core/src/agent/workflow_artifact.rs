//! Parsing boundary for root workflow-artifact calls.
//!
//! The model may describe a routine change, but it never supplies its scope,
//! eligibility, artifact ID, content hash, status, or approval. The host
//! binds the proposal to the exact prior user prompt, materializes it from
//! trusted scope and impact facts, then validates and publishes atomically.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workflow::{
    ArtifactRefV1, ChangeContractV1, RoutineEligibilityV1, VerificationCriterionV1,
    WorkflowArtifactKindV1,
};

const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const CONTRACT_ID_PREFIX: &str = "change-";
const CONTRACT_ID_HASH_HEX: usize = 24;

/// A host-owned operation parsed from one `workflow_artifact` function call.
/// Its variants intentionally contain no status or approval chosen by the
/// model. The caller must still check root identity and workflow phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowArtifactAction {
    ProposeRoutineContract {
        expected_workflow_revision: u64,
        proposal: RoutineContractProposal,
    },
    ReadExact {
        kind: WorkflowArtifactKindV1,
        reference: ArtifactRefV1,
    },
}

/// Bounded descriptive text supplied by the model. This is deliberately not
/// a contract: it contains no component or impact authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutineContractProposal {
    request: String,
    prompt_sha256: Option<String>,
    cause: String,
    expected_outcome: String,
    verification: Vec<String>,
}

/// The trusted component set currently selected by the host for this change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustedRoutineChangeScope {
    pub affected_components: Vec<String>,
}

/// Explicit host-observed impact facts. Any asserted fact prevents routine
/// publication; callers must set `unknown_impact` when they cannot establish
/// each relevant fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrustedRoutineImpactFacts {
    pub security: bool,
    pub migration: bool,
    pub public_api: bool,
    pub dependency: bool,
    pub deployment: bool,
    pub user_contract: bool,
    pub unknown_impact: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RoutineContractIdentity<'a> {
    component: &'a str,
    request: &'a str,
    cause: &'a str,
    expected_outcome: &'a str,
    verification: &'a [String],
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum WireOperation {
    ProposeRoutineContract {
        expected_revision: u64,
        request: String,
        #[serde(default)]
        prompt_sha256: Option<String>,
        cause: String,
        expected_outcome: String,
        verification: Vec<String>,
    },
    Read {
        kind: WorkflowArtifactKindV1,
        id: String,
        revision: u64,
        content_hash: String,
    },
}

/// Parse one bounded tool call. This is intentionally independent from
/// trusted user input, scope, impact facts, and a store: parsing cannot
/// construct a contract, publish an artifact, or grant approval.
pub(crate) fn parse(call: &polaris_provider::ToolCall) -> Result<WorkflowArtifactAction, String> {
    if call.name != "workflow_artifact" {
        return Err("Only workflow_artifact tools are accepted".into());
    }
    let encoded = serde_json::to_vec(&call.arguments)
        .map_err(|_| "Cannot canonicalize workflow_artifact arguments".to_string())?;
    if encoded.len() > MAX_ARGUMENT_BYTES {
        return Err("workflow_artifact arguments exceed the limit".into());
    }
    let operation: WireOperation = serde_json::from_value(call.arguments.clone())
        .map_err(|_| "Invalid workflow_artifact arguments".to_string())?;
    match operation {
        WireOperation::ProposeRoutineContract {
            expected_revision,
            request,
            prompt_sha256,
            cause,
            expected_outcome,
            verification,
        } => Ok(WorkflowArtifactAction::ProposeRoutineContract {
            expected_workflow_revision: expected_revision,
            proposal: RoutineContractProposal {
                request,
                prompt_sha256,
                cause,
                expected_outcome,
                verification,
            },
        }),
        WireOperation::Read {
            kind,
            id,
            revision,
            content_hash,
        } => {
            let reference = ArtifactRefV1 {
                id,
                revision,
                content_hash,
            };
            reference
                .validate()
                .map_err(|_| "Invalid workflow artifact reference".to_string())?;
            Ok(WorkflowArtifactAction::ReadExact { kind, reference })
        }
    }
}

/// Materialize a model proposal only after the session owner supplies the
/// exact prior user prompt, trusted current scope, and explicit impact facts.
/// The result is fail-closed: no unknown, prohibited, or multi-component
/// change can become a routine contract.
pub(crate) fn materialize_routine_contract(
    proposal: RoutineContractProposal,
    previous_user_prompt: &str,
    scope: &TrustedRoutineChangeScope,
    impact: TrustedRoutineImpactFacts,
    trusted_verification: &[VerificationCriterionV1],
) -> Result<ChangeContractV1, String> {
    if !proposal_matches_user_prompt(&proposal, previous_user_prompt) {
        return Err("The routine proposal is not bound to the latest user prompt".into());
    }
    let eligibility = RoutineEligibilityV1 {
        affected_components: scope.affected_components.clone(),
        security: impact.security,
        migration: impact.migration,
        public_api: impact.public_api,
        dependency: impact.dependency,
        distribution: impact.deployment,
        major_user_contract: impact.user_contract,
        unknown_impact: impact.unknown_impact,
    };
    eligibility
        .validate()
        .map_err(|_| "Invalid trusted routine change scope".to_string())?;
    if eligibility.blocker().is_some() {
        return Err("The trusted scope or impact facts do not authorize a routine change".into());
    }
    let trusted_descriptions = trusted_verification
        .iter()
        .map(|criterion| criterion.description.as_str())
        .collect::<Vec<_>>();
    if proposal
        .verification
        .iter()
        .map(String::as_str)
        .ne(trusted_descriptions.iter().copied())
    {
        return Err("Routine proposal verification criteria do not match host authority".into());
    }
    let component = eligibility
        .affected_components
        .first()
        .expect("routine eligibility has exactly one component after blocker check");
    let identity = RoutineContractIdentity {
        component,
        request: previous_user_prompt,
        cause: &proposal.cause,
        expected_outcome: &proposal.expected_outcome,
        verification: &proposal.verification,
    };
    let id = stable_contract_id(&identity)?;
    ChangeContractV1::draft_typed(
        id,
        eligibility,
        previous_user_prompt,
        proposal.cause,
        proposal.expected_outcome,
        trusted_verification.to_vec(),
    )
    .map_err(|_| "Invalid routine change contract content or limits".to_string())
}

fn proposal_matches_user_prompt(
    proposal: &RoutineContractProposal,
    previous_user_prompt: &str,
) -> bool {
    proposal.request == previous_user_prompt
        || proposal
            .prompt_sha256
            .as_deref()
            .is_some_and(|hash| prompt_sha256(previous_user_prompt) == hash)
}

pub(crate) fn prompt_sha256(prompt: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"polaris.workflow.prior_user_prompt.v1\0");
    digest.update(prompt.as_bytes());
    format!("{:x}", digest.finalize())
}

pub(crate) fn event(
    events: Option<&crate::desktop_events::EventSink>,
    call: &polaris_provider::ToolCall,
    outcome: Option<(&str, bool)>,
) {
    if let Some(tx) = events {
        let detail = call.arguments.to_string();
        let value = match outcome {
            None => crate::events::AgentEvent::ToolStarted {
                name: "workflow_artifact".into(),
                detail,
            },
            Some((result, ok)) => crate::events::AgentEvent::ToolFinished {
                name: "workflow_artifact".into(),
                detail,
                ok,
                result: result.into(),
                diff: None,
            },
        };
        let _ = tx.send(value);
    }
}

fn stable_contract_id(identity: &RoutineContractIdentity<'_>) -> Result<String, String> {
    let body = serde_json::to_vec(identity)
        .map_err(|_| "Cannot canonicalize the routine change contract".to_string())?;
    let mut digest = Sha256::new();
    digest.update(b"polaris.workflow.routine_contract_request.v1\0");
    digest.update(body);
    let hex = format!("{:x}", digest.finalize());
    Ok(format!(
        "{CONTRACT_ID_PREFIX}{}",
        &hex[..CONTRACT_ID_HASH_HEX]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::ToolCall;

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "workflow-artifact-test".into(),
            name: "workflow_artifact".into(),
            arguments,
        }
    }

    fn proposal() -> serde_json::Value {
        serde_json::json!({
            "op":"propose_routine_contract",
            "expected_revision":7,
            "request":"Fix a bounded parser error.",
            "cause":"A missing guard accepts an invalid value.",
            "expected_outcome":"The invalid value is rejected.",
            "verification":["A focused regression test passes."]
        })
    }

    fn safe_scope() -> TrustedRoutineChangeScope {
        TrustedRoutineChangeScope {
            affected_components: vec!["polaris-core".into()],
        }
    }

    fn safe_impact() -> TrustedRoutineImpactFacts {
        TrustedRoutineImpactFacts {
            security: false,
            migration: false,
            public_api: false,
            dependency: false,
            deployment: false,
            user_contract: false,
            unknown_impact: false,
        }
    }

    fn trusted_verification() -> Vec<VerificationCriterionV1> {
        vec![
            VerificationCriterionV1::typed(
                "A focused regression test passes.",
                "The bounded parser error remains accepted.",
                true,
                [crate::workflow::CriterionEvidenceKindV1::CommonCase]
                    .into_iter()
                    .collect(),
            )
            .expect("typed criterion"),
        ]
    }

    #[test]
    fn safe_one_component_exact_prompt_materializes_a_routine_contract() {
        let action = parse(&call(proposal())).expect("proposal parses");
        let WorkflowArtifactAction::ProposeRoutineContract {
            expected_workflow_revision,
            proposal,
        } = action
        else {
            panic!("expected proposal")
        };
        assert_eq!(expected_workflow_revision, 7);
        let contract = materialize_routine_contract(
            proposal,
            "Fix a bounded parser error.",
            &safe_scope(),
            safe_impact(),
            &trusted_verification(),
        )
        .expect("trusted safe proposal materializes");
        assert_eq!(
            contract.header.status,
            crate::workflow::ArtifactStatusV1::Draft
        );
        assert!(contract.header.id.starts_with(CONTRACT_ID_PREFIX));
        assert!(contract.eligibility.is_routine());
        assert_eq!(contract.eligibility.affected_components, ["polaris-core"]);
        assert_eq!(contract.verification.len(), 1);
        assert!(contract.header.content_hash.len() == 64);
    }

    #[test]
    fn same_proposal_has_a_stable_id() {
        let first = parse(&call(proposal())).expect("first");
        let second = parse(&call(proposal())).expect("second");
        let id = |action: WorkflowArtifactAction| match action {
            WorkflowArtifactAction::ProposeRoutineContract { proposal, .. } => {
                materialize_routine_contract(
                    proposal,
                    "Fix a bounded parser error.",
                    &safe_scope(),
                    safe_impact(),
                    &trusted_verification(),
                )
                .expect("safe proposal materializes")
                .header
                .id
            }
            _ => panic!("proposal expected"),
        };
        assert_eq!(id(first), id(second));
    }

    #[test]
    fn model_component_and_request_cannot_forge_routine_eligibility() {
        let mut value = proposal();
        value["component"] = serde_json::json!("core");
        value["request"] = serde_json::json!("Public API change.");
        assert!(parse(&call(value)).is_err());
    }

    #[test]
    fn host_public_api_fact_rejects_euphemistic_proposal() {
        let mut value = proposal();
        value["request"] = serde_json::json!("Polish a small internal detail.");
        let WorkflowArtifactAction::ProposeRoutineContract { proposal, .. } =
            parse(&call(value)).expect("proposal parses")
        else {
            panic!("expected proposal")
        };
        assert!(
            materialize_routine_contract(
                proposal,
                "Polish a small internal detail.",
                &safe_scope(),
                TrustedRoutineImpactFacts {
                    public_api: true,
                    ..safe_impact()
                },
                &trusted_verification(),
            )
            .is_err()
        );
    }

    #[test]
    fn trusted_multi_component_scope_rejects_routine_contract() {
        let WorkflowArtifactAction::ProposeRoutineContract { proposal, .. } =
            parse(&call(proposal())).expect("proposal parses")
        else {
            panic!("expected proposal")
        };
        let scope = TrustedRoutineChangeScope {
            affected_components: vec!["polaris-core".into(), "polaris-provider".into()],
        };
        assert!(
            materialize_routine_contract(
                proposal,
                "Fix a bounded parser error.",
                &scope,
                safe_impact(),
                &trusted_verification(),
            )
            .is_err()
        );
    }

    #[test]
    fn prompt_hash_binds_a_descriptive_proposal_to_the_exact_user_prompt() {
        let user_prompt = "Fix the bounded parser error without changing public behavior.";
        let mut value = proposal();
        value["request"] = serde_json::json!("Bounded parser guard.");
        value["prompt_sha256"] = serde_json::json!(prompt_sha256(user_prompt));
        let WorkflowArtifactAction::ProposeRoutineContract { proposal, .. } =
            parse(&call(value)).expect("hash-bound proposal parses")
        else {
            panic!("expected proposal")
        };
        let contract = materialize_routine_contract(
            proposal,
            user_prompt,
            &safe_scope(),
            safe_impact(),
            &trusted_verification(),
        )
        .expect("hash-bound proposal materializes");
        assert_eq!(contract.symptom_or_request, user_prompt);
    }

    #[test]
    fn m27_stale_prompt_hash_cannot_authorize_the_current_turn() {
        let mut value = proposal();
        value["request"] = serde_json::json!("Bounded parser guard.");
        value["prompt_sha256"] = serde_json::json!(prompt_sha256("An earlier request."));
        let WorkflowArtifactAction::ProposeRoutineContract { proposal, .. } =
            parse(&call(value)).expect("hash-bound proposal parses")
        else {
            panic!("expected proposal")
        };
        assert!(
            materialize_routine_contract(
                proposal,
                "Fix the bounded parser error without changing public behavior.",
                &safe_scope(),
                safe_impact(),
                &trusted_verification(),
            )
            .is_err()
        );
    }

    #[test]
    fn unbound_proposal_cannot_materialize_a_contract() {
        let WorkflowArtifactAction::ProposeRoutineContract { proposal, .. } =
            parse(&call(proposal())).expect("proposal parses")
        else {
            panic!("expected proposal")
        };
        assert!(
            materialize_routine_contract(
                proposal,
                "A different user request.",
                &safe_scope(),
                safe_impact(),
                &trusted_verification(),
            )
            .is_err()
        );
    }

    #[test]
    fn m27_model_verification_must_exactly_match_host_authority() {
        let mut value = proposal();
        value["verification"] = serde_json::json!(["A different check passes."]);
        let WorkflowArtifactAction::ProposeRoutineContract { proposal, .. } =
            parse(&call(value)).expect("proposal parses")
        else {
            panic!("expected proposal")
        };
        assert!(
            materialize_routine_contract(
                proposal,
                "Fix a bounded parser error.",
                &safe_scope(),
                safe_impact(),
                &trusted_verification(),
            )
            .is_err()
        );
    }

    #[test]
    fn proposal_rejects_model_supplied_approval_or_hash() {
        let mut value = proposal();
        value["status"] = serde_json::json!("approved");
        assert!(parse(&call(value)).is_err());
        let mut value = proposal();
        value["content_hash"] = serde_json::json!("0".repeat(64));
        assert!(parse(&call(value)).is_err());
    }

    #[test]
    fn read_requires_exact_valid_reference_and_kind() {
        let action = parse(&call(serde_json::json!({
            "op":"read", "kind":"feature_spec", "id":"feature-login",
            "revision":2, "content_hash":"a".repeat(64)
        })))
        .expect("read parses");
        assert!(matches!(
            action,
            WorkflowArtifactAction::ReadExact {
                kind: WorkflowArtifactKindV1::FeatureSpec,
                ..
            }
        ));
        assert!(
            parse(&call(serde_json::json!({
                "op":"read", "kind":"feature_spec", "id":"feature-login",
                "revision":0, "content_hash":"a".repeat(64)
            })))
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_and_cross_operation_fields() {
        let mut value = proposal();
        value["unknown"] = serde_json::json!(true);
        assert!(parse(&call(value)).is_err());
        assert!(
            parse(&call(serde_json::json!({
                "op":"read", "kind":"task_graph", "id":"task-1", "revision":1,
                "content_hash":"a".repeat(64), "expected_revision":1
            })))
            .is_err()
        );
    }
}
