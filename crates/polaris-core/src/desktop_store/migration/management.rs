//! Retained project mutation exclusion and historical management observation.
//! Historical paths/scopes never produce execution or activation capabilities.
use super::super::writer::migration::{file_stamp, read_held_bytes};
use super::*;
use crate::management_effect::{
    ManagementMigrationAnchorV2, ManagementMigrationAnchorsV2, ManagementMigrationTreeV2,
};
use crate::{
    management_effect::{
        ManagementEffectHashV1, ManagementEffectInspectOpenV1, ManagementEffectInspectV1,
        ManagementEffectStoreV1,
    },
    project_mutation::{ProjectMutationLeaseV1, ProjectMutationOwnerV1},
};
use polaris_tools::entry_kernel::private_file;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::Metadata, os::unix::fs::MetadataExt};

pub(super) struct ManagementProjectObservationV2 {
    state: PathPin,
    state_metadata: Metadata,
    state_children: BTreeMap<String, Metadata>,
    workspace: PathPin,
    workspace_metadata: Option<Metadata>,
    source: PathPin,
    lock: Option<(File, Metadata)>,
    namespace: Option<(File, Metadata)>,
    historical: Option<HistoricalNamespace>,
    live_project: Option<PathPin>,
    owner: Option<ProjectMutationOwnerV1>,
    lease: Option<ProjectMutationLeaseV1>,
    history: Option<ManagementEffectInspectV1>,
    retired: Option<ManagementTombstonePin>,
    retirement_id: Option<ManagementEffectHashV1>,
    stage: Option<ManagementStagePin>,
}
struct ManagementStagePin {
    file: File,
    metadata: Metadata,
    tree: Option<ManagementMigrationTreeV2>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManagementTombstoneV2 {
    protocol_version: u32,
    migration_id: String,
    retired_source_anchor: EntryAnchorV2,
}
struct ManagementTombstonePin {
    file: File,
    metadata: Metadata,
    bytes: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalNamespace {
    version: u32,
    project_root: PathBuf,
    root_device: u64,
    root_inode: u64,
}
impl ManagementProjectObservationV2 {
    pub(super) fn inspect(path: &Path) -> StoreResult<Self> {
        let id = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(StoreError::RecoveryRequired)?;
        if id.len() != 16
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(StoreError::RecoveryRequired);
        }
        let state = PathPin::inspect(path)?;
        if !state.existing() {
            return Err(StoreError::RecoveryRequired);
        }
        let state_metadata = state.ancestor.metadata()?;
        // A fresh migration must not adopt an already-published management v2
        // store or a foreign cutover. Recovery is a separate physical phase.
        let state_children = inspect_state_children(&state)?;
        if state_children
            .keys()
            .any(|name| name.starts_with("management-effects-") && name != "management-effects-v1")
        {
            return Err(StoreError::RecoveryRequired);
        }
        let workspace = PathPin::inspect(&path.join("rewind-workspace"))?;
        let source = PathPin::inspect(&path.join("management-effects-v1"))?;
        if workspace.mount != state.mount
            || source.mount != state.mount
            || (source.existing() && !workspace.existing())
        {
            return Err(StoreError::RecoveryRequired);
        }
        let workspace_metadata = workspace
            .existing()
            .then(|| workspace.ancestor.metadata())
            .transpose()?;
        let (owner, lease, lock, namespace, historical, live_project) = if workspace.existing() {
            let owner =
                ProjectMutationOwnerV1::open_existing(&workspace.path).map_err(project_error)?;
            let lease = owner.try_mutation().map_err(project_error)?;
            owner
                .verify_migration(&lease, &state.mount)
                .map_err(project_error)?;
            let lock = child(&workspace.ancestor, "writer.lease", libc::O_RDONLY)?;
            private_file(&lock).map_err(|_| StoreError::RecoveryRequired)?;
            if native_mount(&lock)? != state.mount {
                return Err(StoreError::RecoveryRequired);
            }
            let lock_metadata = lock.metadata()?;
            let namespace = match child(&workspace.ancestor, "namespace.json", libc::O_RDONLY) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                result => {
                    let file = result?;
                    private_file(&file).map_err(|_| StoreError::RecoveryRequired)?;
                    if native_mount(&file)? != state.mount {
                        return Err(StoreError::RecoveryRequired);
                    }
                    let metadata = file.metadata()?;
                    Some((file, metadata))
                }
            };
            let historical: Option<HistoricalNamespace> = namespace
                .as_ref()
                .map(|(file, metadata)| {
                    serde_json::from_slice(&read_held_bytes(file, metadata, 64 * 1024)?)
                        .map_err(StoreError::from)
                })
                .transpose()?;
            if let Some(ns) = &historical {
                normalized(&ns.project_root)?;
                if ns.version != 1
                    || ns.root_inode == 0
                    || crate::project::project_id(&ns.project_root) != id
                {
                    return Err(StoreError::RecoveryRequired);
                }
            }
            let live_project = historical
                .as_ref()
                .map(live_identity)
                .transpose()?
                .flatten();
            (
                Some(owner),
                Some(lease),
                Some((lock, lock_metadata)),
                namespace,
                historical,
                live_project,
            )
        } else {
            (None, None, None, None, None, None)
        };
        let scope = historical.as_ref().map(|ns| {
            ManagementEffectHashV1::digest(ns.project_root.as_os_str().as_encoded_bytes())
        });
        let history = if source.existing() {
            match ManagementEffectStoreV1::inspect_migration_history(
                path,
                lease.as_ref().ok_or(StoreError::RecoveryRequired)?,
                scope,
            )
            .map_err(management_error)?
            {
                ManagementEffectInspectOpenV1::Present(history) => {
                    history.verify_read_snapshot().map_err(management_error)?;
                    Some(history)
                }
                ManagementEffectInspectOpenV1::Absent => return Err(StoreError::RecoveryRequired),
            }
        } else {
            None
        };
        #[cfg(test)]
        run_hook(&AFTER_MANAGEMENT_HISTORY);
        let value = Self {
            state,
            state_metadata,
            state_children,
            workspace,
            workspace_metadata,
            source,
            lock,
            namespace,
            historical,
            live_project,
            owner,
            lease,
            history,
            retired: None,
            retirement_id: None,
            stage: None,
        };
        value.verify_current()?;
        Ok(value)
    }

