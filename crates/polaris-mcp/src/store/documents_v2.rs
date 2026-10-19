//! Closed v2 document envelopes for the store migration coordinator.
//!
//! This codec performs no filesystem mutation. The coordinator must publish
//! the envelopes, epoch and gate in one verified activation transaction.
use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{Document, SecretDocument, Secrets};
use crate::Error;
use crate::registration::{MAX_STORE_BYTES, Registry, valid_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKindV2 {
    Credential,
    Registry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentPresenceV2 {
    Absent,
    Present,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentMetadataV2 {
    pub kind: DocumentKindV2,
    pub presence: DocumentPresenceV2,
    pub revision: u64,
    pub token: String,
    pub receipt_hash: String,
}
impl DocumentMetadataV2 {
    pub fn validate(&self) -> Result<(), Error> {
        if !digest_id(&self.token)
            || !digest_id(&self.receipt_hash)
            || (self.presence == DocumentPresenceV2::Absent) != (self.revision == 0)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryBaselineProjectionV1 {
    pub source_file_hash: Option<String>,
    pub canonical_payload_hash: String,
}

/// This receipt contains only metadata and reference names projected to a hash.
/// Secret document bytes, sizes and value-derived hashes have no wire fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationBaselineReceiptV1 {
    pub migration_id: String,
    pub document_kind: DocumentKindV2,
    pub source_presence: DocumentPresenceV2,
    pub source_schema_version: Option<u32>,
    pub preserved_revision: u64,
    pub random_token: String,
    pub reference_set_hash: String,
    pub registry_projection: Option<RegistryBaselineProjectionV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedDocumentReceiptV1 {
    pub operation_id: String,
    pub plan_hash: String,
    pub document_kind: DocumentKindV2,
    pub before: DocumentMetadataV2,
    pub after_token: String,
    pub after_revision: u64,
    pub reference_set_hash: String,
    pub registry_payload_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentReceiptV2 {
    MigrationBaseline { receipt: MigrationBaselineReceiptV1 },
    Managed { receipt: ManagedDocumentReceiptV1 },
}
impl DocumentReceiptV2 {
    pub fn canonical_hash(&self) -> Result<String, Error> {
        safe_hash(self)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretEnvelopeWireV2 {
    schema_version: u32,
    random_token: String,
    receipt: DocumentReceiptV2,
    payload: Option<SecretDocument>,
}

/// Secret-bearing wire data has no public serializer or plaintext Debug.
pub struct CredentialDocumentV2(SecretEnvelopeWireV2);
impl fmt::Debug for CredentialDocumentV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialDocumentV2(<redacted>)")
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryEnvelopeWireV2 {
    schema_version: u32,
    random_token: String,
    receipt: DocumentReceiptV2,
    payload: Option<Registry>,
}
pub struct RegistryDocumentV2(RegistryEnvelopeWireV2);
impl fmt::Debug for RegistryDocumentV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryDocumentV2")
            .field("metadata", &self.metadata())
            .finish_non_exhaustive()
    }
}

fn digest_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn safe_hash<T: Serialize>(value: &T) -> Result<String, Error> {
    let bytes = serde_json::to_vec(value).map_err(|_| Error::Protocol)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
#[cfg(unix)]
fn fresh_token() -> Result<String, Error> {
    let mut bytes = [0_u8; 32];
    // Kernel CSPRNG, with no caller-controlled seed or filesystem fallback.
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(Error::Auth);
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
#[cfg(not(unix))]
fn fresh_token() -> Result<String, Error> {
    Err(Error::Protection)
}
fn presence<T>(payload: &Option<T>) -> DocumentPresenceV2 {
    if payload.is_some() {
        DocumentPresenceV2::Present
    } else {
        DocumentPresenceV2::Absent
    }
}
fn reference_hash<'a>(references: impl Iterator<Item = &'a str>) -> Result<String, Error> {
    let references: BTreeSet<_> = references.collect();
    if references.iter().any(|value| !valid_id(value)) {
        return Err(Error::Auth);
    }
    safe_hash(&references)
}
fn registry_reference_hash(payload: &Option<Registry>) -> Result<String, Error> {
    let references: BTreeSet<&str> = payload
        .iter()
        .flat_map(|registry| {
            registry.servers.iter().flat_map(|server| {
                match &server.transport {
                    crate::registration::Destination::Stdio { secret_env, .. } => secret_env,
                    crate::registration::Destination::StreamableHttp { secret_headers, .. } => {
                        secret_headers
                    }
                }
                .values()
                .map(String::as_str)
            })
        })
        .collect();
    reference_hash(references.into_iter())
}
fn metadata(
    kind: DocumentKindV2,
    presence: DocumentPresenceV2,
    revision: u64,
    token: &str,
    receipt: &DocumentReceiptV2,
) -> Result<DocumentMetadataV2, Error> {
    let result = DocumentMetadataV2 {
        kind,
        presence,
        revision,
        token: token.to_owned(),
        receipt_hash: receipt.canonical_hash()?,
    };
    result.validate()?;
    Ok(result)
}

fn validate_receipt(
    receipt: &DocumentReceiptV2,
    kind: DocumentKindV2,
    presence: DocumentPresenceV2,
    revision: u64,
    token: &str,
    references: &str,
    registry_payload_hash: Option<&str>,
) -> Result<(), Error> {
    if !digest_id(token) || !digest_id(references) {
        return Err(Error::Protocol);
    }
    match receipt {
        DocumentReceiptV2::MigrationBaseline { receipt } => {
            if !digest_id(&receipt.migration_id)
                || receipt.document_kind != kind
                || receipt.source_presence != presence
                || receipt.source_schema_version
                    != match presence {
                        DocumentPresenceV2::Absent => None,
                        DocumentPresenceV2::Present => Some(1),
                    }
                || receipt.preserved_revision != revision
                || receipt.random_token != token
                || receipt.reference_set_hash != references
            {
                return Err(Error::Protocol);
            }
            match (&receipt.registry_projection, registry_payload_hash) {
                (None, None) => {}
                (Some(projection), Some(expected)) => {
                    if projection.canonical_payload_hash != expected
                        || !digest_id(expected)
                        || match presence {
                            DocumentPresenceV2::Absent => projection.source_file_hash.is_some(),
                            DocumentPresenceV2::Present => !projection
                                .source_file_hash
                                .as_deref()
                                .is_some_and(digest_id),
                        }
                    {
                        return Err(Error::Protocol);
                    }
                }
                _ => return Err(Error::Protocol),
            }
        }
        DocumentReceiptV2::Managed { receipt } => {
            receipt.before.validate()?;
            if !valid_id(&receipt.operation_id)
                || !digest_id(&receipt.plan_hash)
                || receipt.document_kind != kind
                || receipt.before.kind != kind
                || presence != DocumentPresenceV2::Present
                || receipt.before.revision.checked_add(1) != Some(revision)
                || receipt.after_revision != revision
                || receipt.after_token != token
                || receipt.after_token == receipt.before.token
                || receipt.reference_set_hash != references
                || receipt.registry_payload_hash.as_deref() != registry_payload_hash
            {
                return Err(Error::Protocol);
            }
        }
    }
    Ok(())
}

fn baseline_receipt(
    migration_id: &str,
    kind: DocumentKindV2,
    presence: DocumentPresenceV2,
    revision: u64,
    token: &str,
    references: String,
    registry_projection: Option<RegistryBaselineProjectionV1>,
) -> DocumentReceiptV2 {
    DocumentReceiptV2::MigrationBaseline {
        receipt: MigrationBaselineReceiptV1 {
            migration_id: migration_id.to_owned(),
            document_kind: kind,
            source_presence: presence,
            source_schema_version: match presence {
                DocumentPresenceV2::Absent => None,
                DocumentPresenceV2::Present => Some(1),
            },
            preserved_revision: revision,
            random_token: token.to_owned(),
            reference_set_hash: references,
            registry_projection,
        },
    }
}
fn managed_receipt(
    operation_id: &str,
    plan_hash: &str,
    before: DocumentMetadataV2,
    after_token: &str,
    after_revision: u64,
    reference_set_hash: String,
    registry_payload_hash: Option<String>,
) -> DocumentReceiptV2 {
    DocumentReceiptV2::Managed {
        receipt: ManagedDocumentReceiptV1 {
            operation_id: operation_id.to_owned(),
            plan_hash: plan_hash.to_owned(),
            document_kind: before.kind,
            before,
            after_token: after_token.to_owned(),
            after_revision,
            reference_set_hash,
            registry_payload_hash,
        },
    }
}

impl CredentialDocumentV2 {
    pub fn baseline(migration_id: &str, source: Option<Secrets>) -> Result<Self, Error> {
        Self::baseline_with_token(migration_id, source, fresh_token()?)
    }
    fn baseline_with_token(
        migration_id: &str,
        source: Option<Secrets>,
        token: String,
    ) -> Result<Self, Error> {
        let payload = source.map(|source| source.0);
        let revision = payload.as_ref().map_or(0, |value| value.revision);
        let references = reference_hash(
            payload
                .iter()
                .flat_map(|value| value.values.keys().map(String::as_str)),
        )?;
        let receipt = baseline_receipt(
            migration_id,
            DocumentKindV2::Credential,
            presence(&payload),
            revision,
            &token,
            references,
            None,
        );
        let result = Self(SecretEnvelopeWireV2 {
            schema_version: 2,
            random_token: token,
            receipt,
            payload,
        });
        result.validate()?;
        Ok(result)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_STORE_BYTES {
            return Err(Error::Limit);
        }
        let result = Self(serde_json::from_slice(bytes).map_err(|_| Error::Protocol)?);
        result.validate()?;
        Ok(result)
    }
    pub fn encode(&self) -> Result<Zeroizing<Vec<u8>>, Error> {
        self.validate()?;
        let bytes = Zeroizing::new(serde_json::to_vec(&self.0).map_err(|_| Error::Protocol)?);
        if bytes.len() > MAX_STORE_BYTES {
            return Err(Error::Limit);
        }
        Ok(bytes)
    }
    pub fn validate(&self) -> Result<(), Error> {
        if self.0.schema_version != 2 {
            return Err(Error::Protocol);
        }
        if let Some(payload) = &self.0.payload {
            payload.validate()?;
        }
        let revision = self
            .0
            .payload
            .as_ref()
            .map_or(0, |payload| payload.revision);
        let references = reference_hash(
            self.0
                .payload
                .iter()
                .flat_map(|payload| payload.values.keys().map(String::as_str)),
        )?;
        validate_receipt(
            &self.0.receipt,
            DocumentKindV2::Credential,
            presence(&self.0.payload),
            revision,
            &self.0.random_token,
            &references,
            None,
        )?;
        self.metadata_unchecked().map(|_| ())
    }
    fn metadata_unchecked(&self) -> Result<DocumentMetadataV2, Error> {
        metadata(
            DocumentKindV2::Credential,
            presence(&self.0.payload),
            self.0
                .payload
                .as_ref()
                .map_or(0, |payload| payload.revision),
            &self.0.random_token,
            &self.0.receipt,
        )
    }
    pub fn metadata(&self) -> Result<DocumentMetadataV2, Error> {
        self.validate()?;
        self.metadata_unchecked()
    }
    pub fn receipt(&self) -> &DocumentReceiptV2 {
        &self.0.receipt
    }
    pub fn verify_preserved_source(&self, source: Option<&Secrets>) -> Result<(), Error> {
        self.validate()?;
        match (&self.0.payload, source) {
            (None, None) => Ok(()),
            (Some(after), Some(before))
                if after.revision == before.0.revision && after.values == before.0.values =>
            {
                Ok(())
            }
            _ => Err(Error::Auth),
        }
    }
    /// Validate references against this decoded, zeroizing document without
    /// exposing or cloning its secret payload into a resource-admission API.
    pub(crate) fn validate_registry_references(&self, registry: &Registry) -> Result<(), Error> {
        self.validate()?;
        super::check_references(registry, self.0.payload.as_ref())
    }

    pub fn into_snapshot(self) -> Option<Secrets> {
        self.0.payload.map(Secrets)
    }
    /// Produce bytes for the coordinator; this does not authorize publication.
    pub fn replacement(
        &self,
        operation_id: &str,
        plan_hash: &str,
        next: Secrets,
    ) -> Result<Self, Error> {
        let before = self.metadata()?;
        let token = fresh_token()?;
        let payload = Some(next.0);
        let revision = payload.as_ref().unwrap().revision;
        let references = reference_hash(
            payload
                .iter()
                .flat_map(|value| value.values.keys().map(String::as_str)),
        )?;
        let receipt = managed_receipt(
            operation_id,
            plan_hash,
            before,
            &token,
            revision,
            references,
            None,
        );
        let result = Self(SecretEnvelopeWireV2 {
            schema_version: 2,
            random_token: token,
            receipt,
            payload,
        });
        result.validate()?;
        Ok(result)
    }
}

impl RegistryDocumentV2 {
    /// source_file_hash comes from the pinned v1 bytes. payload may have the
    /// approved typed path mapping; its canonical hash records the new payload.
    pub fn baseline(
        migration_id: &str,
        payload: Option<Registry>,
        source_file_hash: Option<String>,
    ) -> Result<Self, Error> {
        let token = fresh_token()?;
        let revision = payload.as_ref().map_or(0, |value| value.revision);
        let projection = RegistryBaselineProjectionV1 {
            source_file_hash,
            canonical_payload_hash: safe_hash(&payload)?,
        };
        let receipt = baseline_receipt(
            migration_id,
            DocumentKindV2::Registry,
            presence(&payload),
            revision,
            &token,
            registry_reference_hash(&payload)?,
            Some(projection),
        );
        let result = Self(RegistryEnvelopeWireV2 {
            schema_version: 2,
            random_token: token,
            receipt,
            payload,
        });
        result.validate()?;
        Ok(result)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_STORE_BYTES {
            return Err(Error::Limit);
        }
        let result = Self(serde_json::from_slice(bytes).map_err(|_| Error::Protocol)?);
        result.validate()?;
        Ok(result)
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let bytes = serde_json::to_vec(&self.0).map_err(|_| Error::Protocol)?;
        if bytes.len() > MAX_STORE_BYTES {
            return Err(Error::Limit);
        }
        Ok(bytes)
    }
    pub fn validate(&self) -> Result<(), Error> {
        if self.0.schema_version != 2 {
            return Err(Error::Protocol);
        }
        if let Some(payload) = &self.0.payload {
            payload.validate()?;
        }
        let revision = self
            .0
            .payload
            .as_ref()
            .map_or(0, |payload| payload.revision);
        let payload_hash = safe_hash(&self.0.payload)?;
        validate_receipt(
            &self.0.receipt,
            DocumentKindV2::Registry,
            presence(&self.0.payload),
            revision,
            &self.0.random_token,
            &registry_reference_hash(&self.0.payload)?,
            Some(&payload_hash),
        )?;
        self.metadata_unchecked().map(|_| ())
    }
    fn metadata_unchecked(&self) -> Result<DocumentMetadataV2, Error> {
        metadata(
            DocumentKindV2::Registry,
            presence(&self.0.payload),
            self.0
                .payload
                .as_ref()
                .map_or(0, |payload| payload.revision),
            &self.0.random_token,
            &self.0.receipt,
        )
    }
    pub fn metadata(&self) -> Result<DocumentMetadataV2, Error> {
        self.validate()?;
        self.metadata_unchecked()
    }
    pub fn receipt(&self) -> &DocumentReceiptV2 {
        &self.0.receipt
    }
    pub fn snapshot(&self) -> Option<&Registry> {
        self.0.payload.as_ref()
    }
    pub fn into_snapshot(self) -> Option<Registry> {
        self.0.payload
    }
    pub fn replacement(
        &self,
        operation_id: &str,
        plan_hash: &str,
        next: Registry,
    ) -> Result<Self, Error> {
        let before = self.metadata()?;
        let token = fresh_token()?;
        let revision = next.revision;
        let payload = Some(next);
        let receipt = managed_receipt(
            operation_id,
            plan_hash,
            before,
            &token,
            revision,
            registry_reference_hash(&payload)?,
            Some(safe_hash(&payload)?),
        );
        let result = Self(RegistryEnvelopeWireV2 {
            schema_version: 2,
            random_token: token,
            receipt,
            payload,
        });
        result.validate()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
