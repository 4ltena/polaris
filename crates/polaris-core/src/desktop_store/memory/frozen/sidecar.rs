//! Typed streaming projection, with no dependence on JSON field order.
use super::*;
use bounded::{Budget, COUNT, RECORD, SOURCE, key, sequence, value};
use polaris_desktop_protocol::snapshot::{Configuration, Draft, PendingApproval, Task};
use serde::de::{DeserializeSeed, MapAccess, Visitor};
use std::fmt;

pub(super) struct SidecarSeed<'a, 'b> {
    pub(super) budget: &'a Budget,
    pub(super) collector: &'a mut Collector<'b>,
    pub(super) submit: &'a mut dyn FnMut(&StructuredDelta) -> polaris_memory::Result<()>,
}
impl<'de> DeserializeSeed<'de> for SidecarSeed<'_, '_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for SidecarSeed<'_, '_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("frozen desktop sidecar")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = BTreeSet::new();
        let mut version = 1;
        let mut saved_workflow = None;
        let mut workflow_artifacts = None;
        while let Some(name) = key(&mut map, self.budget)? {
            if !seen.insert(name.clone()) {
                return Err(serde::de::Error::custom(
                    "Duplicate pinned memory sidecar field",
                ));
            }
            match name.as_str() {
                "desktop_sidecar_schema_version" => {
                    version = value::<_, u8>(&mut map, self.budget, 64)?;
                    if version != 2 {
                        return Err(serde::de::Error::custom(
                            "unsupported desktop sidecar version",
                        ));
                    }
                }
                "session_revision" => {
                    self.collector.revision = Some(value(&mut map, self.budget, 64)?)
                }
                "configuration" => {
                    let config: Configuration = value(&mut map, self.budget, RECORD)?;
                    self.collector.mode = Some(config.history_mode);
                }
                "conversation_memory" => {
                    self.budget.arm(256).map_err(serde::de::Error::custom)?;
                    map.next_value_seed(MemorySeed {
                        budget: self.budget,
                        collector: self.collector,
                        submit: self.submit,
                    })?;
                }
                "runs" => sequence(&mut map, self.budget, RECORD, COUNT, |r: RunRecord| {
                    self.collector.run(r)
                })?,
                "draft" => {
                    let _: Draft = value(&mut map, self.budget, RECORD)?;
                }
                "inherited_turn_count" => {
                    let count: DecimalU64 = value(&mut map, self.budget, 64)?;
                    self.collector.inherited_turn_count = count.get();
                }
                "policy_revision" => {
                    let _: DecimalU64 = value(&mut map, self.budget, 64)?;
                }
                "role_bindings" => {
                    let roles: Option<SavedRoleBindings> = value(&mut map, self.budget, RECORD)?;
                    if let Some(roles) = roles {
                        roles.validate().map_err(serde::de::Error::custom)?;
                    }
                }
                "tier_bindings" => {
                    let tiers: Option<crate::desktop_store::SavedTierBindings> =
                        value(&mut map, self.budget, RECORD)?;
                    if let Some(tiers) = tiers {
                        tiers.validate().map_err(serde::de::Error::custom)?;
                    }
                }
                "goal" => {
                    let goal: Option<crate::desktop_store::SavedGoal> =
                        value(&mut map, self.budget, RECORD)?;
                    if let Some(goal) = goal {
                        goal.validate().map_err(serde::de::Error::custom)?;
                    }
                }
                "instructions" => {
                    let mut ids = BTreeSet::new();
                    sequence(
                        &mut map,
                        self.budget,
                        RECORD,
                        crate::desktop_store::goal::MAX_INSTRUCTIONS,
                        |instruction: crate::desktop_store::SavedInstruction| {
                            instruction.validate()?;
                            if !ids.insert(instruction.instruction_id) {
                                return Err(StoreError::Corrupt("instruction"));
                            }
                            Ok(())
                        },
                    )?;
                }
                "workflow" => {
                    let workflow: Option<SavedWorkflow> = value(&mut map, self.budget, RECORD)?;
                    if let Some(workflow) = workflow {
                        workflow.validate().map_err(serde::de::Error::custom)?;
                        saved_workflow = Some(workflow);
                    }
                }
                "workflow_artifacts" => {
                    let graph: crate::workflow::WorkflowArtifactGraphV1 =
                        value(&mut map, self.budget, RECORD)?;
                    graph.validate().map_err(serde::de::Error::custom)?;
                    workflow_artifacts = Some(graph);
                }
                "tasks" => sequence(&mut map, self.budget, RECORD, COUNT, |_: Task| Ok(()))?,
                "children" => {
                    sequence(&mut map, self.budget, RECORD, COUNT, |_: SavedChild| Ok(()))?
                }
                "source_applies" => sequence(
                    &mut map,
                    self.budget,
                    RECORD,
                    COUNT,
                    |_: SavedSourceApply| Ok(()),
                )?,
                "requests" => {
                    sequence(&mut map, self.budget, RECORD, COUNT, |_: RequestRecord| {
                        Ok(())
                    })?
                }
                "unresolved_approvals" => sequence(
                    &mut map,
                    self.budget,
                    RECORD,
                    COUNT,
                    |_: PendingApproval| Ok(()),
                )?,
                "approval_records" => sequence(
                    &mut map,
                    self.budget,
                    RECORD,
                    COUNT,
                    |record: crate::desktop_store::model::ApprovalRecord| {
                        let id = record.pending.operation_id;
                        self.collector.charge(&id)?;
                        self.collector.approvals.insert(id);
                        Ok(())
                    },
                )?,
                _ => {
                    return Err(serde::de::Error::custom(
                        "Unknown pinned memory sidecar field",
                    ));
                }
            }
        }
        if let Some(reference) = saved_workflow
            .as_ref()
            .and_then(|workflow| workflow.state.active_change_contract.as_ref())
        {
            let graph = workflow_artifacts.as_ref().ok_or_else(|| {
                serde::de::Error::custom("Missing pinned memory active artifact graph")
            })?;
            graph
                .approved_routine_contract(reference)
                .map_err(serde::de::Error::custom)?;
        }
        let mut required = vec![
            "session_revision",
            "configuration",
            "conversation_memory",
            "draft",
            "policy_revision",
            "tasks",
            "runs",
            "requests",
            "unresolved_approvals",
        ];
        if version == 2 {
            required.extend([
                "inherited_turn_count",
                "source_applies",
                "children",
                "role_bindings",
                "tier_bindings",
                "goal",
                "instructions",
                "approval_records",
                "workflow",
                "workflow_artifacts",
            ]);
        }
        for required in required {
            if !seen.contains(required) {
                return Err(serde::de::Error::custom(format!(
                    "Missing required pinned memory field: {required}"
                )));
            }
        }
        Ok(())
    }
}

