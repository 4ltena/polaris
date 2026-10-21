//! Written start precedes dispatch; raw prompts never enter typed audit records.
use super::*;
use polaris_provider::decision::{Decider, Decision, DecisionError, Question};
use std::{collections::BTreeMap, path::PathBuf};
struct Judge {
    log: PathBuf,
    pending: bool,
}
#[async_trait::async_trait]
impl Decider for Judge {
    fn name(&self) -> &str {
        "fixture"
    }
    async fn decide(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        assert!(
            std::fs::read_to_string(&self.log)
                .unwrap()
                .contains("start")
        );
        if self.pending {
            std::future::pending::<()>().await;
        }
        Ok(Decision {
            answers: BTreeMap::new(),
            input_tokens: Some(2),
            output_tokens: None,
        })
    }
}
#[tokio::test]
async fn typed_audit_correlates_start_attempt_and_settlement_without_prompt_content() {
    for pending in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("audit.jsonl");
        let meter = DecisionMeter::default();
        install_decision_observer(&meter, Arc::new(Mutex::new(AuditLog::open(&log).unwrap())));
        let judge = meter.wrap(Arc::new(Judge {
            log: log.clone(),
            pending,
        }));
        let state = serde_json::json!({"private":"never-store-audit-content"});
        let questions = BTreeMap::new();
        if pending {
            let mut future = Box::pin(judge.observe(&state, &questions));
            assert!(futures_util::poll!(future.as_mut()).is_pending());
            drop(future);
        } else {
            judge.decide(&state, &questions).await.unwrap();
        }
        let saved = std::fs::read_to_string(log).unwrap();
        assert!(!saved.contains("never-store-audit-content"));
        let rows: Vec<serde_json::Value> = saved
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let details: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| serde_json::from_str(row["detail"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(details.len(), if pending { 1 } else { 3 });
        assert!(details.iter().all(|row|row["request_id"]==details[0]["request_id"] && row["stage"]=="typed"));
        assert_eq!(details[0]["phase"], "start");
        if !pending {
            assert_eq!(details[1]["phase"], "attempt");
            assert_eq!(details[1]["attempt"]["input_tokens"], 2);
            assert!(details[1]["attempt"]["output_tokens"].is_null());
            assert_eq!(details[2]["phase"], "settled");
            assert_eq!(details[2]["effective"]["backend"], "fixture");
        }
    }
}
