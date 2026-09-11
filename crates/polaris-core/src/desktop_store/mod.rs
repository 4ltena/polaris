//! v3保存。単一writer、世代公開、要求台帳と復旧を提供する。
//!
//! PrototypeRootは新規TempDir、DesktopRootは明示した私用領域を所有する。認可、provider実行、
//! 実会話移行は呼び出さない。呼出側は要求・台帳照会の前に対象を認可すること。

mod bootstrap;
mod children;
mod disk;
mod memory;
mod model;
mod roles;
mod source_apply;
mod workflow;
mod writer;

pub use bootstrap::{
    BootstrapIdentity, BootstrapProvider, BootstrapTier, ExpectedBootstrapFile,
    ExpectedBootstrapStore, MAX_OWNER_BOOTSTRAP_BYTES, OwnerBootstrapDocument,
    ValidatedOwnerBootstrap, read_owner_bootstrap, validate_owner_configuration,
};
pub use children::SavedChild;
pub use memory::{MemoryResources, MemorySnapshot, SavedMemory, SummaryWork};
pub use model::{
    Acceptance, HistoryGap, InitialState, IntentReceipt, Marker, Operation, Published,
    RequestRecord, RequestResult, RunRecord, Sidecar, StoreError, StoreResult,
};
pub use roles::{SavedRoleBinding, SavedRoleBindings, SavedRuntime, ToolSupport};
pub use source_apply::{
    MAX_SOURCE_APPLY_BYTES, MAX_SOURCE_APPLY_ENTRIES, MAX_SOURCE_APPLY_LEDGER_BYTES,
    MAX_SOURCE_APPLY_RECORDS, SavedSourceApply, SourceApplyCandidate, SourceApplyEntry,
    SourceApplyGuard, SourceApplyIdentity, SourceApplyIdentityProof, SourceApplyPayload,
    SourceApplyResult, SourceApplyVersion,
};
pub use workflow::SavedWorkflow;
pub(crate) use writer::validate_turn_suffix;
pub use writer::{DesktopRoot, PrototypeRoot, Writer};

#[cfg(all(test, unix))]
mod tests;

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
