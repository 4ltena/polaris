//! Opt-in, machine-readable observations for the offline comparison harness.
//!
//! This module deliberately accepts only already-resolved values. In
//! particular, it has no endpoint, credential, prompt, or provider handle to
//! serialize accidentally.

use std::collections::BTreeMap;

use serde_json::{Value, json};

const PREFIX: &str = "polaris-benchmark: ";

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct BenchmarkReport {
    enabled: bool,
}

impl BenchmarkReport {
    pub(super) fn from_environment() -> Self {
        Self::new(matches!(std::env::var("POLARIS_BENCHMARK_REPORT"), Ok(value) if value == "1"))
    }

    fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub(super) fn configured(&self, configuration: Configured<'_>) {
        self.emit(configured_event(configuration));
    }

    pub(super) fn completed(
        &self,
        succeeded: bool,
        decision: polaris_provider::decision::DecisionUsage,
        model_usage: BTreeMap<String, polaris_provider::Usage>,
        coverage: polaris_provider::UsageReport,
    ) {
        self.emit(completed_event(succeeded, decision, model_usage, coverage));
    }

    fn emit(&self, event: Value) {
        if let Some(line) = self.render(event) {
            eprintln!("{line}");
        }
    }

    fn render(&self, event: Value) -> Option<String> {
        self.enabled.then(|| format!("{PREFIX}{event}"))
    }
}

fn configured_event(configuration: Configured<'_>) -> Value {
    json!({
        "schema_version": 1,
        "event": "configured",
        "root_model": configuration.root_model,
        "root_effort": configuration.root_effort,
        "decision": {
            "enabled": configuration.decision_enabled,
            "active": configuration.decision_active,
            "model": configuration.decision_model,
        },
        "tiers": configuration.tiers.into_iter().map(|(name, tier)| {
            (name, json!({"model": tier.model, "effort": tier.effort}))
        }).collect::<BTreeMap<_, _>>(),
        "history_mode": configuration.history_mode,
        "tool_memory": configuration.tool_memory,
        "workflow": {
            "enabled": configuration.workflow_enabled,
            "mode": configuration.workflow_mode,
        },
    })
}

fn completed_event(
    succeeded: bool,
    decision: polaris_provider::decision::DecisionUsage,
    model_usage: BTreeMap<String, polaris_provider::Usage>,
    coverage: polaris_provider::UsageReport,
) -> Value {
    json!({
        "schema_version": 1,
        "event": "completed",
        "succeeded": succeeded,
        "decision": {
            "reported": decision.reported,
            "missing": decision.missing,
            "failed": decision.failed,
            "input_tokens": decision.input_tokens,
            "output_tokens": decision.output_tokens,
            "accounting_scope": "completed_logical_decisions",
            "physical_attempts_known": false,
        },
        "model_usage": model_usage,
        "coverage": {
            "reported": coverage.reported_responses,
            "missing": coverage.missing_responses,
            "failed": coverage.failed_requests,
        },
    })
}

pub(super) struct Configured<'a> {
    pub(super) root_model: &'a str,
    pub(super) root_effort: &'a str,
    pub(super) decision_enabled: bool,
    pub(super) decision_active: bool,
    pub(super) decision_model: &'a str,
    pub(super) tiers: BTreeMap<&'static str, Tier>,
    pub(super) history_mode: &'a str,
    pub(super) tool_memory: &'a str,
    pub(super) workflow_enabled: bool,
    pub(super) workflow_mode: &'a str,
}

pub(super) struct Tier {
    model: String,
    effort: String,
}

pub(super) fn effective_tiers(
    tiers: &polaris_core::config::TierConfig,
    root_model: &str,
    root_effort: &str,
) -> BTreeMap<&'static str, Tier> {
    polaris_skills::Tier::ALL
        .into_iter()
        .map(|tier| {
            let configured = tiers.for_tier(tier);
            (
                tier.name(),
                Tier {
                    model: configured
                        .model
                        .clone()
                        .unwrap_or_else(|| root_model.to_owned()),
                    effort: configured
                        .effort
                        .clone()
                        .unwrap_or_else(|| root_effort.to_owned()),
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_report_emits_no_event() {
        let report = BenchmarkReport::new(false);
        assert_eq!(report.render(json!({"event": "configured"})), None);
    }

    #[test]
    fn enabled_report_has_the_exact_prefix_and_one_json_value() {
        let line = BenchmarkReport::new(true)
            .render(completed_event(
                true,
                Default::default(),
                Default::default(),
                Default::default(),
            ))
            .expect("enabled report");
        let json = line.strip_prefix(PREFIX).expect("exact prefix");
        assert_eq!(
            serde_json::from_str::<Value>(json).unwrap()["event"],
            "completed"
        );
    }

    #[test]
    fn effective_tiers_include_every_weight_and_only_inherit_missing_values() {
        let tiers = polaris_core::config::TierConfig {
            dwarf: polaris_core::config::TierSettings {
                model: Some("small".into()),
                effort: None,
            },
            giant: polaris_core::config::TierSettings {
                model: None,
                effort: Some("high".into()),
            },
            ..Default::default()
        };
        let actual = effective_tiers(&tiers, "root", "medium");
        assert_eq!(actual.len(), 4);
        assert_eq!(actual["dwarf"].model, "small");
        assert_eq!(actual["dwarf"].effort, "medium");
        assert_eq!(actual["giant"].model, "root");
        assert_eq!(actual["giant"].effort, "high");
        assert_eq!(actual["subgiant"].model, "root");
        assert_eq!(actual["supergiant"].effort, "medium");
    }

    #[test]
    fn configured_event_has_no_credential_or_endpoint_field() {
        let payload = configured_event(Configured {
            root_model: "root",
            root_effort: "medium",
            decision_enabled: true,
            decision_active: true,
            decision_model: "jev-latest",
            tiers: effective_tiers(&Default::default(), "root", "medium"),
            history_mode: "legacy",
            tool_memory: "off",
            workflow_enabled: true,
            workflow_mode: "manual",
        });
        let text = payload.to_string();
        for forbidden in ["key", "token", "endpoint", "base_url", "prompt", "https://"] {
            assert!(!text.contains(forbidden), "unexpected {forbidden}: {text}");
        }
    }

    #[test]
    fn completed_event_declares_logical_decision_accounting_scope() {
        let payload = completed_event(
            false,
            polaris_provider::decision::DecisionUsage {
                input_tokens: 40,
                output_tokens: 5,
                reported: 1,
                missing: 2,
                failed: 3,
            },
            Default::default(),
            Default::default(),
        );
        assert_eq!(payload["event"], "completed");
        assert_eq!(
            payload["decision"]["accounting_scope"],
            "completed_logical_decisions"
        );
        assert_eq!(payload["decision"]["physical_attempts_known"], false);
        assert_eq!(payload["decision"]["input_tokens"], 40);
        assert_eq!(payload["decision"]["missing"], 2);
    }
}
