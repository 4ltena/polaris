//! Durable, side-effect-free control state for delegated agents.
//!
//! This module deliberately has no Tokio handles, provider calls, or process
//! identifiers.  The owner persists this value before it starts a child and
//! translates the returned actions into runtime work afterwards.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_CHILDREN: usize = 128;
const MAX_TEXT: usize = 512;
const MAX_ACTIVITY: usize = 384;
const MAX_REFS: usize = 32;
const STAR_NAMES: &[&str] = &[
    "Sirius",
    "Canopus",
    "Arcturus",
    "Vega",
    "Capella",
    "Rigel",
    "Procyon",
    "Achernar",
    "Betelgeuse",
    "Hadar",
    "Altair",
    "Acrux",
    "Aldebaran",
    "Antares",
    "Spica",
    "Pollux",
    "Fomalhaut",
    "Deneb",
    "Mimosa",
    "Regulus",
    "Adhara",
    "Castor",
    "Gacrux",
    "Shaula",
    "Bellatrix",
    "Elnath",
    "Miaplacidus",
    "Alnilam",
    "Alnair",
    "Alioth",
    "Dubhe",
    "Mirfak",
];

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ControlError> {
                let value = value.into();
                validate_id(&value)?;
                Ok(Self(value))
            }
        }
    };
}

