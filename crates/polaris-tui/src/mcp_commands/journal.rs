//! MCP management effects retain the existing irreversible rewind boundary.
use polaris_core::{
    management_effect::{
        ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
        ManagementEffectStoreV1, ManagementInventoryIdV1, ManagementOperationIdV1,
        ManagementPermissionScopeV1, ManagementRewindDispositionV1, ManagementUnknownReasonV1,
        PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1, PreparedManagementEffectV1,
        StartedManagementEffectV1,
    },
    project_mutation::ProjectMutationLeaseV1,
};

pub(super) struct Journal<'a> {
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectMutationLeaseV1,
}

impl Journal<'_> {
    pub fn prepare(
        &self,
        inventory: &str,
        revision: u64,
        request: &[u8],
    ) -> Result<PreparedManagementEffectV1, String> {
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random()
                .map_err(|_| "Cannot create an MCP management operation ID")?,
            inventory: ManagementInventoryIdV1::parse(inventory).map_err(|e| e.to_string())?,
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: None,
                generation: revision,
                revision,
                permission: ManagementPermissionScopeV1::System,
                account_binding: None,
            },
            // Callers supply metadata only. Credential bytes never enter this hash.
            request_hash: ManagementEffectHashV1::digest(request),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        match self
            .effects
            .prepare(
                self.lease,
                self.effects.cursor().map_err(|e| e.to_string())?,
                request,
            )
            .map_err(|e| e.to_string())?
        {
            PrepareManagementEffectOutcomeV1::Prepared(prepared) => Ok(prepared),
            PrepareManagementEffectOutcomeV1::Existing(_) => {
                Err("MCP management operation already exists".into())
            }
        }
    }

    pub fn deny(&self, prepared: &PreparedManagementEffectV1) -> Result<(), String> {
        self.effects
            .deny(self.lease, prepared, ManagementDenialReasonV1::UserDeclined)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn start(
        &self,
        prepared: &PreparedManagementEffectV1,
    ) -> Result<StartedManagementEffectV1, String> {
        self.effects
            .start(self.lease, prepared)
            .map_err(|e| e.to_string())
    }

    pub fn complete(
        &self,
        started: &StartedManagementEffectV1,
        receipt: &[u8],
    ) -> Result<(), String> {
        if self
            .effects
            .complete_started(self.lease, started, ManagementEffectHashV1::digest(receipt))
            .is_err()
        {
            let _ = self.effects.mark_outcome_unknown(
                self.lease,
                started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            );
            return Err(
                "Cannot save the MCP management receipt. Outcome reconciliation is required".into(),
            );
        }
        Ok(())
    }

    pub fn unknown(&self, started: &StartedManagementEffectV1) -> String {
        let _ = self.effects.mark_outcome_unknown(
            self.lease,
            started,
            ManagementUnknownReasonV1::UnclassifiedEffect,
        );
        "Cannot verify the MCP operation outcome. Do not rerun; reconcile management state".into()
    }
}
