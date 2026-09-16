//! Explicit workflow focus, independent of tool permissions and completion evidence.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, str::FromStr};

pub const WORKFLOW_SCHEMA_VERSION: u32 = 1;
pub const WORKFLOW_TOKEN_LIMIT: usize = 384;

/// Selects whether workflow focus can be proposed by the model.  Manual is
/// deliberately the default so existing CLI and TUI phase controls retain
/// their established behaviour.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    #[default]
    Manual,
    Adaptive,
}

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

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfig {
    pub enabled: bool,
    #[serde(default)]
    pub mode: WorkflowMode,
    pub initial_phase: Phase,
    pub always: Vec<String>,
    pub phase_skills: BTreeMap<Phase, Vec<String>>,
}

/// The fixed skill body selected for one provider request, paired with the
/// controller state observed at that request boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowProfile {
    pub state: WorkflowStateV1,
    pub body: String,
    pub ids: Vec<String>,
    pub tokens: usize,
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
    reservation: Option<WorkflowReservation>,
    next_reservation: u64,
    active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkflowReservation {
    id: u64,
    phase: Phase,
    from_revision: u64,
    publishing: bool,
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
        // A manual request received before durable publishing wins over a
        // model proposal. Once publishing has started it becomes the next
        // boundary request and can never rewrite the acknowledged revision.
        if self
            .reservation
            .is_some_and(|reservation| !reservation.publishing)
        {
            self.reservation = None;
        }
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
        // Kept for the in-memory legacy controller API. Durable session
        // callers use `start_turn` and checkpoint a pending phase first.
        if self.active {
            return Err("作業ターンが既に実行中です".into());
        }
        if let Some(phase) = self.pending_phase {
            self.apply_phase(phase)?;
            self.pending_phase = None;
        }
        self.start_turn(manifest_hash)
    }
    fn start_turn(&mut self, manifest_hash: String) -> Result<WorkflowStateV1, String> {
        if self.active {
            return Err("作業ターンが既に実行中です".into());
        }
        if manifest_hash.is_empty() {
            return Err("skill manifest hashがありません".into());
        }
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
        let mut candidates = polaris_skills::workflow_profile::recommended_bindings();
        candidates.extend_from_slice(skills);
        let phases: Vec<Phase> = match self.config.mode {
            WorkflowMode::Manual => {
                let mut phases = vec![controller.state.phase];
                if let Some(pending) = controller.pending_phase
                    && pending != controller.state.phase
                {
                    phases.push(pending);
                }
                phases
            }
            // Freeze every phase at the real-turn boundary. A later adaptive
            // transition only selects from this snapshot; it never reads disk
            // or a changing catalog in the middle of a user turn.
            WorkflowMode::Adaptive => Phase::ALL.to_vec(),
        };
        let profiles = resolve_profiles(&self.config, &candidates, &phases)?;
        // The resolver's skill-only limit is necessary but not sufficient:
        // validate the final state diagnostics for every snapshot that an
        // adaptive turn may select before any provider request is sent.
        for (phase, frozen) in &profiles {
            let mut snapshot = controller.state.clone();
            snapshot.phase = *phase;
            build_profile(snapshot, frozen, &self.gates)?;
        }
        let current = profiles
            .get(&controller.state.phase)
            .expect("current phase is always resolved");
        let had_manifest = !controller.state.skill_manifest_hash.is_empty();
        let previous_skills = controller.state.skills.clone();
        controller.start_turn(current.manifest_hash.clone())?;
        controller.state.skills = current.entries.clone();
        let profile = build_profile(controller.state.clone(), current, &self.gates)?;
        let changed_skill_ids = if had_manifest {
            changed_skill_ids(&previous_skills, &profile.state.skills)
        } else {
            Vec::new()
        };
        Ok(WorkflowTurn {
            body: profile.body.clone(),
            ids: profile.ids.clone(),
            tokens: profile.tokens,
            state: profile.state.clone(),
            changed_skill_ids,
            mode: self.config.mode,
            config: self.config.clone(),
            candidates,
            profiles,
            controller: self.controller.clone(),
        })
    }

    /// Refresh only mandatory user skills at a new real turn. Optional catalog
    /// search is unaffected, and an in-flight turn retains its earlier snapshot.
    pub fn begin_turn_from_disk(
        &self,
        skills: &[polaris_skills::Skill],
    ) -> Result<WorkflowTurn, String> {
        self.begin_turn(&self.refresh_required_skills(skills)?)
    }

    /// Reads mandatory user skills once at a real-turn boundary. Callers that
    /// retry a child schema response retain this returned snapshot rather than
    /// reopening host paths in the retry.
    pub(crate) fn refresh_required_skills(
        &self,
        skills: &[polaris_skills::Skill],
    ) -> Result<Vec<polaris_skills::Skill>, String> {
        let controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let phases: Vec<Phase> = match self.config.mode {
            WorkflowMode::Manual => {
                let mut phases = vec![controller.state.phase];
                if let Some(pending) = controller.pending_phase
                    && pending != controller.state.phase
                {
                    phases.push(pending);
                }
                phases
            }
            WorkflowMode::Adaptive => Phase::ALL.to_vec(),
        };
        drop(controller);
        let names: Vec<&str> = self
            .config
            .always
            .iter()
            .chain(
                phases
                    .iter()
                    .flat_map(|phase| self.config.phase_skills.get(phase).into_iter().flatten()),
            )
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
        Ok(refreshed)
    }
}
pub struct WorkflowTurn {
    pub body: String,
    pub ids: Vec<String>,
    pub tokens: usize,
    pub state: WorkflowStateV1,
    pub changed_skill_ids: Vec<String>,
    mode: WorkflowMode,
    config: WorkflowConfig,
    candidates: Vec<polaris_skills::Skill>,
    profiles: BTreeMap<Phase, FrozenWorkflowProfile>,
    controller: std::sync::Arc<std::sync::Mutex<Workflow>>,
}