struct MemorySeed<'a, 'b> {
    budget: &'a Budget,
    collector: &'a mut Collector<'b>,
    submit: &'a mut dyn FnMut(&StructuredDelta) -> polaris_memory::Result<()>,
}
impl<'de> DeserializeSeed<'de> for MemorySeed<'_, '_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_option(self)
    }
}
impl<'de> Visitor<'de> for MemorySeed<'_, '_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("saved memory projection")
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<(), E> {
        Err(E::custom("Missing pinned memory"))
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = BTreeSet::new();
        while let Some(name) = key(&mut map, self.budget)? {
            if !seen.insert(name.clone()) {
                return Err(serde::de::Error::custom("Duplicate pinned memory field"));
            }
            match name.as_str() {
                "schema_version" => {
                    self.collector.memory_version = Some(value(&mut map, self.budget, 64)?)
                }
                "epoch" => self.collector.epoch = Some(value(&mut map, self.budget, 64)?),
                "generation" => self.collector.generation = Some(value(&mut map, self.budget, 64)?),
                "resources" => {
                    self.collector.resources = Some(value(&mut map, self.budget, SOURCE)?)
                }
                "sources" => sequence(
                    &mut map,
                    self.budget,
                    SOURCE,
                    COUNT,
                    |source: PublishedSource| self.collector.source(source),
                )?,
                "attempts" => sequence(
                    &mut map,
                    self.budget,
                    RECORD,
                    COUNT,
                    |attempt: SummaryAttempt| self.collector.attempt(attempt, self.submit),
                )?,
                "ancestors" => sequence(
                    &mut map,
                    self.budget,
                    RECORD,
                    1024,
                    |ancestor: fork::FrozenAncestor| {
                        self.collector.charge(&ancestor)?;
                        self.collector.ancestors.push(ancestor);
                        Ok(())
                    },
                )?,
                _ => return Err(serde::de::Error::custom("Unknown pinned memory field")),
            }
        }
        for required in [
            "schema_version",
            "epoch",
            "generation",
            "resources",
            "sources",
            "attempts",
        ] {
            if !seen.contains(required) {
                return Err(serde::de::Error::custom(format!(
                    "Missing required pinned memory field: {required}"
                )));
            }
        }
        Ok(())
    }
}
