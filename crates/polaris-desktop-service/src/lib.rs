//! 保存専用desktop serviceと、TempDir専用のP3 fake service。実provider・認証は接続しない。

mod engine;
#[cfg(unix)]
mod os_home;
#[cfg(unix)]
pub use os_home::{TrustedHomeError, trusted_user_home};
#[cfg(target_os = "macos")]
mod owner_launch;
#[cfg(target_os = "macos")]
pub use owner_launch::{OwnerLaunchError, OwnerLaunchLimits, PinnedRecoveryBase, compose_existing};
#[cfg(target_os = "macos")]
mod recovery_base;
#[cfg(target_os = "macos")]
pub use recovery_base::{RecoveryBaseError, prepare_recovery_base};
mod launch_arguments;
pub use launch_arguments::{
    LaunchArguments, LaunchArgumentsError, OwnerLaunchArguments, parse_launch_args,
};
#[cfg(target_os = "macos")]
mod package_manifest;
#[cfg(target_os = "macos")]
pub use package_manifest::{PackageManifestError, PackagedExecutionHelper, read_package_manifest};
mod execution;
#[cfg(target_os = "macos")]
mod auth_tokens;
#[cfg(target_os = "macos")]
pub use auth_tokens::DesktopAuthTokens;
#[cfg(target_os = "macos")]
mod configured_factory;
#[cfg(target_os = "macos")]
pub use configured_factory::{
    ConfiguredRunFactory, ConfirmedSourceGrant, FactoryError, TrustedBackend, TrustedFactoryResources,
    WorkspacePreparationRecipe,
};
mod transport;
mod recovery_transport;
pub use recovery_transport::{RecoveryTransport, RecoveryTransportError};
#[cfg(target_os = "macos")]
pub use recovery_transport::{serve_source_recovery, serve_source_recovery_draining};
#[cfg(unix)]
mod recovery_socket;
#[cfg(unix)]
pub use recovery_socket::take_recovery_socket;

#[cfg(all(test, target_os = "macos"))]
mod core_integration_tests;

pub use engine::{DesktopService, FakeService, Options, ServiceConfig, project_id, session_id};
#[cfg(target_os = "macos")]
pub use engine::{
    SourceApplyRuntime, SourceRecoveryHandle, TrustedRunCompletion, TrustedRunFactory, TrustedRunInputs,
    TrustedSourceFactory,
};
pub use transport::{Exit, ServiceError};

#[cfg(target_os = "macos")]
mod source_apply;
#[cfg(target_os = "macos")]
mod source_apply_io;
#[cfg(target_os = "macos")]
mod production_source_factory;
#[cfg(target_os = "macos")]
pub use production_source_factory::ProductionSourceFactory;
#[cfg(target_os = "macos")]
pub use source_apply::{
    CurrentSourcePolicy, PreparedSourceRecovery, SourceApplyClock, SourceApplyController,
    SourceApplyError, SourceApplyRequest, SourceApplyState, SourceDecisionReceipt,
};