#[derive(Debug, Clone)]
struct FrozenWorkflowProfile {
    body: String,
    manifest_hash: String,
    ids: Vec<String>,
    entries: Vec<WorkflowSkillV1>,
}

impl WorkflowTurn {
    /// Metadata from the catalog frozen at this real-turn boundary. Bodies
    /// remain private to workflow profile resolution; optional discovery only
    /// receives names, descriptions, stable IDs, and source identities.
    pub(crate) fn catalog_entries(&self) -> Vec<polaris_tools::skill::SkillCatalogEntry> {
        skill_catalog_entries(&self.candidates)
    }

    /// Complete skills frozen at the real-turn boundary. This is deliberately
    /// only exposed within core so exact `skill` lookup can disclose the same
    /// body whose metadata was advertised, including refreshed mandatory and
    /// built-in entries.
    pub(crate) fn skill_corpus(&self) -> &[polaris_skills::Skill] {
        &self.candidates
    }
}

pub(crate) fn skill_catalog_entries(
    skills: &[polaris_skills::Skill],
) -> Vec<polaris_tools::skill::SkillCatalogEntry> {
    skills
        .iter()
        .map(|skill| {
            let source_identity = skill.path.to_string_lossy().into_owned();
            let id = if source_identity.starts_with("builtin:") {
                format!("builtin:{}", skill.name)
            } else {
                format!("user:{}", skill.name)
            };
            polaris_tools::skill::SkillCatalogEntry {
                id,
                source_identity,
                name: normalize_metadata(&skill.name),
                description: normalize_metadata(&skill.description),
            }
        })
        .collect()
}

impl WorkflowTurn {
    /// Refreshes the manual turn's single fixed profile after its owner has
    /// durably committed a phase that was pending before this real turn
    /// started. The profile still comes from this turn's captured catalog;
    /// callers must not use this after the first provider request.
    pub fn refresh_manual_profile(&mut self, gates: &WorkflowGatesV1) -> Result<(), String> {
        if self.mode != WorkflowMode::Manual {
            return Err("adaptive modeではmanual profileを更新できません".into());
        }
        let state = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?
            .state
            .clone();
        let frozen = self.frozen_for(state.phase)?;
        let profile = build_profile(state, &frozen, gates)?;
        self.body = profile.body;
        self.ids = profile.ids;
        self.tokens = profile.tokens;
        self.state = profile.state;
        Ok(())
    }

    /// Returns the profile for the currently committed focus. Pending manual
    /// work is deliberately not applied here: its owner must checkpoint it
    /// through `prepare_pending` before the next provider request.
    pub fn profile(&self, gates: &WorkflowGatesV1) -> Result<WorkflowProfile, String> {
        if self.mode == WorkflowMode::Manual {
            return Ok(WorkflowProfile {
                state: self.state.clone(),
                body: self.body.clone(),
                ids: self.ids.clone(),
                tokens: self.tokens,
            });
        }
        let state = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?
            .state
            .clone();
        let frozen = self
            .profiles
            .get(&state.phase)
            .ok_or("この実ターンに固定された段階skillがありません")?;
        build_profile(state, frozen, gates)
    }

