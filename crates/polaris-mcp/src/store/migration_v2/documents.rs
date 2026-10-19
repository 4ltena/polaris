//! Read-only v1 document pins and directly compared v2 baseline preparation.
use super::paths::{ManagedPathTransformV2, transform_managed_paths_v2};
use crate::{
    Error,
    entry_gate::{EntryAnchorV2, child, owned_directory, private_file},
    registration::{MAX_STORE_BYTES, Registry},
    store::{
        Document, SecretDocument, Secrets, check_references,
        documents_v2::{CredentialDocumentV2, RegistryDocumentV2},
    },
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, Metadata},
    io,
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
};
use zeroize::Zeroizing;

/// Pins documents only. The caller must retain the canonical source-tree and
/// gate/stop capabilities; this view is not a complete inventory or stop proof.
pub struct SourceDocumentsV1 {
    root: File,
    root_metadata: Metadata,
    registry_file: Option<SourceFileV1>,
    secret_file: Option<SourceFileV1>,
    registry: Option<Registry>,
}
struct SourceFileV1 {
    file: File,
    metadata: Metadata,
    // Both buffers are guarded so a type/field decode error cannot leave the
    // source credential bytes in an ordinary Vec allocation.
    bytes: Zeroizing<Vec<u8>>,
}
pub struct BaselineDocumentsV2 {
    pub credential: CredentialDocumentV2,
    pub registry: RegistryDocumentV2,
    pub mapped_paths: Option<ManagedPathTransformV2>,
}
impl BaselineDocumentsV2 {
    /// Pure schema/reference validation, without exposing secret values or
    /// granting descriptor, mutation, stopping or activation authority.
    pub fn validate(&self) -> Result<(), Error> {
        self.credential.validate()?;
        self.registry.validate()?;
        if let Some(registry) = self.registry.snapshot() {
            self.credential.validate_registry_references(registry)?;
        }
        Ok(())
    }
}
/// Read the fixed credential name through the caller's retained descriptors.
/// Bytes stay zeroizing; root/gate ownership and activation remain with the host.
pub fn read_baseline_credential(root: &File, file: &File) -> Result<CredentialDocumentV2, Error> {
    let mount = crate::NativeMountIdentityV2::inspect(root).map_err(|_| Error::Protection)?;
    if crate::NativeMountIdentityV2::inspect(file).map_err(|_| Error::Protection)? != mount {
        return Err(Error::Protection);
    }
    let metadata = file.metadata().map_err(storage)?;
    let bytes = read_pinned(root, "secrets.json", file, &metadata)?;
    let document = CredentialDocumentV2::decode(&bytes)?;
    let named = child(root, "secrets.json", libc::O_RDONLY).map_err(|_| Error::Protection)?;
    if crate::NativeMountIdentityV2::inspect(root).map_err(|_| Error::Protection)? != mount
        || crate::NativeMountIdentityV2::inspect(file).map_err(|_| Error::Protection)? != mount
        || crate::NativeMountIdentityV2::inspect(&named).map_err(|_| Error::Protection)? != mount
        || !stable(&metadata, &file.metadata().map_err(storage)?)
        || !stable(&metadata, &named.metadata().map_err(storage)?)
    {
        return Err(Error::Protection);
    }
    Ok(document)
}
impl SourceDocumentsV1 {
    pub fn inspect(root: &File) -> Result<Self, Error> {
        owned_directory(root, true).map_err(|_| Error::Protection)?;
        let root_metadata = root.metadata().map_err(storage)?;
        let root = root.try_clone().map_err(storage)?;
        let registry_file = SourceFileV1::inspect_optional(&root, "servers.json")?;
        let secret_file = SourceFileV1::inspect_optional(&root, "secrets.json")?;
        let registry: Option<Registry> = registry_file
            .as_ref()
            .map(|pin| serde_json::from_slice(&pin.bytes).map_err(|_| Error::Protocol))
            .transpose()?;
        if let Some(registry) = &registry {
            registry.validate()?;
        }
        let secrets = decode_secrets(secret_file.as_ref())?;
        if let Some(registry) = &registry {
            check_references(registry, secrets.as_ref().map(|value| &value.0))?;
        }
        let view = Self {
            root,
            root_metadata,
            registry_file,
            secret_file,
            registry,
        };
        view.verify_current()?;
        Ok(view)
    }