id_type!(LogicalChildId);
id_type!(ChildAttemptId);
id_type!(RootRunId);
id_type!(RootAttemptId);
id_type!(TaskId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildBindingV1 {
    pub root_run_id: RootRunId,
    pub root_attempt_id: RootAttemptId,
    pub active_task_id: TaskId,
    pub parent: ParentBindingV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParentBindingV1 {
    Root,
    Child { logical_child_id: LogicalChildId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitySnapshotV1 {
    pub sandbox: SandboxCapabilityV1,
    pub write_roots: Vec<String>,
    pub tool_names: Vec<String>,
    pub approval: ApprovalCapabilityV1,
    pub mcp: McpCapabilityV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxCapabilityV1 {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalCapabilityV1 {
    Never,
    OnRequest,
    TrustedOwnerOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCapabilityV1 {
    None,
    ReadSnapshotsOnly,
}

impl CapabilitySnapshotV1 {
    pub fn does_not_escalate_from(&self, parent: &Self) -> bool {
        sandbox_rank(self.sandbox) <= sandbox_rank(parent.sandbox)
            && approval_rank(self.approval) <= approval_rank(parent.approval)
            && mcp_rank(self.mcp) <= mcp_rank(parent.mcp)
            && subset(&self.write_roots, &parent.write_roots)
            && subset(&self.tool_names, &parent.tool_names)
    }

    fn validate(&self) -> Result<(), ControlError> {
        validate_names(&self.write_roots, MAX_REFS)?;
        validate_names(&self.tool_names, MAX_REFS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStateV1 {
    Queued,
    Running,
    Waiting { reason: WaitReasonV1 },
    Cancelling,
    Succeeded,
    Failed { class: FailureClassV1 },
    Cancelled,
    OutcomeUnknown,
}

impl ChildStateV1 {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed { .. } | Self::Cancelled | Self::OutcomeUnknown
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReasonV1 {
    Approval,
    Input,
    Dependency,
    ExternalService,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClassV1 {
    SafeTransient,
    Input,
    Authentication,
    Permission,
    Build,
    Test,
    Deterministic,
    ExternalWriteOutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeActivityV1 {
    pub summary: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UsageAttributionV1 {
    Known {
        input_tokens: u64,
        output_tokens: u64,
    },
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildAttemptV1 {
    pub attempt_id: ChildAttemptId,
    pub attempt_number: u8,
    pub restart_of: Option<ChildAttemptId>,
    /// The exact reduced capability set accepted for the single retry. It is
    /// retained so a restored control plane can re-check the retry boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_capabilities: Option<CapabilitySnapshotV1>,
    pub state: ChildStateV1,
    pub activity: Option<SafeActivityV1>,
    pub usage: UsageAttributionV1,
    pub terminal_reason: Option<String>,
    pub result_refs: Vec<String>,
    pub artifact_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalChildV1 {
    pub logical_child_id: LogicalChildId,
    pub display_name: String,
    pub binding: ChildBindingV1,
    pub role: String,
    pub tier: String,
    pub model: String,
    pub effort: String,
    pub capabilities: CapabilitySnapshotV1,
    pub attempts: Vec<ChildAttemptV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildControlPlaneV1 {
    pub schema_version: u8,
    pub children: Vec<LogicalChildV1>,
    pub cursor: u64,
}

impl Default for ChildControlPlaneV1 {
    fn default() -> Self {
        Self {
            schema_version: 1,
            children: Vec::new(),
            cursor: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterChildV1 {
    pub logical_child_id: LogicalChildId,
    pub attempt_id: ChildAttemptId,
    pub binding: ChildBindingV1,
    pub role: String,
    pub tier: String,
    pub model: String,
    pub effort: String,
    pub capabilities: CapabilitySnapshotV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterResultV1 {
    Registered { display_name: String, cursor: u64 },
    AlreadyRegistered { display_name: String, cursor: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelResultV1 {
    Requested { cursor: u64 },
    AlreadyRequested { cursor: u64 },
    AlreadyTerminal { state: ChildStateV1, cursor: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishResultV1 {
    Finished { cursor: u64 },
    AlreadyFinished { cursor: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelPlanV1 {
    pub root_run_id: RootRunId,
    pub root_attempt_id: RootAttemptId,
    pub targets: Vec<ChildAttemptRefV1>,
    pub cursor: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildAttemptRefV1 {
    pub logical_child_id: LogicalChildId,
    pub attempt_id: ChildAttemptId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitSnapshotV1 {
    pub cursor: u64,
    pub children: Vec<LogicalChildV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageProjectionV1 {
    pub child_usage: UsageAttributionV1,
    /// Child usage is already included in root provider accounting.  Consumers
    /// must display it as attribution, never add it to the root total again.
    pub root_total_increment: u64,
}

impl ChildControlPlaneV1 {
    pub fn register_before_start(
        &mut self,
        request: RegisterChildV1,
    ) -> Result<RegisterResultV1, ControlError> {
        self.validate()?;
        validate_register(&request)?;
        if let Some(existing) = self
            .children
            .iter()
            .find(|child| child.logical_child_id == request.logical_child_id)
        {
            let attempt = existing.attempts.first().ok_or(ControlError::Corrupt)?;
            if attempt.attempt_id == request.attempt_id
                && existing.binding == request.binding
                && existing.role == request.role
                && existing.tier == request.tier
                && existing.model == request.model
                && existing.effort == request.effort
                && existing.capabilities == request.capabilities
            {
                return Ok(RegisterResultV1::AlreadyRegistered {
                    display_name: existing.display_name.clone(),
                    cursor: self.cursor,
                });
            }
            return Err(ControlError::Conflict);
        }
        if self.children.len() >= MAX_CHILDREN {
            return Err(ControlError::Limit);
        }
        self.ensure_parent(
            &request.binding,
            &request.capabilities,
            &request.model,
            &request.tier,
        )?;
        let display_name = display_name_for(self.children.len());
        self.children.push(LogicalChildV1 {
            logical_child_id: request.logical_child_id,
            display_name: display_name.clone(),
            binding: request.binding,
            role: request.role,
            tier: request.tier,
            model: request.model,
            effort: request.effort,
            capabilities: request.capabilities,
            attempts: vec![ChildAttemptV1 {
                attempt_id: request.attempt_id,
                attempt_number: 0,
                restart_of: None,
                retry_capabilities: None,
                state: ChildStateV1::Queued,
                activity: None,
                usage: UsageAttributionV1::Missing,
                terminal_reason: None,
                result_refs: Vec::new(),
                artifact_refs: Vec::new(),
            }],
        });
        let cursor = self.bump();
        Ok(RegisterResultV1::Registered {
            display_name,
            cursor,
        })
    }

    pub fn mark_running(
        &mut self,
        reference: &ChildAttemptRefV1,
        binding: &ChildBindingV1,
    ) -> Result<u64, ControlError> {
        let attempt = self.exact_attempt_mut(reference, binding)?;
        match &attempt.state {
            ChildStateV1::Queued | ChildStateV1::Waiting { .. } => {
                attempt.state = ChildStateV1::Running;
                Ok(self.bump())
            }
            ChildStateV1::Running => Ok(self.cursor),
            _ => Err(ControlError::InvalidTransition),
        }
    }

    pub fn mark_activity(
        &mut self,
        reference: &ChildAttemptRefV1,
        binding: &ChildBindingV1,
        activity: SafeActivityV1,
    ) -> Result<u64, ControlError> {
        if activity.summary.is_empty() || activity.summary.len() > MAX_ACTIVITY {
            return Err(ControlError::Invalid);
        }
        let attempt = self.exact_attempt_mut(reference, binding)?;
        if attempt.state.is_terminal() {
            return Err(ControlError::InvalidTransition);
        }
        if attempt
            .activity
            .as_ref()
            .is_some_and(|current| current.sequence > activity.sequence)
        {
            return Ok(self.cursor);
        }
        attempt.activity = Some(activity);
        Ok(self.bump())
    }

    pub fn cancel_requested(
        &mut self,
        reference: &ChildAttemptRefV1,
        binding: &ChildBindingV1,
    ) -> Result<CancelResultV1, ControlError> {
        let attempt = self.exact_attempt_mut(reference, binding)?;
        if attempt.state.is_terminal() {
            return Ok(CancelResultV1::AlreadyTerminal {
                state: attempt.state.clone(),
                cursor: self.cursor,
            });
        }
        if matches!(&attempt.state, ChildStateV1::Cancelling) {
            return Ok(CancelResultV1::AlreadyRequested {
                cursor: self.cursor,
            });
        }
        attempt.state = ChildStateV1::Cancelling;
        attempt.activity = None;
        let cursor = self.bump();
        Ok(CancelResultV1::Requested { cursor })
    }

    pub fn cancel_confirmed(
        &mut self,
        reference: &ChildAttemptRefV1,
        binding: &ChildBindingV1,
        reason: impl Into<String>,
    ) -> Result<FinishResultV1, ControlError> {
        self.finish(
            reference,
            binding,
            ChildStateV1::Cancelled,
            reason.into(),
            UsageAttributionV1::Missing,
            Vec::new(),
            Vec::new(),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "terminal attribution fields are separately validated at this control boundary"
    )]
    pub fn finish(
        &mut self,
        reference: &ChildAttemptRefV1,
        binding: &ChildBindingV1,
        terminal: ChildStateV1,
        reason: String,
        usage: UsageAttributionV1,
        result_refs: Vec<String>,
        artifact_refs: Vec<String>,
    ) -> Result<FinishResultV1, ControlError> {
        if !terminal.is_terminal()
            || reason.len() > MAX_TEXT
            || result_refs.len() > MAX_REFS
            || artifact_refs.len() > MAX_REFS
        {
            return Err(ControlError::Invalid);
        }
        validate_names(&result_refs, MAX_REFS)?;
        validate_names(&artifact_refs, MAX_REFS)?;
        let attempt = self.exact_attempt_mut(reference, binding)?;
        if attempt.state.is_terminal() {
            return if attempt.state == terminal {
                Ok(FinishResultV1::AlreadyFinished {
                    cursor: self.cursor,
                })
            } else {
                Err(ControlError::Conflict)
            };
        }
        if matches!(&attempt.state, ChildStateV1::Cancelling)
            && !matches!(
                terminal,
                ChildStateV1::Cancelled | ChildStateV1::OutcomeUnknown
            )
        {
            return Err(ControlError::InvalidTransition);
        }
        attempt.state = terminal;
        attempt.activity = None;
        attempt.terminal_reason = Some(reason);
        attempt.usage = usage;
        attempt.result_refs = result_refs;
        attempt.artifact_refs = artifact_refs;
        let cursor = self.bump();
        Ok(FinishResultV1::Finished { cursor })
    }

    pub fn outcome_unknown_on_restore(&mut self) -> u64 {
        let mut changed = false;
        for child in &mut self.children {
            for attempt in &mut child.attempts {
                if !attempt.state.is_terminal() {
                    attempt.state = ChildStateV1::OutcomeUnknown;
                    attempt.activity = None;
                    attempt.terminal_reason =
                        Some("owner restored before terminal observation".into());
                    changed = true;
                }
            }
        }
        if changed { self.bump() } else { self.cursor }
    }

    pub fn restart_as_new_attempt(
        &mut self,
        logical_child_id: &LogicalChildId,
        binding: &ChildBindingV1,
        new_attempt_id: ChildAttemptId,
        capabilities: CapabilitySnapshotV1,
    ) -> Result<ChildAttemptRefV1, ControlError> {
        capabilities.validate()?;
        if self
            .children
            .iter()
            .flat_map(|entry| &entry.attempts)
            .any(|attempt| attempt.attempt_id == new_attempt_id)
        {
            return Err(ControlError::Conflict);
        }
        let child = self
            .children
            .iter_mut()
            .find(|child| &child.logical_child_id == logical_child_id)
            .ok_or(ControlError::NotFound)?;
        if child.binding != *binding {
            return Err(ControlError::WrongBinding);
        }
        let old = child.attempts.last().ok_or(ControlError::Corrupt)?;
        match &old.state {
            ChildStateV1::Failed {
                class: FailureClassV1::SafeTransient,
            } => {}
            _ => return Err(ControlError::RestartDenied),
        };
        if child.attempts.len() > 1 || !capabilities.does_not_escalate_from(&child.capabilities) {
            return Err(ControlError::RestartDenied);
        }
        let old_id = old.attempt_id.clone();
        child.attempts.push(ChildAttemptV1 {
            attempt_id: new_attempt_id.clone(),
            attempt_number: 1,
            restart_of: Some(old_id),
            retry_capabilities: Some(capabilities),
            state: ChildStateV1::Queued,
            activity: None,
            usage: UsageAttributionV1::Missing,
            terminal_reason: None,
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
        });
        self.bump();
        Ok(ChildAttemptRefV1 {
            logical_child_id: logical_child_id.clone(),
            attempt_id: new_attempt_id,
        })
    }

    pub fn cancel_plan_for_root(
        &self,
        root_run_id: &RootRunId,
        root_attempt_id: &RootAttemptId,
    ) -> CancelPlanV1 {
        let targets = self
            .children
            .iter()
            .filter(|child| {
                child.binding.root_run_id == *root_run_id
                    && child.binding.root_attempt_id == *root_attempt_id
            })
            .filter_map(|child| {
                child
                    .attempts
                    .last()
                    .filter(|attempt| !attempt.state.is_terminal())
                    .map(|attempt| ChildAttemptRefV1 {
                        logical_child_id: child.logical_child_id.clone(),
                        attempt_id: attempt.attempt_id.clone(),
                    })
            })
            .collect();
        CancelPlanV1 {
            root_run_id: root_run_id.clone(),
            root_attempt_id: root_attempt_id.clone(),
            targets,
            cursor: self.cursor,
        }
    }

    pub fn wait_snapshot(&self, after_cursor: Option<u64>) -> Option<WaitSnapshotV1> {
        if after_cursor.is_some_and(|cursor| cursor >= self.cursor) {
            None
        } else {
            Some(WaitSnapshotV1 {
                cursor: self.cursor,
                children: self.children.clone(),
            })
        }
    }

    pub fn usage_projection(
        &self,
        reference: &ChildAttemptRefV1,
        binding: &ChildBindingV1,
    ) -> Result<UsageProjectionV1, ControlError> {
        let attempt = self.exact_attempt(reference, binding)?;
        Ok(UsageProjectionV1 {
            child_usage: attempt.usage.clone(),
            root_total_increment: 0,
        })
    }

    pub fn validate(&self) -> Result<(), ControlError> {
        if self.schema_version != 1 || self.children.len() > MAX_CHILDREN {
            return Err(ControlError::Invalid);
        }
        let mut logical_ids = BTreeSet::new();
        let mut attempts = BTreeSet::new();
        let mut names = BTreeSet::new();
        for child in &self.children {
            validate_id(&child.logical_child_id.0)?;
            validate_binding(&child.binding)?;
            if !logical_ids.insert(&child.logical_child_id)
                || !names.insert(&child.display_name)
                || child.display_name != display_name_for(logical_ids.len() - 1)
                || child.attempts.is_empty()
                || child.attempts.len() > 2
            {
                return Err(ControlError::Invalid);
            }
            validate_id(&child.display_name)?;
            validate_texts(&[&child.role, &child.tier, &child.model, &child.effort])?;
            child.capabilities.validate()?;
            if let ParentBindingV1::Child { logical_child_id } = &child.binding.parent {
                let parent = self
                    .children
                    .iter()
                    .find(|candidate| candidate.logical_child_id == *logical_child_id)
                    .ok_or(ControlError::Invalid)?;
                if parent.binding.root_run_id != child.binding.root_run_id
                    || parent.binding.root_attempt_id != child.binding.root_attempt_id
                    || parent.binding.active_task_id != child.binding.active_task_id
                {
                    return Err(ControlError::WrongBinding);
                }
                if !child
                    .capabilities
                    .does_not_escalate_from(&parent.capabilities)
                    || !model_does_not_escalate(&child.model, &parent.model)
                    || !tier_does_not_escalate(&child.tier, &parent.tier)
                {
                    return Err(ControlError::Invalid);
                }
                let mut seen = BTreeSet::new();
                let mut current = logical_child_id;
                while let Some(node) = self
                    .children
                    .iter()
                    .find(|entry| entry.logical_child_id == *current)
                {
                    if !seen.insert(current) {
                        return Err(ControlError::Invalid);
                    }
                    match &node.binding.parent {
                        ParentBindingV1::Root => break,
                        ParentBindingV1::Child { logical_child_id } => current = logical_child_id,
                    }
                }
            }
            for (index, attempt) in child.attempts.iter().enumerate() {
                validate_id(&attempt.attempt_id.0)?;
                if !attempts.insert(&attempt.attempt_id)
                    || attempt.attempt_number as usize != index
                    || (index == 0 && attempt.restart_of.is_some())
                    || (index == 0 && attempt.retry_capabilities.is_some())
                    || (index == 1
                        && attempt.restart_of.as_ref() != Some(&child.attempts[0].attempt_id))
                    || attempt.result_refs.len() > MAX_REFS
                    || attempt.artifact_refs.len() > MAX_REFS
                {
                    return Err(ControlError::Invalid);
                }
                if index == 1 {
                    let predecessor = &child.attempts[0];
                    if !matches!(
                        predecessor.state,
                        ChildStateV1::Failed {
                            class: FailureClassV1::SafeTransient
                        }
                    ) || attempt
                        .retry_capabilities
                        .as_ref()
                        .is_none_or(|retry| !retry.does_not_escalate_from(&child.capabilities))
                    {
                        return Err(ControlError::Invalid);
                    }
                }
                validate_names(&attempt.result_refs, MAX_REFS)?;
                validate_names(&attempt.artifact_refs, MAX_REFS)?;
                if attempt
                    .activity
                    .as_ref()
                    .is_some_and(|a| a.summary.is_empty() || a.summary.len() > MAX_ACTIVITY)
                    || (attempt.state.is_terminal() && attempt.activity.is_some())
                {
                    return Err(ControlError::Invalid);
                }
            }
        }
        Ok(())
    }

    fn exact_attempt<'a>(
        &'a self,
        reference: &ChildAttemptRefV1,
        binding: &ChildBindingV1,
    ) -> Result<&'a ChildAttemptV1, ControlError> {
        let child = self
            .children
            .iter()
            .find(|child| child.logical_child_id == reference.logical_child_id)
            .ok_or(ControlError::NotFound)?;
        if child.binding != *binding {
            return Err(ControlError::WrongBinding);
        }
        child
            .attempts
            .iter()
            .find(|attempt| attempt.attempt_id == reference.attempt_id)
            .ok_or(ControlError::NotFound)
    }
    fn exact_attempt_mut<'a>(
        &'a mut self,
        reference: &ChildAttemptRefV1,
        binding: &ChildBindingV1,
    ) -> Result<&'a mut ChildAttemptV1, ControlError> {
        let child = self
            .children
            .iter_mut()
            .find(|child| child.logical_child_id == reference.logical_child_id)
            .ok_or(ControlError::NotFound)?;
        if child.binding != *binding {
            return Err(ControlError::WrongBinding);
        }
        let attempt = child
            .attempts
            .iter_mut()
            .find(|attempt| attempt.attempt_id == reference.attempt_id)
            .ok_or(ControlError::NotFound)?;
        Ok(attempt)
    }
    fn ensure_parent(
        &self,
        binding: &ChildBindingV1,
        capabilities: &CapabilitySnapshotV1,
        model: &str,
        tier: &str,
    ) -> Result<(), ControlError> {
        if let ParentBindingV1::Child { logical_child_id } = &binding.parent {
            let parent = self
                .children
                .iter()
                .find(|child| child.logical_child_id == *logical_child_id)
                .ok_or(ControlError::NotFound)?;
            if parent.binding.root_run_id != binding.root_run_id
                || parent.binding.root_attempt_id != binding.root_attempt_id
                || parent.binding.active_task_id != binding.active_task_id
            {
                return Err(ControlError::WrongBinding);
            }
            if !capabilities.does_not_escalate_from(&parent.capabilities)
                || !model_does_not_escalate(model, &parent.model)
                || !tier_does_not_escalate(tier, &parent.tier)
            {
                return Err(ControlError::Invalid);
            }
        }
        Ok(())
    }
    fn bump(&mut self) -> u64 {
        self.cursor = self.cursor.saturating_add(1);
        self.cursor
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ControlError {
    #[error("unknown child or attempt")]
    NotFound,
    #[error("wrong root, task, or parent binding")]
    WrongBinding,
    #[error("conflicting idempotent operation")]
    Conflict,
    #[error("invalid state transition")]
    InvalidTransition,
    #[error("restart is unsafe or exhausted")]
    RestartDenied,
    #[error("bounded control-plane limit exceeded")]
    Limit,
    #[error("invalid control-plane value")]
    Invalid,
    #[error("corrupt control-plane state")]
    Corrupt,
}

fn validate_register(request: &RegisterChildV1) -> Result<(), ControlError> {
    validate_id(&request.logical_child_id.0)?;
    validate_id(&request.attempt_id.0)?;
    validate_binding(&request.binding)?;
    validate_texts(&[
        &request.role,
        &request.tier,
        &request.model,
        &request.effort,
    ])?;
    request.capabilities.validate()
}

fn validate_binding(binding: &ChildBindingV1) -> Result<(), ControlError> {
    validate_id(&binding.root_run_id.0)?;
    validate_id(&binding.root_attempt_id.0)?;
    validate_id(&binding.active_task_id.0)?;
    if let ParentBindingV1::Child { logical_child_id } = &binding.parent {
        validate_id(&logical_child_id.0)?;
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), ControlError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(ControlError::Invalid)
    } else {
        Ok(())
    }
}
fn validate_texts(values: &[&String]) -> Result<(), ControlError> {
    if values
        .iter()
        .any(|value| value.is_empty() || value.len() > MAX_TEXT)
    {
        Err(ControlError::Invalid)
    } else {
        Ok(())
    }
}
fn validate_names(values: &[String], max: usize) -> Result<(), ControlError> {
    if values.len() > max
        || values
            .iter()
            .any(|value| value.is_empty() || value.len() > MAX_TEXT)
        || values
            .iter()
            .enumerate()
            .any(|(index, value)| values[..index].contains(value))
    {
        Err(ControlError::Invalid)
    } else {
        Ok(())
    }
}
fn display_name_for(index: usize) -> String {
    let star = STAR_NAMES[index % STAR_NAMES.len()];
    let wave = index / STAR_NAMES.len();
    if wave == 0 {
        star.into()
    } else {
        format!("{star}-{wave}")
    }
}
fn subset(child: &[String], parent: &[String]) -> bool {
    child.iter().all(|value| parent.contains(value))
}
fn sandbox_rank(value: SandboxCapabilityV1) -> u8 {
    match value {
        SandboxCapabilityV1::ReadOnly => 0,
        SandboxCapabilityV1::WorkspaceWrite => 1,
        SandboxCapabilityV1::FullAccess => 2,
    }
}
fn approval_rank(value: ApprovalCapabilityV1) -> u8 {
    match value {
        ApprovalCapabilityV1::Never => 0,
        ApprovalCapabilityV1::OnRequest => 1,
        ApprovalCapabilityV1::TrustedOwnerOnly => 2,
    }
}
fn mcp_rank(value: McpCapabilityV1) -> u8 {
    match value {
        McpCapabilityV1::None => 0,
        McpCapabilityV1::ReadSnapshotsOnly => 1,
    }
}

/// Unknown model and tier names are never ordered optimistically. A nested
/// child may retain an unknown value exactly, but cannot use a different one.
fn model_does_not_escalate(child: &str, parent: &str) -> bool {
    match (model_rank(child), model_rank(parent)) {
        (Some(child), Some(parent)) => child <= parent,
        _ => child == parent,
    }
}

fn tier_does_not_escalate(child: &str, parent: &str) -> bool {
    match (tier_rank(child), tier_rank(parent)) {
        (Some(child), Some(parent)) => child <= parent,
        _ => child == parent,
    }
}

fn model_rank(value: &str) -> Option<u8> {
    match value {
        "local" => Some(0),
        "luna" | "gpt-5.6-luna" => Some(1),
        "terra" | "gpt-5.6-terra" => Some(2),
        "sol" | "gpt-5.6-sol" => Some(3),
        "astra" | "gpt-6-astra" => Some(4),
        _ => None,
    }
}

fn tier_rank(value: &str) -> Option<u8> {
    match value {
        "dwarf" => Some(0),
        "subgiant" => Some(1),
        "giant" => Some(2),
        "supergiant" => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(value: &str) -> String {
        value.into()
    }
    fn binding(task: &str) -> ChildBindingV1 {
        ChildBindingV1 {
            root_run_id: RootRunId::new("root").unwrap(),
            root_attempt_id: RootAttemptId::new("root-a").unwrap(),
            active_task_id: TaskId::new(task).unwrap(),
            parent: ParentBindingV1::Root,
        }
    }
    fn caps() -> CapabilitySnapshotV1 {
        CapabilitySnapshotV1 {
            sandbox: SandboxCapabilityV1::WorkspaceWrite,
            write_roots: vec![id("project")],
            tool_names: vec![id("read"), id("write")],
            approval: ApprovalCapabilityV1::Never,
            mcp: McpCapabilityV1::ReadSnapshotsOnly,
        }
    }
    fn request(logical: &str, attempt: &str, task: &str) -> RegisterChildV1 {
        RegisterChildV1 {
            logical_child_id: LogicalChildId::new(logical).unwrap(),
            attempt_id: ChildAttemptId::new(attempt).unwrap(),
            binding: binding(task),
            role: id("reviewer"),
            tier: id("subgiant"),
            model: id("luna"),
            effort: id("medium"),
            capabilities: caps(),
        }
    }
    fn reference(logical: &str, attempt: &str) -> ChildAttemptRefV1 {
        ChildAttemptRefV1 {
            logical_child_id: LogicalChildId::new(logical).unwrap(),
            attempt_id: ChildAttemptId::new(attempt).unwrap(),
        }
    }

    fn child_binding(task: &str, parent: &str) -> ChildBindingV1 {
        ChildBindingV1 {
            root_run_id: RootRunId::new("root").unwrap(),
            root_attempt_id: RootAttemptId::new("root-a").unwrap(),
            active_task_id: TaskId::new(task).unwrap(),
            parent: ParentBindingV1::Child {
                logical_child_id: LogicalChildId::new(parent).unwrap(),
            },
        }
    }

    fn nested_request(
        logical: &str,
        attempt: &str,
        parent: &str,
        capabilities: CapabilitySnapshotV1,
        model: &str,
        tier: &str,
    ) -> RegisterChildV1 {
        RegisterChildV1 {
            logical_child_id: LogicalChildId::new(logical).unwrap(),
            attempt_id: ChildAttemptId::new(attempt).unwrap(),
            binding: child_binding("task", parent),
            role: id("file-reader"),
            tier: id(tier),
            model: id(model),
            effort: id("medium"),
            capabilities,
        }
    }

    fn restricted_caps() -> CapabilitySnapshotV1 {
        CapabilitySnapshotV1 {
            sandbox: SandboxCapabilityV1::ReadOnly,
            write_roots: vec![],
            tool_names: vec![id("read")],
            approval: ApprovalCapabilityV1::Never,
            mcp: McpCapabilityV1::None,
        }
    }

    #[test]
    fn parallel_and_multiwave_names_are_unique() {
        let mut plane = ChildControlPlaneV1::default();
        for n in 0..33 {
            plane
                .register_before_start(request(&format!("c{n}"), &format!("a{n}"), "task"))
                .unwrap();
        }
        assert_eq!(
            plane
                .children
                .iter()
                .map(|c| &c.display_name)
                .collect::<BTreeSet<_>>()
                .len(),
            33
        );
        assert_eq!(plane.children[0].display_name, "Sirius");
        assert_eq!(plane.children[32].display_name, "Sirius-1");
    }
    #[test]
    fn exact_cancel_only_changes_one_child() {
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(request("one", "a1", "task"))
            .unwrap();
        plane
            .register_before_start(request("two", "a2", "task"))
            .unwrap();
        plane
            .cancel_requested(&reference("one", "a1"), &binding("task"))
            .unwrap();
        assert!(matches!(
            plane.children[0].attempts[0].state,
            ChildStateV1::Cancelling
        ));
        assert!(matches!(
            plane.children[1].attempts[0].state,
            ChildStateV1::Queued
        ));
    }
    #[test]
    fn cancel_finish_race_is_idempotent() {
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(request("one", "a1", "task"))
            .unwrap();
        let r = reference("one", "a1");
        plane.cancel_requested(&r, &binding("task")).unwrap();
        plane
            .cancel_confirmed(&r, &binding("task"), "user")
            .unwrap();
        assert!(matches!(
            plane.cancel_requested(&r, &binding("task")).unwrap(),
            CancelResultV1::AlreadyTerminal { .. }
        ));
        assert!(matches!(
            plane
                .cancel_confirmed(&r, &binding("task"), "user")
                .unwrap(),
            FinishResultV1::AlreadyFinished { .. }
        ));
    }

    #[test]
    fn cancellation_cannot_race_into_success_or_failure() {
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(request("one", "a1", "task"))
            .unwrap();
        let r = reference("one", "a1");
        plane.cancel_requested(&r, &binding("task")).unwrap();
        for terminal in [
            ChildStateV1::Succeeded,
            ChildStateV1::Failed {
                class: FailureClassV1::SafeTransient,
            },
        ] {
            assert_eq!(
                plane
                    .finish(
                        &r,
                        &binding("task"),
                        terminal,
                        "late completion".into(),
                        UsageAttributionV1::Missing,
                        vec![],
                        vec![],
                    )
                    .unwrap_err(),
                ControlError::InvalidTransition
            );
        }
        plane
            .finish(
                &r,
                &binding("task"),
                ChildStateV1::OutcomeUnknown,
                "transport lost".into(),
                UsageAttributionV1::Missing,
                vec![],
                vec![],
            )
            .unwrap();
    }

    #[test]
    fn nested_children_cannot_escalate_capabilities_model_or_tier() {
        let mut plane = ChildControlPlaneV1::default();
        let mut parent = request("parent", "p1", "task");
        parent.model = id("astra");
        parent.tier = id("supergiant");
        plane.register_before_start(parent).unwrap();
        plane
            .register_before_start(nested_request(
                "child",
                "c1",
                "parent",
                restricted_caps(),
                "luna",
                "dwarf",
            ))
            .unwrap();

        let mut sandbox = restricted_caps();
        sandbox.sandbox = SandboxCapabilityV1::FullAccess;
        assert_eq!(
            plane
                .register_before_start(nested_request(
                    "sandbox", "c2", "parent", sandbox, "luna", "dwarf"
                ))
                .unwrap_err(),
            ControlError::Invalid
        );
        assert_eq!(
            plane
                .register_before_start(nested_request(
                    "model",
                    "c3",
                    "parent",
                    restricted_caps(),
                    "unknown-model",
                    "dwarf"
                ))
                .unwrap_err(),
            ControlError::Invalid
        );
        assert_eq!(
            plane
                .register_before_start(nested_request(
                    "tier",
                    "c4",
                    "parent",
                    restricted_caps(),
                    "luna",
                    "unknown-tier"
                ))
                .unwrap_err(),
            ControlError::Invalid
        );
    }
    #[test]
    fn restore_unknown_never_autoresends() {
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(request("one", "a1", "task"))
            .unwrap();
        plane.outcome_unknown_on_restore();
        assert_eq!(
            plane
                .restart_as_new_attempt(
                    &LogicalChildId::new("one").unwrap(),
                    &binding("task"),
                    ChildAttemptId::new("a2").unwrap(),
                    caps()
                )
                .unwrap_err(),
            ControlError::RestartDenied
        );
    }
    #[test]
    fn restart_caps_and_limit_are_enforced() {
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(request("one", "a1", "task"))
            .unwrap();
        let r = reference("one", "a1");
        plane
            .finish(
                &r,
                &binding("task"),
                ChildStateV1::Failed {
                    class: FailureClassV1::SafeTransient,
                },
                "network".into(),
                UsageAttributionV1::Missing,
                vec![],
                vec![],
            )
            .unwrap();
        let mut escalated = caps();
        escalated.sandbox = SandboxCapabilityV1::FullAccess;
        assert_eq!(
            plane
                .restart_as_new_attempt(
                    &LogicalChildId::new("one").unwrap(),
                    &binding("task"),
                    ChildAttemptId::new("a2").unwrap(),
                    escalated
                )
                .unwrap_err(),
            ControlError::RestartDenied
        );
        plane
            .restart_as_new_attempt(
                &LogicalChildId::new("one").unwrap(),
                &binding("task"),
                ChildAttemptId::new("a2").unwrap(),
                caps(),
            )
            .unwrap();
        assert_eq!(
            plane
                .restart_as_new_attempt(
                    &LogicalChildId::new("one").unwrap(),
                    &binding("task"),
                    ChildAttemptId::new("a3").unwrap(),
                    caps()
                )
                .unwrap_err(),
            ControlError::RestartDenied
        );
    }

    #[test]
    fn restored_retry_requires_safe_transient_predecessor_and_reduced_capabilities() {
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(request("one", "a1", "task"))
            .unwrap();
        let r = reference("one", "a1");
        plane
            .finish(
                &r,
                &binding("task"),
                ChildStateV1::Failed {
                    class: FailureClassV1::SafeTransient,
                },
                "network".into(),
                UsageAttributionV1::Missing,
                vec![],
                vec![],
            )
            .unwrap();
        plane
            .restart_as_new_attempt(
                &LogicalChildId::new("one").unwrap(),
                &binding("task"),
                ChildAttemptId::new("a2").unwrap(),
                restricted_caps(),
            )
            .unwrap();
        assert!(plane.validate().is_ok());

        let mut bad_predecessor = plane.clone();
        bad_predecessor.children[0].attempts[0].state = ChildStateV1::Succeeded;
        assert_eq!(bad_predecessor.validate(), Err(ControlError::Invalid));

        let mut bad_retry = plane;
        bad_retry.children[0].attempts[1].retry_capabilities = Some(CapabilitySnapshotV1 {
            sandbox: SandboxCapabilityV1::FullAccess,
            write_roots: vec![id("project")],
            tool_names: vec![id("read"), id("write")],
            approval: ApprovalCapabilityV1::Never,
            mcp: McpCapabilityV1::ReadSnapshotsOnly,
        });
        assert_eq!(bad_retry.validate(), Err(ControlError::Invalid));
    }
    #[test]
    fn wrong_binding_is_rejected() {
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(request("one", "a1", "task"))
            .unwrap();
        assert_eq!(
            plane
                .mark_running(&reference("one", "a1"), &binding("other"))
                .unwrap_err(),
            ControlError::WrongBinding
        );
    }
    #[test]
    fn child_usage_never_increments_root_total() {
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(request("one", "a1", "task"))
            .unwrap();
        let r = reference("one", "a1");
        plane
            .finish(
                &r,
                &binding("task"),
                ChildStateV1::Succeeded,
                "ok".into(),
                UsageAttributionV1::Known {
                    input_tokens: 2,
                    output_tokens: 3,
                },
                vec![],
                vec![],
            )
            .unwrap();
        let projection = plane.usage_projection(&r, &binding("task")).unwrap();
        assert_eq!(projection.root_total_increment, 0);
        assert!(matches!(
            projection.child_usage,
            UsageAttributionV1::Known {
                input_tokens: 2,
                output_tokens: 3
            }
        ));
    }
}
