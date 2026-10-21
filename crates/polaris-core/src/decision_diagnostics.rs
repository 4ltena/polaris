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
            .unwrap_or_else(|| "未確認".into()),
        value
            .revision
            .as_deref()
            .map(text)
            .unwrap_or_else(|| "未確認".into()),
        value
            .quantization
            .as_deref()
            .map(text)
            .unwrap_or_else(|| "未確認".into())
    )
}

pub fn display(
    config: &crate::config::DecisionConfig,
    decider: Option<&dyn Decider>,
) -> Vec<String> {
    let mut lines = vec![String::new(), "型付き判断（観測）".into()];
    lines.push(format!(
        "  設定: {} / enabled={}",
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
                "  判断器: 構築不可（実接続は未確認）"
            } else {
                "  判断器: 無効"
            }
            .into(),
        );
        return lines;
    };
    let Some(meter) = decider.telemetry() else {
        lines.push("  記録: 計測器なし（実行・消費は未確認）".into());
        return lines;
    };
    let usage = meter.snapshot();
    lines.push(format!(
        "  使用量: in {} / out {}（確認できた分のみ）",
        usage.input_tokens, usage.output_tokens
    ));
    lines.push(format!(
        "  試行: 成功・使用量あり {} / 成功・欠測 {} / 失敗 {}",
        usage.reported, usage.missing, usage.failed
    ));
    lines.push(format!(
        "  未確定stage: {} / 監査書込失敗 {}（消費は不明）",
        usage.abandoned, usage.observer_failures
    ));
    let traces = meter.recent();
    let Some(trace) = traces.last() else {
        lines.push("  直近stage: 完了・破棄の観測なし（実接続は未確認）".into());
        return lines;
    };
    lines.push(format!(
        "  直近stage: {} / {}ms",
        text(&trace.request_id),
        trace.elapsed_ms
    ));
    if trace.abandoned {
        lines.push("  回答元: 未確定（future破棄。実計算の停止・消費は未確認）".into());
    } else {
        if let Some(effective) = &trace.effective {
            lines.push(format!("  観測backend: {}", identity(effective)));
        }
        if let Some(capabilities) = trace.capabilities {
            lines.push(format!(
                "  回答の校正: {:?} / 通信境界: {:?}",
                capabilities.band_calibration, capabilities.network
            ));
        }
        for (index, attempt) in trace.attempts.iter().enumerate() {
            if let Some(endpoint) = &attempt.endpoint_origin {
                lines.push(format!(
                    "  接続先origin（試行{}）: {}",
                    index + 1,
                    text(endpoint)
                ));
            }
            if let Some(capacity) = &attempt.capacity {
                lines.push(format!("  容量（試行{}）: {} / 理由={} / budget={:?} / required={:?} / 同居予定={:?} bytes", index+1, capacity.status, capacity.reason.as_deref().map(text).unwrap_or_else(|| "なし".into()), capacity.budget_bytes, capacity.required_bytes, capacity.colocated_bytes));
            }

            lines.push(format!(
                "  試行{}: {} / 呼出={} / {}ms / {:?} / 理由={}",
                index + 1,
                identity(&attempt.identity),
                attempt.invoked,
                attempt.elapsed_ms,
                attempt.error,
                attempt
                    .reason
                    .as_deref()
                    .map(text)
                    .unwrap_or_else(|| "なし".into())
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
        assert!(before.contains("観測なし"));
        judge
            .decide(&serde_json::Value::Null, &BTreeMap::new())
            .await
            .unwrap();
        let after = display(&config, Some(judge.as_ref())).join("\n");
        assert!(after.contains("観測backend"));
        assert!(after.contains("out 3"));
        assert!(after.contains("欠測 1"));
        assert!(!after.contains('\x1b'));
        assert!(after.contains("revision=未確認"));
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
        assert!(shown.contains("構築不可"));
        assert!(
            display(&Default::default(), None)
                .join("\n")
                .contains("判断器: 無効")
        );
    }
}