    pub fn verify_current(&self) -> Result<(), Error> {
        owned_directory(&self.root, true).map_err(|_| Error::Protection)?;
        if !stable(&self.root_metadata, &self.root.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        // A second pass seals changes made between the two document reads.
        for _ in 0..2 {
            SourceFileV1::verify_optional(&self.root, "servers.json", &self.registry_file)?;
            SourceFileV1::verify_optional(&self.root, "secrets.json", &self.secret_file)?;
        }
        if !stable(&self.root_metadata, &self.root.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        Ok(())
    }

    /// Reobserve only the same retired root. A rename can change root ctime;
    /// document identities, full stamps, bytes and absence remain unchanged.
    /// The host owns the rename and its external parent/name binding.
    pub fn inspect_retired_same_root(&self, root: &File) -> Result<Self, Error> {
        let verify = || -> Result<(), Error> {
            for file in [&self.root, root] {
                owned_directory(file, true).map_err(|_| Error::Protection)?;
                let actual = file.metadata().map_err(storage)?;
                let old = &self.root_metadata;
                if EntryAnchorV2::of(&actual) != EntryAnchorV2::of(old)
                    || actual.len() != old.len()
                    || (actual.mtime(), actual.mtime_nsec()) != (old.mtime(), old.mtime_nsec())
                {
                    return Err(Error::Protection);
                }
            }
            SourceFileV1::verify_optional(&self.root, "servers.json", &self.registry_file)?;
            SourceFileV1::verify_optional(&self.root, "secrets.json", &self.secret_file)
        };
        verify()?;
        let fresh = Self::inspect(root)?;
        for (old, new) in [
            (&self.registry_file, &fresh.registry_file),
            (&self.secret_file, &fresh.secret_file),
        ] {
            match (old, new) {
                (None, None) => (),
                (Some(old), Some(new))
                    if stable(&old.metadata, &new.metadata)
                        && old.bytes.as_slice() == new.bytes.as_slice() => {}
                _ => return Err(Error::Protection),
            }
        }
        verify()?;
        fresh.verify_current()?;
        Ok(fresh)
    }

    /// The borrowed v1 payload is usable only while this document view remains
    /// pinned; subsequent phases must verify_current again before acceptance.
    pub fn verified_registry(&self) -> Result<Option<&Registry>, Error> {
        self.verify_current()?;
        Ok(self.registry.as_ref())
    }

    /// Uses the fixed typed transform, not caller-supplied mapped document
    /// bytes. Every source credential comparison remains in zeroizing memory.
    pub fn baselines(
        &self,
        migration_id: &str,
        logical_home: &Path,
        pins: &BTreeMap<String, Vec<u8>>,
    ) -> Result<BaselineDocumentsV2, Error> {
        self.verify_current()?;
        let mapped_paths = match &self.registry {
            Some(registry) => Some(transform_managed_paths_v2(registry, logical_home, pins)?),
            None if pins.is_empty() => None,
            None => return Err(Error::InvalidRegistration),
        };
        let next_registry = mapped_paths.as_ref().map(|mapped| mapped.registry.clone());
        let source_file_hash = self
            .registry_file
            .as_ref()
            .map(|pin| format!("{:x}", Sha256::digest(pin.bytes.as_slice())));
        let registry = RegistryDocumentV2::baseline(migration_id, next_registry, source_file_hash)?;
        let credential = CredentialDocumentV2::baseline(
            migration_id,
            decode_secrets(self.secret_file.as_ref())?,
        )?;
        // Reparse the retained source with immediate per-value wipe guards and
        // compare references, value bytes and revision directly. No value hash
        // or size is returned, serialized or displayed.
        let source = decode_secrets(self.secret_file.as_ref())?;
        credential.verify_preserved_source(source.as_ref())?;
        self.verify_current()?;
        Ok(BaselineDocumentsV2 {
            credential,
            registry,
            mapped_paths,
        })
    }

    pub fn verify_credential_baseline(&self, baseline: &CredentialDocumentV2) -> Result<(), Error> {
        self.verify_current()?;
        let source = decode_secrets(self.secret_file.as_ref())?;
        baseline.verify_preserved_source(source.as_ref())?;
        self.verify_current()
    }
}
fn decode_secrets(pin: Option<&SourceFileV1>) -> Result<Option<Secrets>, Error> {
    pin.map(|pin| {
        let document: SecretDocument =
            serde_json::from_slice(&pin.bytes).map_err(|_| Error::Protocol)?;
        document.validate()?;
        Ok(Secrets(document))
    })
    .transpose()
}
impl SourceFileV1 {
    fn inspect_optional(root: &File, name: &str) -> Result<Option<Self>, Error> {
        let file = match child(root, name, libc::O_RDONLY | libc::O_NONBLOCK) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result.map_err(|_| Error::Protection)?,
        };
        private_file(&file).map_err(|_| Error::Protection)?;
        let metadata = file.metadata().map_err(storage)?;
        if metadata.len() > MAX_STORE_BYTES as u64 {
            return Err(Error::Limit);
        }
        let bytes = read_pinned(root, name, &file, &metadata)?;
        Ok(Some(Self {
            file,
            metadata,
            bytes,
        }))
    }
    fn verify_optional(root: &File, name: &str, pin: &Option<Self>) -> Result<(), Error> {
        match pin {
            Some(pin) => {
                let current = read_pinned(root, name, &pin.file, &pin.metadata)?;
                if current.as_slice() != pin.bytes.as_slice() {
                    return Err(Error::Protection);
                }
            }
            None => match child(root, name, libc::O_RDONLY | libc::O_NONBLOCK) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    #[cfg(test)]
                    if let Some(hook) = AFTER_ABSENT_DOCUMENT.with(|slot| slot.borrow_mut().take())
                    {
                        hook(name);
                    }
                }
                _ => return Err(Error::Protection),
            },
        }
        Ok(())
    }
}
fn read_pinned(
    root: &File,
    name: &str,
    file: &File,
    expected: &Metadata,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    private_file(file).map_err(|_| Error::Protection)?;
    let before = file.metadata().map_err(storage)?;
    if before.len() > MAX_STORE_BYTES as u64 {
        return Err(Error::Limit);
    }
    if !stable(expected, &before) {
        return Err(Error::Protection);
    }
    let mut bytes = Zeroizing::new(vec![0; before.len() as usize]);
    let mut offset = 0;
    while offset < bytes.len() {
        match file.read_at(&mut bytes[offset..], offset as u64) {
            Ok(0) => return Err(Error::Protection),
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(storage(error)),
        }
    }
    let named =
        child(root, name, libc::O_RDONLY | libc::O_NONBLOCK).map_err(|_| Error::Protection)?;
    private_file(&named).map_err(|_| Error::Protection)?;
    if !stable(expected, &file.metadata().map_err(storage)?)
        || !stable(expected, &named.metadata().map_err(storage)?)
    {
        return Err(Error::Protection);
    }
    Ok(bytes)
}
fn stable(a: &Metadata, b: &Metadata) -> bool {
    EntryAnchorV2::of(a) == EntryAnchorV2::of(b)
        && a.len() == b.len()
        && (a.mtime(), a.mtime_nsec(), a.ctime(), a.ctime_nsec())
            == (b.mtime(), b.mtime_nsec(), b.ctime(), b.ctime_nsec())
}
#[cfg(test)]
std::thread_local! {
    // One-shot thread-owned injection only after the exact absence read.
    static AFTER_ABSENT_DOCUMENT: std::cell::RefCell<Option<Box<dyn FnOnce(&str)>>> =
        const { std::cell::RefCell::new(None) };
}
fn storage(_: io::Error) -> Error {
    Error::Storage
}

#[cfg(test)]
mod tests;
