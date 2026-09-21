//! The owner's configured judge reaches a root run even without MCP.
use super::*;
use polaris_provider::decision::{Answer, Decider, Decision, DecisionError, Question};
use std::collections::BTreeMap;

struct Judge;
#[async_trait::async_trait]
impl Decider for Judge {
    async fn decide(
        &self,
        _: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        Ok(Decision {
            answers: questions
                .keys()
                .map(|key| {
                    (
                        key.clone(),
                        Answer {
                            noul: Some(1.0),
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

struct JudgedFactory(Arc<dyn TrustedRunFactory>);
impl TrustedRunFactory for JudgedFactory {
    fn decider(&self) -> Option<Arc<dyn Decider>> {
        Some(Arc::new(Judge))
    }
    fn workflow_config(&self) -> Option<&polaris_core::workflow::WorkflowConfig> {
        self.0.workflow_config()
    }
    fn prepare(
        &self,
        target: &RunTarget,
        published: &Published,
    ) -> Result<TrustedRunInputs, ServiceError> {
        let input = self.0.prepare(target, published)?;
        assert!(input.mcp.is_none());
        let root = input.prepared.snapshot().path();
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(
            root.join("docs/filemap.md"),
            "## `src`\n\n- `chosen.rs` — chosen file\n",
        )
        .unwrap();
        polaris_core::decisions::append(root, "2026-09-19", "settled root question").unwrap();
        Ok(input)
    }
}

#[test]
fn owner_judge_reaches_root_filemap_and_decisions_without_mcp() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut fixture = fixture_with_history(
        Behavior::Complete,
        false,
        None,
        Some(Arc::new(RecordingProvider {
            calls: requests.clone(),
            summary: false,
            fail: false,
            hold: false,
            read_source: false,
        })),
    );
    let engine = fixture.service.engine.as_mut().unwrap();
    let trusted = engine.trusted_runs.as_mut().unwrap();
    trusted.factory = Arc::new(JudgedFactory(trusted.factory.clone()));
    start(engine);
    until(engine, |engine| engine.active.is_none());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].system.contains("src/chosen.rs"));
    assert!(requests[0].system.contains("settled root question"));
    assert_eq!(
        engine
            .store
            .snapshot()
            .unwrap()
            .raw
            .last()
            .unwrap()
            .message
            .content,
        "strict raw completion"
    );
}
