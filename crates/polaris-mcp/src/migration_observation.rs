//! Read-only migration observations and strict, non-authoritative wire codecs.
//! The host owner retains entry exclusion, fresh native stop proof and all
//! mutation/activation authority. A decoded stop record is historical data.
pub use crate::store::ledger_v1::file_store::empty_payload_head;
pub use crate::store::migration_v2::{
    documents::{BaselineDocumentsV2, SourceDocumentsV1, read_baseline_credential},
    inventory::{CompleteTreeProjectionV2, VerifiedTreeCopyV2, VerifiedTreeEntryV2},
    mapped::MappedStagingViewV2,
    namespace::MigrationParentObservationV2,
    paths::*,
    projection::ProjectionTreeV2,
    records::*,
};
