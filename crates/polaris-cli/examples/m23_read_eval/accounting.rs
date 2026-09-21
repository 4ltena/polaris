//! M23 model/effort binding for the shared physical-accounting implementation.
#[path = "../eval_support/accounting.rs"]
mod shared;
use polaris_provider::{
    Usage, UsageReport,
    attempts::{AttemptKind, AttemptRecord},
};
use serde_json::Value;
pub(super) use shared::{RowCost, read_metrics};
pub(super) fn assess_row(row: &Value, model: &str, usage: Usage) -> Result<RowCost, String> {
    shared::assess_row(row, model, super::EFFORT, usage)
}
pub(super) fn reconcile(
    attempts: &[AttemptRecord],
    rows: &[Value],
    report: &UsageReport,
) -> Result<Value, String> {
    shared::reconcile(
        attempts,
        rows,
        report,
        super::EFFORT,
        &[
            (AttemptKind::Completion, super::ROOT_MODEL),
            (AttemptKind::Child, super::CHILD_MODEL),
        ],
    )
}