    /// Prepares a manual request that was queued while the real turn was
    /// active. It has the same durable reservation boundary as a model
    /// proposal, but does not need a model-supplied revision.
    pub fn prepare_pending(
        &self,
        gates: &WorkflowGatesV1,
    ) -> Result<Option<PreparedWorkflowTransition>, String> {
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let Some(phase) = controller.pending_phase else {
            return Ok(None);
        };
        self.prepare(&mut controller, phase, gates, true).map(Some)
    }

    /// Reserves an adaptive model proposal without changing the committed
    /// state. The caller persists `candidate.state` outside the mutex, then
    /// calls `commit` only after its durable ACK.
    pub fn prepare_transition(
        &self,
        phase: Phase,
        expected_revision: u64,
        gates: &WorkflowGatesV1,
    ) -> Result<PreparedWorkflowTransition, String> {
        if self.mode != WorkflowMode::Adaptive {
            return Err("manual modeではmodelによるworkflow変更はできません".into());
        }
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        if controller.state.revision != expected_revision {
            return Err("workflow revisionが競合しました".into());
        }
        if controller.pending_phase.is_some() {
            return Err("手動のworkflow変更が保留中です".into());
        }
        self.prepare(&mut controller, phase, gates, false)
    }

    fn prepare(
        &self,
        controller: &mut Workflow,
        phase: Phase,
        gates: &WorkflowGatesV1,
        pending: bool,
    ) -> Result<PreparedWorkflowTransition, String> {
        if controller.reservation.is_some() {
            return Err("workflow変更は保存確認待ちです".into());
        }
        let previous = controller.state.clone();
        let frozen = self.frozen_for(phase)?;
        let revision = if phase == previous.phase {
            previous.revision
        } else {
            previous
                .revision
                .checked_add(1)
                .ok_or("workflow revisionが上限に達しました")?
        };
        let mut candidate_state = previous.clone();
        candidate_state.phase = phase;
        candidate_state.revision = revision;
        let candidate = build_profile(candidate_state, &frozen, gates)?;
        let reservation_id = controller.next_reservation;
        controller.next_reservation = controller.next_reservation.wrapping_add(1);
        controller.reservation = Some(WorkflowReservation {
            id: reservation_id,
            phase,
            from_revision: previous.revision,
            publishing: false,
        });
        Ok(PreparedWorkflowTransition {
            previous,
            candidate,
            reservation_id,
            pending,
            controller: self.controller.clone(),
            finished: false,
        })
    }

    /// Resolves a manually queued phase from the catalog snapshot captured at
    /// real-turn start. This never reads disk or a changed skill catalog.
    fn frozen_for(&self, phase: Phase) -> Result<FrozenWorkflowProfile, String> {
        if let Some(frozen) = self.profiles.get(&phase) {
            return Ok(frozen.clone());
        }
        resolve_profiles(&self.config, &self.candidates, &[phase])?
            .remove(&phase)
            .ok_or_else(|| "この実ターンに固定された段階skillがありません".into())
    }
}

