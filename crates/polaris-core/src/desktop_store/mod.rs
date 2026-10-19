//! v3保存。単一writer、世代公開、要求台帳と復旧を提供する。
//!
//! PrototypeRootは新規TempDir、DesktopRootは明示した私用領域を所有する。認可、provider実行、
//! 実会話移行は呼び出さない。呼出側は要求・台帳照会の前に対象を認可すること。

mod account;
mod artifacts;
mod bootstrap;
mod children;
mod disk;
mod goal;
pub mod hub;
mod memory;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) mod migration;
mod model;
mod roles;
mod session_manager;
mod source_apply;
mod star_names;
mod subagent_control;
mod workflow;
mod writer;

pub use account::SavedAccount;
pub use artifacts::WorkflowArtifactStore;
pub use bootstrap::{
    BootstrapIdentity, BootstrapProvider, BootstrapTier, ExpectedBootstrapFile,
    ExpectedBootstrapStore, MAX_OWNER_BOOTSTRAP_BYTES, OwnerBootstrapDocument,
    ValidatedOwnerBootstrap, read_owner_bootstrap, validate_owner_configuration,
};
pub use children::SavedChild;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use disk::{D27ReadFileV1, StateNextClassificationV1, StateNextInspectV1};
pub use goal::{AppliedInstruction, MAX_INSTRUCTIONS, SavedGoal, SavedInstruction};
pub use hub::{
    ApprovalSummary, Changes, Delta, GoalSummary, HubView, InstructionSummary, ListChange,
    RunSummary, TaskSummary, Verification,
};
pub use memory::{
    MemoryResources, MemorySnapshot, SavedMemory, SavedStructuredResponse, SummaryWork,
};
pub use model::{
    Acceptance, AdmittedRun, HistoryGap, InitialState, IntentReceipt, Marker, Operation, Published,
    RequestRecord, RequestResult, RunRecord, SavedAdmissionBinding, Sidecar, StoreError,
    StoreResult,
};
pub use roles::{
    EFFORTS, SavedRoleBinding, SavedRoleBindings, SavedRuntime, SavedTier, SavedTierBinding,
    SavedTierBindings, ToolSupport,
};
pub use session_manager::{
    MAX_SESSION_MANAGER_COMMITS, MAX_SESSION_MANAGER_STATE_BYTES, SessionManagerStore,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use source_apply::{
    MAX_D27_MARKER_BYTES, SourceApplyPrecommitV1, source_apply_result_capacity_m2a,
};
pub use source_apply::{
    MAX_SOURCE_APPLY_BYTES, MAX_SOURCE_APPLY_ENTRIES, MAX_SOURCE_APPLY_LEDGER_BYTES,
    MAX_SOURCE_APPLY_RECORDS, SavedSourceApply, SourceApplyCandidate, SourceApplyEntry,
    SourceApplyGuard, SourceApplyIdentity, SourceApplyIdentityProof, SourceApplyPayload,
    SourceApplyResult, SourceApplyVersion,
};
pub use subagent_control::{
    SubagentControlBindingV1, SubagentControlOwner, SubagentControlSnapshotV1,
};
pub use workflow::SavedWorkflow;
pub use writer::mcp::{
    MCP_TOOL_OPERATION, McpDisposition, McpPublishedResult, McpTerminalInput,
    McpTerminalPublication, SavedMcpOperation,
};
pub(crate) use writer::mcp::{local_mcp_terminal, mcp_reply_budget};
pub use writer::prompt::{McpPromptIntent, McpPromptPublication, McpPromptResult, SavedMcpPrompt};
pub(crate) use writer::prompt::{local_mcp_prompt_reply_budget, local_mcp_prompt_terminal};
pub use writer::recipe::{
    Current as RecipeCurrent, SCHEMA_VERSION as RECIPE_SCHEMA_VERSION, SavedRecipeIntent,
    SavedRecipeStep, SavedRecipeTerminal, StepOutcome, reusable_steps,
};
pub(crate) use writer::validate_turn_suffix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use writer::{
    D27PublicationKindV1, D27RecoveryInspectV1, D27RecoveryOpenError, D27RecoveryOpenOutcomeV1,
    DesktopCapacityIdentityV1, DesktopLogicalCapacityV1, JoinedD27MarkerRecoveryV1,
    LegacyUnboundIntentDiagnosticV1, SourceApplyConsumedReceiptV1, SourceApplyPrecommitTokenV1,
    SourceApplyResultEvidenceV1,
};
pub use writer::{DesktopRoot, PrototypeRoot, Writer};
pub use writer::{OwnerRequestHistory, RunDetailEntry, RunDetails};

/// Presentation label shared by the native desktop and terminal child list.
pub fn child_display_name(index: usize) -> &'static str {
    star_names::at(index)
}

#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, unix))]
pub(crate) use tests::inject as inject_test_failure;

#[cfg(all(test, not(unix)))]
mod unsupported_tests {
    //! 未保証OSでは成功に見せず、作成入口で明示的に拒否する。
    #[test]
    fn desktop_store_reports_unsupported_platform() {
        assert!(matches!(
            super::PrototypeRoot::new(),
            Err(super::StoreError::UnsupportedPlatform)
        ));
    }
}
