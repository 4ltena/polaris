//! Trusted existing-store composition. No discovery, provisioning or replay.
use crate::{
    ConfiguredRunFactory, ConfirmedSourceGrant, CurrentSourcePolicy, DesktopService,
    ProductionSourceFactory, SourceApplyClock, SourceApplyRuntime, TrustedBackend,
    TrustedFactoryResources, WorkspacePreparationRecipe, launch_arguments::OwnerLaunchArguments,
    package_manifest::PackagedExecutionHelper,
};
use polaris_core::{
    desktop_store::{
        BootstrapIdentity, BootstrapTier, DesktopRoot, ExpectedBootstrapStore, SourceApplyIdentity,
        Writer, read_owner_bootstrap,
    },
    isolated_run::toolchains::ToolchainPlan,
    isolated_workspace::Limits,
};
use std::{fs::File, path::PathBuf, sync::Arc};

/// Caller created and synced both this directory and its parent's naming entry.
/// The recovery socket descriptor is unrelated to this directory descriptor.
pub struct PinnedRecoveryBase {
    pub directory: File,
    pub identity: BootstrapIdentity,
    pub provenance: PathBuf,
}
pub struct OwnerLaunchLimits {
    pub workspace: Limits,
    pub runtime_roots: Vec<PathBuf>,
    pub toolchains: Option<ToolchainPlan>,
    pub copy_mutations: bool,
    pub source: Limits,
    pub clock: Arc<dyn SourceApplyClock>,
    pub approval_ttl_ms: u64,
}
#[derive(Debug, thiserror::Error)]
#[error("trusted owner launch refused")]
pub struct OwnerLaunchError;

/// Runs on the trusted startup worker. Backend credentials must already be
/// registered before this call; no token method, network or environment lookup
/// is performed here. An unconfigured store must be provisioned separately.
#[allow(
    clippy::too_many_arguments,
    reason = "Each argument is a separately validated startup capability"
)]
pub fn compose_existing(
    args: OwnerLaunchArguments,
    root: DesktopRoot,
    writer: Writer,
    helper: PackagedExecutionHelper,
    backend: TrustedBackend,
    resources: TrustedFactoryResources,
    recovery: PinnedRecoveryBase,
    limits: OwnerLaunchLimits,
) -> Result<DesktopService, OwnerLaunchError> {
    let identity = (
        args.store_identity.device.get(),
        args.store_identity.inode.get(),
    );
    if root.identity().map_err(|_| OwnerLaunchError)? != identity
        || writer.root_identity().map_err(|_| OwnerLaunchError)? != identity
        || writer.coordinates() != (&args.store.project_id, &args.store.session_id)
        || limits.approval_ttl_ms == 0
    {
        return Err(OwnerLaunchError);
    }
    let saved = writer.snapshot().map_err(|_| OwnerLaunchError)?;
    let bootstrap = read_owner_bootstrap(
        &args.store.store_root.join(&args.bootstrap_name),
        &args.bootstrap_file,
        &ExpectedBootstrapStore {
            path: &args.store.store_root,
            identity: args.store_identity,
        },
        &saved,
    )
    .map_err(|_| OwnerLaunchError)?;
    let doc = bootstrap.document();
    if args.confirmed_source_path != std::path::Path::new(&doc.source_path)
        || args.confirmed_source_identity != doc.source_identity
        || args.confirmed_tier != doc.tier
        || args.confirmed_policy_revision != doc.policy_revision
    {
        return Err(OwnerLaunchError);
    }
    let policy = CurrentSourcePolicy {
        source_path: args.confirmed_source_path,
        source_identity: SourceApplyIdentity {
            device: args.confirmed_source_identity.device,
            inode: args.confirmed_source_identity.inode,
        },
        policy_revision: args.confirmed_policy_revision,
        read_allowed: true,
        write_allowed: args.confirmed_tier != BootstrapTier::ReadOnly,
    };
    let workspace_reader = crate::WorkspaceViewReader::new(
        policy.source_path.clone(),
        crate::WorkspaceSourceIdentity {
            device: policy.source_identity.device.get(),
            inode: policy.source_identity.inode.get(),
        },
    )
    .map_err(|_| OwnerLaunchError)?;
    let source = ProductionSourceFactory::new(
        Arc::new(ConfirmedSourceGrant {
            policy: policy.clone(),
            tier: args.confirmed_tier,
        }),
        recovery.directory,
        (
            recovery.identity.device.get(),
            recovery.identity.inode.get(),
        ),
        recovery.provenance,
    )
    .map_err(|_| OwnerLaunchError)?;
    let recipe = WorkspacePreparationRecipe::new(
        policy.source_path.clone(),
        helper.helper_path,
        helper.sha256,
        limits.workspace,
        limits.runtime_roots,
        limits.toolchains,
        limits.copy_mutations,
    )
    .map_err(|_| OwnerLaunchError)?;
    let runs = ConfiguredRunFactory::with_preparation_recipe(
        bootstrap,
        ConfirmedSourceGrant {
            policy,
            tier: args.confirmed_tier,
        },
        backend,
        resources,
        recipe,
    )
    .map_err(|_| OwnerLaunchError)?;
    DesktopService::from_trusted_store(
        writer,
        Arc::new(runs),
        SourceApplyRuntime {
            factory: Arc::new(source),
            clock: limits.clock,
            limits: limits.source,
            approval_ttl_ms: limits.approval_ttl_ms,
        },
    )
    .map(|service| service.with_workspace_reader(workspace_reader))
    .map_err(|_| OwnerLaunchError)
}

#[cfg(test)]
mod tests;
