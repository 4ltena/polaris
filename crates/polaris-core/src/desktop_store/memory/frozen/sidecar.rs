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
        while let Some(name) = key(&mut map, self.budget)? {
            if !seen.insert(name.clone()) {
                return Err(serde::de::Error::custom("固定記憶の付随field重複"));
            }
            match name.as_str() {
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
                "workflow" => {
                    let workflow: Option<SavedWorkflow> = value(&mut map, self.budget, RECORD)?;
                    if let Some(workflow) = workflow {
                        workflow.validate().map_err(serde::de::Error::custom)?;
                    }
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
                _ => return Err(serde::de::Error::custom("固定記憶の未知の付随field")),
            }
        }
        for required in [
            "session_revision",
            "configuration",
            "conversation_memory",
            "draft",
            "policy_revision",
            "tasks",
            "runs",
            "requests",
            "unresolved_approvals",
        ] {
            if !seen.contains(required) {
                return Err(serde::de::Error::custom(format!(
                    "固定記憶の必須field不在: {required}"
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
        Err(E::custom("固定記憶がありません"))
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = BTreeSet::new();
        while let Some(name) = key(&mut map, self.budget)? {
            if !seen.insert(name.clone()) {
                return Err(serde::de::Error::custom("固定記憶のfield重複"));
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
                _ => return Err(serde::de::Error::custom("固定記憶の未知のfield")),
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
                    "固定記憶の必須field不在: {required}"
                )));
            }
        }
        Ok(())
    }
}
