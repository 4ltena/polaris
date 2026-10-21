//! Persist the effective typed source after failover, not the configured primary.
#![cfg(unix)]
use super::*;
use polaris_provider::decision::{
    Answer, Decision, DecisionError, DecisionIdentity,
    fallback::{FallbackDecider, FallbackPolicy},
};
use std::{sync::Arc, time::Duration};

struct Judge(bool);
#[async_trait::async_trait]
impl Decider for Judge {
    fn name(&self) -> &str {
        if self.0 { "effective" } else { "configured" }
    }
    fn identity(&self) -> DecisionIdentity {
        DecisionIdentity {
            backend: self.name().into(),
            model: Some("fixture-model".into()),
            revision: None,
            quantization: None,
        }
    }
    async fn decide(
        &self,
        _: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        if !self.0 {
            return Err(DecisionError::Unavailable);
        }
        let answers = questions
            .iter()
            .map(|(key, question)| {
                let mut answer = Answer {
                    confidence: Some(1.0),
                    ..Default::default()
                };
                match question {
                    Question::Choice { criteria, .. } => {
                        let chosen = criteria.keys().next().unwrap();
                        answer.choice = Some(chosen.clone());
                        answer.probabilities = criteria
                            .keys()
                            .map(|key| (key.clone(), if key == chosen { 1.0 } else { 0.0 }))
                            .collect();
                    }
                    Question::Score { criteria, .. } => {
                        answer.score = Some((criteria.len() - 1) as f64);
                        answer.probabilities = (0..criteria.len())
                            .map(|index| {
                                (
                                    index.to_string(),
                                    if index + 1 == criteria.len() {
                                        1.0
                                    } else {
                                        0.0
                                    },
                                )
                            })
                            .collect();
                    }
                    Question::Noul { .. } => answer.noul = Some(1.0),
                }
                (key.clone(), answer)
            })
            .collect();
        Ok(Decision {
            answers,
            input_tokens: Some(3),
            output_tokens: Some(1),
        })
    }
}

#[tokio::test]
async fn fallback_route_and_score_records_persist_actual_identity() {
    let chain = FallbackDecider::new(
        vec![Arc::new(Judge(false)), Arc::new(Judge(true))],
        FallbackPolicy {
            max_attempts: 2,
            attempt_timeout: Duration::from_secs(1),
            total_timeout: Duration::from_secs(2),
        },
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let store = VerificationStore::open(directory.path(), "synthetic").unwrap();
    let binding = VerificationBindingV1 {
        project_graph_hash: hash('a'),
        analyzer_hash: hash('b'),
        tool_hash: hash('c'),
        source_hash: hash('d'),
        diff_hash: hash('e'),
        config_hash: hash('f'),
        analysis_request_hash: hash('a'),
    };
    let contract = contract();
    let need = derive_verification_needs(ActiveVerificationArtifactsV1::RoutineContract {
        active_contract: &contract.reference(),
        contract: &contract,
    })
    .unwrap()
    .remove(0);
    let routes = vec![VerificationRouteCandidateV1 {
        id: "test".into(),
        route: VerificationRouteV1::ExistingTest,
        source_ref: "synthetic-plan".into(),
    }];
    let route = select_verification_route(Some(&chain), &store, &binding, &need, &routes)
        .await
        .unwrap();
    assert!(route.decision_ref.is_some());
    let record = route.decision.unwrap();
    assert_eq!(record.provider, "effective");
    assert_eq!(record.model, "fixture-model");
    let inventory = build_complete_rust_inventory(vec![entry("one")]).unwrap();
    let candidates = CandidateSetV1 {
        ids: vec!["one".into()],
        mandatory_ids: BTreeSet::new(),
        inventory_complete: true,
    };
    let selected = select_with_advisory_jev(
        Some(&chain),
        &store,
        &binding,
        &need,
        &inventory,
        &candidates,
        AllTestsScopeV1::Workspace,
    )
    .await;
    assert!(selected.decision_ref.is_some());
    let record = selected.decision.unwrap();
    assert_eq!(record.provider, "effective");
    assert_eq!(record.model, "fixture-model");
}
