//! Private retained admission for physical MCP migration.
//! Observed topology cannot prove that historical custom locations are known.
use super::{
    account_home::TrustedUserHomeV2,
    entry_gate::{EntryMigrationGuardV2, ProductionEntryManifestV2, ZeroProductionProcessesV2},
    preflight::SourcePreflightV2,
};
use crate::desktop_store::migration::{
    TransportRootsObservationV2, discovery::TransportRootDiscoveryV2,
};
use polaris_mcp::Error;
use std::{path::PathBuf, sync::Arc};
#[path = "intent_file.rs"]
mod intent_file;
mod retirement;

pub(super) struct MigrationAdmissionV2 {
    source: SourcePreflightV2,
    guard: EntryMigrationGuardV2,
    roots: TransportRootsObservationV2,
    discovery: TransportRootDiscoveryV2,
    stopped: ZeroProductionProcessesV2,
}
impl MigrationAdmissionV2 {
    pub(super) fn begin(
        home: Arc<TrustedUserHomeV2>,
        id: &str,
        manifest: ProductionEntryManifestV2,
        custom_data_paths: &[PathBuf],
        custom_settings_files: &[PathBuf],
    ) -> Result<Self, Error> {
        // Source observation precedes every gate/intent write. After EX entry
        // admission, retain all storage owners before asking for native zero.
        let source = SourcePreflightV2::inspect_trusted(home.clone())?;
        let mut data = vec![home.verified_path().map_err(protection)?.join(".polaris")];
        data.extend_from_slice(custom_data_paths);
        if let Some(current) = std::env::var_os("POLARIS_DATA_DIR") {
            data.push(PathBuf::from(current));
        }
        #[cfg(target_os = "macos")]
        let settings = {
            let mut settings = custom_settings_files.to_vec();
            settings.push(
                home.verified_path()
                    .map_err(protection)?
                    .join("Library/Application Support/Polaris/desktop/settings.json"),
            );
            settings
        };
        #[cfg(target_os = "linux")]
        let settings = custom_settings_files.to_vec();
        let discovery = TransportRootDiscoveryV2::inspect(&data, &settings).map_err(protection)?;
        let guard = EntryMigrationGuardV2::begin_trusted(home, id, manifest).map_err(protection)?;
        Self::assemble(source, guard, discovery)
    }
    fn assemble(
        source: SourcePreflightV2,
        guard: EntryMigrationGuardV2,
        discovery: TransportRootDiscoveryV2,
    ) -> Result<Self, Error> {
        source.bind_guard(&guard)?;
        let roots =
            TransportRootsObservationV2::inspect(discovery.verified_roots().map_err(protection)?)
                .map_err(protection)?;
        discovery.verify_current().map_err(protection)?;
        source.bind_guard(&guard)?;
        let stopped = guard.prove_stopped().map_err(protection)?;
        let admission = Self {
            source,
            guard,
            roots,
            discovery,
            stopped,
        };
        admission.verify_before_retire()?;
        Ok(admission)
    }
    // Call immediately before a retirement; Result<()> cannot be retained as
    // an activation permit. Every call repeats actual native process inventory.
    pub(super) fn verify_before_retire(&self) -> Result<(), Error> {
        self.guard.verify_migrating().map_err(protection)?;
        self.source.bind_guard(&self.guard)?;
        self.discovery.verify_current().map_err(protection)?;
        self.roots.verify_current().map_err(protection)?;
        self.discovery.verify_current().map_err(protection)?;
        self.stopped.verify_for(&self.guard).map_err(protection)?;
        #[cfg(test)]
        if let Some(hook) = AFTER_STOP_CHECK.with(|s| s.borrow_mut().take()) {
            hook();
        }
        // Native enumeration and later root readers must not leave earlier
        // source/root observations or the admission gate unclosed.
        self.source.bind_guard(&self.guard)?;
        self.roots.verify_current().map_err(protection)?;
        self.discovery.verify_current().map_err(protection)?;
        self.guard.verify_migrating().map_err(protection)
    }
}
fn protection<T>(_: T) -> Error {
    Error::Protection
}
#[cfg(test)]
thread_local! {
    static AFTER_STOP_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
