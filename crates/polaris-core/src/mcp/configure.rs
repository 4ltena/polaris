//! Host-owned preparation and application of the one supported local Unity MCP runtime.
//!
//! Preparation is deliberately read-only. It binds an existing registration and a
//! physical source tree to descriptors and hashes without creating the MCP store,
//! its lock, an effect record, or a destination. The approved application is split
//! into C28 runtime publication followed by C27 registry replacement.

mod install;
mod source;

#[cfg(test)]
mod tests;

use crate::{
    management_effect::{ManagementEffectStoreV1, mcp_change::McpLocalSourceChangeOwnerV1},
    mcp::local::LocalHost,
    project_mutation::ProjectMutationLeaseV1,
};
use polaris_mcp::{
    registration::{Destination, ProtocolProfile, Registry},
    store::{PublishedRuntime, RuntimeVersionPlan, Store},
};
use std::{
    fs::OpenOptions,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use self::source::UnitySourceBinding;

const PRESET: &str = "unity-local-v2";

#[cfg(test)]
pub(crate) fn set_unity_copy_fault(hook: Option<Box<dyn FnOnce()>>) {
    install::set_copy_fault(hook);
}

/// Read-only data the CLI may render after sanitizing every string and path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnityLocalV2Preview {
    pub server_id: String,
    pub project_id: String,
    pub source: PathBuf,
    pub old_cwd: PathBuf,
    pub new_cwd: PathBuf,
    pub node: PathBuf,
    pub entry: PathBuf,
    pub node_sha256: String,
    pub entry_sha256: String,
    pub manifest_sha256: String,
    pub selected_files: usize,
    pub selected_bytes: u64,
    pub fixed_exclusions: Vec<PathBuf>,
    pub hardlink_pair: [PathBuf; 2],
    pub hardlink_pair_sha256: String,
    pub secret_references: Vec<String>,
    pub registry_before_revision: u64,
    pub registry_after_revision: u64,
    pub protocol_profile: ProtocolProfile,
    pub sandbox_profile: String,
}

/// Opaque read-only preparation retained across the visible confirmation card.
pub struct UnityLocalV2Preparation {
    before: Registry,
    next: Registry,
    source: UnitySourceBinding,
    destination: RuntimeVersionPlan,
    c27_operation_id: crate::management_effect::ManagementOperationIdV1,
    preview: UnityLocalV2Preview,
}

/// Durable result of the two completed effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnityLocalV2Receipt {
    pub server_id: String,
    pub cwd: PathBuf,
    pub registry_revision: u64,
    pub protocol_profile: ProtocolProfile,
}

/// Published C28 artifact retained by descriptor until C27 finishes.
pub(crate) struct PublishedUnityRuntime {
    runtime: PublishedRuntime,
    installed: install::InstalledRuntime,
    receipt: String,
}

impl PublishedUnityRuntime {
    pub(crate) fn path(&self) -> &Path {
        self.runtime.path()
    }

    pub(crate) fn receipt(&self) -> &str {
        &self.receipt
    }

    pub(crate) fn revalidate(&self) -> Result<(), String> {
        let bound = self
            .runtime
            .try_clone_directory()
            .map_err(|_| "Cannot retain the deployed Unity runtime descriptor")?;
        self.installed
            .verify(&bound)
            .map_err(|_| "Deployed Unity runtime content changed")?;
        let named = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.runtime.path())
            .map_err(|_| "Cannot reverify the deployed Unity runtime path")?;
        let bound_metadata = bound
            .metadata()
            .map_err(|_| "Cannot verify the deployed Unity runtime descriptor")?;
        let named_metadata = named
            .metadata()
            .map_err(|_| "Cannot verify the deployed Unity runtime path")?;
        if (bound_metadata.dev(), bound_metadata.ino())
            != (named_metadata.dev(), named_metadata.ino())
        {
            return Err("Deployed Unity runtime namespace changed".into());
        }
        self.installed
            .verify(&named)
            .map_err(|_| "Deployed Unity runtime content changed".into())
    }
}

