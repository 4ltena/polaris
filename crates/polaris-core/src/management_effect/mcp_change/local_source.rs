//! C28 local-source publication followed by C27 registry replacement.
//!
//! These are explicit local-source variants of the existing inventory rows:
//! C28 copies an already present, descriptor-bound source without download or
//! package scripts; C27 changes only the matching registry transport while
//! retaining every project, capability, timeout and secret reference.

use super::*;
use crate::mcp::configure::{UnityLocalV2Preparation, UnityLocalV2Receipt};
use std::{path::Path, str::FromStr};

#[cfg(test)]
thread_local! {
    static LOCAL_SOURCE_FAULT: std::cell::Cell<Option<&'static str>> = const {
        std::cell::Cell::new(None)
    };
    static LOCAL_SOURCE_BEFORE_SAVE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn set_local_source_fault(point: Option<&'static str>) {
    LOCAL_SOURCE_FAULT.with(|current| current.set(point));
}

#[cfg(test)]
pub(crate) fn set_local_source_before_save_hook(hook: Option<Box<dyn FnOnce()>>) {
    LOCAL_SOURCE_BEFORE_SAVE.with(|current| *current.borrow_mut() = hook);
}

#[cfg(test)]
fn local_source_fault(point: &'static str) -> bool {
    LOCAL_SOURCE_FAULT.with(|current| {
        if current.get() == Some(point) {
            current.set(None);
            true
        } else {
            false
        }
    })
}

#[cfg(not(test))]
fn local_source_fault(_point: &'static str) -> bool {
    false
}

#[cfg(test)]
fn run_before_save_hook() {
    if let Some(hook) = LOCAL_SOURCE_BEFORE_SAVE.with(|current| current.borrow_mut().take()) {
        hook();
    }
}

#[cfg(not(test))]
fn run_before_save_hook() {}

pub struct McpLocalSourceChangeOwnerV1<'a> {
    host: &'a LocalHost,
    effects: &'a ManagementEffectStoreV1,
    lease: &'a ProjectMutationLeaseV1,
}

impl<'a> McpLocalSourceChangeOwnerV1<'a> {
    pub fn new(
        host: &'a LocalHost,
        effects: &'a ManagementEffectStoreV1,
        lease: &'a ProjectMutationLeaseV1,
        project_root: &Path,
    ) -> Result<Self, String> {
        let root = project_root
            .canonicalize()
            .map_err(|_| "MCP projectを確認できません")?;
        if host.project() != crate::project::project_id(&root)
            || effects.project_scope_hash()
                != ManagementEffectHashV1::digest(root.as_os_str().as_encoded_bytes())
        {
            return Err("MCP hostと管理ownerのprojectが一致しません".into());
        }
        Ok(Self {
            host,
            effects,
            lease,
        })
    }

