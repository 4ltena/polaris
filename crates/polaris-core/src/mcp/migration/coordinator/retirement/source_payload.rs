//! Read-only source preparation borrowed from the retained retirement owner.
//! It grants no staging write, durable phase transition or activation authority.
use super::{verify_mount, *};
use polaris_mcp::registration::{MAX_JSON_BYTES, Registry};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(in crate::mcp::migration::coordinator) struct SourcePayloadV2<'a> {
    source: &'a SourcePreflightV2,
    logical_home: PathBuf,
    pub(super) inventory: Vec<SourceInventoryEntryV2>,
    pub(super) documents: BaselineDocumentsV2,
    pub(super) pins: BTreeMap<String, PreparedPinV2>,
    pub(super) unregistered_pins: Vec<UnregisteredPinMappingV2>,
    pub(super) catalog: CatalogRegenerationRecordV2,
}
pub(super) struct PreparedPinV2 {
    relative_path: Vec<u8>,
    metadata: Metadata,
    source_sha256: String,
}
impl<'a> SourcePayloadV2<'a> {
    pub(super) fn inspect(
        source: &'a SourcePreflightV2,
        id: &str,
        logical_home: &Path,
    ) -> Result<Self, Error> {
        let (tree, documents) = source.staging_observations()?;
        let copy = tree.verify_before_copy()?;
        let mut inventory = Vec::new();
        let mut pins: BTreeMap<String, PreparedPinV2> = BTreeMap::new();
        for entry in copy.entries() {
            let relative_path = entry.relative_path().to_vec();
            if unfinished_pin(&relative_path)
                || (entry.is_directory() && pin_id(&relative_path).is_some())
            {
                return Err(Error::Protection);
            }
            let anchor = entry.anchor();
            inventory.push(if entry.is_directory() {
                SourceInventoryEntryV2::Directory {
                    relative_path,
                    anchor,
                }
            } else if relative_path == b"secrets.json" {
                // Never open a secret through the public streaming/hash path.
                // SourceDocuments alone reads it in zeroizing memory.
                SourceInventoryEntryV2::SecretDocument {
                    relative_path,
                    anchor,
                }
            } else {
                let file = entry.open_readonly()?;
                verify_mount(source.held_root(), &file)?;
                let metadata = file.metadata().map_err(storage)?;
                let sha256 = public_hash(&file, &metadata)?;
                verify_mount(source.held_root(), &file)?;
                if let Some(server_id) = pin_id(&relative_path) {
                    pins.insert(
                        server_id.into(),
                        PreparedPinV2 {
                            relative_path: relative_path.clone(),
                            metadata: metadata.clone(),
                            source_sha256: sha256.clone(),
                        },
                    );
                }
                SourceInventoryEntryV2::PublicFile {
                    relative_path,
                    anchor,
                    size: metadata.len(),
                    sha256,
                }
            });
        }
        copy.verify_after_copy()?;
        source.verify_current()?;
        let registry = documents.verified_registry()?;
        let registered = |id: &str| {
            registry.is_some_and(|r| r.servers.iter().any(|server| server.server_id == id))
        };
        let registered_pins = pins
            .iter()
            .filter(|(id, _)| registered(id))
            .map(|(id, pin)| Ok((id.clone(), pin.read_verified(source)?)))
            .collect::<Result<BTreeMap<_, _>, Error>>()?;
        let baseline = documents.baselines(id, logical_home, &registered_pins)?;
        drop(registered_pins);
        baseline.validate()?;
        let mut unregistered_pins = Vec::new();
        for (server_id, pin) in &pins {
            if registered(server_id) {
                baseline
                    .mapped_paths
                    .as_ref()
                    .and_then(|m| m.pins.get(server_id))
                    .ok_or(Error::InvalidRegistration)?;
            } else {
                let bytes = pin.read_verified(source)?;
                let transformed =
                    transform_unregistered_pin_v2(registry, server_id, &bytes, logical_home)?;
                unregistered_pins.push(UnregisteredPinMappingV2 {
                    server_id: server_id.clone(),
                    fields: transformed.fields.try_into().map_err(|_| Error::Protocol)?,
                });
            }
        }
        let catalog = CatalogRegenerationRecordV2::new(id, &baseline.registry)?;
        let value = Self {
            source,
            logical_home: logical_home.into(),
            inventory,
            documents: baseline,
            pins,
            unregistered_pins,
            catalog,
        };
        preparation_complete();
        value.verify_current()?;
        Ok(value)
    }

    pub(super) fn verify_current(&self) -> Result<(), Error> {
        let (_, source_documents) = self.source.staging_observations()?;
        let registry = source_documents.verified_registry()?;
        for server_id in self.pins.keys() {
            // Drop each buffer before opening the next unregistered pin.
            self.encode_pin(server_id, registry)?;
        }
        source_documents.verify_credential_baseline(&self.documents.credential)?;
        self.documents.validate()?;
        self.catalog.validate_registry(&self.documents.registry)?;
        self.source.verify_current()
    }

