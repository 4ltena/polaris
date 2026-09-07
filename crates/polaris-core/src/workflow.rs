//! Explicit workflow focus, independent of tool permissions and completion evidence.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, str::FromStr};

pub const WORKFLOW_SCHEMA_VERSION: u32 = 1;
pub const WORKFLOW_TOKEN_LIMIT: usize = 384;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    General,
    Brainstorm,
    Specify,
    Implement,
    Review,
    Verify,
    Deliver,
}

impl Phase {
    pub const ALL: [Self; 7] = [
        Self::General,
        Self::Brainstorm,
        Self::Specify,
        Self::Implement,
        Self::Review,
        Self::Verify,
        Self::Deliver,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Brainstorm => "brainstorm",
            Self::Specify => "specify",
            Self::Implement => "implement",
            Self::Review => "review",
            Self::Verify => "verify",
            Self::Deliver => "deliver",
        }
    }
}
impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for Phase {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|phase| phase.as_str() == value)
            .ok_or_else(|| format!("不明な作業段階です: {value}"))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkflowConfig {
    pub enabled: bool,
    pub initial_phase: Phase,
    pub always: Vec<String>,
    pub phase_skills: BTreeMap<Phase, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStateV1 {
    pub schema_version: u32,
    pub phase: Phase,
    pub revision: u64,
    pub skill_manifest_hash: String,
    pub skills: Vec<WorkflowSkillV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSkillV1 {
    pub id: String,
    pub source: String,
    pub body_hash: String,
}
impl Default for WorkflowStateV1 {
    fn default() -> Self {
        Self {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            phase: Phase::General,
            revision: 0,
            skill_manifest_hash: String::new(),
            skills: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationEvidence {
    pub artifact_hash: String,
    pub passed: bool,
    pub stale: bool,
}

/// Audit evidence only. A matching entry cannot bypass the existing Gate/sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalGrant {
    pub operation: String,
    pub target: String,
    pub artifact_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGatesV1 {
    pub schema_version: u32,
    pub approved_spec_hash: Option<String>,
    pub verification: Option<VerificationEvidence>,
    pub external_grants: Vec<ExternalGrant>,
}
impl Default for WorkflowGatesV1 {
    fn default() -> Self {
        Self {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            approved_spec_hash: None,
            verification: None,
            external_grants: Vec::new(),
        }
    }
}
impl WorkflowGatesV1 {
    pub fn revalidate_artifact(&mut self, current_hash: &str) {
        if let Some(evidence) = &mut self.verification {
            // Once stale, only a new verification can make the evidence current.
            evidence.stale |= evidence.artifact_hash != current_hash;
        }
    }

    pub fn for_fork(&self, artifact_hash: &str) -> Self {
        let mut child = self.clone();
        child.external_grants.clear();
        child.revalidate_artifact(artifact_hash);
        child
    }
}

/// Runtime turn boundary. Models and retrieved text have no transition API.
/// The controller does not own gates, so focus changes cannot grant permission.
#[derive(Debug, Clone, Default)]
pub struct Workflow {
    state: WorkflowStateV1,
    pending_phase: Option<Phase>,
    active: bool,
}
impl Workflow {
    pub fn restore(state: WorkflowStateV1) -> Result<Self, String> {
        if state.schema_version != WORKFLOW_SCHEMA_VERSION {
            return Err("未対応のworkflow保存形式です".into());
        }
        Ok(Self {
            state,
            ..Self::default()
        })
    }
    pub fn state(&self) -> &WorkflowStateV1 {
        &self.state
    }
    pub fn pending_phase(&self) -> Option<Phase> {
        self.pending_phase
    }

    /// Called only by explicit UI/CLI input or structured parent configuration.
    pub fn request_phase(&mut self, phase: Phase) -> Result<(), String> {
        if self.active {
            self.pending_phase = Some(phase);
        } else {
            self.apply_phase(phase)?;
        }
        Ok(())
    }
    fn apply_phase(&mut self, phase: Phase) -> Result<(), String> {
        if self.state.phase != phase {
            let revision = self
                .state
                .revision
                .checked_add(1)
                .ok_or("workflow revisionが上限に達しました")?;
            self.state.phase = phase;
            self.state.revision = revision;
            self.state.skill_manifest_hash.clear();
        }
        Ok(())
    }
    pub fn next_phase(&self) -> Phase {
        self.pending_phase.unwrap_or(self.state.phase)
    }

    /// Resolve skills and validate their budget BEFORE opening a turn. The hash
    /// freezes the resolved bodies; callers retain that same snapshot for tools.
    pub fn begin_turn(&mut self, manifest_hash: String) -> Result<WorkflowStateV1, String> {
        if self.active {
            return Err("作業ターンが既に実行中です".into());
        }
        if manifest_hash.is_empty() {
            return Err("skill manifest hashがありません".into());
        }
        if let Some(phase) = self.pending_phase {
            self.apply_phase(phase)?;
        }
        self.pending_phase = None;
        self.state.skill_manifest_hash = manifest_hash;
        self.active = true;
        Ok(self.state.clone())
    }
    pub fn finish_turn(&mut self) {
        self.active = false;
    }
}

/// Shared controller permits an explicit UI phase request while a turn future
/// owns the session. A turn guard releases it even if that future is dropped.
#[derive(Debug, Clone)]
pub struct SessionWorkflow {
    pub config: WorkflowConfig,
    controller: std::sync::Arc<std::sync::Mutex<Workflow>>,
    pub gates: WorkflowGatesV1,
}
impl SessionWorkflow {
    pub fn new(config: WorkflowConfig) -> Self {
        let state = WorkflowStateV1 {
            phase: config.initial_phase,
            ..Default::default()
        };
        Self::restore(config, WorkflowGatesV1::default(), state)
            .expect("new workflow uses supported schemas")
    }
    pub fn restore(
        config: WorkflowConfig,
        gates: WorkflowGatesV1,
        state: WorkflowStateV1,
    ) -> Result<Self, String> {
        if gates.schema_version != WORKFLOW_SCHEMA_VERSION {
            return Err("未対応のgate保存形式です".into());
        }
        Ok(Self {
            config,
            gates,
            controller: std::sync::Arc::new(std::sync::Mutex::new(Workflow::restore(state)?)),
        })
    }
    pub fn request_phase(&self, phase: Phase) -> Result<(), String> {
        self.controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?
            .request_phase(phase)
    }
    pub fn state(&self) -> WorkflowStateV1 {
        self.controller
            .lock()
            .expect("workflow controller poisoned")
            .state()
            .clone()
    }
    pub fn for_fork(&self, artifact_hash: &str) -> Result<Self, String> {
        Self::restore(
            self.config.clone(),
            self.gates.for_fork(artifact_hash),
            self.state(),
        )
    }
    pub fn begin_turn(&self, skills: &[polaris_skills::Skill]) -> Result<WorkflowTurn, String> {
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let phase = controller.next_phase();
        let mut candidates = polaris_skills::workflow_profile::recommended_bindings();
        candidates.extend_from_slice(skills);
        let profile = polaris_skills::workflow_profile::resolve(
            &self.config.always,
            self.config
                .phase_skills
                .get(&phase)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            &candidates,
        )
        .map_err(|error| error.to_string())?;
        let verification = match &self.gates.verification {
            None => "missing",
            Some(evidence) if evidence.stale => "stale",
            Some(evidence) if evidence.passed => "passed",
            Some(_) => "failed",
        };
        // Gates are evidence, never authorization instructions. Keep diagnostics
        // bounded without embedding arbitrary external targets or all grant data.
        let body = format!(
            "{}\nWorkflow focus: {phase}. Spec approval: {}. Verification: {verification}. External grants: {} (existing tool permission checks still apply).\n",
            profile.body,
            if self.gates.approved_spec_hash.is_some() {
                "recorded"
            } else {
                "missing"
            },
            self.gates.external_grants.len()
        );
        let tokens = crate::budget::count_tokens(&body);
        if tokens > WORKFLOW_TOKEN_LIMIT {
            return Err(format!(
                "段階別skillと状態表示が上限を超えました: {tokens}/{WORKFLOW_TOKEN_LIMIT} tokens"
            ));
        }
        let entries: Vec<WorkflowSkillV1> = profile
            .entries
            .into_iter()
            .map(|entry| WorkflowSkillV1 {
                id: entry.id,
                source: entry.source,
                body_hash: entry.body_hash,
            })
            .collect();
        let mut changed_skill_ids = Vec::new();
        if !controller.state.skill_manifest_hash.is_empty() {
            for entry in entries.iter().chain(&controller.state.skills) {
                if (!entries.contains(entry) || !controller.state.skills.contains(entry))
                    && !changed_skill_ids.contains(&entry.id)
                {
                    changed_skill_ids.push(entry.id.clone());
                }
            }
        }
        controller.begin_turn(profile.manifest_hash)?;
        controller.state.skills = entries;
        let state = controller.state.clone();
        Ok(WorkflowTurn {
            body,
            ids: profile.ids,
            tokens,
            state,
            changed_skill_ids,
            controller: self.controller.clone(),
        })
    }

    /// Refresh only mandatory user skills at a new real turn. Optional catalog
    /// search is unaffected, and an in-flight turn retains its earlier snapshot.
    pub fn begin_turn_from_disk(
        &self,
        skills: &[polaris_skills::Skill],
    ) -> Result<WorkflowTurn, String> {
        let phase = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?
            .next_phase();
        let names: Vec<&str> = self
            .config
            .always
            .iter()
            .chain(self.config.phase_skills.get(&phase).into_iter().flatten())
            .map(String::as_str)
            .collect();
        let mut refreshed = skills.to_vec();
        for skill in &mut refreshed {
            if names.iter().any(|name| {
                *name == skill.name || name.strip_prefix("user:") == Some(skill.name.as_str())
            }) {
                let text = std::fs::read_to_string(&skill.path).map_err(|error| {
                    format!("必須skill {}を読み込めません: {error}", skill.name)
                })?;
                let (name, description, body) =
                    polaris_skills::frontmatter::parse(&text, &skill.name)
                        .map_err(|error| error.to_string())?;
                skill.name = name;
                skill.description = description;
                skill.body = body;
            }
        }
        self.begin_turn(&refreshed)
    }
}
pub struct WorkflowTurn {
    pub body: String,
    pub ids: Vec<String>,
    pub tokens: usize,
    pub state: WorkflowStateV1,
    pub changed_skill_ids: Vec<String>,
    controller: std::sync::Arc<std::sync::Mutex<Workflow>>,
}
impl Drop for WorkflowTurn {
    fn drop(&mut self) {
        self.controller
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .finish_turn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_user_skill_reloads_only_at_turn_boundary_and_missing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        let text = |body: &str| format!("---\nname: rule\ndescription: Test rule\n---\n{body}\n");
        std::fs::write(&path, text("Keep identifiers exact.")).unwrap();
        let skill = polaris_skills::Skill {
            name: "rule".into(),
            description: "Test rule".into(),
            body: "stale catalog text".into(),
            path: path.clone(),
        };
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            always: vec!["user:rule".into()],
            ..Default::default()
        });
        let first = runtime
            .begin_turn_from_disk(std::slice::from_ref(&skill))
            .unwrap();
        assert!(first.body.contains("Keep identifiers exact."));
        std::fs::write(&path, text("Preserve source evidence.")).unwrap();
        assert!(!first.body.contains("Preserve source evidence."));
        drop(first);
        let second = runtime
            .begin_turn_from_disk(std::slice::from_ref(&skill))
            .unwrap();
        assert!(second.body.contains("Preserve source evidence."));
        assert_eq!(second.changed_skill_ids, vec!["user:rule"]);
        drop(second);
        std::fs::remove_file(&path).unwrap();
        assert!(runtime.begin_turn_from_disk(&[skill]).is_err());
    }
    #[test]
    fn new_session_uses_configured_phase_but_resume_keeps_saved_focus() {
        let config = WorkflowConfig {
            initial_phase: Phase::Review,
            ..Default::default()
        };
        assert_eq!(
            SessionWorkflow::new(config.clone()).state().phase,
            Phase::Review
        );
        let saved = WorkflowStateV1 {
            phase: Phase::Verify,
            revision: 7,
            ..Default::default()
        };
        let runtime =
            SessionWorkflow::restore(config, WorkflowGatesV1::default(), saved.clone()).unwrap();
        assert_eq!(runtime.state(), saved);
    }
    #[test]
    fn resume_identifies_changed_skill_body_and_source_by_name() {
        let config = WorkflowConfig {
            enabled: true,
            always: vec!["user:rule".into()],
            ..Default::default()
        };
        let mut skill = polaris_skills::Skill {
            name: "rule".into(),
            description: "Rule".into(),
            body: "Keep original facts.".into(),
            path: "/first/rule/SKILL.md".into(),
        };
        let first = SessionWorkflow::new(config.clone());
        let turn = first.begin_turn(&[skill.clone()]).unwrap();
        let saved = turn.state.clone();
        assert!(turn.changed_skill_ids.is_empty());
        drop(turn);
        skill.path = "/second/rule/SKILL.md".into();
        skill.body = "Keep original facts and scope.".into();
        let resumed =
            SessionWorkflow::restore(config, WorkflowGatesV1::default(), saved.clone()).unwrap();
        let next = resumed.begin_turn(&[skill]).unwrap();
        assert_eq!(next.changed_skill_ids, vec!["user:rule"]);
        assert_ne!(next.state.skill_manifest_hash, saved.skill_manifest_hash);
        assert_eq!(next.state.skills[0].source, "/second/rule/SKILL.md");
    }
    #[test]
    fn shipped_profile_with_gate_diagnostics_is_bounded_and_cancel_safe() {
        for phase in Phase::ALL {
            let mut config = WorkflowConfig {
                enabled: true,
                always: vec!["builtin:workflow-core".into()],
                ..Default::default()
            };
            if phase != Phase::General {
                config
                    .phase_skills
                    .insert(phase, vec![format!("builtin:{phase}")]);
            }
            let runtime = SessionWorkflow::restore(
                config,
                WorkflowGatesV1::default(),
                WorkflowStateV1 {
                    phase,
                    ..Default::default()
                },
            )
            .unwrap();
            let turn = runtime.begin_turn(&[]).unwrap();
            assert!(turn.body.contains("Spec approval: missing"));
            assert!(turn.tokens <= WORKFLOW_TOKEN_LIMIT);
            runtime.request_phase(Phase::Review).unwrap();
            assert_eq!(runtime.state(), turn.state);
            drop(turn);
            assert_eq!(runtime.begin_turn(&[]).unwrap().state.phase, Phase::Review);
        }
    }
    #[test]
    fn focus_is_explicit_and_never_grants_completion() {
        let gates = WorkflowGatesV1::default();
        let mut workflow = Workflow::default();
        workflow.request_phase(Phase::Deliver).unwrap();
        assert_eq!(workflow.state().phase, Phase::Deliver);
        assert!(gates.approved_spec_hash.is_none());
        assert!(gates.verification.is_none());
        assert!(gates.external_grants.is_empty());
        assert!("automatic".parse::<Phase>().is_err());
    }
    #[test]
    fn tool_roundtrips_freeze_focus_and_manifest_until_next_user() {
        let mut workflow = Workflow::default();
        let first = workflow.begin_turn("first".into()).unwrap();
        workflow.request_phase(Phase::Review).unwrap();
        workflow.request_phase(Phase::Verify).unwrap();
        assert_eq!(workflow.state(), &first);
        assert!(workflow.begin_turn("invalid".into()).is_err());
        workflow.finish_turn();
        assert_eq!(workflow.state(), &first);
        let second = workflow.begin_turn("second".into()).unwrap();
        assert_eq!(second.phase, Phase::Verify);
        assert_eq!(second.revision, 1);
        assert_eq!(second.skill_manifest_hash, "second");
    }
    #[test]
    fn fork_drops_grants_and_changed_artifacts_stay_stale() {
        let parent = WorkflowGatesV1 {
            verification: Some(VerificationEvidence {
                artifact_hash: "old".into(),
                passed: true,
                stale: false,
            }),
            external_grants: vec![ExternalGrant {
                operation: "push".into(),
                target: "main".into(),
                artifact_hash: "old".into(),
            }],
            ..Default::default()
        };
        let mut child = parent.for_fork("new");
        assert!(child.external_grants.is_empty());
        child.revalidate_artifact("old");
        assert!(child.verification.unwrap().stale);
        assert!(!parent.verification.unwrap().stale);
        assert_eq!(parent.external_grants.len(), 1);
    }
    #[test]
    fn unsupported_state_is_not_resumed() {
        assert!(
            Workflow::restore(WorkflowStateV1 {
                schema_version: 2,
                ..Default::default()
            })
            .is_err()
        );
    }
}