impl UnityLocalV2Preparation {
    /// Bind the current single stdio registration and selected physical source.
    /// This function must run before asking for approval and performs no writes.
    pub fn prepare(host: &LocalHost, server_id: &str, source: &Path) -> Result<Self, String> {
        let store = host.store().ok_or("MCP store does not exist")?;
        let snapshot = store
            .load_snapshot_noncreating()
            .map_err(|_| "Cannot inspect the MCP registry without creating it")?;
        let before = snapshot.registry.ok_or("MCP registry does not exist")?;
        let current = before
            .servers
            .iter()
            .find(|server| server.server_id == server_id)
            .ok_or("The specified MCP server does not exist")?;
        if !current.enabled_projects.contains(host.project()) {
            return Err("Unity MCP is not enabled for this project".into());
        }

        let c28_operation_id = crate::management_effect::ManagementOperationIdV1::random()
            .map_err(|_| "Cannot create a runtime deployment ID")?;
        let c27_operation_id = crate::management_effect::ManagementOperationIdV1::random()
            .map_err(|_| "Cannot create a registry change ID")?;
        let destination = store
            .plan_runtime_version(server_id, &c28_operation_id.as_hex())
            .map_err(|_| "Cannot verify the Unity runtime destination")?;
        let binding = UnitySourceBinding::inspect(source, current)?;
        let mut next = before.clone();
        let target = next
            .servers
            .iter_mut()
            .find(|server| server.server_id == server_id)
            .ok_or("The specified MCP server does not exist")?;
        binding.rewrite_server(target, destination.destination())?;
        next.revision = before
            .revision
            .checked_add(1)
            .ok_or("MCP revision limit reached")?;
        next.validate().map_err(|_| "Invalid Unity MCP update")?;

        let (old_cwd, node, entry, mut secret_references) = match &current.transport {
            Destination::Stdio {
                command,
                args,
                cwd,
                secret_env,
                ..
            } => (
                cwd.clone(),
                command.clone(),
                args.first()
                    .map(PathBuf::from)
                    .ok_or("Existing Unity registration has no entry argument")?,
                secret_env.values().cloned().collect::<Vec<_>>(),
            ),
            _ => {
                return Err(
                    "The Unity preset can update only an existing stdio registration".into(),
                );
            }
        };
        secret_references.sort();
        secret_references.dedup();
        let preview = UnityLocalV2Preview {
            server_id: server_id.to_owned(),
            project_id: host.project().to_owned(),
            source: binding.source().to_owned(),
            old_cwd,
            new_cwd: destination.destination().to_owned(),
            node,
            entry,
            node_sha256: binding.node_sha256().to_owned(),
            entry_sha256: binding.entry_sha256().to_owned(),
            manifest_sha256: binding.manifest_sha256().to_owned(),
            selected_files: binding.file_count(),
            selected_bytes: binding.total_bytes(),
            fixed_exclusions: binding.fixed_exclusion_paths(),
            hardlink_pair: binding.hardlink_pair(),
            hardlink_pair_sha256: binding.hardlink_pair_sha256().to_owned(),
            secret_references,
            registry_before_revision: before.revision,
            registry_after_revision: next.revision,
            protocol_profile: ProtocolProfile::Auto2024Compat,
            sandbox_profile: "loopback".into(),
        };
        Ok(Self {
            before,
            next,
            source: binding,
            destination,
            c27_operation_id,
            preview,
        })
    }

    pub fn preset(&self) -> &'static str {
        PRESET
    }

    pub fn preview(&self) -> &UnityLocalV2Preview {
        &self.preview
    }

    /// Apply an already approved preparation while the caller owns the project
    /// mutation lease and a newly opened management effect store.
    pub fn apply(
        self,
        host: &LocalHost,
        effects: &ManagementEffectStoreV1,
        lease: &ProjectMutationLeaseV1,
        project_root: &Path,
    ) -> Result<UnityLocalV2Receipt, String> {
        let owner = McpLocalSourceChangeOwnerV1::new(host, effects, lease, project_root)?;
        owner.apply(self)
    }

    pub(crate) fn before(&self) -> &Registry {
        &self.before
    }

    pub(crate) fn next(&self) -> &Registry {
        &self.next
    }

    pub(crate) fn destination(&self) -> &RuntimeVersionPlan {
        &self.destination
    }

    pub(crate) fn c27_operation_id(&self) -> crate::management_effect::ManagementOperationIdV1 {
        self.c27_operation_id
    }

    pub(crate) fn revalidate(&self) -> Result<(), String> {
        self.source.revalidate()
    }

    pub(crate) fn publish_runtime(&self, store: &Store) -> Result<PublishedUnityRuntime, String> {
        let staging = store
            .create_runtime_version_staging(&self.destination)
            .map_err(|_| "Cannot create the Unity runtime private partial directory")?;
        let staging_directory = staging
            .try_clone_directory()
            .map_err(|_| "Cannot retain the Unity runtime partial descriptor")?;
        let installed = install::populate(
            &self.source,
            &staging_directory,
            &self.preview.server_id,
            self.destination.operation_id(),
        )?;
        let receipt = installed.receipt_hash().to_owned();
        let published = staging
            .publish(|directory| installed.verify(directory))
            .map_err(|_| "Unity runtime publication outcome is unknown")?;
        if published.path() != self.destination.destination() {
            return Err("Published Unity runtime identity mismatch".into());
        }
        Ok(PublishedUnityRuntime {
            runtime: published,
            installed,
            receipt,
        })
    }
}
