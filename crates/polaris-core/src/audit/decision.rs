//! Metadata-only typed-stage audit. An unmatched start has unknown completion/usage.
use super::{AuditLog, Record};
use polaris_provider::decision::{
    DecisionIdentity, DecisionMeter, DecisionObserver, DecisionTrace,
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub fn install_decision_observer(meter: &DecisionMeter, audit: Arc<Mutex<AuditLog>>) -> bool {
    meter.install_observer(Arc::new(Observer(audit)))
}
struct Observer(Arc<Mutex<AuditLog>>);
impl Observer {
    async fn record(&self, detail: serde_json::Value, result: &str) -> bool {
        self.0
            .lock()
            .await
            .record(&Record {
                tool: "typed_decision",
                detail: &detail.to_string(),
                sandbox: None,
                target: None,
                result,
                caller: "typed-stage",
            })
            .is_ok()
    }
}
#[async_trait::async_trait]
impl DecisionObserver for Observer {
    async fn started(&self, request_id: &str, configured: &DecisionIdentity) -> bool {
        self.record(
            serde_json::json!({
                "schema_version": 1, "stage": "typed", "phase": "start",
                "request_id": request_id, "configured": configured,
                "scope": "logical_stage_not_physical_dispatch",
            }),
            "started",
        )
        .await
    }
    async fn settled(&self, trace: &DecisionTrace) -> bool {
        // Write attempt metadata first. A missing completion record remains
        // unsettled even if one or more adapter replies were already recorded.
        for (index, attempt) in trace.attempts.iter().enumerate() {
            if !self
                .record(
                    serde_json::json!({
                        "schema_version": 1, "stage": "typed", "phase": "attempt",
                        "request_id": trace.request_id, "index": index, "attempt": attempt,
                    }),
                    "metadata",
                )
                .await
            {
                return false;
            }
        }
        self.record(
            serde_json::json!({
                "schema_version": 1, "stage": "typed", "phase": "settled",
                "request_id": trace.request_id, "effective": trace.effective,
                "capabilities": trace.capabilities, "error": trace.error,
                "elapsed_ms": trace.elapsed_ms, "attempt_count": trace.attempts.len(),
            }),
            "settled",
        )
        .await
    }
}

#[cfg(test)]
mod tests;