    /// Read-only continuation of this same owner after the coordinator has
    /// renamed the source and durably installed its exact tombstone.
    #[cfg(test)]
    pub(super) fn rebind_retired(self, id: ManagementEffectHashV1) -> StoreResult<Self> {
        self.rebind_retirement(id, true)
    }
    pub(super) fn rebind_renamed(self, id: ManagementEffectHashV1) -> StoreResult<Self> {
        self.rebind_retirement(id, false)
    }
    fn rebind_retirement(
        mut self,
        id: ManagementEffectHashV1,
        with_tombstone: bool,
    ) -> StoreResult<Self> {
        if self.retirement_id.is_some() || !self.source.existing() {
            return Err(StoreError::RecoveryRequired);
        }
        self.state.verify_current()?;
        let phase_state_metadata = self.state.ancestor.metadata()?;
        self.verify_workspace()?;
        let name = format!("management-effects-v1.retired-{}", id.as_hex());
        let children = inspect_state_children(&self.state)?;
        let mut expected_names = self.state_children.keys().cloned().collect::<BTreeSet<_>>();
        if !expected_names.contains("management-effects-v1") || !expected_names.insert(name.clone())
        {
            return Err(StoreError::RecoveryRequired);
        }
        if !with_tombstone {
            expected_names.remove("management-effects-v1");
        }
        if children.keys().cloned().collect::<BTreeSet<_>>() != expected_names {
            return Err(StoreError::RecoveryRequired);
        }
        for (child_name, expected) in &self.state_children {
            if child_name != "management-effects-v1"
                && children
                    .get(child_name)
                    .is_none_or(|current| file_stamp(current) != file_stamp(expected))
            {
                return Err(StoreError::RecoveryRequired);
            }
        }
        let tombstone = if with_tombstone {
            Some(self.read_tombstone(id, &children)?)
        } else {
            None
        };
        let history = self.history.take().ok_or(StoreError::RecoveryRequired)?;
        self.history = Some(
            history
                .rebind_retired_for_migration(&self.state.ancestor, id)
                .map_err(management_error)?,
        );
        self.source.path = self.state.path.join(&name);
        self.source.ancestor_path = self.source.path.clone();
        self.retired = tombstone;
        self.retirement_id = Some(id);
        self.state_children = children;
        self.state_metadata = phase_state_metadata;
        self.verify_current()?;
        Ok(self)
    }
    pub(super) fn bind_tombstone(mut self) -> StoreResult<Self> {
        let id = self.retirement_id.ok_or(StoreError::RecoveryRequired)?;
        if self.retired.is_some() {
            return Err(StoreError::RecoveryRequired);
        }
        self.state.verify_current()?;
        let metadata = self.state.ancestor.metadata()?;
        self.verify_workspace()?;
        self.source.verify_current()?;
        self.history
            .as_ref()
            .ok_or(StoreError::RecoveryRequired)?
            .verify_read_snapshot()
            .map_err(management_error)?;
        let children = inspect_state_children(&self.state)?;
        if children.len() != self.state_children.len() + 1
            || !children.contains_key("management-effects-v1")
            || self.state_children.iter().any(|(name, expected)| {
                children
                    .get(name)
                    .is_none_or(|current| file_stamp(current) != file_stamp(expected))
            })
        {
            return Err(StoreError::RecoveryRequired);
        }
        self.retired = Some(self.read_tombstone(id, &children)?);
        self.state_children = children;
        self.state_metadata = metadata;
        self.verify_current()?;
        Ok(self)
    }
    fn read_tombstone(
        &self,
        id: ManagementEffectHashV1,
        children: &BTreeMap<String, Metadata>,
    ) -> StoreResult<ManagementTombstonePin> {
        let file = child(
            &self.state.ancestor,
            "management-effects-v1",
            libc::O_RDONLY | libc::O_NONBLOCK,
        )?;
        private_file(&file).map_err(|_| StoreError::RecoveryRequired)?;
        if native_mount(&file)? != self.state.mount {
            return Err(StoreError::RecoveryRequired);
        }
        let metadata = file.metadata()?;
        let bytes = read_held_bytes(&file, &metadata, 64 * 1024)?;
        let tombstone: ManagementTombstoneV2 = serde_json::from_slice(&bytes)?;
        if tombstone.protocol_version != 2
            || tombstone.migration_id != id.as_hex()
            || tombstone.retired_source_anchor
                != EntryAnchorV2::of(&self.source.ancestor.metadata()?)
            || children
                .get("management-effects-v1")
                .is_none_or(|entry| file_stamp(entry) != file_stamp(&metadata))
        {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(ManagementTombstonePin {
            file,
            metadata,
            bytes,
        })
    }
    pub(super) fn source_target(&self) -> StoreResult<Option<(&File, EntryAnchorV2)>> {
        self.verify_current()?;
        if self.source.existing() && self.retirement_id.is_none() {
            Ok(Some((
                &self.state.ancestor,
                EntryAnchorV2::of(&self.source.ancestor.metadata()?),
            )))
        } else {
            Ok(None)
        }
    }
    pub(super) fn namespace_snapshot(&self) -> StoreResult<(Metadata, Vec<String>)> {
        self.verify_current()?;
        Ok((
            self.state_metadata.clone(),
            self.state_children.keys().cloned().collect(),
        ))
    }
    pub(super) fn needs_staging(&self) -> StoreResult<bool> {
        self.verify_current()?;
        Ok(self.retired.is_some() && self.stage.is_none())
    }
    pub(super) fn staging_parent(&self) -> StoreResult<&File> {
        self.verify_current()?;
        if self.retired.is_none() {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(&self.state.ancestor)
    }
    pub(super) fn bind_stage(mut self, file: File) -> StoreResult<Self> {
        if self.stage.is_some() || self.retired.is_none() {
            return Err(StoreError::RecoveryRequired);
        }
        self.state.verify_current()?;
        let metadata = self.state.ancestor.metadata()?;
        self.verify_workspace()?;
        self.history
            .as_ref()
            .ok_or(StoreError::RecoveryRequired)?
            .verify_read_snapshot()
            .map_err(management_error)?;
        let name = self.stage_name()?;
        let children = inspect_state_children(&self.state)?;
        let stage_metadata = file.metadata()?;
        owned_directory(&file, true).map_err(|_| StoreError::RecoveryRequired)?;
        if native_mount(&file)? != self.state.mount
            || !disk::bounded_directory_names(&file, 0)?.is_empty()
            || children.len() != self.state_children.len() + 1
            || children
                .get(&name)
                .is_none_or(|v| file_stamp(v) != file_stamp(&stage_metadata))
            || self.state_children.iter().any(|(name, old)| {
                children
                    .get(name)
                    .is_none_or(|v| file_stamp(v) != file_stamp(old))
            })
        {
            return Err(StoreError::RecoveryRequired);
        }
        self.stage = Some(ManagementStagePin {
            file,
            metadata: stage_metadata,
            tree: None,
        });
        self.state_children = children;
        self.state_metadata = metadata;
        self.verify_current()?;
        Ok(self)
    }
    pub(super) fn prepare_stage(&self) -> StoreResult<(&File, ManagementMigrationTreeV2)> {
        self.verify_current()?;
        let stage = self.stage.as_ref().ok_or(StoreError::RecoveryRequired)?;
        if stage.tree.is_some() {
            return Err(StoreError::RecoveryRequired);
        }
        let anchors = self.stage_anchors()?;
        let tree = self
            .history
            .as_ref()
            .ok_or(StoreError::RecoveryRequired)?
            .prepare_migration_v2(
                self.retirement_id.ok_or(StoreError::RecoveryRequired)?,
                anchors,
            )
            .map_err(management_error)?;
        self.verify_current()?;
        Ok((&stage.file, tree))
    }
    pub(super) fn finish_stage(mut self, tree: ManagementMigrationTreeV2) -> StoreResult<Self> {
        self.verify_current()?;
        let stage = self.stage.as_ref().ok_or(StoreError::RecoveryRequired)?;
        if stage.tree.is_some() {
            return Err(StoreError::RecoveryRequired);
        }
        tree.verify_staging(&stage.file).map_err(management_error)?;
        let expected = self
            .history
            .as_ref()
            .ok_or(StoreError::RecoveryRequired)?
            .prepare_migration_v2(
                self.retirement_id.ok_or(StoreError::RecoveryRequired)?,
                self.stage_anchors()?,
            )
            .map_err(management_error)?;
        expected
            .verify_staging(&stage.file)
            .map_err(management_error)?;
        let stage = self.stage.as_mut().ok_or(StoreError::RecoveryRequired)?;
        stage.metadata = stage.file.metadata()?;
        stage.tree = Some(tree);
        self.verify_current()?;
        Ok(self)
    }
    fn stage_name(&self) -> StoreResult<String> {
        Ok(format!(
            "management-effects-v2.staging-{}",
            self.retirement_id
                .ok_or(StoreError::RecoveryRequired)?
                .as_hex()
        ))
    }
    fn stage_anchors(&self) -> StoreResult<ManagementMigrationAnchorsV2> {
        let anchor = |file: &File| -> StoreResult<ManagementMigrationAnchorV2> {
            let m = file.metadata()?;
            Ok(ManagementMigrationAnchorV2 {
                device: m.dev(),
                inode: m.ino(),
                uid: m.uid(),
                mode: m.mode() & 0o7777,
                links: m.nlink(),
            })
        };
        Ok(ManagementMigrationAnchorsV2 {
            canonical_parent: anchor(&self.state.ancestor)?,
            retired_source: anchor(&self.source.ancestor)?,
            tombstone: anchor(
                &self
                    .retired
                    .as_ref()
                    .ok_or(StoreError::RecoveryRequired)?
                    .file,
            )?,
            active_v2_root: anchor(
                &self
                    .stage
                    .as_ref()
                    .ok_or(StoreError::RecoveryRequired)?
                    .file,
            )?,
        })
    }
    pub(super) fn verify_current(&self) -> StoreResult<()> {
        self.verify_coordinates()?;
        if let Some(history) = &self.history {
            history.verify_read_snapshot().map_err(management_error)?;
        }
        #[cfg(test)]
        run_hook(&AFTER_MANAGEMENT_VERIFY);
        self.verify_coordinates()?;
        if let Some(history) = &self.history {
            history.verify_read_snapshot().map_err(management_error)?;
        }
        self.verify_coordinates()
    }
    pub(super) fn migration_binding(
        &self,
    ) -> StoreResult<Option<polaris_mcp::migration_observation::ManagementMigrationBindingV2>> {
        use polaris_mcp::{
            migration_observation::ManagementMigrationBindingV2,
            store::ledger_v1::TombstoneLedgerAnchorV1,
        };
        self.verify_current()?;
        if self.history.is_none() {
            if self.retired.is_some() || self.stage.is_some() || self.retirement_id.is_some() {
                return Err(StoreError::RecoveryRequired);
            }
            self.verify_current()?;
            return Ok(None);
        }
        let stage = self.stage.as_ref().ok_or(StoreError::RecoveryRequired)?;
        let tree = stage.tree.as_ref().ok_or(StoreError::RecoveryRequired)?;
        let tombstone = self.retired.as_ref().ok_or(StoreError::RecoveryRequired)?;
        let manifest = tree.manifest();
        if Some(manifest.migration_id) != self.retirement_id
            || manifest.anchors != self.stage_anchors()?
        {
            return Err(StoreError::RecoveryRequired);
        }
        let binding = ManagementMigrationBindingV2 {
            state_path: self.state.path.as_os_str().as_encoded_bytes().to_vec(),
            migration_id: manifest.migration_id.as_hex().into(),
            project_scope: manifest.project_scope.as_hex().into(),
            canonical_parent_anchor: EntryAnchorV2::of(&self.state.ancestor.metadata()?),
            retired_source_anchor: EntryAnchorV2::of(&self.source.ancestor.metadata()?),
            tombstone: TombstoneLedgerAnchorV1 {
                anchor: EntryAnchorV2::of(&tombstone.file.metadata()?),
                file_hash: ManagementEffectHashV1::digest(&tombstone.bytes)
                    .as_hex()
                    .into(),
            },
            planned_active_v2_root_anchor: EntryAnchorV2::of(&stage.file.metadata()?),
            migration_manifest_hash: tree
                .manifest_file_hash()
                .map_err(management_error)?
                .as_hex()
                .into(),
        };
        self.verify_current()?;
        Ok(Some(binding))
    }
    // Historical coordinates can only deny runtime overlap; they never supply
    // a live project capability. The retained observer still closes the source.
    pub(super) fn runtime_exclusions(&self) -> StoreResult<(String, Vec<PathBuf>)> {
        self.verify_current()?;
        let id = self
            .state
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(StoreError::RecoveryRequired)?
            .to_owned();
        let storage = self
            .state
            .path
            .parent()
            .and_then(Path::parent)
            .ok_or(StoreError::RecoveryRequired)?;
        let mut paths = vec![
            storage.join("state"),
            storage.join("sessions"),
            storage.join("sessions-v2"),
        ];
        if let Some(namespace) = &self.historical {
            paths.push(namespace.project_root.clone());
        }
        Ok((id, paths))
    }
    fn verify_coordinates(&self) -> StoreResult<()> {
        self.state.verify_current()?;
        self.workspace.verify_current()?;
        self.source.verify_current()?;
        if file_stamp(&self.state.ancestor.metadata()?) != file_stamp(&self.state_metadata) {
            return Err(StoreError::RecoveryRequired);
        }
        let children = inspect_state_children(&self.state)?;
        let stage_name = self.stage.as_ref().map(|_| self.stage_name()).transpose()?;
        if children.len() != self.state_children.len()
            || children.iter().any(|(name, meta)| {
                if stage_name.as_ref() == Some(name) {
                    return false;
                }
                self.state_children
                    .get(name)
                    .is_none_or(|old| file_stamp(meta) != file_stamp(old))
            })
        {
            return Err(StoreError::RecoveryRequired);
        }
        if let Some(stage) = &self.stage {
            let named = child(
                &self.state.ancestor,
                &self.stage_name()?,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
            )?;
            let identity = |m: &Metadata| (m.dev(), m.ino(), m.uid(), m.mode());
            for file in [&stage.file, &named] {
                owned_directory(file, true).map_err(|_| StoreError::RecoveryRequired)?;
                if native_mount(file)? != self.state.mount
                    || identity(&file.metadata()?) != identity(&stage.metadata)
                {
                    return Err(StoreError::RecoveryRequired);
                }
                if let Some(tree) = &stage.tree {
                    if file_stamp(&file.metadata()?) != file_stamp(&stage.metadata) {
                        return Err(StoreError::RecoveryRequired);
                    }
                    tree.verify_staging(file).map_err(management_error)?;
                }
            }
        }
        if let Some(pin) = &self.retired {
            let named = child(
                &self.state.ancestor,
                "management-effects-v1",
                libc::O_RDONLY | libc::O_NONBLOCK,
            )?;
            for file in [&pin.file, &named] {
                private_file(file).map_err(|_| StoreError::RecoveryRequired)?;
                if native_mount(file)? != self.state.mount
                    || file_stamp(&file.metadata()?) != file_stamp(&pin.metadata)
                    || read_held_bytes(file, &pin.metadata, 64 * 1024)? != pin.bytes
                {
                    return Err(StoreError::RecoveryRequired);
                }
            }
        }
        self.verify_workspace()
    }
    fn verify_workspace(&self) -> StoreResult<()> {
        self.workspace.verify_current()?;
        if let Some(metadata) = &self.workspace_metadata {
            if file_stamp(&self.workspace.ancestor.metadata()?) != file_stamp(metadata) {
                return Err(StoreError::RecoveryRequired);
            }
        }
        if let (Some(owner), Some(lease)) = (&self.owner, &self.lease) {
            owner
                .verify_migration(lease, &self.state.mount)
                .map_err(project_error)?;
        }
        for (name, pin) in [
            ("writer.lease", &self.lock),
            ("namespace.json", &self.namespace),
        ] {
            if !self.workspace.existing() {
                continue;
            }
            let named = child(&self.workspace.ancestor, name, libc::O_RDONLY);
            match (pin, named) {
                (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => (),
                (Some((file, metadata)), Ok(named)) => {
                    for file in [file, &named] {
                        private_file(file).map_err(|_| StoreError::RecoveryRequired)?;
                        if file_stamp(&file.metadata()?) != file_stamp(metadata)
                            || native_mount(file)? != self.state.mount
                        {
                            return Err(StoreError::RecoveryRequired);
                        }
                    }
                }
                _ => return Err(StoreError::RecoveryRequired),
            }
        }
        if let Some(project) = &self.live_project {
            project.verify_current()?;
            let ns = self
                .historical
                .as_ref()
                .ok_or(StoreError::RecoveryRequired)?;
            if (project.anchor.device, project.anchor.inode) != (ns.root_device, ns.root_inode) {
                return Err(StoreError::RecoveryRequired);
            }
        }
        Ok(())
    }
}
fn inspect_state_children(state: &PathPin) -> StoreResult<BTreeMap<String, Metadata>> {
    state.verify_current()?;
    let before = state.ancestor.metadata()?;
    let names = disk::bounded_directory_names(&state.ancestor, MAX_ROOTS)?;
    let mut entries = BTreeMap::new();
    for name in names {
        let file = child(&state.ancestor, &name, libc::O_RDONLY | libc::O_NONBLOCK)?;
        if native_mount(&file)? != state.mount {
            return Err(StoreError::RecoveryRequired);
        }
        let metadata = file.metadata()?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(StoreError::RecoveryRequired);
        }
        entries.insert(name, metadata);
    }
    for (name, expected) in &entries {
        let file = child(&state.ancestor, name, libc::O_RDONLY | libc::O_NONBLOCK)?;
        if native_mount(&file)? != state.mount
            || file_stamp(&file.metadata()?) != file_stamp(expected)
        {
            return Err(StoreError::RecoveryRequired);
        }
    }
    if file_stamp(&state.ancestor.metadata()?) != file_stamp(&before) {
        return Err(StoreError::RecoveryRequired);
    }
    state.verify_current()?;
    Ok(entries)
}
fn normalized(path: &Path) -> StoreResult<()> {
    if !path.is_absolute()
        || path
            .as_os_str()
            .as_encoded_bytes()
            .split(|b| *b == b'/')
            .skip(1)
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(())
}
fn live_identity(ns: &HistoricalNamespace) -> StoreResult<Option<PathPin>> {
    // Deleted/replaced projects retain only the historical coordinate, never
    // a substitute live path. Existing matching roots remain descriptor-pinned.
    let file = match crate::project_mutation::open_directory_nofollow(&ns.project_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => result?,
    };
    let metadata = file.metadata()?;
    if (metadata.dev(), metadata.ino()) != (ns.root_device, ns.root_inode) {
        return Ok(None);
    }
    PathPin::inspect(&ns.project_root).map(Some)
}
fn project_error(error: crate::project_mutation::ProjectMutationError) -> StoreError {
    match error {
        crate::project_mutation::ProjectMutationError::Busy => StoreError::Busy,
        crate::project_mutation::ProjectMutationError::Io(error) => StoreError::Io(error),
        _ => StoreError::RecoveryRequired,
    }
}
fn management_error(error: crate::management_effect::ManagementEffectStoreError) -> StoreError {
    match error {
        crate::management_effect::ManagementEffectStoreError::Io(error)
            if error.kind() == std::io::ErrorKind::WouldBlock =>
        {
            StoreError::Busy
        }
        _ => StoreError::RecoveryRequired,
    }
}
#[cfg(test)]
thread_local! {
    static AFTER_MANAGEMENT_HISTORY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_MANAGEMENT_VERIFY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
