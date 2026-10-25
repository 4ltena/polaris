//! MCP project admission changes retain the registry CAS owner and effect journal.
use super::*;
use crate::{mcp::local::LocalHost, project_mutation::ProjectMutationLeaseV1};
use polaris_mcp::registration::Registry;
use std::path::Path;

mod local_source;
pub use local_source::McpLocalSourceChangeOwnerV1;
#[cfg(test)]
pub(crate) use local_source::{set_local_source_before_save_hook, set_local_source_fault};

pub struct McpRegistryChangeOwnerV1<'a> {
    host: &'a LocalHost,
    effects: &'a ManagementEffectStoreV1,
    lease: &'a ProjectMutationLeaseV1,
}

pub struct PreparedMcpRegistryChangeV1 {
    before: Registry,
    next: Registry,
    server_id: String,
    enabled: bool,
    effect: PreparedManagementEffectV1,
}

impl PreparedMcpRegistryChangeV1 {
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn before_revision(&self) -> u64 {
        self.before.revision
    }
}

impl<'a> McpRegistryChangeOwnerV1<'a> {
    pub fn new(
        host: &'a LocalHost,
        effects: &'a ManagementEffectStoreV1,
        lease: &'a ProjectMutationLeaseV1,
        project_root: &Path,
    ) -> Result<Self, String> {
        let root = project_root
            .canonicalize()
            .map_err(|_| "Cannot verify the MCP project")?;
        if host.project() != crate::project::project_id(&root)
            || effects.project_scope_hash()
                != ManagementEffectHashV1::digest(root.as_os_str().as_encoded_bytes())
        {
            return Err("MCP host and management owner project mismatch".into());
        }
        Ok(Self {
            host,
            effects,
            lease,
        })
    }

