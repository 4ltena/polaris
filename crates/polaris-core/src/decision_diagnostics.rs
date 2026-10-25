//! Read-only typed-stage diagnostics shared by CLI and TUI; no connection probes.
use polaris_provider::decision::{Decider, DecisionIdentity};

fn text(value: &str) -> String {
    value.escape_debug().take(512).collect()
}
fn identity(value: &DecisionIdentity) -> String {
    format!(
        "{} / model={} / revision={} / quantization={}",
        text(&value.backend),
        value
            .model
            .as_deref()
            .map(text)
            .unwrap_or_else(|| "unverified".into()),
        value
            .revision
            .as_deref()
            .map(text)
            .unwrap_or_else(|| "unverified".into()),
        value
            .quantization
            .as_deref()
            .map(text)
            .unwrap_or_else(|| "unverified".into())
    )
}

pub fn display(
    config: &crate::config::DecisionConfig,
    decider: Option<&dyn Decider>,
) -> Vec<String> {
    let mut lines = vec![String::new(), "Typed decisions (observations)".into()];
    lines.push(format!(
        "  Configuration: {} / enabled={}",
        identity(&DecisionIdentity {
            backend: config.backend.as_str().into(),
            model: if config.backend == crate::config::DecisionBackend::SystemOne {
                config
                    .system_one
                    .as_ref()
                    .map(|profile| profile.model.clone())
            } else {
                config.model.clone()
            },
            revision: None,
            quantization: None,
        }),
        config.enabled
    ));
    let Some(decider) = decider else {
        lines.push(
            if config.enabled {
                "  Decider: cannot construct (connection unverified)"
            } else {
                "  Decider: disabled"
            }
            .into(),
        );
        return lines;
    };
    let Some(meter) = decider.telemetry() else {
        lines.push("  Records: no meter (execution and usage unverified)".into());
        return lines;
    };
    let usage = meter.snapshot();
    lines.push(format!(
        "  Usage: in {} / out {} (verified observations only)",
        usage.input_tokens, usage.output_tokens
    ));
    lines.push(format!(
        "  Attempts: success with usage {} / success without usage {} / failed {}",
        usage.reported, usage.missing, usage.failed
    ));
    lines.push(format!(
        "  Unresolved stages: {} / audit write failures {} (usage unknown)",
        usage.abandoned, usage.observer_failures
    ));
    let traces = meter.recent();
    let Some(trace) = traces.last() else {
        lines.push(
            "  Latest stage: no completion or discard observed (connection unverified)".into(),
        );
        return lines;
    };
    lines.push(format!(
        "  Latest stage: {} / {} ms",
        text(&trace.request_id),
        trace.elapsed_ms
    ));
    if trace.abandoned {
        lines.push("  Response source: unresolved (future dropped; actual computation stop and usage unverified)".into());
    } else {
        if let Some(effective) = &trace.effective {
            lines.push(format!("  Observed backend: {}", identity(effective)));
        }
        if let Some(capabilities) = trace.capabilities {
            lines.push(format!(
                "  Answer calibration: {:?} / network boundary: {:?}",
                capabilities.band_calibration, capabilities.network
            ));
        }
        for (index, attempt) in trace.attempts.iter().enumerate() {
            if let Some(endpoint) = &attempt.endpoint_origin {
                lines.push(format!(
                    "  Endpoint origin (attempt {}): {}",
                    index + 1,
                    text(endpoint)
                ));
            }
            if let Some(capacity) = &attempt.capacity {
                lines.push(format!("  Capacity (attempt {}): {} / reason={} / budget={:?} / required={:?} / planned coexistence={:?} bytes", index+1, capacity.status, capacity.reason.as_deref().map(text).unwrap_or_else(|| "none".into()), capacity.budget_bytes, capacity.required_bytes, capacity.colocated_bytes));
            }

            lines.push(format!(
                "  Attempt {}: {} / call={} / {} ms / {:?} / reason={}",
                index + 1,
                identity(&attempt.identity),
                attempt.invoked,
                attempt.elapsed_ms,
                attempt.error,
                attempt
                    .reason
                    .as_deref()
                    .map(text)
                    .unwrap_or_else(|| "none".into())
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::decision::{Decision, DecisionError, DecisionMeter, Question};
    use std::{collections::BTreeMap, sync::Arc};
    struct Judge;
    #[async_trait::async_trait]
    impl Decider for Judge {
        fn name(&self) -> &str {
            "configured\x1b[31m"
        }
        async fn decide(
            &self,
            _: &serde_json::Value,
            _: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            Ok(Decision {
                answers: BTreeMap::new(),
                input_tokens: None,
                output_tokens: Some(3),
            })
        }
    }
    #[tokio::test]
    async fn diagnostics_show_observed_usage_and_escape_control_characters() {
        let meter = DecisionMeter::default();
        let judge = meter.wrap(Arc::new(Judge));
        let config = crate::config::DecisionConfig {
            enabled: true,
            ..Default::default()
        };
        let before = display(&config, Some(judge.as_ref())).join("\n");
        assert!(before.contains("no completion or discard observed"));
        judge
            .decide(&serde_json::Value::Null, &BTreeMap::new())
            .await
            .unwrap();
        let after = display(&config, Some(judge.as_ref())).join("\n");
        assert!(after.contains("Observed backend"));
        assert!(after.contains("out 3"));
        assert!(after.contains("without usage 1"));
        assert!(!after.contains('\x1b'));
        assert!(after.contains("revision=unverified"));
    }
    #[test]
    fn unavailable_backend_keeps_the_explicit_configuration_visible() {
        let config = crate::config::DecisionConfig {
            enabled: true,
            backend: crate::config::DecisionBackend::Clef,
            model: Some("unavailable-fixture".into()),
            ..Default::default()
        };
        let shown = display(&config, None).join("\n");
        assert!(shown.contains("clef") && shown.contains("model=unavailable-fixture"));
        assert!(shown.contains("cannot construct"));
        assert!(
            display(&Default::default(), None)
                .join("\n")
                .contains("Decider: disabled")
        );
    }
}