    pub fn apply(
        &self,
        preparation: UnityLocalV2Preparation,
    ) -> Result<UnityLocalV2Receipt, String> {
        let store = self.host.store().ok_or("MCP storeが存在しません")?;
        self.require_registry(preparation.before())?;
        preparation.revalidate()?;

        let c28_id = ManagementOperationIdV1::from_str(preparation.destination().operation_id())
            .map_err(|_| "runtime配置IDを確認できません")?;
        let c28_request = ManagementEffectHashV1::digest(
            &serde_json::to_vec(&(
                "mcp-local-source-runtime-v1",
                self.host.project(),
                &preparation.preview().server_id,
                preparation.destination().destination(),
                &preparation.preview().source,
                &preparation.preview().manifest_sha256,
                &preparation.preview().node_sha256,
                &preparation.preview().entry_sha256,
                preparation.before().revision,
            ))
            .map_err(|_| "runtime配置内容を確認できません")?,
        );
        if local_source_fault("c28_before_prepare") {
            return Err("C28 prepare前の試験faultです".into());
        }
        let c28 = self.prepare_effect("C28", c28_id, preparation.before().revision, c28_request)?;
        if local_source_fault("c28_after_prepare") {
            return Err("C28 prepare後の試験faultです".into());
        }
        let c28_started = self
            .effects
            .start(self.lease, &c28)
            .map_err(|e| e.to_string())?;
        if local_source_fault("c28_after_start") {
            return Err(self.unknown(&c28_started, "C28 start後の試験faultです".into()));
        }
        let published = match preparation.publish_runtime(store) {
            Ok(value) => value,
            Err(error) => return Err(self.unknown(&c28_started, error)),
        };
        if local_source_fault("c28_after_publish") {
            return Err(self.unknown(&c28_started, "C28 publish後の試験faultです".into()));
        }
        let c28_receipt = ManagementEffectHashV1::digest(
            format!(
                "mcp-local-source-runtime-v1\0{}\0{}\0{}",
                preparation.preview().server_id,
                published.path().display(),
                published.receipt()
            )
            .as_bytes(),
        );
        if local_source_fault("c28_complete")
            || self
                .effects
                .complete_started(self.lease, &c28_started, c28_receipt)
                .is_err()
        {
            let _ = self.effects.mark_outcome_unknown(
                self.lease,
                &c28_started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            );
            return Err("Unity runtimeは配置済みですがC28 receiptが不明です".into());
        }
        if local_source_fault("c28_after_complete") {
            return Err("C28 complete後の試験faultです".into());
        }

        // C27 cannot begin before the completed C28 receipt. Its request binds
        // the approved logical values; live source, registry, runtime and host
        // admission are all rechecked after start and immediately before CAS.
        let target = preparation
            .next()
            .servers
            .iter()
            .find(|server| server.server_id == preparation.preview().server_id)
            .ok_or("更新対象のUnity MCPがありません")?;
        let destination = target
            .destination_hash()
            .map_err(|_| "更新後のUnity destinationが無効です")?;
        let c27_request = ManagementEffectHashV1::digest(
            &serde_json::to_vec(&(
                "mcp-local-source-registry-v1",
                self.host.project(),
                &preparation.preview().server_id,
                preparation.before().revision,
                preparation.next().revision,
                destination,
                published.path(),
                published.receipt(),
            ))
            .map_err(|_| "registry変更内容を確認できません")?,
        );
        if local_source_fault("c27_before_prepare") {
            return Err("C27 prepare前の試験faultです".into());
        }
        let c27 = self.prepare_effect(
            "C27",
            preparation.c27_operation_id(),
            preparation.before().revision,
            c27_request,
        )?;
        if local_source_fault("c27_after_prepare") {
            return Err("C27 prepare後の試験faultです".into());
        }
        let c27_started = self
            .effects
            .start(self.lease, &c27)
            .map_err(|e| e.to_string())?;
        if local_source_fault("c27_after_start") {
            return Err(self.unknown(&c27_started, "C27 start後の試験faultです".into()));
        }
        let live_recheck = (|| -> Result<(), String> {
            preparation.revalidate()?;
            self.require_registry(preparation.before())?;
            published.revalidate()?;
            if published.path() != preparation.destination().destination() {
                return Err("配置済みUnity runtimeのidentityが一致しません".into());
            }
            self.host
                .validate(target)
                .map_err(|_| "配置済みUnity runtimeがhost入場検査に失敗しました".to_owned())?;
            preparation
                .next()
                .validate()
                .map_err(|_| "更新後のMCP registryが無効です".to_owned())
        })();
        if let Err(error) = live_recheck {
            return Err(self.unknown(&c27_started, error));
        }
        run_before_save_hook();
        match store.save_registry(preparation.before().revision, preparation.next()) {
            Ok(()) => {}
            Err(polaris_mcp::Error::RevisionConflict) => {
                // CAS rejected before replacement, so this operation did not
                // change the registry. The v1 effect journal has no terminal
                // known-failure state after start; retain conservative unknown
                // without inventing a successful completion receipt.
                return Err(self.unknown(
                    &c27_started,
                    "registry CASは競合し、この操作によるregistry変更はありません。配置済みUnity runtimeは未参照で保持します"
                        .into(),
                ));
            }
            Err(_) => {
                return Err(self.unknown(
                    &c27_started,
                    "Unity runtimeは保持しましたがregistry CASの結果が不明です".into(),
                ));
            }
        }
        if local_source_fault("c27_after_save") {
            return Err(self.unknown(&c27_started, "C27 save後の試験faultです".into()));
        }
        if !store
            .load_registry()
            .is_ok_and(|value| value.as_ref() == Some(preparation.next()))
        {
            return Err(self.unknown(
                &c27_started,
                "Unity runtimeは保持しましたがregistry CASの結果が不明です".into(),
            ));
        }
        let receipt = ManagementEffectHashV1::digest(
            format!(
                "mcp-local-source-registry-v1\0{}\0{}\0{}",
                self.host.project(),
                preparation.preview().server_id,
                preparation.next().revision
            )
            .as_bytes(),
        );
        if local_source_fault("c27_complete")
            || self
                .effects
                .complete_started(self.lease, &c27_started, receipt)
                .is_err()
        {
            let _ = self.effects.mark_outcome_unknown(
                self.lease,
                &c27_started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            );
            return Err("MCP registryは保存済みですがC27 receiptが不明です".into());
        }
        if local_source_fault("c27_after_complete") {
            return Err("C27 complete後の試験faultです".into());
        }
        Ok(UnityLocalV2Receipt {
            server_id: preparation.preview().server_id.clone(),
            cwd: published.path().to_owned(),
            registry_revision: preparation.next().revision,
            protocol_profile: polaris_mcp::registration::ProtocolProfile::Auto2024Compat,
        })
    }

    fn require_registry(&self, expected: &Registry) -> Result<(), String> {
        let store = self.host.store().ok_or("MCP storeが存在しません")?;
        if store
            .load_snapshot_noncreating()
            .is_ok_and(|snapshot| snapshot.registry.as_ref() == Some(expected))
        {
            Ok(())
        } else {
            Err("確認中にMCP registryまたはsecret参照が変更されました".into())
        }
    }

    fn prepare_effect(
        &self,
        inventory: &str,
        operation_id: ManagementOperationIdV1,
        revision: u64,
        request_hash: ManagementEffectHashV1,
    ) -> Result<PreparedManagementEffectV1, String> {
        let request = PrepareManagementEffectV1 {
            operation_id,
            inventory: ManagementInventoryIdV1::parse(inventory)
                .map_err(|_| "MCP inventoryが無効です")?,
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: None,
                generation: revision,
                revision,
                permission: ManagementPermissionScopeV1::System,
                account_binding: None,
            },
            request_hash,
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
            PrepareManagementEffectOutcomeV1::Prepared(effect) => Ok(effect),
            PrepareManagementEffectOutcomeV1::Existing(_) => {
                Err("MCP管理操作が既に存在します".into())
            }
        }
    }

    fn unknown(&self, started: &StartedManagementEffectV1, message: String) -> String {
        let _ = self.effects.mark_outcome_unknown(
            self.lease,
            started,
            ManagementUnknownReasonV1::UnclassifiedEffect,
        );
        format!("{message}。再実行せず管理状態を照合してください")
    }
}