    pub fn prepare(
        &self,
        server_id: &str,
        enabled: bool,
    ) -> Result<PreparedMcpRegistryChangeV1, String> {
        let store = self.host.store().ok_or("MCP registry does not exist")?;
        let before = store
            .load_snapshot()
            .map_err(|_| "Cannot read the MCP registry")?
            .registry
            .ok_or("MCP registry does not exist")?;
        let mut next = before.clone();
        let server = next
            .servers
            .iter_mut()
            .find(|server| server.server_id == server_id)
            .ok_or("MCP server is not registered")?;
        if server.enabled_projects.contains(self.host.project()) == enabled {
            return Err("MCP server already has the requested project state".into());
        }
        if enabled {
            server
                .enabled_projects
                .insert(self.host.project().to_owned());
            self.host
                .validate(server)
                .map_err(|_| "MCP server failed host admission checks")?;
        } else {
            server.enabled_projects.remove(self.host.project());
        }
        let destination = server
            .destination_hash()
            .map_err(|_| "Invalid MCP destination")?;
        next.revision = before
            .revision
            .checked_add(1)
            .ok_or("MCP revision limit reached")?;
        next.validate().map_err(|_| "Invalid MCP registry change")?;
        // The journal stores identities and hashes only, never secret values or references.
        let request_hash = ManagementEffectHashV1::digest(
            &serde_json::to_vec(&(
                self.host.project(),
                server_id,
                enabled,
                before.revision,
                destination,
            ))
            .map_err(|_| "Invalid MCP request")?,
        );
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random()
                .map_err(|_| "Cannot reserve a management operation ID")?,
            inventory: ManagementInventoryIdV1::parse(if enabled { "C22" } else { "C23" })
                .map_err(|_| "Invalid inventory")?,
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: None,
                generation: before.revision,
                revision: before.revision,
                permission: ManagementPermissionScopeV1::System,
                account_binding: None,
            },
            request_hash,
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let outcome = self
            .effects
            .prepare(
                self.lease,
                self.effects.cursor().map_err(|e| e.to_string())?,
                request,
            )
            .map_err(|e| e.to_string())?;
        let PrepareManagementEffectOutcomeV1::Prepared(effect) = outcome else {
            return Err("MCP management operation already exists".into());
        };
        Ok(PreparedMcpRegistryChangeV1 {
            before,
            next,
            server_id: server_id.to_owned(),
            enabled,
            effect,
        })
    }

    pub fn apply(
        &self,
        prepared: &PreparedMcpRegistryChangeV1,
        accepted: bool,
    ) -> Result<Option<u64>, String> {
        if !accepted {
            self.effects
                .deny(
                    self.lease,
                    &prepared.effect,
                    ManagementDenialReasonV1::UserDeclined,
                )
                .map_err(|e| e.to_string())?;
            return Ok(None);
        }
        let store = self.host.store().ok_or("MCP registry does not exist")?;
        match store.load_registry() {
            Ok(Some(current)) if current == prepared.before => {}
            Ok(_) => {
                self.effects
                    .deny(
                        self.lease,
                        &prepared.effect,
                        ManagementDenialReasonV1::StaleRequest,
                    )
                    .map_err(|e| e.to_string())?;
                return Err("MCP registry changed during confirmation".into());
            }
            Err(_) => {
                self.effects
                    .deny(
                        self.lease,
                        &prepared.effect,
                        ManagementDenialReasonV1::ResourceUnavailable,
                    )
                    .map_err(|e| e.to_string())?;
                return Err("Cannot recheck the MCP registry".into());
            }
        }
        if prepared.enabled {
            let server = prepared
                .next
                .servers
                .iter()
                .find(|server| server.server_id == prepared.server_id)
                .ok_or("MCP target does not exist")?;
            if self.host.validate(server).is_err() {
                self.effects
                    .deny(
                        self.lease,
                        &prepared.effect,
                        ManagementDenialReasonV1::PermissionDenied,
                    )
                    .map_err(|e| e.to_string())?;
                return Err("MCP server no longer passes host admission checks".into());
            }
        }
        let started = self
            .effects
            .start(self.lease, &prepared.effect)
            .map_err(|e| e.to_string())?;
        let saved = store
            .save_registry(prepared.before.revision, &prepared.next)
            .is_ok()
            && store
                .load_registry()
                .is_ok_and(|current| current.as_ref() == Some(&prepared.next));
        if !saved {
            self.effects
                .mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::UnclassifiedEffect,
                )
                .map_err(|e| e.to_string())?;
            return Err("MCP operation outcome is unknown. It will not be rerun until the registry is checked".into());
        }
        let receipt = ManagementEffectHashV1::digest(
            format!(
                "MCP registry receipt {} {} {}",
                self.host.project(),
                prepared.server_id,
                prepared.next.revision
            )
            .as_bytes(),
        );
        if self
            .effects
            .complete_started(self.lease, &started, receipt)
            .is_err()
        {
            // A started operation also blocks work after restart; do not repeat its write.
            let _ = self.effects.mark_outcome_unknown(
                self.lease,
                &started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            );
            return Err("MCP registry saved, but the management receipt is unknown".into());
        }
        Ok(Some(prepared.next.revision))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_mcp::registration::{Capabilities, Destination, ProtocolProfile, Server, Timeouts};

    #[test]
    fn m3d_mcp_registry_confirmed_changes_and_cancel() {
        if std::env::var_os("POLARIS_M3D_CORE_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["management_effect::mcp_change::tests::m3d_mcp_registry_confirmed_changes_and_cancel", "--exact", "--nocapture"])
                .env("POLARIS_M3D_CORE_CHILD", "1").env("HOME", home.path())
                .env_remove("POLARIS_DATA_DIR").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let state = crate::project::state_dir(&project).unwrap();
        let workspace = crate::rewind_workspace::WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        let host = LocalHost::open(&project, true).unwrap();
        let store = host.store().unwrap();
        store
            .save_registry(
                0,
                &Registry {
                    schema_version: 1,
                    revision: 1,
                    servers: vec![Server {
                        server_id: "fixture".into(),
                        enabled_projects: ["other-project".into()].into(),
                        transport: Destination::StreamableHttp {
                            url: "https://example.com/mcp".into(),
                            secret_headers: Default::default(),
                        },
                        protocol_profile: ProtocolProfile::default(),
                        capabilities: Capabilities::default(),
                        timeouts: Timeouts::default(),
                    }],
                },
            )
            .unwrap();
        let owner = McpRegistryChangeOwnerV1::new(&host, &effects, &lease, &project).unwrap();
        let enable = owner.prepare("fixture", true).unwrap();
        assert_eq!(owner.apply(&enable, true).unwrap(), Some(2));
        let current = store.load_registry().unwrap().unwrap();
        assert!(current.servers[0].enabled_projects.contains(host.project()));
        assert!(
            current.servers[0]
                .enabled_projects
                .contains("other-project")
        );
        assert!(owner.prepare("missing", false).is_err());
        let disable = owner.prepare("fixture", false).unwrap();
        assert_eq!(owner.apply(&disable, false).unwrap(), None);
        assert_eq!(store.load_registry().unwrap().unwrap(), current);
        let disable = owner.prepare("fixture", false).unwrap();
        assert_eq!(owner.apply(&disable, true).unwrap(), Some(3));
        let current = store.load_registry().unwrap().unwrap();
        assert!(!current.servers[0].enabled_projects.contains(host.project()));
        assert!(
            current.servers[0]
                .enabled_projects
                .contains("other-project")
        );
        assert!(effects.blocker().unwrap().is_none());
    }
}
