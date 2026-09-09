//! 共通core ownerへのservice内部入口。実行・回収registryはcoreの一箇所に保持する。
pub(crate) use polaris_core::execution_owner::{CleanupOwner, RunExecution, now_ms};