fn normalize_metadata(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A candidate state reserved for a single durable checkpoint. This owns no
/// mutex guard and is therefore safe to carry across an async persistence
/// await. Dropping it releases an uncommitted reservation.
pub struct PreparedWorkflowTransition {
    pub previous: WorkflowStateV1,
    pub candidate: WorkflowProfile,
    reservation_id: u64,
    pending: bool,
    controller: std::sync::Arc<std::sync::Mutex<Workflow>>,
    finished: bool,
}

impl PreparedWorkflowTransition {
    pub fn is_noop(&self) -> bool {
        self.previous.phase == self.candidate.state.phase
            && self.previous.revision == self.candidate.state.revision
    }

    /// Marks the reservation as externally publishing. Manual changes after
    /// this point queue for the following boundary instead of rewriting this
    /// candidate.
    pub fn begin_persist(&mut self) -> Result<(), String> {
        let id = self.reservation_id;
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let reservation = controller
            .reservation
            .as_mut()
            .filter(|reservation| reservation.id == id)
            .ok_or("workflow変更の予約が失効しました")?;
        reservation.publishing = true;
        Ok(())
    }

    pub fn commit(mut self) -> Result<WorkflowStateV1, String> {
        let id = self.reservation_id;
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let reservation = controller
            .reservation
            .as_ref()
            .copied()
            .filter(|reservation| reservation.id == id && reservation.publishing)
            .ok_or("workflow変更の予約が失効しました")?;
        if reservation.from_revision != self.previous.revision {
            return Err("workflow変更の保存前revisionが一致しません".into());
        }
        controller.reservation = None;
        controller.state = self.candidate.state.clone();
        if self.pending && controller.pending_phase == Some(reservation.phase) {
            controller.pending_phase = None;
        }
        self.finished = true;
        Ok(controller.state.clone())
    }

    pub fn cancel(mut self) {
        self.release();
    }

    fn release(&mut self) {
        let id = self.reservation_id;
        if let Ok(mut controller) = self.controller.lock()
            && controller
                .reservation
                .is_some_and(|reservation| reservation.id == id)
        {
            controller.reservation = None;
        }
        self.finished = true;
    }
}

impl Drop for PreparedWorkflowTransition {
    fn drop(&mut self) {
        if !self.finished {
            self.release();
        }
    }
}

fn resolve_profiles(
    config: &WorkflowConfig,
    candidates: &[polaris_skills::Skill],
    phases: &[Phase],
) -> Result<BTreeMap<Phase, FrozenWorkflowProfile>, String> {
    phases
        .iter()
        .map(|phase| {
            let profile = polaris_skills::workflow_profile::resolve(
                &config.always,
                config
                    .phase_skills
                    .get(phase)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                candidates,
            )
            .map_err(|error| error.to_string())?;
            let entries = profile
                .entries
                .into_iter()
                .map(|entry| WorkflowSkillV1 {
                    id: entry.id,
                    source: entry.source,
                    body_hash: entry.body_hash,
                })
                .collect();
            Ok((
                *phase,
                FrozenWorkflowProfile {
                    body: profile.body,
                    manifest_hash: profile.manifest_hash,
                    ids: profile.ids,
                    entries,
                },
            ))
        })
        .collect()
}

fn build_profile(
    mut state: WorkflowStateV1,
    frozen: &FrozenWorkflowProfile,
    gates: &WorkflowGatesV1,
) -> Result<WorkflowProfile, String> {
    state.skill_manifest_hash = frozen.manifest_hash.clone();
    state.skills = frozen.entries.clone();
    let verification = match &gates.verification {
        None => "missing",
        Some(evidence) if evidence.stale => "stale",
        Some(evidence) if evidence.passed => "passed",
        Some(_) => "failed",
    };
    let body = format!(
        "{}\nWorkflow focus: {}. Spec approval: {}. Verification: {verification}. External grants: {} (existing tool permission checks still apply).\n",
        frozen.body,
        state.phase,
        if gates.approved_spec_hash.is_some() {
            "recorded"
        } else {
            "missing"
        },
        gates.external_grants.len(),
    );
    let tokens = crate::budget::count_tokens(&body);
    if tokens > WORKFLOW_TOKEN_LIMIT {
        return Err(format!(
            "段階別skillと状態表示が上限を超えました: {tokens}/{WORKFLOW_TOKEN_LIMIT} tokens"
        ));
    }
    Ok(WorkflowProfile {
        state,
        body,
        ids: frozen.ids.clone(),
        tokens,
    })
}

fn changed_skill_ids(previous: &[WorkflowSkillV1], next: &[WorkflowSkillV1]) -> Vec<String> {
    let mut changed = Vec::new();
    for entry in next.iter().chain(previous) {
        if (!next.contains(entry) || !previous.contains(entry)) && !changed.contains(&entry.id) {
            changed.push(entry.id.clone());
        }
    }
    changed
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
            let checkpoint = runtime.begin_turn(&[]).unwrap();
            let mut pending = checkpoint
                .prepare_pending(&WorkflowGatesV1::default())
                .unwrap()
                .unwrap();
            pending.begin_persist().unwrap();
            pending.commit().unwrap();
            drop(checkpoint);
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
    fn adaptive_transition_reserves_then_commits_without_mutating_first() {
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            phase_skills: Phase::ALL
                .into_iter()
                .filter(|phase| *phase != Phase::General)
                .map(|phase| (phase, vec![format!("builtin:{phase}")]))
                .collect(),
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let before = turn.profile(&WorkflowGatesV1::default()).unwrap();
        let mut prepared = turn
            .prepare_transition(
                Phase::Implement,
                before.state.revision,
                &WorkflowGatesV1::default(),
            )
            .unwrap();
        assert_eq!(prepared.previous, before.state);
        assert_eq!(prepared.candidate.state.phase, Phase::Implement);
        assert_eq!(runtime.state(), before.state);
        prepared.begin_persist().unwrap();
        let committed = prepared.commit().unwrap();
        assert_eq!(committed.phase, Phase::Implement);
        assert_eq!(committed.revision, 1);
        assert_eq!(
            turn.profile(&WorkflowGatesV1::default()).unwrap().state,
            committed
        );
    }

    #[test]
    fn manual_request_invalidates_unpublished_model_reservation() {
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::Review, 0, &WorkflowGatesV1::default())
            .unwrap();
        runtime.request_phase(Phase::Verify).unwrap();
        assert!(prepared.begin_persist().is_err());
        let pending = turn
            .prepare_pending(&WorkflowGatesV1::default())
            .unwrap()
            .unwrap();
        assert_eq!(pending.candidate.state.phase, Phase::Verify);
    }

    #[test]
    fn manual_request_during_publish_waits_for_the_next_boundary() {
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::Review, 0, &WorkflowGatesV1::default())
            .unwrap();
        prepared.begin_persist().unwrap();
        runtime.request_phase(Phase::Verify).unwrap();
        assert_eq!(prepared.commit().unwrap().phase, Phase::Review);
        let pending = turn
            .prepare_pending(&WorkflowGatesV1::default())
            .unwrap()
            .unwrap();
        assert_eq!(pending.candidate.state.phase, Phase::Verify);
    }

    #[test]
    fn profile_does_not_apply_pending_phase_without_checkpoint() {
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        runtime.request_phase(Phase::Review).unwrap();
        assert_eq!(
            turn.profile(&WorkflowGatesV1::default())
                .unwrap()
                .state
                .phase,
            Phase::General
        );
        let mut pending = turn
            .prepare_pending(&WorkflowGatesV1::default())
            .unwrap()
            .unwrap();
        assert_eq!(pending.candidate.state.phase, Phase::Review);
        pending.begin_persist().unwrap();
        pending.commit().unwrap();
        assert_eq!(runtime.state().phase, Phase::Review);
    }

    #[test]
    fn manual_pending_refreshes_only_at_the_next_real_turn_boundary() {
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            always: vec!["builtin:workflow-core".into()],
            phase_skills: Phase::ALL
                .into_iter()
                .filter(|phase| *phase != Phase::General)
                .map(|phase| (phase, vec![format!("builtin:{phase}")]))
                .collect(),
            ..Default::default()
        });
        let first = runtime.begin_turn(&[]).unwrap();
        let general_body = first.body.clone();
        runtime.request_phase(Phase::Review).unwrap();
        // A request during a real turn never rewrites that turn's prompt.
        assert_eq!(
            first.profile(&WorkflowGatesV1::default()).unwrap().body,
            general_body
        );
        drop(first);

        let mut next = runtime.begin_turn(&[]).unwrap();
        let mut pending = next
            .prepare_pending(&WorkflowGatesV1::default())
            .unwrap()
            .unwrap();
        pending.begin_persist().unwrap();
        pending.commit().unwrap();
        next.refresh_manual_profile(&WorkflowGatesV1::default())
            .unwrap();
        assert_eq!(next.state.phase, Phase::Review);
        assert!(next.body.contains("builtin:review"));

        runtime.request_phase(Phase::Verify).unwrap();
        assert_eq!(
            next.profile(&WorkflowGatesV1::default()).unwrap().body,
            next.body
        );
    }

    #[test]
    fn pending_same_phase_is_a_durable_noop_and_is_consumed() {
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        runtime.request_phase(Phase::General).unwrap();
        let pending = turn
            .prepare_pending(&WorkflowGatesV1::default())
            .unwrap()
            .unwrap();
        assert!(pending.is_noop());
        let mut pending = pending;
        pending.begin_persist().unwrap();
        pending.commit().unwrap();
        assert!(
            turn.prepare_pending(&WorkflowGatesV1::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn noop_transition_is_invalidated_by_manual_change_before_persisting() {
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::General, 0, &WorkflowGatesV1::default())
            .unwrap();
        assert!(prepared.is_noop());
        runtime.request_phase(Phase::Review).unwrap();
        assert!(prepared.begin_persist().is_err());
        assert_eq!(
            turn.prepare_pending(&WorkflowGatesV1::default())
                .unwrap()
                .unwrap()
                .candidate
                .state
                .phase,
            Phase::Review
        );
    }

    #[test]
    fn noop_transition_queues_manual_change_after_persisting() {
        let runtime = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::General, 0, &WorkflowGatesV1::default())
            .unwrap();
        prepared.begin_persist().unwrap();
        runtime.request_phase(Phase::Review).unwrap();
        assert_eq!(prepared.commit().unwrap().phase, Phase::General);
        assert_eq!(
            turn.prepare_pending(&WorkflowGatesV1::default())
                .unwrap()
                .unwrap()
                .candidate
                .state
                .phase,
            Phase::Review
        );
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