    pub(super) fn open_pin(&self, server_id: &str) -> Result<File, Error> {
        self.pins
            .get(server_id)
            .ok_or(Error::Protocol)?
            .open_verified(self.source)
    }

    pub(super) fn staged_pin_bytes(&self, server_id: &str) -> Result<Vec<u8>, Error> {
        let (_, documents) = self.source.staging_observations()?;
        let bytes = self.encode_pin(server_id, documents.verified_registry()?)?;
        self.source.verify_current()?;
        Ok(bytes)
    }
    fn encode_pin(&self, server_id: &str, registry: Option<&Registry>) -> Result<Vec<u8>, Error> {
        let pin = self.pins.get(server_id).ok_or(Error::Protocol)?;
        let source_bytes = pin.read_verified(self.source)?;
        let mapped = if let Some(row) = self
            .unregistered_pins
            .binary_search_by(|r| r.server_id.as_str().cmp(server_id))
            .ok()
            .map(|index| &self.unregistered_pins[index])
        {
            let transformed = transform_unregistered_pin_v2(
                registry,
                server_id,
                &source_bytes,
                &self.logical_home,
            )?;
            if transformed.fields.as_slice() != row.fields {
                return Err(Error::Protection);
            }
            transformed.pin
        } else {
            self.documents
                .mapped_paths
                .as_ref()
                .and_then(|m| m.pins.get(server_id))
                .ok_or(Error::InvalidRegistration)?
                .clone()
        };
        let bytes = serde_json::to_vec(&mapped).map_err(|_| Error::Protocol)?;
        if bytes.len() > MAX_JSON_BYTES {
            return Err(Error::Limit);
        }
        Ok(bytes)
    }
}
impl PreparedPinV2 {
    fn open_verified(&self, source: &SourcePreflightV2) -> Result<File, Error> {
        let copy = source.staging_observations()?.0.verify_before_copy()?;
        let entry = copy
            .entries()
            .find(|entry| entry.relative_path() == self.relative_path)
            .ok_or(Error::Protection)?;
        let file = entry.open_readonly()?;
        verify_mount(source.held_root(), &file)?;
        if !same_metadata(&self.metadata, &file.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        copy.verify_after_copy()?;
        Ok(file)
    }
    fn read_verified(&self, source: &SourcePreflightV2) -> Result<Vec<u8>, Error> {
        let file = self.open_verified(source)?;
        let bytes = read_pin(source.held_root(), &file, &self.metadata)?;
        if format!("{:x}", Sha256::digest(&bytes)) != self.source_sha256 {
            return Err(Error::Protection);
        }
        Ok(bytes)
    }
}
fn pin_id(path: &[u8]) -> Option<&str> {
    let mut components = path.split(|b| *b == b'/');
    if components.next()? != b"servers" {
        return None;
    }
    let id = std::str::from_utf8(components.next()?).ok()?;
    (components.next()? == b"pinned.json" && components.next().is_none()).then_some(id)
}
fn unfinished_pin(path: &[u8]) -> bool {
    let mut components = path.split(|b| *b == b'/');
    components.next() == Some(b"servers".as_slice())
        && components.next().is_some()
        && components.next() == Some(b"pinned.json.partial".as_slice())
        && components.next().is_none()
}
fn read_pin(root: &File, file: &File, metadata: &Metadata) -> Result<Vec<u8>, Error> {
    verify_mount(root, file)?;
    if metadata.len() > MAX_JSON_BYTES as u64 {
        return Err(Error::Limit);
    }
    if !same_metadata(metadata, &file.metadata().map_err(storage)?) {
        return Err(Error::Protection);
    }
    let mut bytes = vec![0; metadata.len() as usize];
    file.read_exact_at(&mut bytes, 0).map_err(storage)?;
    verify_mount(root, file)?;
    if !same_metadata(metadata, &file.metadata().map_err(storage)?) {
        return Err(Error::Protection);
    }
    Ok(bytes)
}
// Called only for inventoried non-secret entries. Stream public package bytes
// without adding a new whole-store size limit; require exact length and EOF.
pub(super) fn public_hash(file: &File, metadata: &Metadata) -> Result<String, Error> {
    if !same_metadata(metadata, &file.metadata().map_err(storage)?) {
        return Err(Error::Protection);
    }
    let mut hash = Sha256::new();
    let mut bytes = [0; 32 * 1024];
    let mut offset = 0;
    while offset < metadata.len() {
        let limit = (metadata.len() - offset).min(bytes.len() as u64) as usize;
        let n = match file.read_at(&mut bytes[..limit], offset) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result.map_err(storage)?,
        };
        if n == 0 {
            return Err(Error::Protection);
        }
        hash.update(&bytes[..n]);
        offset += n as u64;
    }
    if file.read_at(&mut bytes[..1], offset).map_err(storage)? != 0
        || !same_metadata(metadata, &file.metadata().map_err(storage)?)
    {
        return Err(Error::Protection);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn preparation_complete() {
    #[cfg(test)]
    AFTER_PREPARATION.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}
#[cfg(test)]
thread_local! {
    static AFTER_PREPARATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
