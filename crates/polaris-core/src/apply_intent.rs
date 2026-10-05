//! Canonical evidence for an owned, recoverable workspace apply.
//!
//! These objects contain stable identities, hashes and bounded metadata. File
//! contents and credentials never enter the intent descriptor or event chain.

use crate::management_effect::{
    ManagementDenialReasonV1, ManagementEffectHashV1, ManagementOperationIdV1,
    ManagementUnknownReasonV1, PrepareManagementEffectV1,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{os::unix::ffi::OsStrExt, path::Path};

pub const MAX_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_INTENT_GENERATION_BYTES: u64 = 65_536;
pub const MAX_INTENT_GENERATIONS: u64 = 12;
pub const FREE_SPACE_FLOOR_BYTES: u64 = 64 * 1024 * 1024;
pub const DIRECTORY_OVERHEAD_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ApplyIntentError {
    #[error("noncanonical apply evidence")]
    Noncanonical,
    #[error("invalid apply evidence: {0}")]
    Invalid(&'static str),
    #[error("apply evidence exceeds its bound: {actual} > {maximum}")]
    TooLarge { actual: u64, maximum: u64 },
    #[error("apply capacity arithmetic overflow")]
    Overflow,
    #[error("apply capacity unavailable in {0}")]
    Capacity(&'static str),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
type Result<T> = std::result::Result<T, ApplyIntentError>;
type Hash = ManagementEffectHashV1;

fn checked_len(bytes: &[u8], maximum: u64) -> Result<u64> {
    let actual = u64::try_from(bytes.len()).map_err(|_| ApplyIntentError::Overflow)?;
    if actual > maximum {
        return Err(ApplyIntentError::TooLarge { actual, maximum });
    }
    Ok(actual)
}
fn nonzero(hash: Hash) -> Result<()> {
    if hash.as_hex().bytes().all(|b| b == b'0') {
        return Err(ApplyIntentError::Invalid("zero digest"));
    }
    Ok(())
}
fn digest(domain: &[u8], bytes: &[u8]) -> Hash {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    Hash::from_hex(&format!("{:x}", hasher.finalize())).expect("SHA-256 lowercase hex")
}
fn length_prefixed_digest(domain: &[u8], value: &str) -> Result<Hash> {
    let length = u32::try_from(value.len()).map_err(|_| ApplyIntentError::Overflow)?;
    let mut bytes = Vec::with_capacity(value.len() + 4);
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
    Ok(digest(domain, &bytes))
}

/// Every persistent reader checks the typed value and its exact canonical bytes.
pub trait CanonicalApplyEvidence: Serialize + DeserializeOwned {
    const MAXIMUM_BYTES: u64;
    fn validate(&self) -> Result<()>;

    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        checked_len(&bytes, Self::MAXIMUM_BYTES)?;
        Ok(bytes)
    }

    fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
        checked_len(bytes, Self::MAXIMUM_BYTES)?;
        let value: Self = serde_json::from_slice(bytes)?;
        if value.canonical_bytes()? != bytes {
            return Err(ApplyIntentError::Noncanonical);
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityV1 {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKindV1 {
    Project,
    Source,
    State,
    RecoveryParent,
    DesktopRoot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ApplyAdmissionErrorV1 {
    #[error("root encoding unsupported")]
    UnsupportedRootEncoding { root: RootKindV1 },
}

impl ApplyAdmissionErrorV1 {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct Wire {
            schema_version: u8,
            code: &'static str,
            root: RootKindV1,
        }
        let Self::UnsupportedRootEncoding { root } = self;
        serde_json::to_vec(&Wire {
            schema_version: 1,
            code: "root_encoding_unsupported",
            root: *root,
        })
        .expect("closed root encoding diagnostic")
    }
}

/// Call only after canonicalization and no-follow identity verification. This
/// check itself performs no capacity read, directory creation or store open.
pub fn require_root_encoding(
    path: &Path,
    root: RootKindV1,
) -> std::result::Result<&str, ApplyAdmissionErrorV1> {
    let raw = path.as_os_str().as_bytes();
    let text = std::str::from_utf8(raw)
        .map_err(|_| ApplyAdmissionErrorV1::UnsupportedRootEncoding { root })?;
    if std::ffi::OsStr::new(text).as_bytes() != raw {
        return Err(ApplyAdmissionErrorV1::UnsupportedRootEncoding { root });
    }
    Ok(text)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ApplyInventoryV1 {
    C12,
    D27,
}

pub fn d27_operation_binding(operation_id: &str) -> Result<Hash> {
    length_prefixed_digest(b"polaris-d27-operation-binding-v1\0", operation_id)
}
pub fn d27_result_id_digest(result_id: &str) -> Result<Hash> {
    length_prefixed_digest(b"polaris-d27-result-id-v1\0", result_id)
}
pub fn d27_session_binding(session_id: &str) -> Result<Hash> {
    length_prefixed_digest(b"polaris-d27-session-binding-v1\0", session_id)
}
pub fn d27_management_operation_id(binding: Hash) -> Result<ManagementOperationIdV1> {
    nonzero(binding)?;
    binding.as_hex()[..32]
        .parse()
        .map_err(|_| ApplyIntentError::Invalid("operation id"))
}
fn raw_hex(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        return Err(ApplyIntentError::Invalid("hex length"));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let nibble = |b| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => Err(ApplyIntentError::Invalid("lowercase hex")),
            };
            Ok(nibble(pair[0])? * 16 + nibble(pair[1])?)
        })
        .collect()
}
pub fn c12_operation_key(project: Hash, operation: ManagementOperationIdV1) -> Result<Hash> {
    nonzero(project)?;
    if operation.as_hex().bytes().all(|b| b == b'0') {
        return Err(ApplyIntentError::Invalid("zero operation id"));
    }
    let mut bytes = raw_hex(&project.as_hex())?;
    bytes.push(1);
    bytes.extend(raw_hex(&operation.as_hex())?);
    Ok(digest(b"polaris-apply-operation-key-v1\0", &bytes))
}
pub fn d27_operation_key(project: Hash, binding: Hash) -> Result<Hash> {
    nonzero(project)?;
    nonzero(binding)?;
    let mut bytes = raw_hex(&project.as_hex())?;
    bytes.push(2);
    bytes.extend(raw_hex(&binding.as_hex())?);
    Ok(digest(b"polaris-apply-operation-key-v1\0", &bytes))
}
pub fn recovery_digest(operation_key: Hash) -> Result<Hash> {
    nonzero(operation_key)?;
    Ok(digest(
        b"polaris-apply-recovery-v1\0",
        &raw_hex(&operation_key.as_hex())?,
    ))
}
pub fn recovery_child(operation_key: Hash) -> Result<String> {
    Ok(format!(
        "polaris-apply-v1-{}",
        &recovery_digest(operation_key)?.as_hex()[..32]
    ))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeforeV1 {
    pub device: u64,
    pub inode: u64,
    pub hash: Hash,
    pub mode: u32,
    pub observed_mode: u32,
    pub bytes: u64,
}
impl BeforeV1 {
    fn validate(&self) -> Result<()> {
        nonzero(self.hash)?;
        if self.mode > 0o777 || self.observed_mode > 0o7777 {
            return Err(ApplyIntentError::Invalid("permission mode"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AfterV1 {
    pub hash: Hash,
    pub mode: u32,
    pub bytes: u64,
}
impl AfterV1 {
    fn validate(&self) -> Result<()> {
        nonzero(self.hash)?;
        if self.mode > 0o777 {
            return Err(ApplyIntentError::Invalid("permission mode"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEntryV1 {
    pub path: String,
    pub before: Option<BeforeV1>,
    pub after: Option<AfterV1>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestV1 {
    pub schema_version: u8,
    pub source_identity: IdentityV1,
    pub entries: Vec<RequestEntryV1>,
}
pub fn validate_relative_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.starts_with('/')
        || path.bytes().any(|b| b == b'\\' || b < 32 || b == 127)
        || path.split('/').any(|p| {
            p.is_empty()
                || p == "."
                || p == ".."
                || std::ffi::OsStr::new(p).as_bytes() != p.as_bytes()
        })
    {
        return Err(ApplyIntentError::Invalid("relative path"));
    }
    Ok(())
}
fn validate_paths<'a>(paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut previous: Option<&str> = None;
    for path in paths {
        validate_relative_path(path)?;
        if previous.is_some_and(|last| last.as_bytes() >= path.as_bytes()) {
            return Err(ApplyIntentError::Invalid("path order or duplicate"));
        }
        previous = Some(path);
    }
    Ok(())
}
impl CanonicalApplyEvidence for RequestV1 {
    const MAXIMUM_BYTES: u64 = MAX_ARTIFACT_BYTES;
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(ApplyIntentError::Invalid("request version"));
        }
        validate_paths(self.entries.iter().map(|e| e.path.as_str()))?;
        for entry in &self.entries {
            if let Some(before) = &entry.before {
                before.validate()?;
            }
            if let Some(after) = &entry.after {
                after.validate()?;
            }
            match (&entry.before, &entry.after) {
                (None, None) => return Err(ApplyIntentError::Invalid("empty change")),
                (Some(b), Some(a))
                    if b.hash == a.hash
                        && b.mode == a.mode
                        && b.observed_mode == a.mode
                        && b.bytes == a.bytes =>
                {
                    return Err(ApplyIntentError::Invalid("unchanged entry"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}
impl RequestV1 {
    pub fn request_hash(&self) -> Result<Hash> {
        Ok(digest(
            b"polaris-workspace-apply-request-v1\0",
            &self.canonical_bytes()?,
        ))
    }
    pub fn path_set_digest(&self) -> Result<Hash> {
        self.validate()?;
        let count = u64::try_from(self.entries.len()).map_err(|_| ApplyIntentError::Overflow)?;
        let mut bytes = count.to_be_bytes().to_vec();
        for entry in &self.entries {
            let len = u32::try_from(entry.path.len()).map_err(|_| ApplyIntentError::Overflow)?;
            bytes.extend_from_slice(&len.to_be_bytes());
            bytes.extend_from_slice(entry.path.as_bytes());
        }
        Ok(digest(b"polaris-apply-path-set-v1\0", &bytes))
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedEntryV1 {
    pub path: String,
    pub current: Option<BeforeV1>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedSourceV1 {
    pub schema_version: u8,
    pub source_identity: IdentityV1,
    pub entries: Vec<ObservedEntryV1>,
}
impl CanonicalApplyEvidence for ObservedSourceV1 {
    const MAXIMUM_BYTES: u64 = MAX_ARTIFACT_BYTES;
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(ApplyIntentError::Invalid("observation version"));
        }
        validate_paths(self.entries.iter().map(|e| e.path.as_str()))?;
        for entry in &self.entries {
            if let Some(current) = &entry.current {
                current.validate()?;
            }
        }
        Ok(())
    }
}
impl ObservedSourceV1 {
    pub fn observation_hash(&self) -> Result<Hash> {
        Ok(digest(
            b"polaris-apply-observed-source-v1\0",
            &self.canonical_bytes()?,
        ))
    }
    fn verify_paths(&self, request: &RequestV1) -> Result<()> {
        self.validate()?;
        request.validate()?;
        if self.source_identity != request.source_identity
            || self.entries.len() != request.entries.len()
            || self
                .entries
                .iter()
                .zip(&request.entries)
                .any(|(a, b)| a.path != b.path)
        {
            return Err(ApplyIntentError::Invalid(
                "observation paths or source identity",
            ));
        }
        Ok(())
    }
    pub fn verify_before(&self, request: &RequestV1) -> Result<()> {
        self.verify_paths(request)?;
        if self
            .entries
            .iter()
            .zip(&request.entries)
            .any(|(a, b)| a.current != b.before)
        {
            return Err(ApplyIntentError::Invalid("source before changed"));
        }
        Ok(())
    }
    pub fn verify_after(&self, request: &RequestV1) -> Result<()> {
        self.verify_paths(request)?;
        for (observed, requested) in self.entries.iter().zip(&request.entries) {
            match (&observed.current, &requested.after) {
                (None, None) => {}
                (Some(o), Some(a))
                    if o.hash == a.hash
                        && o.mode == a.mode
                        && o.observed_mode == a.mode
                        && o.bytes == a.bytes => {}
                _ => return Err(ApplyIntentError::Invalid("source after differs")),
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityPlanV1 {
    pub schema_version: u8,
    pub state_device: u64,
    pub recovery_device: u64,
    pub desktop_device: Option<u64>,
    pub state_identity: IdentityV1,
    pub recovery_parent_identity: IdentityV1,
    pub desktop_root_identity: Option<IdentityV1>,
    pub state_allocation_unit: u64,
    pub recovery_allocation_unit: u64,
    pub desktop_allocation_unit: Option<u64>,
    pub intent_final_bytes: u64,
    pub intent_temporary_bytes: u64,
    pub request_bytes: u64,
    pub source_before_bytes: u64,
    pub d27_source_binding_bytes: Option<u64>,
    pub management_event_record_max_bytes: u64,
    pub management_event_records_required: u64,
    pub management_event_journal_append_bytes: u64,
    pub management_event_journal_required_bytes: u64,
    pub management_generation_seal_required_bytes: u64,
    pub generic_effect_bytes: u64,
    pub inventory_commit_bytes: u64,
    pub journal_bytes: u64,
    pub report_bytes: u64,
    pub archive_seal_bytes: u64,
    pub desktop_result_required_bytes: Option<u64>,
    pub state_directory_overhead_bytes: u64,
    pub recovery_directory_overhead_bytes: u64,
    pub state_required_bytes: u64,
    pub recovery_required_bytes: u64,
    pub desktop_required_bytes: Option<u64>,
    pub state_available_bytes_at_admission: u64,
    pub recovery_available_bytes_at_admission: u64,
    pub desktop_available_bytes_at_admission: Option<u64>,
    pub free_space_floor_bytes: u64,
    pub management_record_limit_bytes_at_admission: u64,
    pub management_records_remaining_at_admission: u64,
    pub management_bytes_remaining_at_admission: u64,
    pub desktop_records_remaining_at_admission: Option<u64>,
    pub desktop_bytes_remaining_at_admission: Option<u64>,
    pub archive_count_at_admission: u64,
    pub archive_file_bytes_at_admission: u64,
    pub formula_version: u8,
    pub formula_hash: Hash,
}

pub fn allocated_bytes(bytes: u64, unit: u64) -> Result<u64> {
    if unit == 0 {
        return Err(ApplyIntentError::Invalid("zero allocation unit"));
    }
    let units = (bytes / unit)
        .checked_add(u64::from(bytes % unit != 0))
        .ok_or(ApplyIntentError::Overflow)?;
    units.checked_mul(unit).ok_or(ApplyIntentError::Overflow)
}
fn sum(values: impl IntoIterator<Item = u64>) -> Result<u64> {
    values.into_iter().try_fold(0_u64, |a, b| {
        a.checked_add(b).ok_or(ApplyIntentError::Overflow)
    })
}
fn management_seal_reservation(unit: u64) -> Result<u64> {
    sum([
        allocated_bytes(64 * 1024 * 1024, unit)?,
        allocated_bytes(65_536, unit)?,
        allocated_bytes(4096, unit)?,
        allocated_bytes(4096, unit)?,
        DIRECTORY_OVERHEAD_BYTES,
    ])
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityConstantsV1 {
    pub free_space_floor_bytes: u64,
    pub directory_overhead_bytes: u64,
    pub max_intent_generation_bytes: u64,
    pub max_intent_generations: u64,
    pub max_source_apply_bytes: u64,
    pub max_source_apply_total_bytes: u64,
    pub management_max_archive_bytes: u64,
    pub management_max_checkpoint_bytes: u64,
    pub management_max_manifest_bytes: u64,
    pub management_max_pointer_bytes: u64,
}
impl Default for CapacityConstantsV1 {
    fn default() -> Self {
        Self {
            free_space_floor_bytes: FREE_SPACE_FLOOR_BYTES,
            directory_overhead_bytes: DIRECTORY_OVERHEAD_BYTES,
            max_intent_generation_bytes: MAX_INTENT_GENERATION_BYTES,
            max_intent_generations: MAX_INTENT_GENERATIONS,
            max_source_apply_bytes: 1024 * 1024,
            max_source_apply_total_bytes: 8 * 1024 * 1024,
            management_max_archive_bytes: 64 * 1024 * 1024,
            management_max_checkpoint_bytes: 65_536,
            management_max_manifest_bytes: 4096,
            management_max_pointer_bytes: 4096,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityPayloadLengthsV1 {
    pub request_bytes: u64,
    pub source_before_bytes: u64,
    pub d27_source_binding_bytes: Option<u64>,
    pub journal_bytes: u64,
    pub report_bytes: u64,
    pub archive_seal_bytes: u64,
    pub desktop_result_required_bytes: Option<u64>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateCapacityComponentV1 {
    pub device: u64,
    pub allocation_unit: u64,
    pub intent_final_bytes: u64,
    pub intent_temporary_bytes: u64,
    pub management_event_journal_required_bytes: u64,
    pub management_generation_seal_required_bytes: u64,
    pub generic_effect_bytes: u64,
    pub inventory_commit_bytes: u64,
    pub archive_seal_allocated_bytes: u64,
    pub directory_overhead_bytes: u64,
    pub required_bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCapacityComponentV1 {
    pub device: u64,
    pub allocation_unit: u64,
    pub journal_allocated_bytes: u64,
    pub report_allocated_bytes: u64,
    pub directory_overhead_bytes: u64,
    pub required_bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopCapacityComponentV1 {
    pub device: u64,
    pub allocation_unit: u64,
    pub inventory_commit_bytes: u64,
    pub required_bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityRootComponentsV1 {
    pub state: StateCapacityComponentV1,
    pub recovery: RecoveryCapacityComponentV1,
    pub desktop: Option<DesktopCapacityComponentV1>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityLogicalAdmissionV1 {
    pub management_record_limit_bytes_at_admission: u64,
    pub management_event_record_max_bytes: u64,
    pub management_event_records_required: u64,
    pub management_event_journal_append_bytes: u64,
    pub management_records_remaining_at_admission: u64,
    pub management_bytes_remaining_at_admission: u64,
    pub desktop_records_remaining_at_admission: Option<u64>,
    pub desktop_bytes_remaining_at_admission: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityRootKindV1 {
    State,
    Recovery,
    Desktop,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityDeviceGroupV1 {
    pub device: u64,
    pub allocation_unit: u64,
    pub roots: Vec<CapacityRootKindV1>,
    pub required_bytes: u64,
    pub free_space_floor_bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityFormulaParametersV1 {
    pub schema_version: u8,
    pub formula_version: u8,
    pub inventory_id: ApplyInventoryV1,
    pub constants: CapacityConstantsV1,
    pub payload_lengths: CapacityPayloadLengthsV1,
    pub root_components: CapacityRootComponentsV1,
    pub logical_admission: CapacityLogicalAdmissionV1,
    pub device_groups: Vec<CapacityDeviceGroupV1>,
}

/// A sample from a pinned private directory, never a caller-supplied device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CapacityRootSampleV1 {
    pub identity: IdentityV1,
    pub allocation_unit: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapacityRootsV1 {
    pub state: CapacityRootSampleV1,
    pub recovery: CapacityRootSampleV1,
    pub desktop: Option<CapacityRootSampleV1>,
}

fn available_from_statvfs(unit: u64, blocks: u64) -> Result<u64> {
    if unit == 0 {
        return Err(ApplyIntentError::Invalid("zero allocation unit"));
    }
    blocks.checked_mul(unit).ok_or(ApplyIntentError::Overflow)
}

fn sample_statvfs(file: &File) -> Result<(u64, u64)> {
    let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: live directory descriptor and valid, correctly sized output.
    if unsafe { libc::fstatvfs(file.as_raw_fd(), value.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let value = unsafe { value.assume_init() };
    let unit = value.f_frsize as u64;
    Ok((unit, available_from_statvfs(unit, value.f_bavail as u64)?))
}

impl CapacityRootsV1 {
    pub(crate) fn from_pinned_files(
        state: &File,
        recovery: &File,
        desktop: Option<&File>,
    ) -> Result<Self> {
        Self::sample_with(state, recovery, desktop, sample_statvfs)
    }

    fn sample_with(
        state: &File,
        recovery: &File,
        desktop: Option<&File>,
        mut sample: impl FnMut(&File) -> Result<(u64, u64)>,
    ) -> Result<Self> {
        let files = [Some(state), Some(recovery), desktop];
        let mut identities = Vec::new();
        for file in files.into_iter().flatten() {
            let metadata = file.metadata()?;
            private_metadata(&metadata, true)?;
            identities.push(file_identity(&metadata));
        }
        let mut by_device = std::collections::BTreeMap::new();
        let mut roots = Vec::new();
        for (file, identity) in files.into_iter().flatten().zip(&identities) {
            let (unit, available) = match by_device.get(&identity.device) {
                Some(sampled) => *sampled,
                None => {
                    let sampled = sample(file)?;
                    if sampled.0 == 0 {
                        return Err(ApplyIntentError::Invalid("zero allocation unit"));
                    }
                    by_device.insert(identity.device, sampled);
                    sampled
                }
            };
            roots.push(CapacityRootSampleV1 {
                identity: *identity,
                allocation_unit: unit,
                available_bytes: available,
            });
        }
        for (file, expected) in files.into_iter().flatten().zip(identities) {
            let metadata = file.metadata()?;
            private_metadata(&metadata, true)?;
            if file_identity(&metadata) != expected {
                return Err(ApplyIntentError::Invalid("capacity root changed"));
            }
        }
        Ok(Self {
            state: roots[0],
            recovery: roots[1],
            desktop: roots.get(2).copied(),
        })
    }
}

/// Fresh, nonpersistent capacity information. These rows never authorize an
/// operation and never become part of an intent, generation, or seal hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveCapacityDeviceGroupV1 {
    pub device: u64,
    pub allocation_unit: u64,
    pub roots: Vec<CapacityRootKindV1>,
    pub available_bytes_now: u64,
    pub required_remaining_bytes: u64,
    pub free_space_floor_bytes: u64,
    pub shortage_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyRecoveryDiagnosticsV1 {
    pub schema_version: u8,
    pub operation_key: Hash,
    pub latest_phase: ApplyPhaseV1,
    pub state_identity: IdentityV1,
    pub recovery_parent_identity: IdentityV1,
    pub desktop_root_identity: Option<IdentityV1>,
    pub state_remaining_component_bytes: u64,
    pub recovery_remaining_component_bytes: u64,
    pub desktop_remaining_component_bytes: Option<u64>,
    pub device_groups: Vec<LiveCapacityDeviceGroupV1>,
    pub archive_count_now: u64,
    pub archive_file_bytes_now: u64,
    pub terminal_active_archive_pending: bool,
    pub last_errno: Option<i32>,
    pub next_action: String,
}

impl CapacityPlanV1 {
    /// The owner computes unfinished components from joined durable evidence.
    /// This calculation compares static pinned capabilities, but deliberately
    /// does not compare fresh availability with the historical admission value.
    pub(crate) fn live_capacity_groups(
        &self,
        fresh: &CapacityRootsV1,
        state_remaining: u64,
        recovery_remaining: u64,
        desktop_remaining: Option<u64>,
    ) -> Result<Vec<LiveCapacityDeviceGroupV1>> {
        self.validate()?;
        if fresh.state.identity != self.state_identity
            || fresh.recovery.identity != self.recovery_parent_identity
            || fresh.state.allocation_unit != self.state_allocation_unit
            || fresh.recovery.allocation_unit != self.recovery_allocation_unit
            || fresh.desktop.map(|d| d.identity) != self.desktop_root_identity
            || fresh.desktop.map(|d| d.allocation_unit) != self.desktop_allocation_unit
            || desktop_remaining.is_some() != fresh.desktop.is_some()
        {
            return Err(ApplyIntentError::Invalid("live capacity static identity"));
        }
        let mut components = vec![
            (CapacityRootKindV1::State, fresh.state, state_remaining),
            (
                CapacityRootKindV1::Recovery,
                fresh.recovery,
                recovery_remaining,
            ),
        ];
        if let (Some(root), Some(remaining)) = (fresh.desktop, desktop_remaining) {
            components.push((CapacityRootKindV1::Desktop, root, remaining));
        }
        let mut groups = std::collections::BTreeMap::new();
        for (kind, root, remaining) in components {
            let row =
                groups
                    .entry(root.identity.device)
                    .or_insert_with(|| LiveCapacityDeviceGroupV1 {
                        device: root.identity.device,
                        allocation_unit: root.allocation_unit,
                        roots: Vec::new(),
                        available_bytes_now: root.available_bytes,
                        required_remaining_bytes: 0,
                        free_space_floor_bytes: FREE_SPACE_FLOOR_BYTES,
                        shortage_bytes: 0,
                    });
            if row.allocation_unit != root.allocation_unit
                || row.available_bytes_now != root.available_bytes
            {
                return Err(ApplyIntentError::Invalid("live same-device sample"));
            }
            row.roots.push(kind);
            row.required_remaining_bytes = sum([row.required_remaining_bytes, remaining])?;
        }
        for row in groups.values_mut() {
            row.shortage_bytes = sum([row.required_remaining_bytes, FREE_SPACE_FLOOR_BYTES])?
                .saturating_sub(row.available_bytes_now);
        }
        Ok(groups.into_values().collect())
    }
}

/// Counts actual envelope+LF bytes for the longest remaining legal chain.
/// Sequence/hash widths use their schema maxima; hashes have fixed width, so
/// descriptor hashing never feeds a variable-size value back into capacity.
pub(crate) fn management_append_lengths(prepare: &PrepareManagementEffectV1) -> Result<Vec<u64>> {
    use serde_json::json;
    let hash = "f".repeat(64);
    let id = prepare.operation_id.as_hex();
    let unknown = [
        ManagementUnknownReasonV1::ProcessRestart,
        ManagementUnknownReasonV1::InterruptedAfterStart,
        ManagementUnknownReasonV1::ReceiptPersistenceFailed,
        ManagementUnknownReasonV1::DetachedEffect,
        ManagementUnknownReasonV1::UnclassifiedEffect,
    ]
    .into_iter()
    .map(|reason| json!({"event":"outcome_unknown","operation_id":id,"reason":reason}));
    let encode = |event: serde_json::Value| -> Result<u64> {
        let record = json!({"version":1,"sequence":u64::MAX,"previous_hash":hash,"event":event,"event_hash":hash});
        sum([
            u64::try_from(serde_json::to_vec(&record)?.len())
                .map_err(|_| ApplyIntentError::Overflow)?,
            1,
        ])
    };
    let unknown_max = unknown
        .map(encode)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .ok_or(ApplyIntentError::Invalid("missing unknown capacity shape"))?;
    let terminal_max = ["completed", "reconciled_abandon"]
        .into_iter()
        .map(|event| encode(json!({"event":event,"operation_id":id,"evidence_hash":hash})))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .expect("two terminal shapes");
    let full = vec![
        encode(json!({"event":"prepared","request":prepare}))?,
        encode(json!({"event":"effect_started","operation_id":id}))?,
        unknown_max,
        terminal_max,
    ];
    // Denied is a separate, shorter legal branch, but its largest record must
    // fit the same single-record limit. It must not disappear from the bound.
    for reason in [
        ManagementDenialReasonV1::UserDeclined,
        ManagementDenialReasonV1::PolicyDenied,
        ManagementDenialReasonV1::PermissionDenied,
        ManagementDenialReasonV1::StaleRequest,
        ManagementDenialReasonV1::ResourceUnavailable,
        ManagementDenialReasonV1::InvalidState,
    ] {
        let denied = encode(json!({"event":"denied","operation_id":id,"reason":reason}))?;
        if denied > *full.iter().max().expect("nonempty chain")
            || sum([full[0], denied])? > sum(full.iter().copied())?
        {
            return Err(ApplyIntentError::Invalid(
                "denial exceeds reserved longest chain",
            ));
        }
    }
    Ok(full)
}

pub(crate) struct CapacityAdmissionV1<'a> {
    pub roots: CapacityRootsV1,
    pub request: &'a RequestV1,
    pub source_before: &'a ObservedSourceV1,
    pub d27_source_binding: Option<&'a D27SourceBindingV1>,
    pub prepare: &'a PrepareManagementEffectV1,
    pub management_record_limit: u64,
    pub management_records_remaining: u64,
    pub management_bytes_remaining: u64,
    /// Production upper bounds supplied by the workspace/inventory encoders.
    pub inventory_commit_bytes: u64,
    pub journal_bytes: u64,
    pub report_bytes: u64,
    pub archive_seal_bytes: u64,
    pub desktop_result_required_bytes: Option<u64>,
    pub desktop_records_remaining: Option<u64>,
    pub desktop_bytes_remaining: Option<u64>,
    pub archive_count: u64,
    pub archive_file_bytes: u64,
}

impl CapacityPlanV1 {
    pub(crate) fn build(input: CapacityAdmissionV1<'_>) -> Result<Self> {
        input.source_before.verify_before(input.request)?;
        let request_bytes = input.request.canonical_bytes()?.len() as u64;
        let source_before_bytes = input.source_before.canonical_bytes()?.len() as u64;
        let binding_bytes = input
            .d27_source_binding
            .map(CanonicalApplyEvidence::canonical_bytes)
            .transpose()?;
        let d27_source_binding_bytes = binding_bytes.as_ref().map(|b| b.len() as u64);
        if input.roots.desktop.is_some() != d27_source_binding_bytes.is_some()
            || (input.roots.desktop.is_some() && input.prepare.inventory.to_string() != "D27")
            || (input.roots.desktop.is_none() && input.prepare.inventory.to_string() != "C12")
        {
            return Err(ApplyIntentError::Invalid(
                "capacity inventory or binding presence",
            ));
        }
        if let Some(binding) = input.d27_source_binding {
            if binding.manifest.project_request()? != *input.request
                || binding.source_before_hash != input.source_before.observation_hash()?
                || binding.source_before_bytes != source_before_bytes
            {
                return Err(ApplyIntentError::Invalid("capacity binding artifacts"));
            }
        }
        let records = management_append_lengths(input.prepare)?;
        let append = sum(records.iter().copied())?;
        let u = input.roots.state.allocation_unit;
        let seal = allocated_bytes(input.archive_seal_bytes, u)?;
        let final_bytes = sum([
            allocated_bytes(request_bytes, u)?,
            allocated_bytes(source_before_bytes, u)?,
            allocated_bytes(d27_source_binding_bytes.unwrap_or(0), u)?,
            allocated_bytes(MAX_INTENT_GENERATION_BYTES, u)?
                .checked_mul(MAX_INTENT_GENERATIONS)
                .ok_or(ApplyIntentError::Overflow)?,
        ])?;
        let temporary = sum([allocated_bytes(MAX_INTENT_GENERATION_BYTES, u)?, seal])?;
        let journal_reserve = allocated_bytes(append, u)?;
        let seal_reserve = management_seal_reservation(u)?;
        let generic = sum([journal_reserve, seal_reserve])?;
        let desktop = input.roots.desktop;
        let state_required = sum([
            final_bytes,
            temporary,
            generic,
            if desktop.is_some() {
                0
            } else {
                input.inventory_commit_bytes
            },
            seal,
            DIRECTORY_OVERHEAD_BYTES,
        ])?;
        let recovery_required = sum([
            allocated_bytes(input.journal_bytes, input.roots.recovery.allocation_unit)?,
            allocated_bytes(input.report_bytes, input.roots.recovery.allocation_unit)?,
            DIRECTORY_OVERHEAD_BYTES,
        ])?;
        let mut plan = Self {
            schema_version: 1,
            state_device: input.roots.state.identity.device,
            recovery_device: input.roots.recovery.identity.device,
            desktop_device: desktop.map(|d| d.identity.device),
            state_identity: input.roots.state.identity,
            recovery_parent_identity: input.roots.recovery.identity,
            desktop_root_identity: desktop.map(|d| d.identity),
            state_allocation_unit: u,
            recovery_allocation_unit: input.roots.recovery.allocation_unit,
            desktop_allocation_unit: desktop.map(|d| d.allocation_unit),
            intent_final_bytes: final_bytes,
            intent_temporary_bytes: temporary,
            request_bytes,
            source_before_bytes,
            d27_source_binding_bytes,
            management_event_record_max_bytes: *records.iter().max().expect("nonempty chain"),
            management_event_records_required: records.len() as u64,
            management_event_journal_append_bytes: append,
            management_event_journal_required_bytes: journal_reserve,
            management_generation_seal_required_bytes: seal_reserve,
            generic_effect_bytes: generic,
            inventory_commit_bytes: input.inventory_commit_bytes,
            journal_bytes: input.journal_bytes,
            report_bytes: input.report_bytes,
            archive_seal_bytes: input.archive_seal_bytes,
            desktop_result_required_bytes: input.desktop_result_required_bytes,
            state_directory_overhead_bytes: DIRECTORY_OVERHEAD_BYTES,
            recovery_directory_overhead_bytes: DIRECTORY_OVERHEAD_BYTES,
            state_required_bytes: state_required,
            recovery_required_bytes: recovery_required,
            desktop_required_bytes: desktop.map(|_| input.inventory_commit_bytes),
            state_available_bytes_at_admission: input.roots.state.available_bytes,
            recovery_available_bytes_at_admission: input.roots.recovery.available_bytes,
            desktop_available_bytes_at_admission: desktop.map(|d| d.available_bytes),
            free_space_floor_bytes: FREE_SPACE_FLOOR_BYTES,
            management_record_limit_bytes_at_admission: input.management_record_limit,
            management_records_remaining_at_admission: input.management_records_remaining,
            management_bytes_remaining_at_admission: input.management_bytes_remaining,
            desktop_records_remaining_at_admission: input.desktop_records_remaining,
            desktop_bytes_remaining_at_admission: input.desktop_bytes_remaining,
            archive_count_at_admission: input.archive_count,
            archive_file_bytes_at_admission: input.archive_file_bytes,
            formula_version: 1,
            formula_hash: Hash::digest(b"capacity formula not yet encoded"),
        };
        // Encode once from fixed-width reservations. There is no seed iteration
        // involving the size of the descriptor or generation zero.
        plan.formula_hash = plan.recomputed_formula_hash()?;
        plan.validate()?;
        Ok(plan)
    }
}

impl CapacityPlanV1 {
    pub fn formula_parameters(&self) -> Result<CapacityFormulaParametersV1> {
        let desktop = match (
            self.desktop_device,
            self.desktop_allocation_unit,
            self.desktop_required_bytes,
        ) {
            (None, None, None) => None,
            (Some(device), Some(allocation_unit), Some(required_bytes)) => {
                Some(DesktopCapacityComponentV1 {
                    device,
                    allocation_unit,
                    inventory_commit_bytes: self.inventory_commit_bytes,
                    required_bytes,
                })
            }
            _ => return Err(ApplyIntentError::Invalid("partial desktop root")),
        };
        let state = StateCapacityComponentV1 {
            device: self.state_device,
            allocation_unit: self.state_allocation_unit,
            intent_final_bytes: self.intent_final_bytes,
            intent_temporary_bytes: self.intent_temporary_bytes,
            management_event_journal_required_bytes: self.management_event_journal_required_bytes,
            management_generation_seal_required_bytes: self
                .management_generation_seal_required_bytes,
            generic_effect_bytes: self.generic_effect_bytes,
            inventory_commit_bytes: if desktop.is_some() {
                0
            } else {
                self.inventory_commit_bytes
            },
            archive_seal_allocated_bytes: allocated_bytes(
                self.archive_seal_bytes,
                self.state_allocation_unit,
            )?,
            directory_overhead_bytes: self.state_directory_overhead_bytes,
            required_bytes: self.state_required_bytes,
        };
        let recovery = RecoveryCapacityComponentV1 {
            device: self.recovery_device,
            allocation_unit: self.recovery_allocation_unit,
            journal_allocated_bytes: allocated_bytes(
                self.journal_bytes,
                self.recovery_allocation_unit,
            )?,
            report_allocated_bytes: allocated_bytes(
                self.report_bytes,
                self.recovery_allocation_unit,
            )?,
            directory_overhead_bytes: self.recovery_directory_overhead_bytes,
            required_bytes: self.recovery_required_bytes,
        };
        let mut groups: std::collections::BTreeMap<u64, CapacityDeviceGroupV1> =
            std::collections::BTreeMap::new();
        let mut roots = vec![
            (
                CapacityRootKindV1::State,
                self.state_device,
                self.state_allocation_unit,
                self.state_required_bytes,
            ),
            (
                CapacityRootKindV1::Recovery,
                self.recovery_device,
                self.recovery_allocation_unit,
                self.recovery_required_bytes,
            ),
        ];
        if let Some(d) = &desktop {
            roots.push((
                CapacityRootKindV1::Desktop,
                d.device,
                d.allocation_unit,
                d.required_bytes,
            ));
        }
        for (root, device, unit, required) in roots {
            let group = groups
                .entry(device)
                .or_insert_with(|| CapacityDeviceGroupV1 {
                    device,
                    allocation_unit: unit,
                    roots: Vec::new(),
                    required_bytes: 0,
                    free_space_floor_bytes: FREE_SPACE_FLOOR_BYTES,
                });
            if group.allocation_unit != unit {
                return Err(ApplyIntentError::Invalid("same-device allocation unit"));
            }
            group.roots.push(root);
            group.required_bytes = sum([group.required_bytes, required])?;
        }
        Ok(CapacityFormulaParametersV1 {
            schema_version: 1,
            formula_version: 1,
            inventory_id: if desktop.is_some() {
                ApplyInventoryV1::D27
            } else {
                ApplyInventoryV1::C12
            },
            constants: CapacityConstantsV1::default(),
            payload_lengths: CapacityPayloadLengthsV1 {
                request_bytes: self.request_bytes,
                source_before_bytes: self.source_before_bytes,
                d27_source_binding_bytes: self.d27_source_binding_bytes,
                journal_bytes: self.journal_bytes,
                report_bytes: self.report_bytes,
                archive_seal_bytes: self.archive_seal_bytes,
                desktop_result_required_bytes: self.desktop_result_required_bytes,
            },
            root_components: CapacityRootComponentsV1 {
                state,
                recovery,
                desktop,
            },
            logical_admission: CapacityLogicalAdmissionV1 {
                management_record_limit_bytes_at_admission: self
                    .management_record_limit_bytes_at_admission,
                management_event_record_max_bytes: self.management_event_record_max_bytes,
                management_event_records_required: self.management_event_records_required,
                management_event_journal_append_bytes: self.management_event_journal_append_bytes,
                management_records_remaining_at_admission: self
                    .management_records_remaining_at_admission,
                management_bytes_remaining_at_admission: self
                    .management_bytes_remaining_at_admission,
                desktop_records_remaining_at_admission: self.desktop_records_remaining_at_admission,
                desktop_bytes_remaining_at_admission: self.desktop_bytes_remaining_at_admission,
            },
            device_groups: groups.into_values().collect(),
        })
    }
    pub fn formula_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&self.formula_parameters()?)?)
    }
    pub fn recomputed_formula_hash(&self) -> Result<Hash> {
        Ok(digest(
            b"polaris-capacity-formula-v1\0",
            &self.formula_bytes()?,
        ))
    }
    pub fn plan_hash(&self) -> Result<Hash> {
        Ok(digest(
            b"polaris-apply-capacity-plan-v1\0",
            &self.canonical_bytes()?,
        ))
    }
    fn check_components(&self) -> Result<()> {
        let u = self.state_allocation_unit;
        let generations = allocated_bytes(MAX_INTENT_GENERATION_BYTES, u)?
            .checked_mul(MAX_INTENT_GENERATIONS)
            .ok_or(ApplyIntentError::Overflow)?;
        let final_bytes = sum([
            allocated_bytes(self.request_bytes, u)?,
            allocated_bytes(self.source_before_bytes, u)?,
            allocated_bytes(self.d27_source_binding_bytes.unwrap_or(0), u)?,
            generations,
        ])?;
        let seal = allocated_bytes(self.archive_seal_bytes, u)?;
        let temporary = sum([allocated_bytes(MAX_INTENT_GENERATION_BYTES, u)?, seal])?;
        let management_journal = allocated_bytes(self.management_event_journal_append_bytes, u)?;
        let management_seal = management_seal_reservation(u)?;
        let generic = sum([management_journal, management_seal])?;
        let state = sum([
            final_bytes,
            temporary,
            generic,
            if self.desktop_device.is_some() {
                0
            } else {
                self.inventory_commit_bytes
            },
            seal,
            DIRECTORY_OVERHEAD_BYTES,
        ])?;
        let recovery = sum([
            allocated_bytes(self.journal_bytes, self.recovery_allocation_unit)?,
            allocated_bytes(self.report_bytes, self.recovery_allocation_unit)?,
            DIRECTORY_OVERHEAD_BYTES,
        ])?;
        if self.intent_final_bytes != final_bytes
            || self.intent_temporary_bytes != temporary
            || self.management_event_journal_required_bytes != management_journal
            || self.management_generation_seal_required_bytes != management_seal
            || self.generic_effect_bytes != generic
            || self.state_required_bytes != state
            || self.recovery_required_bytes != recovery
            || self.state_directory_overhead_bytes != DIRECTORY_OVERHEAD_BYTES
            || self.recovery_directory_overhead_bytes != DIRECTORY_OVERHEAD_BYTES
            || self.free_space_floor_bytes != FREE_SPACE_FLOOR_BYTES
            || self.inventory_commit_bytes == 0
            || self
                .desktop_required_bytes
                .is_some_and(|r| r != self.inventory_commit_bytes)
        {
            return Err(ApplyIntentError::Invalid("capacity formula components"));
        }
        Ok(())
    }
    fn check_logical_admission(&self) -> Result<()> {
        if self.management_event_record_max_bytes > self.management_record_limit_bytes_at_admission
        {
            return Err(ApplyIntentError::Capacity("management record"));
        }
        if self.management_event_records_required > self.management_records_remaining_at_admission {
            return Err(ApplyIntentError::Capacity("management records"));
        }
        if self.management_event_journal_append_bytes > self.management_bytes_remaining_at_admission
        {
            return Err(ApplyIntentError::Capacity("management journal"));
        }
        if self.management_event_records_required == 0
            || self.management_event_record_max_bytes == 0
            || self.management_event_journal_append_bytes < self.management_event_record_max_bytes
        {
            return Err(ApplyIntentError::Invalid("management event bounds"));
        }
        if let (Some(result), Some(remaining)) = (
            self.desktop_result_required_bytes,
            self.desktop_bytes_remaining_at_admission,
        ) {
            if result > 1024 * 1024 || remaining < 1024 * 1024 {
                return Err(ApplyIntentError::Capacity("desktop result"));
            }
            // Updating an existing candidate consumes no new record slot. The
            // full pending-result reserve is checked by the Desktop validator.
        }
        Ok(())
    }
    fn check_physical_admission(&self) -> Result<()> {
        let mut availability = std::collections::BTreeMap::new();
        let mut snapshots = vec![
            (self.state_device, self.state_available_bytes_at_admission),
            (
                self.recovery_device,
                self.recovery_available_bytes_at_admission,
            ),
        ];
        if let (Some(device), Some(available)) = (
            self.desktop_device,
            self.desktop_available_bytes_at_admission,
        ) {
            snapshots.push((device, available));
        }
        for (device, available) in snapshots {
            if availability
                .insert(device, available)
                .is_some_and(|old| old != available)
            {
                return Err(ApplyIntentError::Invalid("same-device available snapshot"));
            }
        }
        for group in self.formula_parameters()?.device_groups {
            let required = sum([group.required_bytes, FREE_SPACE_FLOOR_BYTES])?;
            if availability[&group.device] < required {
                return Err(ApplyIntentError::Capacity("filesystem"));
            }
        }
        Ok(())
    }
}
impl CanonicalApplyEvidence for CapacityPlanV1 {
    const MAXIMUM_BYTES: u64 = MAX_INTENT_GENERATION_BYTES;
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.formula_version != 1
            || self.state_device != self.state_identity.device
            || self.recovery_device != self.recovery_parent_identity.device
        {
            return Err(ApplyIntentError::Invalid("capacity version or device"));
        }
        let desktop_fields = [
            self.desktop_device.is_some(),
            self.desktop_root_identity.is_some(),
            self.desktop_allocation_unit.is_some(),
            self.desktop_result_required_bytes.is_some(),
            self.desktop_required_bytes.is_some(),
            self.desktop_available_bytes_at_admission.is_some(),
            self.desktop_records_remaining_at_admission.is_some(),
            self.desktop_bytes_remaining_at_admission.is_some(),
            self.d27_source_binding_bytes.is_some(),
        ];
        if desktop_fields
            .iter()
            .any(|present| *present != desktop_fields[0])
            || self
                .desktop_root_identity
                .is_some_and(|id| Some(id.device) != self.desktop_device)
        {
            return Err(ApplyIntentError::Invalid("partial desktop capacity fields"));
        }
        if self.request_bytes > MAX_ARTIFACT_BYTES
            || self.source_before_bytes > MAX_ARTIFACT_BYTES
            || self
                .d27_source_binding_bytes
                .is_some_and(|b| b > MAX_ARTIFACT_BYTES)
            || self.report_bytes > 256 * 1024 * 1024
            || self.archive_seal_bytes > MAX_INTENT_GENERATION_BYTES
        {
            return Err(ApplyIntentError::Invalid("capacity artifact lengths"));
        }
        self.check_components()?;
        self.check_logical_admission()?;
        self.check_physical_admission()?;
        if self.formula_hash != self.recomputed_formula_hash()? {
            return Err(ApplyIntentError::Invalid("capacity formula hash"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionKindV1 {
    GitSha1,
    GitSha256,
    Sha256,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionV1 {
    pub kind: RevisionKindV1,
    pub value: String,
}
impl RevisionV1 {
    fn validate(&self) -> Result<()> {
        let length = if self.kind == RevisionKindV1::GitSha1 {
            40
        } else {
            64
        };
        if self.value.len() != length
            || !self
                .value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ApplyIntentError::Invalid("revision kind or value"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplyContextV1 {
    Participation {
        base_generation: u64,
        target_generation: u64,
        base_marker_hash: Hash,
        mutation_id: Hash,
        target_participation_hash: Option<Hash>,
        target_marker_hash: Hash,
    },
    Session {
        base_epoch: u64,
        session_binding: Hash,
        base_session_revision: u64,
        consume_target_revision: u64,
        policy_revision: u64,
        source_operation_digest: Hash,
        reserved_result_id: String,
        reserved_result_id_digest: Hash,
        base_marker_evidence_hash: Hash,
        consume_marker_evidence_hash: Hash,
        consume_sidecar_offset: u64,
        consume_sidecar_hash: Hash,
        precommit_hash: Hash,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPhaseV1 {
    Prepared,
    RecoveryPrepared,
    EffectStarted,
    ApplyReported,
    ParticipationCommitted,
    Completed,
    Denied,
    AbandonedBeforeEffect,
    OutcomeUnknown,
    ReconciledAbandon,
}
impl ApplyPhaseV1 {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Denied | Self::AbandonedBeforeEffect | Self::ReconciledAbandon
        )
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyIntentDescriptorV1 {
    pub schema_version: u8,
    pub operation_key: Hash,
    pub management_operation_id: ManagementOperationIdV1,
    pub inventory_id: ApplyInventoryV1,
    pub project_binding: Hash,
    pub source_identity: IdentityV1,
    pub agreed_revision: RevisionV1,
    pub staged_revision: RevisionV1,
    pub previous_applied_revision: Option<RevisionV1>,
    pub context: ApplyContextV1,
    pub share_source_manifest_digest: Hash,
    pub changed_path_count: u64,
    pub path_set_digest: Hash,
    pub workspace_apply_request_digest: Hash,
    pub source_before_observation_digest: Hash,
    pub d27_source_binding_bytes: Option<u64>,
    pub d27_source_binding_hash: Option<Hash>,
    pub recovery_parent_identity: IdentityV1,
    pub recovery_provenance_digest: Hash,
    pub recovery_child: String,
    pub capacity_plan_bytes: u64,
    pub capacity_plan_hash: Hash,
    pub capacity_plan: CapacityPlanV1,
    pub phase: ApplyPhaseV1,
}
impl CanonicalApplyEvidence for ApplyIntentDescriptorV1 {
    const MAXIMUM_BYTES: u64 = MAX_INTENT_GENERATION_BYTES;
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 || self.phase != ApplyPhaseV1::Prepared {
            return Err(ApplyIntentError::Invalid("descriptor version or phase"));
        }
        for hash in [
            self.operation_key,
            self.project_binding,
            self.share_source_manifest_digest,
            self.path_set_digest,
            self.workspace_apply_request_digest,
            self.source_before_observation_digest,
            self.recovery_provenance_digest,
            self.capacity_plan_hash,
        ] {
            nonzero(hash)?;
        }
        self.agreed_revision.validate()?;
        self.staged_revision.validate()?;
        if let Some(previous) = &self.previous_applied_revision {
            previous.validate()?;
        }
        let plan_bytes = self.capacity_plan.canonical_bytes()?;
        if self.capacity_plan_bytes != checked_len(&plan_bytes, MAX_INTENT_GENERATION_BYTES)?
            || self.capacity_plan_hash != self.capacity_plan.plan_hash()?
            || self.capacity_plan.recovery_parent_identity != self.recovery_parent_identity
            || self.d27_source_binding_bytes != self.capacity_plan.d27_source_binding_bytes
            || self.d27_source_binding_bytes.is_some() != self.d27_source_binding_hash.is_some()
            || self.recovery_child != recovery_child(self.operation_key)?
        {
            return Err(ApplyIntentError::Invalid("descriptor artifact binding"));
        }
        if let Some(hash) = self.d27_source_binding_hash {
            nonzero(hash)?;
        }
        match (&self.context, self.inventory_id) {
            (
                ApplyContextV1::Participation {
                    base_generation,
                    target_generation,
                    base_marker_hash,
                    mutation_id,
                    target_participation_hash,
                    target_marker_hash,
                },
                ApplyInventoryV1::C12,
            ) => {
                for h in [*base_marker_hash, *mutation_id, *target_marker_hash] {
                    nonzero(h)?;
                }
                if let Some(h) = target_participation_hash {
                    nonzero(*h)?;
                }
                if base_generation.checked_add(1) != Some(*target_generation)
                    || *mutation_id != self.operation_key
                    || self.operation_key
                        != c12_operation_key(self.project_binding, self.management_operation_id)?
                    || self.agreed_revision != self.staged_revision
                    || self.agreed_revision.kind == RevisionKindV1::Sha256
                    || self
                        .previous_applied_revision
                        .as_ref()
                        .is_some_and(|r| r.kind != self.agreed_revision.kind)
                    || self.d27_source_binding_bytes.is_some()
                    || self.capacity_plan.desktop_device.is_some()
                {
                    return Err(ApplyIntentError::Invalid("C12 descriptor context"));
                }
            }
            (
                ApplyContextV1::Session {
                    base_epoch: _,
                    session_binding,
                    base_session_revision,
                    consume_target_revision,
                    policy_revision: _,
                    source_operation_digest,
                    reserved_result_id,
                    reserved_result_id_digest,
                    base_marker_evidence_hash,
                    consume_marker_evidence_hash,
                    consume_sidecar_offset: _,
                    consume_sidecar_hash,
                    precommit_hash,
                },
                ApplyInventoryV1::D27,
            ) => {
                for h in [
                    *session_binding,
                    *source_operation_digest,
                    *reserved_result_id_digest,
                    *base_marker_evidence_hash,
                    *consume_marker_evidence_hash,
                    *consume_sidecar_hash,
                    *precommit_hash,
                ] {
                    nonzero(h)?;
                }
                if base_session_revision.checked_add(1) != Some(*consume_target_revision)
                    || polaris_desktop_protocol::ids::ResultId::new(reserved_result_id.clone())
                        .is_err()
                    || *reserved_result_id_digest != d27_result_id_digest(reserved_result_id)?
                    || self.operation_key
                        != d27_operation_key(self.project_binding, *source_operation_digest)?
                    || self.management_operation_id
                        != d27_management_operation_id(*source_operation_digest)?
                    || self.agreed_revision.kind != RevisionKindV1::Sha256
                    || self.staged_revision.kind != RevisionKindV1::Sha256
                    || self.staged_revision.value != self.workspace_apply_request_digest.as_hex()
                    || self.previous_applied_revision.is_some()
                    || self.d27_source_binding_bytes.is_none()
                    || self.capacity_plan.desktop_device.is_none()
                {
                    return Err(ApplyIntentError::Invalid("D27 descriptor context"));
                }
            }
            _ => return Err(ApplyIntentError::Invalid("inventory context")),
        }
        Ok(())
    }
}
impl ApplyIntentDescriptorV1 {
    pub fn descriptor_hash(&self) -> Result<Hash> {
        Ok(Hash::digest(&self.canonical_bytes()?))
    }
    pub fn verify_request(&self, request: &RequestV1, before: &ObservedSourceV1) -> Result<()> {
        self.validate()?;
        before.verify_before(request)?;
        let request_bytes = request.canonical_bytes()?;
        let before_bytes = before.canonical_bytes()?;
        if self.source_identity != request.source_identity
            || self.changed_path_count
                != u64::try_from(request.entries.len()).map_err(|_| ApplyIntentError::Overflow)?
            || self.path_set_digest != request.path_set_digest()?
            || self.workspace_apply_request_digest != request.request_hash()?
            || self.source_before_observation_digest != before.observation_hash()?
            || self.capacity_plan.request_bytes != request_bytes.len() as u64
            || self.capacity_plan.source_before_bytes != before_bytes.len() as u64
        {
            return Err(ApplyIntentError::Invalid(
                "request or source-before binding",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyResultV1 {
    Success,
    Failed,
    Partial,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKindV1 {
    C12Generation,
    D27Session,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildDispositionV1 {
    Absent,
    RetainedHeaderOnly,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplyEvidenceV1 {
    Prepared {
        capacity_plan_hash: Hash,
        request_hash: Hash,
        request_bytes: u64,
        source_before_hash: Hash,
        source_before_bytes: u64,
    },
    RecoveryPrepared {
        child_identity: IdentityV1,
        journal_header_hash: Hash,
        journal_header_bytes: u64,
        directory_synced: bool,
        generic_prepared_generation: u64,
        generic_prepared_hash: Hash,
        generic_prepared_sequence: u64,
    },
    EffectStarted {
        generic_generation: u64,
        generic_event_hash: Hash,
        generic_sequence: u64,
    },
    ApplyReported {
        report_hash: Hash,
        report_bytes: u64,
        journal_tip_hash: Hash,
        result: ApplyResultV1,
    },
    ParticipationCommitted {
        marker_kind: MarkerKindV1,
        marker_hash: Hash,
        committed_revision: u64,
        sidecar_hash: Option<Hash>,
        sidecar_bytes: Option<u64>,
        result_id_digest: Option<Hash>,
    },
    Completed {
        readback_hash: Hash,
        generic_generation: u64,
        generic_event_hash: Hash,
        generic_sequence: u64,
    },
    Denied {
        reason: ManagementDenialReasonV1,
        source_readback_hash: Option<Hash>,
        generic_generation: u64,
        generic_event_hash: Hash,
        generic_sequence: u64,
    },
    AbandonedBeforeEffect {
        source_readback_hash: Hash,
        child_disposition: ChildDispositionV1,
        generic_denial_reason: Option<ManagementDenialReasonV1>,
        generic_generation: Option<u64>,
        generic_event_hash: Option<Hash>,
        generic_sequence: Option<u64>,
    },
    OutcomeUnknown {
        reason: ManagementUnknownReasonV1,
        journal_tip_hash: Option<Hash>,
        report_hash: Option<Hash>,
        marker_hash: Option<Hash>,
        generic_generation: u64,
        generic_event_hash: Hash,
        generic_sequence: u64,
    },
    ReconciledAbandon {
        readback_hash: Hash,
        generic_generation: u64,
        generic_event_hash: Hash,
        generic_sequence: u64,
    },
}
impl ApplyEvidenceV1 {
    pub fn phase(&self) -> ApplyPhaseV1 {
        match self {
            Self::Prepared { .. } => ApplyPhaseV1::Prepared,
            Self::RecoveryPrepared { .. } => ApplyPhaseV1::RecoveryPrepared,
            Self::EffectStarted { .. } => ApplyPhaseV1::EffectStarted,
            Self::ApplyReported { .. } => ApplyPhaseV1::ApplyReported,
            Self::ParticipationCommitted { .. } => ApplyPhaseV1::ParticipationCommitted,
            Self::Completed { .. } => ApplyPhaseV1::Completed,
            Self::Denied { .. } => ApplyPhaseV1::Denied,
            Self::AbandonedBeforeEffect { .. } => ApplyPhaseV1::AbandonedBeforeEffect,
            Self::OutcomeUnknown { .. } => ApplyPhaseV1::OutcomeUnknown,
            Self::ReconciledAbandon { .. } => ApplyPhaseV1::ReconciledAbandon,
        }
    }
    pub fn generic_evidence(&self) -> Option<(u64, Hash, u64)> {
        match self {
            Self::Prepared { .. }
            | Self::ApplyReported { .. }
            | Self::ParticipationCommitted { .. } => None,
            Self::RecoveryPrepared {
                generic_prepared_generation: g,
                generic_prepared_hash: h,
                generic_prepared_sequence: s,
                ..
            } => Some((*g, *h, *s)),
            Self::EffectStarted {
                generic_generation: g,
                generic_event_hash: h,
                generic_sequence: s,
            }
            | Self::Completed {
                generic_generation: g,
                generic_event_hash: h,
                generic_sequence: s,
                ..
            }
            | Self::Denied {
                generic_generation: g,
                generic_event_hash: h,
                generic_sequence: s,
                ..
            }
            | Self::OutcomeUnknown {
                generic_generation: g,
                generic_event_hash: h,
                generic_sequence: s,
                ..
            }
            | Self::ReconciledAbandon {
                generic_generation: g,
                generic_event_hash: h,
                generic_sequence: s,
                ..
            } => Some((*g, *h, *s)),
            Self::AbandonedBeforeEffect {
                generic_generation: g,
                generic_event_hash: h,
                generic_sequence: s,
                ..
            } => Some(((*g)?, (*h)?, (*s)?)),
        }
    }
}
impl CanonicalApplyEvidence for ApplyEvidenceV1 {
    const MAXIMUM_BYTES: u64 = MAX_INTENT_GENERATION_BYTES;
    fn validate(&self) -> Result<()> {
        if let Some((_, h, s)) = self.generic_evidence() {
            nonzero(h)?;
            if s == 0 {
                return Err(ApplyIntentError::Invalid("generic event sequence"));
            }
        }
        let optional_hash = |h: Option<Hash>| -> Result<()> {
            if let Some(h) = h {
                nonzero(h)?;
            }
            Ok(())
        };
        match self {
            Self::Prepared {
                capacity_plan_hash,
                request_hash,
                request_bytes,
                source_before_hash,
                source_before_bytes,
            } => {
                for h in [*capacity_plan_hash, *request_hash, *source_before_hash] {
                    nonzero(h)?;
                }
                if *request_bytes > MAX_ARTIFACT_BYTES || *source_before_bytes > MAX_ARTIFACT_BYTES
                {
                    return Err(ApplyIntentError::Invalid("prepared artifact bounds"));
                }
            }
            Self::RecoveryPrepared {
                journal_header_hash,
                directory_synced,
                ..
            } => {
                nonzero(*journal_header_hash)?;
                if !directory_synced {
                    return Err(ApplyIntentError::Invalid("unsynced recovery"));
                }
            }
            Self::ApplyReported {
                report_hash,
                report_bytes,
                journal_tip_hash,
                ..
            } => {
                nonzero(*report_hash)?;
                nonzero(*journal_tip_hash)?;
                if *report_bytes > 256 * 1024 * 1024 {
                    return Err(ApplyIntentError::Invalid("report bounds"));
                }
            }
            Self::ParticipationCommitted {
                marker_kind,
                marker_hash,
                sidecar_hash,
                sidecar_bytes,
                result_id_digest,
                ..
            } => {
                nonzero(*marker_hash)?;
                optional_hash(*sidecar_hash)?;
                optional_hash(*result_id_digest)?;
                if sidecar_hash.is_some() != sidecar_bytes.is_some()
                    || (*marker_kind == MarkerKindV1::C12Generation && result_id_digest.is_some())
                    || (*marker_kind == MarkerKindV1::D27Session
                        && (sidecar_hash.is_none() || result_id_digest.is_none()))
                {
                    return Err(ApplyIntentError::Invalid("committed marker fields"));
                }
            }
            Self::Completed { readback_hash, .. }
            | Self::ReconciledAbandon { readback_hash, .. } => nonzero(*readback_hash)?,
            Self::Denied {
                source_readback_hash,
                ..
            } => optional_hash(*source_readback_hash)?,
            Self::AbandonedBeforeEffect {
                source_readback_hash,
                generic_denial_reason,
                generic_generation,
                generic_event_hash,
                generic_sequence,
                ..
            } => {
                nonzero(*source_readback_hash)?;
                let present = [
                    generic_denial_reason.is_some(),
                    generic_generation.is_some(),
                    generic_event_hash.is_some(),
                    generic_sequence.is_some(),
                ];
                if present.iter().any(|p| *p != present[0]) {
                    return Err(ApplyIntentError::Invalid("partial generic denial evidence"));
                }
            }
            Self::OutcomeUnknown {
                journal_tip_hash,
                report_hash,
                marker_hash,
                ..
            } => {
                for h in [*journal_tip_hash, *report_hash, *marker_hash] {
                    optional_hash(h)?;
                }
            }
            Self::EffectStarted { .. } => {}
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyGenerationV1 {
    pub schema_version: u8,
    pub sequence: u64,
    pub previous_record_hash: Hash,
    pub descriptor_hash: Hash,
    pub phase: ApplyPhaseV1,
    pub evidence: ApplyEvidenceV1,
}
impl CanonicalApplyEvidence for ApplyGenerationV1 {
    const MAXIMUM_BYTES: u64 = MAX_INTENT_GENERATION_BYTES;
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.sequence >= MAX_INTENT_GENERATIONS
            || self.phase != self.evidence.phase()
        {
            return Err(ApplyIntentError::Invalid(
                "generation version, sequence or phase",
            ));
        }
        nonzero(self.descriptor_hash)?;
        if self.sequence == 0 {
            if self.phase != ApplyPhaseV1::Prepared
                || self.previous_record_hash.as_hex() != "0".repeat(64)
            {
                return Err(ApplyIntentError::Invalid("generation zero"));
            }
        } else {
            nonzero(self.previous_record_hash)?;
        }
        self.evidence.validate()
    }
}
impl ApplyGenerationV1 {
    pub fn record_hash(&self) -> Result<Hash> {
        Ok(Hash::digest(&self.canonical_bytes()?))
    }
    pub fn verify_successor(&self, previous: &Self) -> Result<()> {
        self.validate()?;
        previous.validate()?;
        if previous.sequence.checked_add(1) != Some(self.sequence)
            || self.previous_record_hash != previous.record_hash()?
            || self.descriptor_hash != previous.descriptor_hash
        {
            return Err(ApplyIntentError::Invalid("generation hash chain"));
        }
        use ApplyPhaseV1::*;
        let allowed = matches!(
            (previous.phase, self.phase),
            (Prepared, RecoveryPrepared | Denied | AbandonedBeforeEffect)
                | (RecoveryPrepared, EffectStarted | AbandonedBeforeEffect)
                | (EffectStarted, ApplyReported | OutcomeUnknown)
                | (ApplyReported, ParticipationCommitted | OutcomeUnknown)
                | (ParticipationCommitted, Completed | OutcomeUnknown)
                | (OutcomeUnknown, Completed | ReconciledAbandon)
        );
        if !allowed
            || (self.phase == ParticipationCommitted
                && !matches!(
                    previous.evidence,
                    ApplyEvidenceV1::ApplyReported {
                        result: ApplyResultV1::Success,
                        ..
                    }
                ))
        {
            return Err(ApplyIntentError::Invalid("illegal phase transition"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyIntentEnvelopeV1 {
    pub schema_version: u8,
    pub descriptor: Option<ApplyIntentDescriptorV1>,
    pub descriptor_hash: Hash,
    pub generation: ApplyGenerationV1,
    pub record_hash: Hash,
}
impl CanonicalApplyEvidence for ApplyIntentEnvelopeV1 {
    const MAXIMUM_BYTES: u64 = MAX_INTENT_GENERATION_BYTES;
    fn validate(&self) -> Result<()> {
        self.generation.validate()?;
        if self.schema_version != 1
            || self.descriptor_hash != self.generation.descriptor_hash
            || self.record_hash != self.generation.record_hash()?
        {
            return Err(ApplyIntentError::Invalid("envelope hash or version"));
        }
        match (&self.descriptor, self.generation.sequence) {
            (Some(d), 0) => {
                if d.descriptor_hash()? != self.descriptor_hash {
                    return Err(ApplyIntentError::Invalid("descriptor hash"));
                }
                match &self.generation.evidence {
                    ApplyEvidenceV1::Prepared {
                        capacity_plan_hash,
                        request_hash,
                        request_bytes,
                        source_before_hash,
                        source_before_bytes,
                    } if *capacity_plan_hash == d.capacity_plan_hash
                        && *request_hash == d.workspace_apply_request_digest
                        && *request_bytes == d.capacity_plan.request_bytes
                        && *source_before_hash == d.source_before_observation_digest
                        && *source_before_bytes == d.capacity_plan.source_before_bytes => {}
                    _ => return Err(ApplyIntentError::Invalid("prepared descriptor evidence")),
                }
            }
            (None, 1..=11) => {}
            _ => return Err(ApplyIntentError::Invalid("descriptor presence")),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D27ManifestVersionV1 {
    pub device: u64,
    pub inode: u64,
    pub hash: Hash,
    pub normal_mode: u32,
    pub observed_mode: u32,
    pub bytes: u64,
}
impl D27ManifestVersionV1 {
    fn before(&self) -> BeforeV1 {
        BeforeV1 {
            device: self.device,
            inode: self.inode,
            hash: self.hash,
            mode: self.normal_mode,
            observed_mode: self.observed_mode,
            bytes: self.bytes,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D27ManifestProjectionEntryV1 {
    pub path: String,
    pub before: Option<D27ManifestVersionV1>,
    pub after: Option<D27ManifestVersionV1>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D27ManifestProjectionV1 {
    pub schema_version: u8,
    pub source_path: String,
    pub source_identity: IdentityV1,
    pub recovery_parent_path: String,
    pub recovery_parent_identity: IdentityV1,
    pub entries: Vec<D27ManifestProjectionEntryV1>,
}
impl CanonicalApplyEvidence for D27ManifestProjectionV1 {
    const MAXIMUM_BYTES: u64 = MAX_ARTIFACT_BYTES;
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || !Path::new(&self.source_path).is_absolute()
            || !Path::new(&self.recovery_parent_path).is_absolute()
            || [&self.source_path, &self.recovery_parent_path]
                .into_iter()
                .any(|s| {
                    s.bytes().any(|b| b < 32 || b == 127 || b == b'\\')
                        || s.split('/')
                            .skip(1)
                            .any(|p| p.is_empty() || p == "." || p == "..")
                })
        {
            return Err(ApplyIntentError::Invalid("manifest root path"));
        }
        validate_paths(self.entries.iter().map(|e| e.path.as_str()))?;
        for entry in &self.entries {
            if entry.before.is_none() && entry.after.is_none() {
                return Err(ApplyIntentError::Invalid("empty manifest change"));
            }
            for v in [&entry.before, &entry.after].into_iter().flatten() {
                v.before().validate()?;
            }
        }
        self.project_request()?.validate()
    }
}
impl D27ManifestProjectionV1 {
    pub fn project_request(&self) -> Result<RequestV1> {
        Ok(RequestV1 {
            schema_version: 1,
            source_identity: self.source_identity,
            entries: self
                .entries
                .iter()
                .map(|e| RequestEntryV1 {
                    path: e.path.clone(),
                    before: e.before.as_ref().map(D27ManifestVersionV1::before),
                    after: e.after.as_ref().map(|a| AfterV1 {
                        hash: a.hash,
                        mode: a.normal_mode,
                        bytes: a.bytes,
                    }),
                })
                .collect(),
        })
    }
    pub fn manifest_hash(&self) -> Result<Hash> {
        Ok(digest(
            b"polaris-d27-source-manifest-v1\0",
            &self.canonical_bytes()?,
        ))
    }
    pub fn recovery_provenance_digest(&self) -> Result<Hash> {
        self.validate()?;
        let n = u32::try_from(self.recovery_parent_path.len())
            .map_err(|_| ApplyIntentError::Overflow)?;
        let mut bytes = n.to_be_bytes().to_vec();
        bytes.extend_from_slice(self.recovery_parent_path.as_bytes());
        bytes.extend_from_slice(&self.recovery_parent_identity.device.to_be_bytes());
        bytes.extend_from_slice(&self.recovery_parent_identity.inode.to_be_bytes());
        Ok(digest(b"polaris-d27-recovery-provenance-v1\0", &bytes))
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D27SourceBindingV1 {
    pub schema_version: u8,
    pub payload_bytes: u64,
    pub payload_hash: Hash,
    pub manifest_bytes: u64,
    pub manifest_hash: Hash,
    pub request_bytes: u64,
    pub request_hash: Hash,
    pub source_before_bytes: u64,
    pub source_before_hash: Hash,
    pub manifest: D27ManifestProjectionV1,
}
impl CanonicalApplyEvidence for D27SourceBindingV1 {
    const MAXIMUM_BYTES: u64 = MAX_ARTIFACT_BYTES;
    fn validate(&self) -> Result<()> {
        self.manifest.validate()?;
        for h in [
            self.payload_hash,
            self.manifest_hash,
            self.request_hash,
            self.source_before_hash,
        ] {
            nonzero(h)?;
        }
        let manifest = self.manifest.canonical_bytes()?;
        let request = self.manifest.project_request()?;
        if self.schema_version != 1
            || self.payload_bytes > 1024 * 1024
            || self.manifest_bytes != manifest.len() as u64
            || self.manifest_hash != self.manifest.manifest_hash()?
            || self.request_bytes != request.canonical_bytes()?.len() as u64
            || self.request_hash != request.request_hash()?
            || self.source_before_bytes > MAX_ARTIFACT_BYTES
        {
            return Err(ApplyIntentError::Invalid("D27 source binding"));
        }
        Ok(())
    }
}
impl D27SourceBindingV1 {
    pub fn binding_hash(&self) -> Result<Hash> {
        Ok(digest(
            b"polaris-d27-source-binding-v1\0",
            &self.canonical_bytes()?,
        ))
    }
    pub fn verify_artifacts(
        &self,
        descriptor: &ApplyIntentDescriptorV1,
        request: &RequestV1,
        before: &ObservedSourceV1,
    ) -> Result<()> {
        self.validate()?;
        descriptor.verify_request(request, before)?;
        let bytes = self.canonical_bytes()?;
        if descriptor.inventory_id != ApplyInventoryV1::D27
            || descriptor.d27_source_binding_bytes != Some(bytes.len() as u64)
            || descriptor.d27_source_binding_hash != Some(self.binding_hash()?)
            || descriptor.share_source_manifest_digest != self.manifest_hash
            || descriptor.agreed_revision.value != self.payload_hash.as_hex()
            || descriptor.recovery_parent_identity != self.manifest.recovery_parent_identity
            || descriptor.recovery_provenance_digest
                != self.manifest.recovery_provenance_digest()?
            || self.manifest.project_request()? != *request
            || self.source_before_bytes != before.canonical_bytes()?.len() as u64
            || self.source_before_hash != before.observation_hash()?
        {
            return Err(ApplyIntentError::Invalid("D27 durable artifact join"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalReadbackV1 {
    pub schema_version: u8,
    pub descriptor_hash: Hash,
    pub request_hash: Hash,
    pub source_observation_hash: Hash,
    pub journal_tip_hash: Option<Hash>,
    pub report_hash: Option<Hash>,
    pub marker_kind: Option<MarkerKindV1>,
    pub marker_hash: Option<Hash>,
    pub agreed_revision: RevisionV1,
}
impl CanonicalApplyEvidence for FinalReadbackV1 {
    const MAXIMUM_BYTES: u64 = MAX_INTENT_GENERATION_BYTES;
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 || self.marker_kind.is_some() != self.marker_hash.is_some() {
            return Err(ApplyIntentError::Invalid("final readback"));
        }
        for h in [
            Some(self.descriptor_hash),
            Some(self.request_hash),
            Some(self.source_observation_hash),
            self.journal_tip_hash,
            self.report_hash,
            self.marker_hash,
        ]
        .into_iter()
        .flatten()
        {
            nonzero(h)?;
        }
        self.agreed_revision.validate()
    }
}
impl FinalReadbackV1 {
    pub fn readback_hash(&self) -> Result<Hash> {
        Ok(digest(
            b"polaris-apply-final-readback-v1\0",
            &self.canonical_bytes()?,
        ))
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSealV1 {
    pub schema_version: u8,
    pub operation_key: Hash,
    pub descriptor_hash: Hash,
    pub terminal_sequence: u64,
    pub terminal_record_hash: Hash,
    pub terminal_phase: ApplyPhaseV1,
    pub request_hash: Hash,
    pub source_before_hash: Hash,
    pub journal_header_hash: Option<Hash>,
    pub journal_tip_hash: Option<Hash>,
    pub report_hash: Option<Hash>,
    pub marker_kind: Option<MarkerKindV1>,
    pub marker_hash: Option<Hash>,
    pub generic_terminal_generation: Option<u64>,
    pub generic_terminal_hash: Option<Hash>,
    pub generic_terminal_sequence: Option<u64>,
    pub final_readback_hash: Option<Hash>,
}
impl CanonicalApplyEvidence for ArchiveSealV1 {
    const MAXIMUM_BYTES: u64 = MAX_INTENT_GENERATION_BYTES;
    fn validate(&self) -> Result<()> {
        let present = [
            self.generic_terminal_generation.is_some(),
            self.generic_terminal_hash.is_some(),
            self.generic_terminal_sequence.is_some(),
        ];
        if self.schema_version != 1
            || !self.terminal_phase.is_terminal()
            || self.terminal_sequence >= MAX_INTENT_GENERATIONS
            || present.iter().any(|p| *p != present[0])
            || (self.terminal_phase != ApplyPhaseV1::AbandonedBeforeEffect && !present[0])
            || self.generic_terminal_sequence == Some(0)
            || self.marker_kind.is_some() != self.marker_hash.is_some()
            || (self.terminal_phase == ApplyPhaseV1::Completed
                && (self.report_hash.is_none()
                    || self.marker_hash.is_none()
                    || self.final_readback_hash.is_none()))
        {
            return Err(ApplyIntentError::Invalid("archive seal fields"));
        }
        for h in [
            Some(self.operation_key),
            Some(self.descriptor_hash),
            Some(self.terminal_record_hash),
            Some(self.request_hash),
            Some(self.source_before_hash),
            self.journal_header_hash,
            self.journal_tip_hash,
            self.report_hash,
            self.marker_hash,
            self.generic_terminal_hash,
            self.final_readback_hash,
        ]
        .into_iter()
        .flatten()
        {
            nonzero(h)?;
        }
        Ok(())
    }
}
impl ArchiveSealV1 {
    pub fn seal_hash(&self) -> Result<Hash> {
        Ok(digest(
            b"polaris-apply-archive-seal-v1\0",
            &self.canonical_bytes()?,
        ))
    }
    pub fn verify_terminal(
        &self,
        descriptor: &ApplyIntentDescriptorV1,
        terminal: &ApplyGenerationV1,
    ) -> Result<()> {
        self.validate()?;
        terminal.validate()?;
        let generic = self
            .generic_terminal_generation
            .zip(self.generic_terminal_hash)
            .zip(self.generic_terminal_sequence)
            .map(|((g, h), s)| (g, h, s));
        if self.operation_key != descriptor.operation_key
            || self.descriptor_hash != descriptor.descriptor_hash()?
            || self.terminal_sequence != terminal.sequence
            || self.terminal_record_hash != terminal.record_hash()?
            || self.terminal_phase != terminal.phase
            || self.request_hash != descriptor.workspace_apply_request_digest
            || self.source_before_hash != descriptor.source_before_observation_digest
            || generic != terminal.evidence.generic_evidence()
        {
            return Err(ApplyIntentError::Invalid("archive terminal join"));
        }
        match &terminal.evidence {
            ApplyEvidenceV1::Completed { readback_hash, .. }
            | ApplyEvidenceV1::ReconciledAbandon { readback_hash, .. }
                if self.final_readback_hash != Some(*readback_hash) =>
            {
                return Err(ApplyIntentError::Invalid("archive final readback"));
            }
            _ => {}
        }
        Ok(())
    }
}

use crate::project_mutation::ProjectMutationLeaseV1;
use std::{
    ffi::{CString, OsStr},
    fs::{File, Metadata},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileExt, MetadataExt},
    },
    path::{Component, PathBuf},
};

/// A leaf in a no-follow component walk. The descriptor stays open while the
/// corresponding path is rechecked; names never replace that identity.
#[derive(Debug)]
struct IntentDirectory {
    file: File,
    path: PathBuf,
    identity: IdentityV1,
}
fn file_identity(metadata: &Metadata) -> IdentityV1 {
    IdentityV1 {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}
fn private_metadata(metadata: &Metadata, directory: bool) -> Result<()> {
    let wanted = if directory { 0o700 } else { 0o600 };
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != wanted
        || (directory && !metadata.is_dir())
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
    {
        return Err(ApplyIntentError::Invalid("unsafe intent inode"));
    }
    Ok(())
}
fn component(name: &OsStr) -> Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(ApplyIntentError::Invalid("intent component"));
    }
    CString::new(bytes).map_err(|_| ApplyIntentError::Invalid("NUL component"))
}
fn owned_fd(fd: libc::c_int) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful openat returns a new descriptor owned by this call.
    Ok(unsafe { File::from_raw_fd(fd) })
}
impl IntentDirectory {
    fn open(path: &Path) -> Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(ApplyIntentError::Invalid("absolute canonical intent path"));
        }
        let mut file = File::open("/")?;
        for part in path.components() {
            if let Component::Normal(name) = part {
                let name = component(name)?;
                // SAFETY: live parent descriptor and NUL-terminated single component.
                file = owned_fd(unsafe {
                    libc::openat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                })?;
            }
        }
        let metadata = file.metadata()?;
        private_metadata(&metadata, true)?;
        let directory = Self {
            file,
            path: path.to_owned(),
            identity: file_identity(&metadata),
        };
        directory.verify_leaf()?;
        Ok(directory)
    }
    fn verify_leaf(&self) -> Result<()> {
        let metadata = std::fs::symlink_metadata(&self.path)?;
        private_metadata(&metadata, true)?;
        if file_identity(&metadata) != self.identity
            || file_identity(&self.file.metadata()?) != self.identity
        {
            return Err(ApplyIntentError::Invalid("intent directory raced"));
        }
        Ok(())
    }
    fn verify(&self) -> Result<()> {
        self.verify_leaf()?;
        let current = Self::open(&self.path)?;
        if current.identity != self.identity {
            return Err(ApplyIntentError::Invalid("intent directory raced"));
        }
        Ok(())
    }
    fn child(&self, name: &str, create: bool) -> Result<Self> {
        self.verify()?;
        let component = component(OsStr::new(name))?;
        if create {
            // SAFETY: pinned directory and one validated component.
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), component.as_ptr(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.file.sync_all()?;
        }
        let file = owned_fd(unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        let metadata = file.metadata()?;
        private_metadata(&metadata, true)?;
        let child = Self {
            file,
            path: self.path.join(name),
            identity: file_identity(&metadata),
        };
        self.verify()?;
        child.verify()?;
        Ok(child)
    }
    fn optional_child(&self, name: &str) -> Result<Option<Self>> {
        match self.child(name, false) {
            Ok(child) => Ok(Some(child)),
            Err(ApplyIntentError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn entry_identity(&self, name: &CString) -> Result<IdentityV1> {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid output pointer and validated relative component.
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let metadata = unsafe { metadata.assume_init() };
        Ok(IdentityV1 {
            device: metadata.st_dev as u64,
            inode: metadata.st_ino as u64,
        })
    }
    fn open_file(&self, name: &str, write: bool, create: bool) -> Result<File> {
        self.verify()?;
        let name = component(OsStr::new(name))?;
        let flags = (if write {
            libc::O_RDWR | libc::O_APPEND
        } else {
            libc::O_RDONLY
        }) | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if create {
                libc::O_CREAT | libc::O_EXCL
            } else {
                0
            };
        let file =
            owned_fd(unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags, 0o600) })?;
        self.verify_file(&name, &file)?;
        Ok(file)
    }
    fn verify_file(&self, name: &CString, file: &File) -> Result<()> {
        self.verify()?;
        let metadata = file.metadata()?;
        private_metadata(&metadata, false)?;
        if self.entry_identity(name)? != file_identity(&metadata) {
            return Err(ApplyIntentError::Invalid("intent file raced"));
        }
        Ok(())
    }
    fn read(&self, name: &str, maximum: u64) -> Result<Vec<u8>> {
        let file = self.open_file(name, false, false)?;
        let bytes = read_bounded(&file, maximum)?;
        self.verify_file(&component(OsStr::new(name))?, &file)?;
        Ok(bytes)
    }
    fn names(&self, maximum: Option<usize>) -> Result<Vec<String>> {
        let mut names = Vec::new();
        self.visit_names(|name| {
            if maximum.is_some_and(|max| names.len() >= max) {
                return Err(ApplyIntentError::Invalid("intent entry bound"));
            }
            names.push(name.to_owned());
            Ok(())
        })?;
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        Ok(names)
    }
    fn visit_names(&self, mut visit: impl FnMut(&str) -> Result<()>) -> Result<()> {
        self.verify()?;
        // Open "." rather than dup: each enumeration needs an independent
        // directory position, not the shared position of the pinned handle.
        let dot = CString::new(".").expect("constant component");
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error.into());
        }
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }
        let stream = Stream(stream);
        loop {
            // errno must be reset to distinguish end-of-directory from error.
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error().unwrap_or(0) != 0 {
                    return Err(error.into());
                }
                break;
            }
            let raw = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }
            let name = std::str::from_utf8(raw)
                .map_err(|_| ApplyIntentError::Invalid("intent entry encoding"))?;
            if name.bytes().any(|b| b < 32 || b == 127) {
                return Err(ApplyIntentError::Invalid("intent entry name"));
            }
            visit(name)?;
        }
        self.verify()
    }
    fn write_exact_new(&self, name: &str, bytes: &[u8], maximum: u64) -> Result<()> {
        checked_len(bytes, maximum)?;
        let mut file = self.open_file(name, true, true)?;
        #[cfg(test)]
        tests::checkpoint("new-created")?;
        file.write_all(bytes)?;
        #[cfg(test)]
        tests::checkpoint("new-written")?;
        file.sync_data()?;
        #[cfg(test)]
        tests::checkpoint("new-data-synced")?;
        if read_bounded(&file, maximum)? != bytes {
            return Err(ApplyIntentError::Invalid("intent write readback"));
        }
        self.verify_file(&component(OsStr::new(name))?, &file)?;
        self.file.sync_all()?;
        Ok(())
    }
    fn publish_exact(&self, name: &str, bytes: &[u8], maximum: u64) -> Result<()> {
        checked_len(bytes, maximum)?;
        let temporary = format!(".{name}.tmp");
        let mut file = match self.open_file(&temporary, true, false) {
            Ok(file) => file,
            Err(ApplyIntentError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.open_file(&temporary, true, true)?
            }
            Err(e) => return Err(e),
        };
        #[cfg(test)]
        tests::checkpoint("publication-opened")?;
        let original = read_bounded(&file, maximum)?;
        if !bytes.starts_with(&original) {
            return Err(ApplyIntentError::Invalid("foreign intent prefix"));
        }
        self.verify_file(&component(OsStr::new(&temporary))?, &file)?;
        // Append only the missing suffix to this same pinned inode.
        file.write_all(&bytes[original.len()..])?;
        #[cfg(test)]
        tests::checkpoint("publication-written")?;
        file.sync_data()?;
        #[cfg(test)]
        tests::checkpoint("publication-data-synced")?;
        if read_bounded(&file, maximum)? != bytes {
            return Err(ApplyIntentError::Invalid("intent publication readback"));
        }
        self.verify_file(&component(OsStr::new(&temporary))?, &file)?;
        self.file.sync_all()?;
        #[cfg(test)]
        tests::checkpoint("publication-parent-synced")?;
        let expected = file_identity(&file.metadata()?);
        rename_directory_entry(self, &temporary, self, name)?;
        #[cfg(test)]
        tests::checkpoint("publication-renamed")?;
        if self.entry_identity(&component(OsStr::new(name))?)? != expected
            || self.read(name, maximum)? != bytes
        {
            return Err(ApplyIntentError::Invalid("intent publication raced"));
        }
        #[cfg(test)]
        tests::checkpoint("publication-readback")?;
        self.file.sync_all()?;
        #[cfg(test)]
        tests::checkpoint("publication-final-synced")?;
        Ok(())
    }
}
fn read_bounded(file: &File, maximum: u64) -> Result<Vec<u8>> {
    let initial = file.metadata()?;
    private_metadata(&initial, false)?;
    if initial.len() > maximum {
        return Err(ApplyIntentError::TooLarge {
            actual: initial.len(),
            maximum,
        });
    }
    let limit = usize::try_from(maximum.checked_add(1).ok_or(ApplyIntentError::Overflow)?)
        .map_err(|_| ApplyIntentError::Overflow)?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let want = (limit - bytes.len()).min(buffer.len());
        if want == 0 {
            break;
        }
        let count = file.read_at(&mut buffer[..want], bytes.len() as u64)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    checked_len(&bytes, maximum)?;
    let after = file.metadata()?;
    if file_identity(&initial) != file_identity(&after)
        || initial.len() != after.len()
        || initial.mode() != after.mode()
        || initial.uid() != after.uid()
        || initial.nlink() != after.nlink()
        || initial.mtime() != after.mtime()
        || initial.mtime_nsec() != after.mtime_nsec()
        || initial.ctime() != after.ctime()
        || initial.ctime_nsec() != after.ctime_nsec()
        || bytes.len() as u64 != after.len()
    {
        return Err(ApplyIntentError::Invalid(
            "intent file changed while reading",
        ));
    }
    Ok(bytes)
}
fn rename_directory_entry(
    from: &IntentDirectory,
    old: &str,
    to: &IntentDirectory,
    new: &str,
) -> Result<()> {
    from.verify()?;
    to.verify()?;
    let old = component(OsStr::new(old))?;
    let new = component(OsStr::new(new))?;
    #[cfg(target_os = "linux")]
    let code = unsafe {
        libc::renameat2(
            from.file.as_raw_fd(),
            old.as_ptr(),
            to.file.as_raw_fd(),
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let code = unsafe {
        libc::renameatx_np(
            from.file.as_raw_fd(),
            old.as_ptr(),
            to.file.as_raw_fd(),
            new.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if code != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    from.file.sync_all()?;
    to.file.sync_all()?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct InspectedApplyIntentV1 {
    pub request: RequestV1,
    pub source_before: ObservedSourceV1,
    pub d27_source_binding: Option<D27SourceBindingV1>,
    pub generations: Vec<ApplyIntentEnvelopeV1>,
    pub pending_generation: Option<Vec<u8>>,
    pub seal: Option<ArchiveSealV1>,
}
impl InspectedApplyIntentV1 {
    pub fn descriptor(&self) -> &ApplyIntentDescriptorV1 {
        self.generations[0]
            .descriptor
            .as_ref()
            .expect("verified generation zero")
    }
    pub fn latest(&self) -> &ApplyGenerationV1 {
        &self
            .generations
            .last()
            .expect("nonempty verified chain")
            .generation
    }
    fn verify_seal_chain(&self, seal: &ArchiveSealV1) -> Result<()> {
        seal.verify_terminal(self.descriptor(), self.latest())?;
        let mut header = None;
        let mut tip = None;
        let mut report = None;
        let mut marker_kind = None;
        let mut marker = None;
        for envelope in &self.generations {
            match &envelope.generation.evidence {
                ApplyEvidenceV1::RecoveryPrepared {
                    journal_header_hash,
                    ..
                } => {
                    header = Some(*journal_header_hash);
                }
                ApplyEvidenceV1::ApplyReported {
                    report_hash,
                    journal_tip_hash,
                    ..
                } => {
                    report = Some(*report_hash);
                    tip = Some(*journal_tip_hash);
                }
                ApplyEvidenceV1::ParticipationCommitted {
                    marker_kind: kind,
                    marker_hash,
                    ..
                } => {
                    marker_kind = Some(*kind);
                    marker = Some(*marker_hash);
                }
                ApplyEvidenceV1::OutcomeUnknown {
                    journal_tip_hash,
                    report_hash,
                    marker_hash,
                    ..
                } => {
                    // A receipt may become durable before its phase record.
                    // Only pointers explicitly retained by that unknown record
                    // may supplement or replace the earlier durable evidence.
                    if let Some(hash) = journal_tip_hash {
                        tip = Some(*hash);
                    }
                    if let Some(hash) = report_hash {
                        report = Some(*hash);
                    }
                    if let Some(hash) = marker_hash {
                        marker = Some(*hash);
                        marker_kind = Some(match self.descriptor().inventory_id {
                            ApplyInventoryV1::C12 => MarkerKindV1::C12Generation,
                            ApplyInventoryV1::D27 => MarkerKindV1::D27Session,
                        });
                    }
                }
                _ => {}
            }
        }
        if (
            seal.journal_header_hash,
            seal.journal_tip_hash,
            seal.report_hash,
            seal.marker_kind,
            seal.marker_hash,
        ) != (header, tip, report, marker_kind, marker)
        {
            return Err(ApplyIntentError::Invalid("seal durable pointers"));
        }
        Ok(())
    }
}

/// Internal to the shared owner: adapters cannot publish intents directly.
/// The project mutation lease remains borrowed for the entire store lifetime.
#[derive(Debug)]
pub(crate) struct ApplyIntentStoreV1<'lease> {
    state: IntentDirectory,
    root: Option<IntentDirectory>,
    lease: &'lease ProjectMutationLeaseV1,
}
impl<'lease> ApplyIntentStoreV1<'lease> {
    pub(crate) fn inspect(state: &Path, lease: &'lease ProjectMutationLeaseV1) -> Result<Self> {
        let state = IntentDirectory::open(state)?;
        let lock_root = IntentDirectory::open(lease.store_root())?;
        let (device, inode) = lease.store_identity();
        if lease.store_root() != state.path.join("rewind-workspace")
            || lock_root.identity != (IdentityV1 { device, inode })
        {
            return Err(ApplyIntentError::Invalid("project mutation lease binding"));
        }
        let root = state.optional_child("apply-intents-v1")?;
        if let Some(root) = &root {
            let names = root.names(Some(3))?;
            if names != ["active", "archive", "staging"] {
                return Err(ApplyIntentError::Invalid("intent root layout"));
            }
            for name in &names {
                root.child(name, false)?;
            }
        }
        Ok(Self { state, root, lease })
    }
    fn verify(&self) -> Result<()> {
        self.state.verify()?;
        let lock_root = IntentDirectory::open(self.lease.store_root())?;
        let (device, inode) = self.lease.store_identity();
        if lock_root.identity != (IdentityV1 { device, inode }) {
            return Err(ApplyIntentError::Invalid("project mutation lease changed"));
        }
        if let Some(root) = &self.root {
            root.verify()?;
        }
        Ok(())
    }
    fn initialize(&mut self) -> Result<()> {
        self.verify()?;
        if self.root.is_none() {
            let root = self.state.child("apply-intents-v1", true)?;
            for name in ["staging", "active", "archive"] {
                root.child(name, true)?;
            }
            root.file.sync_all()?;
            self.root = Some(root);
        }
        Ok(())
    }
    pub(crate) fn active(&self) -> Result<Option<InspectedApplyIntentV1>> {
        self.verify()?;
        let Some(root) = &self.root else {
            return Ok(None);
        };
        let active = root.child("active", false)?;
        let names = active.names(Some(1))?;
        let Some(key) = names.first() else {
            return Ok(None);
        };
        let key_hash =
            Hash::from_hex(key).map_err(|_| ApplyIntentError::Invalid("active intent key"))?;
        nonzero(key_hash)?;
        let inspected = inspect_intent_directory(&active.child(key, false)?, false)?;
        if inspected.descriptor().operation_key != key_hash {
            return Err(ApplyIntentError::Invalid("active descriptor key"));
        }
        Ok(Some(inspected))
    }
    pub(crate) fn archived(&self, key: Hash) -> Result<Option<InspectedApplyIntentV1>> {
        self.verify()?;
        nonzero(key)?;
        let Some(root) = &self.root else {
            return Ok(None);
        };
        let archive = root.child("archive", false)?;
        let Some(directory) = archive.optional_child(&key.as_hex())? else {
            return Ok(None);
        };
        let inspected = inspect_intent_directory(&directory, true)?;
        if inspected.descriptor().operation_key != key {
            return Err(ApplyIntentError::Invalid("archive descriptor key"));
        }
        Ok(Some(inspected))
    }
    pub(crate) fn archive_inventory(&self) -> Result<(u64, u64)> {
        self.verify()?;
        let Some(root) = &self.root else {
            return Ok((0, 0));
        };
        let archive = root.child("archive", false)?;
        let mut count = 0_u64;
        let mut bytes = 0_u64;
        // Streaming discovery is only capacity evidence, never authorization.
        archive.visit_names(|name| {
            let key = Hash::from_hex(name).map_err(|_| ApplyIntentError::Invalid("archive key"))?;
            nonzero(key)?;
            let directory = archive.child(name, false)?;
            let inspected = inspect_intent_directory(&directory, true)?;
            if inspected.descriptor().operation_key != key {
                return Err(ApplyIntentError::Invalid("archive key binding"));
            }
            count = sum([count, 1])?;
            for name in directory.names(Some(17))? {
                let file = directory.open_file(&name, false, false)?;
                bytes = sum([bytes, file.metadata()?.len()])?;
            }
            Ok(())
        })?;
        Ok((count, bytes))
    }
    pub(crate) fn publish_generation_zero(
        &mut self,
        envelope: ApplyIntentEnvelopeV1,
        request: &RequestV1,
        before: &ObservedSourceV1,
        binding: Option<&D27SourceBindingV1>,
    ) -> Result<()> {
        // All payloads and the fixed-width generation bound are checked before
        // creating any store/staging entry.
        let generation_bytes = envelope.canonical_bytes()?;
        let descriptor = envelope
            .descriptor
            .as_ref()
            .ok_or(ApplyIntentError::Invalid("missing admission descriptor"))?;
        if envelope.generation.sequence != 0 {
            return Err(ApplyIntentError::Invalid("admission sequence"));
        }
        descriptor.verify_request(request, before)?;
        let request_bytes = request.canonical_bytes()?;
        let before_bytes = before.canonical_bytes()?;
        let binding_bytes = match (descriptor.inventory_id, binding) {
            (ApplyInventoryV1::C12, None) => None,
            (ApplyInventoryV1::D27, Some(binding)) => {
                binding.verify_artifacts(descriptor, request, before)?;
                Some(binding.canonical_bytes()?)
            }
            _ => return Err(ApplyIntentError::Invalid("binding artifact presence")),
        };
        if descriptor.capacity_plan.state_identity != self.state.identity {
            return Err(ApplyIntentError::Invalid("state capacity identity"));
        }
        self.verify()?;
        let key = descriptor.operation_key.as_hex();
        if let Some(root) = &self.root {
            if !root.child("staging", false)?.names(Some(1))?.is_empty()
                || self.active()?.is_some()
                || self.archived(descriptor.operation_key)?.is_some()
            {
                return Err(ApplyIntentError::Invalid(
                    "occupied intent slot or archive collision",
                ));
            }
        }
        let mut nonce = [0_u8; 12];
        getrandom::fill(&mut nonce)
            .map_err(|_| ApplyIntentError::Invalid("intent nonce unavailable"))?;
        let suffix = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
        self.initialize()?;
        let root = self.root.as_ref().expect("initialized");
        let staging = root.child("staging", false)?;
        let active = root.child("active", false)?;
        let name = format!(".{key}.{suffix}.tmp");
        publish_admission_directory(
            &staging,
            &active,
            &name,
            AdmissionArtifacts {
                envelope: &envelope,
                request_bytes: &request_bytes,
                before_bytes: &before_bytes,
                binding_bytes: binding_bytes.as_deref(),
                generation_bytes: &generation_bytes,
            },
        )
    }
    pub(crate) fn append(&self, key: Hash, generation: ApplyGenerationV1) -> Result<()> {
        let current = self
            .active()?
            .ok_or(ApplyIntentError::Invalid("no active intent"))?;
        if current.descriptor().operation_key != key {
            return Err(ApplyIntentError::Invalid("append operation key"));
        }
        if generation == *current.latest() {
            generation.validate()?;
            return Ok(());
        }
        generation.verify_successor(current.latest())?;
        let envelope = ApplyIntentEnvelopeV1 {
            schema_version: 1,
            descriptor: None,
            descriptor_hash: generation.descriptor_hash,
            record_hash: generation.record_hash()?,
            generation,
        };
        let bytes = envelope.canonical_bytes()?;
        let root = self.root.as_ref().expect("active root");
        let directory = root.child("active", false)?.child(&key.as_hex(), false)?;
        if current.seal.is_some() {
            return Err(ApplyIntentError::Invalid("sealed active intent"));
        }
        directory.publish_exact(
            &format!("g{:02}.v1.json", envelope.generation.sequence),
            &bytes,
            MAX_INTENT_GENERATION_BYTES,
        )?;
        let published = inspect_intent_directory(&directory, false)?;
        if published.generations.last() != Some(&envelope) {
            return Err(ApplyIntentError::Invalid("append readback"));
        }
        Ok(())
    }
    /// Caller must first verify generic terminal, report, marker and readback.
    /// A failed publication retains active/seal evidence and never frees a slot.
    pub(crate) fn archive_terminal(&self, key: Hash, seal: &ArchiveSealV1) -> Result<()> {
        // Validate and bound the payload before opening any persistent entry.
        let bytes = seal.canonical_bytes()?;
        let current = self
            .active()?
            .ok_or(ApplyIntentError::Invalid("no active intent"))?;
        if current.descriptor().operation_key != key {
            return Err(ApplyIntentError::Invalid("archive operation key"));
        }
        current.verify_seal_chain(seal)?;
        if current.pending_generation.is_some() {
            return Err(ApplyIntentError::Invalid("pending generation at archive"));
        }
        let root = self.root.as_ref().expect("active root");
        let active = root.child("active", false)?;
        let archive = root.child("archive", false)?;
        if archive.optional_child(&key.as_hex())?.is_some() {
            return Err(ApplyIntentError::Invalid("archive collision"));
        }
        let directory = active.child(&key.as_hex(), false)?;
        if let Some(existing) = current.seal {
            if existing != *seal {
                return Err(ApplyIntentError::Invalid("changed archive seal"));
            }
        } else {
            directory.publish_exact("archive-seal.v1.json", &bytes, MAX_INTENT_GENERATION_BYTES)?;
        }
        #[cfg(test)]
        tests::checkpoint("archive-seal-published")?;
        let sealed = inspect_intent_directory(&directory, true)?;
        sealed.verify_seal_chain(seal)?;
        let expected = directory.identity;
        rename_directory_entry(&active, &key.as_hex(), &archive, &key.as_hex())?;
        #[cfg(test)]
        tests::checkpoint("archive-renamed")?;
        let published = archive.child(&key.as_hex(), false)?;
        if published.identity != expected {
            return Err(ApplyIntentError::Invalid("archive publication identity"));
        }
        inspect_intent_directory(&published, true)?;
        active.file.sync_all()?;
        archive.file.sync_all()?;
        #[cfg(test)]
        tests::checkpoint("archive-final-synced")?;
        Ok(())
    }
}
// This private writer receives already joined canonical artifacts. The store
// checks lease/state identity and occupancy before calling it. Keeping the
// publication in one place also lets the frozen wire fixture use stable IDs;
// full store tests separately exercise the real pinned state/lease guard.
struct AdmissionArtifacts<'a> {
    envelope: &'a ApplyIntentEnvelopeV1,
    request_bytes: &'a [u8],
    before_bytes: &'a [u8],
    binding_bytes: Option<&'a [u8]>,
    generation_bytes: &'a [u8],
}
fn publish_admission_directory(
    staging: &IntentDirectory,
    active: &IntentDirectory,
    name: &str,
    artifacts: AdmissionArtifacts<'_>,
) -> Result<()> {
    checked_len(artifacts.request_bytes, MAX_ARTIFACT_BYTES)?;
    checked_len(artifacts.before_bytes, MAX_ARTIFACT_BYTES)?;
    checked_len(artifacts.generation_bytes, MAX_INTENT_GENERATION_BYTES)?;
    if let Some(bytes) = artifacts.binding_bytes {
        checked_len(bytes, MAX_ARTIFACT_BYTES)?;
    }
    let key = artifacts
        .envelope
        .descriptor
        .as_ref()
        .ok_or(ApplyIntentError::Invalid("missing admission descriptor"))?
        .operation_key
        .as_hex();
    let staged = staging.child(name, true)?;
    #[cfg(test)]
    tests::checkpoint("admission-staging-created")?;
    staged.write_exact_new(
        "request.v1.json",
        artifacts.request_bytes,
        MAX_ARTIFACT_BYTES,
    )?;
    #[cfg(test)]
    tests::checkpoint("admission-request-written")?;
    staged.write_exact_new(
        "source-before.v1.json",
        artifacts.before_bytes,
        MAX_ARTIFACT_BYTES,
    )?;
    #[cfg(test)]
    tests::checkpoint("admission-before-written")?;
    if let Some(bytes) = artifacts.binding_bytes {
        staged.write_exact_new("d27-source-binding.v1.json", bytes, MAX_ARTIFACT_BYTES)?;
        #[cfg(test)]
        tests::checkpoint("admission-binding-written")?;
    }
    staged.write_exact_new(
        "g00.v1.json",
        artifacts.generation_bytes,
        MAX_INTENT_GENERATION_BYTES,
    )?;
    #[cfg(test)]
    tests::checkpoint("admission-generation-written")?;
    let readback = inspect_intent_directory(&staged, false)?;
    if readback.generations[0] != *artifacts.envelope {
        return Err(ApplyIntentError::Invalid("staging admission readback"));
    }
    staged.file.sync_all()?;
    staging.file.sync_all()?;
    #[cfg(test)]
    tests::checkpoint("admission-staging-synced")?;
    let identity = staged.identity;
    rename_directory_entry(staging, name, active, &key)?;
    #[cfg(test)]
    tests::checkpoint("admission-renamed")?;
    let published = active.child(&key, false)?;
    if published.identity != identity {
        return Err(ApplyIntentError::Invalid("active publication identity"));
    }
    inspect_intent_directory(&published, false)?;
    #[cfg(test)]
    tests::checkpoint("admission-readback")?;
    Ok(())
}
fn inspect_intent_directory(
    directory: &IntentDirectory,
    require_seal: bool,
) -> Result<InspectedApplyIntentV1> {
    let names = directory.names(Some(18))?;
    let request =
        RequestV1::from_canonical_bytes(&directory.read("request.v1.json", MAX_ARTIFACT_BYTES)?)?;
    let source_before = ObservedSourceV1::from_canonical_bytes(
        &directory.read("source-before.v1.json", MAX_ARTIFACT_BYTES)?,
    )?;
    let mut generations = Vec::new();
    let mut allowed = vec![
        "request.v1.json".to_owned(),
        "source-before.v1.json".to_owned(),
    ];
    for index in 0..MAX_INTENT_GENERATIONS {
        let name = format!("g{index:02}.v1.json");
        if !names.contains(&name) {
            break;
        }
        let envelope = ApplyIntentEnvelopeV1::from_canonical_bytes(
            &directory.read(&name, MAX_INTENT_GENERATION_BYTES)?,
        )?;
        if envelope.generation.sequence != index {
            return Err(ApplyIntentError::Invalid("generation filename"));
        }
        if let Some(previous) = generations.last() {
            let previous: &ApplyIntentEnvelopeV1 = previous;
            envelope.generation.verify_successor(&previous.generation)?;
        }
        generations.push(envelope);
        allowed.push(name);
    }
    let descriptor = generations
        .first()
        .and_then(|g| g.descriptor.as_ref())
        .ok_or(ApplyIntentError::Invalid("missing generation zero"))?;
    descriptor.verify_request(&request, &source_before)?;
    let d27_source_binding = match descriptor.inventory_id {
        ApplyInventoryV1::C12 => None,
        ApplyInventoryV1::D27 => {
            allowed.push("d27-source-binding.v1.json".into());
            let binding = D27SourceBindingV1::from_canonical_bytes(
                &directory.read("d27-source-binding.v1.json", MAX_ARTIFACT_BYTES)?,
            )?;
            binding.verify_artifacts(descriptor, &request, &source_before)?;
            Some(binding)
        }
    };
    let next = generations.len();
    let temporary = format!(".g{next:02}.v1.json.tmp");
    let pending_generation = if next < MAX_INTENT_GENERATIONS as usize && names.contains(&temporary)
    {
        allowed.push(temporary.clone());
        Some(directory.read(&temporary, MAX_INTENT_GENERATION_BYTES)?)
    } else {
        None
    };
    let seal = if names.contains(&"archive-seal.v1.json".to_owned()) {
        allowed.push("archive-seal.v1.json".into());
        Some(ArchiveSealV1::from_canonical_bytes(&directory.read(
            "archive-seal.v1.json",
            MAX_INTENT_GENERATION_BYTES,
        )?)?)
    } else {
        None
    };
    let seal_temporary = ".archive-seal.v1.json.tmp";
    if names.contains(&seal_temporary.to_owned()) {
        if seal.is_some()
            || !generations
                .last()
                .expect("nonempty chain")
                .generation
                .phase
                .is_terminal()
        {
            return Err(ApplyIntentError::Invalid("unexpected seal temporary"));
        }
        // Exact joined seal bytes are available to archive_terminal. Inspection
        // retains this bounded prefix without repairing it.
        directory.read(seal_temporary, MAX_INTENT_GENERATION_BYTES)?;
        allowed.push(seal_temporary.into());
    }
    allowed.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    if names != allowed
        || (require_seal
            && (seal.is_none()
                || pending_generation.is_some()
                || names.contains(&seal_temporary.to_owned())))
    {
        return Err(ApplyIntentError::Invalid(
            "intent entry allowlist or incomplete archive",
        ));
    }
    let inspected = InspectedApplyIntentV1 {
        request,
        source_before,
        d27_source_binding,
        generations,
        pending_generation,
        seal,
    };
    if let Some(seal) = &inspected.seal {
        inspected.verify_seal_chain(seal)?;
    }
    directory.verify()?;
    Ok(inspected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};

    thread_local! {
        static FAILURE: std::cell::Cell<Option<(&'static str, i32, bool)>> = const {
            std::cell::Cell::new(None)
        };
    }
    pub(super) fn checkpoint(point: &'static str) -> Result<()> {
        FAILURE.with(|failure| match failure.get() {
            Some((wanted, errno, _)) if wanted == point => {
                failure.set(Some((wanted, errno, true)));
                Err(std::io::Error::from_raw_os_error(errno).into())
            }
            _ => Ok(()),
        })
    }
    struct InjectFailure;
    impl InjectFailure {
        fn at(point: &'static str, errno: i32) -> Self {
            FAILURE.with(|failure| assert!(failure.replace(Some((point, errno, false))).is_none()));
            Self
        }
        fn assert_reached(&self) {
            FAILURE.with(|failure| assert!(failure.get().unwrap().2));
        }
    }
    impl Drop for InjectFailure {
        fn drop(&mut self) {
            FAILURE.with(|failure| failure.set(None));
        }
    }

    fn hash(hex: char) -> Hash {
        Hash::from_hex(&hex.to_string().repeat(64)).unwrap()
    }
    fn request() -> RequestV1 {
        RequestV1 {
            schema_version: 1,
            source_identity: IdentityV1 {
                device: 7,
                inode: 11,
            },
            entries: vec![RequestEntryV1 {
                path: "a.txt".into(),
                before: Some(BeforeV1 {
                    device: 7,
                    inode: 19,
                    hash: hash('1'),
                    mode: 0o644,
                    observed_mode: 0o644,
                    bytes: 3,
                }),
                after: Some(AfterV1 {
                    hash: hash('2'),
                    mode: 0o644,
                    bytes: 3,
                }),
            }],
        }
    }
    fn observed(request: &RequestV1) -> ObservedSourceV1 {
        ObservedSourceV1 {
            schema_version: 1,
            source_identity: request.source_identity,
            entries: request
                .entries
                .iter()
                .map(|e| ObservedEntryV1 {
                    path: e.path.clone(),
                    current: e.before.clone(),
                })
                .collect(),
        }
    }
    fn abandoned(reason: Option<ManagementDenialReasonV1>) -> ApplyEvidenceV1 {
        ApplyEvidenceV1::AbandonedBeforeEffect {
            source_readback_hash: hash('1'),
            child_disposition: if reason.is_some() {
                ChildDispositionV1::RetainedHeaderOnly
            } else {
                ChildDispositionV1::Absent
            },
            generic_denial_reason: reason,
            generic_generation: reason.map(|_| u64::MAX),
            generic_event_hash: reason.map(|_| hash('2')),
            generic_sequence: reason.map(|_| u64::MAX),
        }
    }
    #[test]
    fn operation_and_recovery_preimages_match_independent_fixed_vectors() {
        let project =
            Hash::from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
                .unwrap();
        let id = "000102030405060708090a0b0c0d0e0f".parse().unwrap();
        let c12 = c12_operation_key(project, id).unwrap();
        assert_eq!(
            c12.as_hex(),
            "810b6eefb97d6e49c2ec893e9bbbcb59290bf1760ce4c20f47c6db174d66d477"
        );
        assert_eq!(
            recovery_digest(c12).unwrap().as_hex(),
            "268585da7b1601cd95ae82da2775fa0fad51804996bf43f5b42b743a5af698a6"
        );
        assert_eq!(
            recovery_child(c12).unwrap(),
            "polaris-apply-v1-268585da7b1601cd95ae82da2775fa0f"
        );
        let binding = d27_operation_binding("desktop-op-1").unwrap();
        assert_eq!(
            binding.as_hex(),
            "7dfd407a8971f05a6d10b9eba10a5e1bc2944ea6602c4d49e8030df3c8dc36dd"
        );
        assert_eq!(
            d27_management_operation_id(binding).unwrap().as_hex(),
            "7dfd407a8971f05a6d10b9eba10a5e1b"
        );
        let d27 = d27_operation_key(project, binding).unwrap();
        assert_eq!(
            d27.as_hex(),
            "7a94f8baf4238025f2d683213f188fcffd1f736f11b7a05054d4be144a25e2ae"
        );
        assert_eq!(
            recovery_child(d27).unwrap(),
            "polaris-apply-v1-057fff678a51e9c427a74c61d99c2c0a"
        );
        assert_eq!(
            d27_result_id_digest("result-1").unwrap().as_hex(),
            "606676eee2f31f45eb23fab9d4004f7cf78d83dcface3954f4edb6d957e2ec48"
        );
        assert!(c12_operation_key(project, "0".repeat(32).parse().unwrap()).is_err());
    }
    #[test]
    fn abandoned_before_effect_all_null_vector_and_hash() {
        let expected = include_bytes!("../tests/fixtures/apply-intent-v1-abandoned-all-null.json");
        let bytes = abandoned(None).canonical_bytes().unwrap();
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(bytes.len(), 258);
        assert_eq!(
            Hash::digest(&bytes).as_hex(),
            "79fe25e19ba38e0152bd44c32cd3cc6bf32d380c0da3a1e66f375d80ab30909d"
        );
        assert_eq!(
            ApplyEvidenceV1::from_canonical_bytes(expected).unwrap(),
            abandoned(None)
        );
    }
    #[test]
    fn abandoned_before_effect_all_non_null_vector_and_hash() {
        let expected =
            include_bytes!("../tests/fixtures/apply-intent-v1-abandoned-all-non-null.json");
        let evidence = abandoned(Some(ManagementDenialReasonV1::ResourceUnavailable));
        let bytes = evidence.canonical_bytes().unwrap();
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(bytes.len(), 384);
        assert_eq!(
            Hash::digest(&bytes).as_hex(),
            "2aa55f58c34f58f15fadf88a08ef0bb81e3e9173efa2ff5ca3c4262742ab5fd2"
        );
        assert_eq!(
            ApplyEvidenceV1::from_canonical_bytes(expected).unwrap(),
            evidence
        );
    }
    #[test]
    fn generic_denial_reason_wire_spellings_are_exhaustive() {
        use ManagementDenialReasonV1::*;
        let expected = |reason| match reason {
            UserDeclined => "user_declined",
            PolicyDenied => "policy_denied",
            PermissionDenied => "permission_denied",
            StaleRequest => "stale_request",
            ResourceUnavailable => "resource_unavailable",
            InvalidState => "invalid_state",
        };
        for reason in [
            UserDeclined,
            PolicyDenied,
            PermissionDenied,
            StaleRequest,
            ResourceUnavailable,
            InvalidState,
        ] {
            assert_eq!(
                serde_json::to_string(&reason).unwrap(),
                format!("\"{}\"", expected(reason))
            );
        }
    }
    #[test]
    fn every_partial_denial_tuple_is_rejected() {
        for mask in 1_u8..15 {
            let value = ApplyEvidenceV1::AbandonedBeforeEffect {
                source_readback_hash: hash('1'),
                child_disposition: ChildDispositionV1::Absent,
                generic_denial_reason: (mask & 1 != 0)
                    .then_some(ManagementDenialReasonV1::UserDeclined),
                generic_generation: (mask & 2 != 0).then_some(0),
                generic_event_hash: (mask & 4 != 0).then_some(hash('2')),
                generic_sequence: (mask & 8 != 0).then_some(1),
            };
            assert!(value.canonical_bytes().is_err(), "mask {mask}");
        }
    }
    #[test]
    fn canonical_reader_rejects_missing_duplicate_unknown_reordered_and_nonminimal_fields() {
        let bytes = request().canonical_bytes().unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for changed in [
            format!(" {text}"),
            format!("{text}\n"),
            text.replacen("\"schema_version\":1,", "", 1),
            text.replacen(
                "\"schema_version\":1,",
                "\"schema_version\":1,\"schema_version\":1,",
                1,
            ),
            text.replacen(
                "\"schema_version\":1,",
                "\"unknown\":0,\"schema_version\":1,",
                1,
            ),
            text.replacen("\"schema_version\":1", "\"schema_version\":1.0", 1),
            text.replacen("\"schema_version\":1", "\"schema_version\":1e0", 1),
            text.replacen("\"schema_version\":1", "\"schema_version\":-1", 1),
            text.replacen("\"a.txt\"", "\"a\\u002etxt\"", 1),
            text.replacen("\"device\":7,\"inode\":11", "\"inode\":11,\"device\":7", 1),
        ] {
            assert!(
                RequestV1::from_canonical_bytes(changed.as_bytes()).is_err(),
                "{changed}"
            );
        }
        assert_eq!(RequestV1::from_canonical_bytes(&bytes).unwrap(), request());
    }
    #[test]
    fn request_paths_are_ordered_lossless_relative_and_unique() {
        for path in [
            "",
            "/absolute",
            "a//b",
            "a/",
            ".",
            "..",
            "a/./b",
            "a/../b",
            "a\\b",
            "a\0b",
            "a\nb",
            "a\u{7f}b",
        ] {
            let mut value = request();
            value.entries[0].path = path.into();
            assert!(value.canonical_bytes().is_err(), "{path:?}");
        }
        let mut value = request();
        value.entries[0].path = "界".repeat(1365);
        assert!(value.validate().is_ok());
        value.entries[0].path.push('界');
        assert!(value.validate().is_err());
        let mut value = request();
        value.entries.push(value.entries[0].clone());
        assert!(value.validate().is_err());
        value.entries[0].path = "z".into();
        value.entries[1].path = "a".into();
        assert!(value.validate().is_err());
    }
    #[test]
    fn permission_bounds_preserve_special_bit_conflict_evidence() {
        let mut value = request();
        value.entries[0].before.as_mut().unwrap().mode = 0o777;
        value.entries[0].before.as_mut().unwrap().observed_mode = 0o7777;
        value.entries[0].after.as_mut().unwrap().mode = 0o777;
        assert!(value.validate().is_ok());
        let mut bad = value.clone();
        bad.entries[0].before.as_mut().unwrap().mode = 0o1000;
        assert!(bad.validate().is_err());
        let mut bad = value.clone();
        bad.entries[0].before.as_mut().unwrap().observed_mode = 0o10000;
        assert!(bad.validate().is_err());
        let mut bad = value.clone();
        bad.entries[0].after.as_mut().unwrap().mode = 0o1000;
        assert!(bad.validate().is_err());
        value.entries[0].after.as_mut().unwrap().hash =
            value.entries[0].before.as_ref().unwrap().hash;
        assert!(
            value.validate().is_ok(),
            "clearing special bits is a change"
        );
        value.entries[0].before.as_mut().unwrap().observed_mode = 0o777;
        assert!(value.validate().is_err());
    }
    #[test]
    fn source_before_requires_every_identity_mode_length_and_path_field() {
        let request = request();
        let before = observed(&request);
        assert!(before.verify_before(&request).is_ok());
        for field in 0..8 {
            let mut changed = before.clone();
            match field {
                0 => changed.source_identity.inode += 1,
                1 => changed.entries[0].path = "b.txt".into(),
                2 => changed.entries[0].current.as_mut().unwrap().device += 1,
                3 => changed.entries[0].current.as_mut().unwrap().inode += 1,
                4 => changed.entries[0].current.as_mut().unwrap().mode = 0o600,
                5 => changed.entries[0].current.as_mut().unwrap().observed_mode = 0o4644,
                6 => changed.entries[0].current.as_mut().unwrap().bytes += 1,
                7 => changed.entries[0].current.as_mut().unwrap().hash = hash('3'),
                _ => unreachable!(),
            }
            assert!(changed.verify_before(&request).is_err(), "field {field}");
        }
    }
    #[test]
    fn final_source_requires_normal_modes_and_create_delete_presence() {
        let mut request = request();
        request.entries.push(RequestEntryV1 {
            path: "created".into(),
            before: None,
            after: Some(AfterV1 {
                hash: hash('3'),
                mode: 0o600,
                bytes: 4,
            }),
        });
        request.entries.push(RequestEntryV1 {
            path: "deleted".into(),
            before: Some(BeforeV1 {
                device: 7,
                inode: 30,
                hash: hash('4'),
                mode: 0o600,
                observed_mode: 0o600,
                bytes: 5,
            }),
            after: None,
        });
        let mut after = ObservedSourceV1 {
            schema_version: 1,
            source_identity: request.source_identity,
            entries: request
                .entries
                .iter()
                .map(|e| ObservedEntryV1 {
                    path: e.path.clone(),
                    current: e.after.as_ref().map(|a| BeforeV1 {
                        device: 7,
                        inode: 40,
                        hash: a.hash,
                        mode: a.mode,
                        observed_mode: a.mode,
                        bytes: a.bytes,
                    }),
                })
                .collect(),
        };
        assert!(after.verify_after(&request).is_ok());
        after.entries[0].current.as_mut().unwrap().observed_mode |= 0o4000;
        assert!(after.verify_after(&request).is_err());
        after.entries[0].current.as_mut().unwrap().observed_mode = 0o644;
        after.entries[1].current = None;
        assert!(after.verify_after(&request).is_err());
    }
    #[test]
    fn unsupported_root_encoding_is_typed_and_never_opens_or_creates_a_store() {
        let root = tempfile::tempdir().unwrap();
        for (kind, name) in [
            (RootKindV1::Project, "project"),
            (RootKindV1::Source, "source"),
            (RootKindV1::State, "state"),
            (RootKindV1::RecoveryParent, "recovery_parent"),
            (RootKindV1::DesktopRoot, "desktop_root"),
        ] {
            let path = root.path().join(std::ffi::OsString::from_vec(vec![0xff]));
            let error = require_root_encoding(&path, kind).unwrap_err();
            assert_eq!(
                error,
                ApplyAdmissionErrorV1::UnsupportedRootEncoding { root: kind }
            );
            assert_eq!(error.canonical_bytes(),format!("{{\"schema_version\":1,\"code\":\"root_encoding_unsupported\",\"root\":\"{name}\"}}").as_bytes());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }
    #[test]
    fn synthetic_capacity_formula_is_only_an_encoder_conformance_vector() {
        let bytes =
            include_bytes!("../tests/fixtures/apply-intent-v1-capacity-formula-encoder-only.json");
        let typed: CapacityFormulaParametersV1 = serde_json::from_slice(bytes).unwrap();
        assert_eq!(serde_json::to_vec(&typed).unwrap().as_slice(), bytes);
        assert_eq!(bytes.len(), 1715);
        assert_eq!(
            digest(b"polaris-capacity-formula-v1\0", bytes).as_hex(),
            "43eaf2f1a41027aa462524549ff738c284dcf412d46f151536529fc57082b71d"
        );
        assert_eq!(
            typed
                .root_components
                .state
                .management_generation_seal_required_bytes,
            68_231_168
        );
        assert_eq!(typed.device_groups[0].required_bytes, 71_225_344);
        // The synthetic one/two-byte payloads are not canonical Request/source
        // artifacts and cannot be admitted as a production generation zero.
        assert!(RequestV1::from_canonical_bytes(b"1").is_err());
        assert!(ObservedSourceV1::from_canonical_bytes(b"{}").is_err());
    }

    fn positive_fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/apply-intent-v1-capacity-positive")
            .join(name)
    }

    #[test]
    fn positive_frozen_capacity_fixture_matches_builder_writer_and_restart_reader() {
        let request_raw = std::fs::read(positive_fixture_path("request.v1.json")).unwrap();
        let before_raw = std::fs::read(positive_fixture_path("source-before.v1.json")).unwrap();
        let zero_raw = std::fs::read(positive_fixture_path("generation-00.v1.json")).unwrap();
        let formula_raw =
            std::fs::read(positive_fixture_path("capacity-formula-parameters.v1.json")).unwrap();
        let plan_raw = std::fs::read(positive_fixture_path("capacity-plan.v1.json")).unwrap();
        let expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(positive_fixture_path("expected.v1.json")).unwrap(),
        )
        .unwrap();
        let request = RequestV1::from_canonical_bytes(&request_raw).unwrap();
        let before = ObservedSourceV1::from_canonical_bytes(&before_raw).unwrap();
        let frozen = ApplyIntentEnvelopeV1::from_canonical_bytes(&zero_raw).unwrap();
        let mut descriptor = frozen.descriptor.clone().unwrap();
        let frozen_plan = &descriptor.capacity_plan;
        let prepare: PrepareManagementEffectV1 = serde_json::from_slice(
            &std::fs::read(positive_fixture_path("prepare-management.v1.json")).unwrap(),
        )
        .unwrap();
        let sample = |identity| CapacityRootSampleV1 {
            identity,
            allocation_unit: 4096,
            available_bytes: 1024 * 1024 * 1024,
        };
        let plan = CapacityPlanV1::build(CapacityAdmissionV1 {
            roots: CapacityRootsV1 {
                state: sample(frozen_plan.state_identity),
                recovery: sample(frozen_plan.recovery_parent_identity),
                desktop: None,
            },
            request: &request,
            source_before: &before,
            d27_source_binding: None,
            prepare: &prepare,
            management_record_limit: 16384,
            management_records_remaining: 4,
            management_bytes_remaining: frozen_plan.management_event_journal_append_bytes,
            inventory_commit_bytes: expected["c12_inventory_components"]["required_bytes"]
                .as_u64()
                .unwrap(),
            journal_bytes: std::fs::metadata(positive_fixture_path("workspace-journal-v2.jsonl"))
                .unwrap()
                .len(),
            report_bytes: std::fs::metadata(positive_fixture_path("apply-report-v1.json"))
                .unwrap()
                .len(),
            archive_seal_bytes: std::fs::metadata(positive_fixture_path("archive-seal.v1.json"))
                .unwrap()
                .len(),
            desktop_result_required_bytes: None,
            desktop_records_remaining: None,
            desktop_bytes_remaining: None,
            archive_count: 0,
            archive_file_bytes: 0,
        })
        .unwrap();
        assert_eq!(plan.canonical_bytes().unwrap(), plan_raw);
        assert_eq!(plan.formula_bytes().unwrap(), formula_raw);
        assert_eq!(
            plan.formula_hash.as_hex(),
            expected["formula_hash"].as_str().unwrap()
        );
        descriptor.capacity_plan_bytes = plan.canonical_bytes().unwrap().len() as u64;
        descriptor.capacity_plan_hash = plan.plan_hash().unwrap();
        descriptor.capacity_plan = plan;
        let descriptor_hash = descriptor.descriptor_hash().unwrap();
        let generation = ApplyGenerationV1 {
            schema_version: 1,
            sequence: 0,
            previous_record_hash: Hash::from_hex(&"0".repeat(64)).unwrap(),
            descriptor_hash,
            phase: ApplyPhaseV1::Prepared,
            evidence: ApplyEvidenceV1::Prepared {
                capacity_plan_hash: descriptor.capacity_plan_hash,
                request_hash: request.request_hash().unwrap(),
                request_bytes: request_raw.len() as u64,
                source_before_hash: before.observation_hash().unwrap(),
                source_before_bytes: before_raw.len() as u64,
            },
        };
        let produced = ApplyIntentEnvelopeV1 {
            schema_version: 1,
            descriptor: Some(descriptor),
            descriptor_hash,
            record_hash: generation.record_hash().unwrap(),
            generation,
        };
        assert_eq!(produced.canonical_bytes().unwrap(), zero_raw);
        let key = produced.descriptor.as_ref().unwrap().operation_key;
        let root = tempfile::tempdir().unwrap();
        {
            let root_dir = IntentDirectory::open(root.path()).unwrap();
            let staging = root_dir.child("staging", true).unwrap();
            let active = root_dir.child("active", true).unwrap();
            publish_admission_directory(
                &staging,
                &active,
                &format!(".{}.{}.tmp", key.as_hex(), "0".repeat(24)),
                AdmissionArtifacts {
                    envelope: &produced,
                    request_bytes: &request_raw,
                    before_bytes: &before_raw,
                    binding_bytes: None,
                    generation_bytes: &zero_raw,
                },
            )
            .unwrap();
            assert_eq!(staging.names(Some(1)).unwrap().len(), 0);
        }
        // Reopen without any writer directory handles. Stable fixture IDs are
        // wire inputs to the private writer; the public store's actual state/
        // lease binding is covered by the full-store tests, not bypassed here.
        let reopened =
            IntentDirectory::open(&root.path().join("active").join(key.as_hex())).unwrap();
        let saved = inspect_intent_directory(&reopened, false).unwrap();
        assert_eq!(saved.generations, vec![frozen]);
        assert_eq!(saved.request, request);
        assert_eq!(saved.source_before, before);
        assert_eq!(
            saved.descriptor().capacity_plan.canonical_bytes().unwrap(),
            plan_raw
        );
        assert_eq!(
            saved.descriptor().capacity_plan.formula_bytes().unwrap(),
            formula_raw
        );
    }

    #[test]
    fn positive_frozen_capacity_oracle_uses_raw_artifacts_and_independent_arithmetic() {
        use sha2::{Digest, Sha256};
        let expected_raw = std::fs::read(positive_fixture_path("expected.v1.json")).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&expected_raw)),
            "c767b7247e2d1260038e618f37f7b4c66ad2ba68fdbbd8aa8a813aae956d56fe"
        );
        let expected: serde_json::Value = serde_json::from_slice(&expected_raw).unwrap();
        for artifact in expected["artifacts"].as_array().unwrap() {
            let raw = std::fs::read(positive_fixture_path(
                artifact["relative_name"].as_str().unwrap(),
            ))
            .unwrap();
            assert_eq!(raw.len() as u64, artifact["byte_length"].as_u64().unwrap());
            assert_eq!(
                format!("{:x}", Sha256::digest(&raw)),
                artifact["sha256"].as_str().unwrap()
            );
        }
        let raw =
            std::fs::read(positive_fixture_path("capacity-formula-parameters.v1.json")).unwrap();
        struct OrderedKeys;
        impl<'de> serde::de::Visitor<'de> for OrderedKeys {
            type Value = Vec<String>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an ordered formula object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut keys = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    keys.push(key);
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
                Ok(keys)
            }
        }
        let keys = serde::de::Deserializer::deserialize_map(
            &mut serde_json::Deserializer::from_slice(&raw),
            OrderedKeys,
        )
        .unwrap();
        assert_eq!(
            keys,
            [
                "schema_version",
                "formula_version",
                "inventory_id",
                "constants",
                "payload_lengths",
                "root_components",
                "logical_admission",
                "device_groups"
            ]
        );
        let parameters: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let mut hash = Sha256::new();
        hash.update(b"polaris-capacity-formula-v1\0");
        hash.update(&raw);
        assert_eq!(
            format!("{:x}", hash.finalize()),
            expected["formula_hash"].as_str().unwrap()
        );
        assert_eq!(
            raw.len() as u64,
            expected["formula_object_length"].as_u64().unwrap()
        );
        assert_eq!(
            raw.len() as u64 + 28,
            expected["formula_preimage_length"].as_u64().unwrap()
        );
        let n = |value: &serde_json::Value| value.as_u64().unwrap();
        let round = |bytes: u64| {
            bytes
                .checked_add(4095)
                .unwrap()
                .checked_div(4096)
                .unwrap()
                .checked_mul(4096)
                .unwrap()
        };
        let add = |values: &[u64]| {
            values
                .iter()
                .try_fold(0_u64, |sum, value| sum.checked_add(*value))
                .unwrap()
        };
        let request = std::fs::read(positive_fixture_path("request.v1.json")).unwrap();
        let before = std::fs::read(positive_fixture_path("source-before.v1.json")).unwrap();
        let seal = std::fs::read(positive_fixture_path("archive-seal.v1.json")).unwrap();
        let journal = std::fs::read(positive_fixture_path("workspace-journal-v2.jsonl")).unwrap();
        let report = std::fs::read(positive_fixture_path("apply-report-v1.json")).unwrap();
        let management: Vec<_> = (1..=4)
            .map(|i| {
                std::fs::read(positive_fixture_path(&format!(
                    "management-{i:02}.v1.jsonl"
                )))
                .unwrap()
                .len() as u64
            })
            .collect();
        let mgmt_append = add(&management);
        let mgmt_seal = add(&[
            round(64 * 1024 * 1024),
            round(65536),
            round(4096),
            round(4096),
            1024 * 1024,
        ]);
        let final_bytes = add(&[
            round(request.len() as u64),
            round(before.len() as u64),
            round(65536).checked_mul(12).unwrap(),
        ]);
        let temporary = add(&[round(65536), round(seal.len() as u64)]);
        let pending =
            std::fs::read(positive_fixture_path("participation-pending.v1.json")).unwrap();
        let target_body =
            std::fs::read(positive_fixture_path("participation-target.v1.json")).unwrap();
        let marker =
            std::fs::read(positive_fixture_path("participation-target-marker.v1.json")).unwrap();
        let pending_final = add(&[
            round(pending.len() as u64),
            round(target_body.len() as u64),
            1024 * 1024,
        ]);
        let body_temporary = round(target_body.len() as u64);
        let generation_final = round(marker.len() as u64);
        let generation_temporary = round(marker.len() as u64);
        let inventory = add(&[
            pending_final,
            body_temporary,
            generation_final,
            generation_temporary,
            1024 * 1024,
        ]);
        for (field, actual) in [
            ("pending_final_bytes", pending_final),
            ("body_publication_temporary_bytes", body_temporary),
            ("generation_final_bytes", generation_final),
            ("generation_temporary_bytes", generation_temporary),
            ("baseline_staging_bytes", 0),
            ("directory_overhead_bytes", 1024 * 1024),
            ("required_bytes", inventory),
        ] {
            assert_eq!(
                actual,
                n(&expected["c12_inventory_components"][field]),
                "{field}"
            );
        }
        let state = add(&[
            final_bytes,
            temporary,
            round(mgmt_append),
            mgmt_seal,
            inventory,
            round(seal.len() as u64),
            1024 * 1024,
        ]);
        let recovery = add(&[
            round(journal.len() as u64),
            round(report.len() as u64),
            1024 * 1024,
        ]);
        assert_eq!(
            final_bytes,
            n(&parameters["root_components"]["state"]["intent_final_bytes"])
        );
        assert_eq!(
            state,
            n(&expected["root_components"]["state"]["required_bytes"])
        );
        assert_eq!(
            recovery,
            n(&expected["root_components"]["recovery"]["required_bytes"])
        );
        assert_eq!(
            add(&[state, recovery]),
            n(&expected["device_groups"][0]["required_bytes"])
        );
        assert_eq!(
            mgmt_append,
            n(&expected["logical_admission"]["management_event_journal_append_bytes"])
        );
        assert_eq!(
            *management.iter().max().unwrap(),
            n(&expected["logical_admission"]["management_event_record_max_bytes"])
        );
        assert!(
            mgmt_seal
                > n(&expected["logical_admission"]["management_record_limit_bytes_at_admission"])
        );
        assert!(parameters["payload_lengths"]["d27_source_binding_bytes"].is_null());
        assert!(parameters["root_components"]["desktop"].is_null());
        assert_eq!(
            parameters["device_groups"][0]["roots"],
            serde_json::json!(["state", "recovery"])
        );
    }
    #[test]
    fn allocation_and_seal_reserve_use_checked_integer_arithmetic() {
        assert_eq!(allocated_bytes(0, 4096).unwrap(), 0);
        assert_eq!(allocated_bytes(4095, 4096).unwrap(), 4096);
        assert_eq!(allocated_bytes(4096, 4096).unwrap(), 4096);
        assert_eq!(allocated_bytes(4097, 4096).unwrap(), 8192);
        assert!(allocated_bytes(1, 0).is_err());
        assert!(allocated_bytes(u64::MAX, 4096).is_err());
        assert!(sum([u64::MAX, 1]).is_err());
        assert_eq!(management_seal_reservation(4096).unwrap(), 68_231_168);
    }

    fn capacity_prepare() -> PrepareManagementEffectV1 {
        use crate::management_effect::{
            ManagementEffectScopeV1, ManagementInventoryIdV1, ManagementPermissionScopeV1,
            ManagementRewindDispositionV1,
        };
        PrepareManagementEffectV1 {
            operation_id: "1".repeat(32).parse().unwrap(),
            inventory: ManagementInventoryIdV1::parse("C12").unwrap(),
            scope: ManagementEffectScopeV1 {
                project: hash('3'),
                session: None,
                generation: u64::MAX,
                revision: u64::MAX,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: hash('4'),
            rewind: ManagementRewindDispositionV1::CapturedRestorable,
        }
    }

    fn capacity_plan(recovery_device: u64) -> CapacityPlanV1 {
        let request = request();
        let before = observed(&request);
        let prepare = capacity_prepare();
        CapacityPlanV1::build(CapacityAdmissionV1 {
            roots: CapacityRootsV1 {
                state: CapacityRootSampleV1 {
                    identity: IdentityV1 {
                        device: 7,
                        inode: 23,
                    },
                    allocation_unit: 4096,
                    available_bytes: 1024 * 1024 * 1024,
                },
                recovery: CapacityRootSampleV1 {
                    identity: IdentityV1 {
                        device: recovery_device,
                        inode: 29,
                    },
                    allocation_unit: 4096,
                    available_bytes: 1024 * 1024 * 1024,
                },
                desktop: None,
            },
            request: &request,
            source_before: &before,
            d27_source_binding: None,
            prepare: &prepare,
            management_record_limit: 16 * 1024,
            management_records_remaining: 100,
            management_bytes_remaining: 16 * 1024 * 1024,
            inventory_commit_bytes: 3 * 4096,
            journal_bytes: 4097,
            report_bytes: 8193,
            archive_seal_bytes: 4096,
            desktop_result_required_bytes: None,
            desktop_records_remaining: None,
            desktop_bytes_remaining: None,
            archive_count: 0,
            archive_file_bytes: 0,
        })
        .unwrap()
    }

    #[test]
    fn capacity_builder_uses_artifact_bytes_and_fixed_generation_reservations() {
        let plan = capacity_plan(7);
        assert_eq!(
            plan.request_bytes,
            request().canonical_bytes().unwrap().len() as u64
        );
        assert_eq!(
            plan.source_before_bytes,
            observed(&request()).canonical_bytes().unwrap().len() as u64
        );
        assert_eq!(plan.intent_final_bytes, 2 * 4096 + 12 * 65536);
        assert_eq!(plan.intent_temporary_bytes, 65536 + 4096);
        assert_eq!(
            plan.recovery_required_bytes,
            8192 + 12288 + DIRECTORY_OVERHEAD_BYTES
        );
        assert_eq!(plan.management_event_records_required, 4);
        assert_eq!(plan.management_generation_seal_required_bytes, 68_231_168);
        assert!(plan.generic_effect_bytes > plan.management_record_limit_bytes_at_admission);
        let bytes = plan.canonical_bytes().unwrap();
        assert_eq!(CapacityPlanV1::from_canonical_bytes(&bytes).unwrap(), plan);
        assert_eq!(capacity_plan(7).canonical_bytes().unwrap(), bytes);
    }

    #[test]
    fn capacity_physical_floor_is_once_per_device_and_boundaries_are_exact() {
        for same_device in [true, false] {
            let mut plan = capacity_plan(if same_device { 7 } else { 8 });
            let groups = plan.formula_parameters().unwrap().device_groups;
            assert_eq!(groups.len(), if same_device { 1 } else { 2 });
            for group in &groups {
                let required = group.required_bytes + FREE_SPACE_FLOOR_BYTES;
                if group.device == plan.state_device {
                    plan.state_available_bytes_at_admission = required;
                }
                if group.device == plan.recovery_device {
                    plan.recovery_available_bytes_at_admission = required;
                }
            }
            assert!(plan.validate().is_ok());
            for group in &groups {
                let mut below = plan.clone();
                if group.device == below.state_device {
                    below.state_available_bytes_at_admission -= 1;
                }
                if group.device == below.recovery_device {
                    below.recovery_available_bytes_at_admission -= 1;
                }
                assert!(matches!(
                    below.validate(),
                    Err(ApplyIntentError::Capacity("filesystem"))
                ));
                let mut above = plan.clone();
                if group.device == above.state_device {
                    above.state_available_bytes_at_admission += 1;
                }
                if group.device == above.recovery_device {
                    above.recovery_available_bytes_at_admission += 1;
                }
                assert!(above.validate().is_ok());
            }
        }
    }

    #[test]
    fn capacity_logical_record_count_and_journal_boundaries_are_independent() {
        let mut exact = capacity_plan(7);
        exact.management_record_limit_bytes_at_admission = exact.management_event_record_max_bytes;
        exact.management_records_remaining_at_admission = exact.management_event_records_required;
        exact.management_bytes_remaining_at_admission = exact.management_event_journal_append_bytes;
        exact.formula_hash = exact.recomputed_formula_hash().unwrap();
        assert!(exact.validate().is_ok());
        for (field, reason) in [
            (0, "management record"),
            (1, "management records"),
            (2, "management journal"),
        ] {
            let mut below = exact.clone();
            match field {
                0 => below.management_record_limit_bytes_at_admission -= 1,
                1 => below.management_records_remaining_at_admission -= 1,
                2 => below.management_bytes_remaining_at_admission -= 1,
                _ => unreachable!(),
            }
            below.formula_hash = below.recomputed_formula_hash().unwrap();
            assert!(
                matches!(below.validate(), Err(ApplyIntentError::Capacity(found)) if found == reason)
            );
        }
    }

    #[test]
    fn desktop_logical_admission_reserves_full_mebibyte_with_no_new_record_slot() {
        let mut plan = capacity_plan(7);
        plan.desktop_result_required_bytes = Some(17);
        plan.desktop_bytes_remaining_at_admission = Some(1024 * 1024);
        plan.desktop_records_remaining_at_admission = Some(0);
        assert!(plan.check_logical_admission().is_ok());
        plan.desktop_bytes_remaining_at_admission = Some(1024 * 1024 - 1);
        assert!(matches!(
            plan.check_logical_admission(),
            Err(ApplyIntentError::Capacity("desktop result"))
        ));
        plan.desktop_bytes_remaining_at_admission = Some(1024 * 1024 + 1);
        plan.desktop_result_required_bytes = Some(1024 * 1024);
        assert!(plan.check_logical_admission().is_ok());
        plan.desktop_result_required_bytes = Some(1024 * 1024 + 1);
        assert!(matches!(
            plan.check_logical_admission(),
            Err(ApplyIntentError::Capacity("desktop result"))
        ));
    }

    #[test]
    fn capacity_same_device_units_and_snapshots_must_match() {
        let mut plan = capacity_plan(7);
        plan.recovery_available_bytes_at_admission -= 1;
        assert!(matches!(
            plan.validate(),
            Err(ApplyIntentError::Invalid("same-device available snapshot"))
        ));
        let mut plan = capacity_plan(7);
        plan.recovery_allocation_unit *= 2;
        assert!(matches!(
            plan.formula_parameters(),
            Err(ApplyIntentError::Invalid("same-device allocation unit"))
        ));
    }

    #[test]
    fn pinned_capacity_sampler_reads_once_per_device_and_refuses_nonprivate_roots() {
        let state = tempfile::tempdir().unwrap();
        let recovery = tempfile::tempdir().unwrap();
        let desktop = tempfile::tempdir().unwrap();
        let state_fd = File::open(state.path()).unwrap();
        let recovery_fd = File::open(recovery.path()).unwrap();
        let desktop_fd = File::open(desktop.path()).unwrap();
        assert_eq!(
            state_fd.metadata().unwrap().dev(),
            recovery_fd.metadata().unwrap().dev()
        );
        assert_eq!(
            state_fd.metadata().unwrap().dev(),
            desktop_fd.metadata().unwrap().dev()
        );
        let mut calls = 0;
        let roots =
            CapacityRootsV1::sample_with(&state_fd, &recovery_fd, Some(&desktop_fd), |file| {
                calls += 1;
                sample_statvfs(file)
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(roots.state.available_bytes, roots.recovery.available_bytes);
        assert_eq!(
            roots.state.available_bytes,
            roots.desktop.unwrap().available_bytes
        );
        assert_ne!(roots.state.identity.inode, roots.recovery.identity.inode);
        assert_eq!(
            CapacityRootsV1::from_pinned_files(&state_fd, &recovery_fd, None)
                .unwrap()
                .state
                .identity,
            roots.state.identity
        );
        std::fs::set_permissions(recovery.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut calls = 0;
        assert!(
            CapacityRootsV1::sample_with(&state_fd, &recovery_fd, None, |_| {
                calls += 1;
                Ok((4096, 100))
            })
            .is_err()
        );
        assert_eq!(calls, 0);
    }

    #[test]
    fn capacity_arithmetic_rejects_statvfs_and_device_sum_overflow() {
        assert_eq!(available_from_statvfs(4096, 2).unwrap(), 8192);
        assert!(matches!(
            available_from_statvfs(0, 1),
            Err(ApplyIntentError::Invalid("zero allocation unit"))
        ));
        assert!(matches!(
            available_from_statvfs(4096, u64::MAX),
            Err(ApplyIntentError::Overflow)
        ));
        let mut plan = capacity_plan(7);
        plan.state_required_bytes = u64::MAX;
        assert!(matches!(
            plan.formula_parameters(),
            Err(ApplyIntentError::Overflow)
        ));
    }

    #[test]
    fn live_capacity_diagnostics_change_without_rewriting_historical_plan() {
        for recovery_device in [7, 8] {
            let plan = capacity_plan(recovery_device);
            let original = plan.canonical_bytes().unwrap();
            let original_hash = plan.plan_hash().unwrap();
            let sample = |identity, available_bytes| CapacityRootSampleV1 {
                identity,
                allocation_unit: 4096,
                available_bytes,
            };
            let mut fresh = CapacityRootsV1 {
                state: sample(plan.state_identity, 0),
                recovery: sample(plan.recovery_parent_identity, 0),
                desktop: None,
            };
            let groups = plan.live_capacity_groups(&fresh, 8192, 4096, None).unwrap();
            assert_eq!(groups.len(), if recovery_device == 7 { 1 } else { 2 });
            for row in &groups {
                assert_eq!(
                    row.shortage_bytes,
                    row.required_remaining_bytes + FREE_SPACE_FLOOR_BYTES
                );
                let exact = row.required_remaining_bytes + FREE_SPACE_FLOOR_BYTES;
                if row.device == fresh.state.identity.device {
                    fresh.state.available_bytes = exact;
                }
                if row.device == fresh.recovery.identity.device {
                    fresh.recovery.available_bytes = exact;
                }
            }
            let exact = plan.live_capacity_groups(&fresh, 8192, 4096, None).unwrap();
            assert!(exact.iter().all(|r| r.shortage_bytes == 0));
            fresh.state.available_bytes -= 1;
            if recovery_device == 7 {
                fresh.recovery.available_bytes -= 1;
            }
            let short = plan.live_capacity_groups(&fresh, 8192, 4096, None).unwrap();
            assert_eq!(
                short.iter().find(|r| r.device == 7).unwrap().shortage_bytes,
                1
            );
            assert_eq!(plan.canonical_bytes().unwrap(), original);
            assert_eq!(plan.plan_hash().unwrap(), original_hash);
            assert_eq!(
                CapacityPlanV1::from_canonical_bytes(&original).unwrap(),
                plan
            );
            fresh.state.identity.inode += 1;
            assert!(matches!(
                plan.live_capacity_groups(&fresh, 8192, 4096, None),
                Err(ApplyIntentError::Invalid("live capacity static identity"))
            ));
        }
    }

    #[test]
    fn live_capacity_diagnostics_reject_overflow_and_inconsistent_pinned_samples() {
        let plan = capacity_plan(7);
        let mut fresh = CapacityRootsV1 {
            state: CapacityRootSampleV1 {
                identity: plan.state_identity,
                allocation_unit: 4096,
                available_bytes: 0,
            },
            recovery: CapacityRootSampleV1 {
                identity: plan.recovery_parent_identity,
                allocation_unit: 4096,
                available_bytes: 0,
            },
            desktop: None,
        };
        assert!(matches!(
            plan.live_capacity_groups(&fresh, u64::MAX, 1, None),
            Err(ApplyIntentError::Overflow)
        ));
        assert!(matches!(
            plan.live_capacity_groups(&fresh, u64::MAX, 0, None),
            Err(ApplyIntentError::Overflow)
        ));
        fresh.recovery.available_bytes = 1;
        assert!(matches!(
            plan.live_capacity_groups(&fresh, 1, 1, None),
            Err(ApplyIntentError::Invalid("live same-device sample"))
        ));
        fresh.recovery.available_bytes = 0;
        fresh.recovery.allocation_unit = 8192;
        assert!(matches!(
            plan.live_capacity_groups(&fresh, 1, 1, None),
            Err(ApplyIntentError::Invalid("live capacity static identity"))
        ));
        assert!(matches!(
            plan.live_capacity_groups(&fresh, 1, 1, Some(0)),
            Err(ApplyIntentError::Invalid("live capacity static identity"))
        ));
    }

    fn d27_binding(manifest: D27ManifestProjectionV1) -> D27SourceBindingV1 {
        let request = manifest.project_request().unwrap();
        let before = observed(&request);
        D27SourceBindingV1 {
            schema_version: 1,
            payload_bytes: 3,
            payload_hash: hash('5'),
            manifest_bytes: manifest.canonical_bytes().unwrap().len() as u64,
            manifest_hash: manifest.manifest_hash().unwrap(),
            request_bytes: request.canonical_bytes().unwrap().len() as u64,
            request_hash: request.request_hash().unwrap(),
            source_before_bytes: before.canonical_bytes().unwrap().len() as u64,
            source_before_hash: before.observation_hash().unwrap(),
            manifest,
        }
    }
    fn d27_manifest() -> D27ManifestProjectionV1 {
        D27ManifestProjectionV1 {
            schema_version: 1,
            source_path: "/fixture/source".into(),
            source_identity: request().source_identity,
            recovery_parent_path: "/fixture/recovery".into(),
            recovery_parent_identity: IdentityV1 {
                device: 7,
                inode: 29,
            },
            entries: vec![D27ManifestProjectionEntryV1 {
                path: "a.txt".into(),
                before: Some(D27ManifestVersionV1 {
                    device: 7,
                    inode: 19,
                    hash: hash('1'),
                    normal_mode: 0o644,
                    observed_mode: 0o644,
                    bytes: 3,
                }),
                after: Some(D27ManifestVersionV1 {
                    device: 7,
                    inode: 21,
                    hash: hash('2'),
                    normal_mode: 0o644,
                    observed_mode: 0o644,
                    bytes: 3,
                }),
            }],
        }
    }

    #[test]
    fn d27_binding_canonical_schema_and_projection_reject_partial_or_changed_metadata() {
        let binding = d27_binding(d27_manifest());
        let bytes = binding.canonical_bytes().unwrap();
        assert_eq!(
            D27SourceBindingV1::from_canonical_bytes(&bytes).unwrap(),
            binding
        );
        assert_eq!(binding.manifest.project_request().unwrap(), request());
        let text = String::from_utf8(bytes).unwrap();
        for changed in [
            text.replacen("\"payload_hash\":", "\"payload_hash\":null,\"ignored\":", 1),
            text.replacen(
                "\"schema_version\":1,",
                "\"schema_version\":1,\"schema_version\":1,",
                1,
            ),
            text.replacen("\"source_before_hash\":", "\"foreign\":", 1),
            format!("{text}\n"),
        ] {
            assert!(D27SourceBindingV1::from_canonical_bytes(changed.as_bytes()).is_err());
        }
        let mut changed = binding.clone();
        changed.manifest.entries[0]
            .before
            .as_mut()
            .unwrap()
            .observed_mode = 0o4644;
        assert!(changed.canonical_bytes().is_err());
        let mut changed = binding.clone();
        changed.request_bytes += 1;
        assert!(changed.canonical_bytes().is_err());
        let mut changed = binding.clone();
        changed.payload_bytes = 1024 * 1024 + 1;
        assert!(changed.canonical_bytes().is_err());
        for path in ["relative", "/a//b", "/a/../b", "/a\\b", "/a\0b"] {
            let mut changed = binding.clone();
            changed.manifest.source_path = path.into();
            assert!(changed.canonical_bytes().is_err(), "{path:?}");
        }
    }

    #[test]
    fn d27_capacity_placement_covers_shared_distinct_and_mixed_devices() {
        for (state_device, recovery_device, desktop_device) in
            [(7, 7, 7), (7, 8, 9), (7, 8, 7), (7, 7, 8)]
        {
            let mut manifest = d27_manifest();
            manifest.recovery_parent_identity.device = recovery_device;
            let binding = d27_binding(manifest);
            let request = binding.manifest.project_request().unwrap();
            let before = observed(&request);
            let mut prepare = capacity_prepare();
            prepare.inventory =
                crate::management_effect::ManagementInventoryIdV1::parse("D27").unwrap();
            let sample = |device, inode| CapacityRootSampleV1 {
                identity: IdentityV1 { device, inode },
                allocation_unit: 4096,
                available_bytes: 1024 * 1024 * 1024,
            };
            let plan = CapacityPlanV1::build(CapacityAdmissionV1 {
                roots: CapacityRootsV1 {
                    state: sample(state_device, 23),
                    recovery: sample(recovery_device, 29),
                    desktop: Some(sample(desktop_device, 41)),
                },
                request: &request,
                source_before: &before,
                d27_source_binding: Some(&binding),
                prepare: &prepare,
                management_record_limit: 16 * 1024,
                management_records_remaining: 100,
                management_bytes_remaining: 16 * 1024 * 1024,
                inventory_commit_bytes: 8192,
                journal_bytes: 4097,
                report_bytes: 8193,
                archive_seal_bytes: 4096,
                desktop_result_required_bytes: Some(256),
                desktop_records_remaining: Some(0),
                desktop_bytes_remaining: Some(1024 * 1024),
                archive_count: 0,
                archive_file_bytes: 0,
            })
            .unwrap();
            assert_eq!(plan.desktop_required_bytes, Some(8192));
            assert_eq!(plan.intent_final_bytes, 3 * 4096 + 12 * 65536);
            let formula = plan.formula_parameters().unwrap();
            assert_eq!(formula.root_components.state.inventory_commit_bytes, 0);
            assert_eq!(
                formula
                    .root_components
                    .desktop
                    .unwrap()
                    .inventory_commit_bytes,
                8192
            );
            let group_sum: u64 = formula.device_groups.iter().map(|g| g.required_bytes).sum();
            assert_eq!(
                group_sum,
                plan.state_required_bytes + plan.recovery_required_bytes + 8192
            );
            let mut exact = plan.clone();
            for group in &formula.device_groups {
                let available = group.required_bytes + FREE_SPACE_FLOOR_BYTES;
                if group.device == state_device {
                    exact.state_available_bytes_at_admission = available;
                }
                if group.device == recovery_device {
                    exact.recovery_available_bytes_at_admission = available;
                }
                if group.device == desktop_device {
                    exact.desktop_available_bytes_at_admission = Some(available);
                }
            }
            assert!(exact.validate().is_ok());
            for group in formula.device_groups {
                let mut below = exact.clone();
                if group.device == state_device {
                    below.state_available_bytes_at_admission -= 1;
                }
                if group.device == recovery_device {
                    below.recovery_available_bytes_at_admission -= 1;
                }
                if group.device == desktop_device {
                    *below.desktop_available_bytes_at_admission.as_mut().unwrap() -= 1;
                }
                assert!(matches!(
                    below.validate(),
                    Err(ApplyIntentError::Capacity("filesystem"))
                ));
            }
            assert_eq!(
                CapacityPlanV1::from_canonical_bytes(&plan.canonical_bytes().unwrap()).unwrap(),
                plan
            );
            let fresh = CapacityRootsV1 {
                state: sample(state_device, 23),
                recovery: sample(recovery_device, 29),
                desktop: Some(sample(desktop_device, 41)),
            };
            let live = plan
                .live_capacity_groups(&fresh, 8192, 4096, Some(2048))
                .unwrap();
            assert_eq!(
                live.len(),
                exact.formula_parameters().unwrap().device_groups.len()
            );
            assert_eq!(
                live.iter().map(|g| g.required_remaining_bytes).sum::<u64>(),
                8192 + 4096 + 2048
            );
            assert_eq!(
                live.iter()
                    .flat_map(|g| g.roots.iter())
                    .filter(|r| **r == CapacityRootKindV1::Desktop)
                    .count(),
                1
            );
            if state_device == recovery_device && recovery_device == desktop_device {
                assert_eq!(
                    live[0].roots,
                    vec![
                        CapacityRootKindV1::State,
                        CapacityRootKindV1::Recovery,
                        CapacityRootKindV1::Desktop
                    ]
                );
                assert_eq!(live[0].free_space_floor_bytes, FREE_SPACE_FLOOR_BYTES);
            }
        }
    }

    #[test]
    fn d27_binding_exact_artifact_size_limit_and_next_byte_are_distinct() {
        let mut manifest = d27_manifest();
        let prototype = manifest.entries.pop().unwrap();
        manifest.entries = (0..4096)
            .map(|i| {
                let mut entry = prototype.clone();
                entry.path = format!("{i:08}-{}", "a".repeat(3000));
                entry
            })
            .collect();
        let initial = d27_binding(manifest.clone())
            .canonical_bytes()
            .unwrap()
            .len();
        let extra = MAX_ARTIFACT_BYTES as usize - initial;
        let per_entry = extra / manifest.entries.len();
        let remainder = extra % manifest.entries.len();
        for (index, entry) in manifest.entries.iter_mut().enumerate() {
            entry
                .path
                .push_str(&"a".repeat(per_entry + usize::from(index < remainder)));
            assert!(entry.path.len() <= 4096);
        }
        let exact = d27_binding(manifest.clone());
        let bytes = exact.canonical_bytes().unwrap();
        assert_eq!(bytes.len() as u64, MAX_ARTIFACT_BYTES);
        assert_eq!(
            D27SourceBindingV1::from_canonical_bytes(&bytes).unwrap(),
            exact
        );
        manifest.entries.last_mut().unwrap().path.push('a');
        let above = d27_binding(manifest);
        assert!(
            matches!(above.canonical_bytes(), Err(ApplyIntentError::TooLarge { actual, maximum }) if actual == MAX_ARTIFACT_BYTES+1 && maximum == MAX_ARTIFACT_BYTES)
        );
        let mut above_raw = bytes;
        above_raw.push(b' ');
        assert!(
            matches!(D27SourceBindingV1::from_canonical_bytes(&above_raw), Err(ApplyIntentError::TooLarge { actual, maximum }) if actual == MAX_ARTIFACT_BYTES+1 && maximum == MAX_ARTIFACT_BYTES)
        );
    }

    struct StoreFixture {
        _root: tempfile::TempDir,
        state: PathBuf,
        recovery: PathBuf,
        lease: ProjectMutationLeaseV1,
    }
    impl StoreFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let state = root.path().join("state");
            let recovery = root.path().join("recovery");
            for path in [&state, &recovery] {
                std::fs::create_dir(path).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let owner = crate::project_mutation::ProjectMutationOwnerV1::at_store_root(
                &state.join("rewind-workspace"),
            )
            .unwrap();
            let lease = owner.try_mutation().unwrap();
            Self {
                _root: root,
                state,
                recovery,
                lease,
            }
        }
        fn admission(&self, id: char) -> (RequestV1, ObservedSourceV1, ApplyIntentEnvelopeV1) {
            let request = request();
            let before = observed(&request);
            let state = File::open(&self.state).unwrap();
            let recovery = File::open(&self.recovery).unwrap();
            let roots = CapacityRootsV1::from_pinned_files(&state, &recovery, None).unwrap();
            let mut plan = capacity_plan(roots.recovery.identity.device);
            plan.state_identity = roots.state.identity;
            plan.state_device = roots.state.identity.device;
            plan.recovery_parent_identity = roots.recovery.identity;
            plan.recovery_device = roots.recovery.identity.device;
            plan.state_allocation_unit = roots.state.allocation_unit;
            plan.recovery_allocation_unit = roots.recovery.allocation_unit;
            // This fixture's 4 KiB allocation arithmetic is checked against
            // the real pinned filesystem rather than silently substituting it.
            assert_eq!(roots.state.allocation_unit, 4096);
            assert_eq!(roots.recovery.allocation_unit, 4096);
            plan.formula_hash = plan.recomputed_formula_hash().unwrap();
            plan.validate().unwrap();
            let operation_id = id.to_string().repeat(32).parse().unwrap();
            let key = c12_operation_key(hash('3'), operation_id).unwrap();
            let revision = RevisionV1 {
                kind: RevisionKindV1::GitSha1,
                value: "a".repeat(40),
            };
            let descriptor = ApplyIntentDescriptorV1 {
                schema_version: 1,
                operation_key: key,
                management_operation_id: operation_id,
                inventory_id: ApplyInventoryV1::C12,
                project_binding: hash('3'),
                source_identity: request.source_identity,
                agreed_revision: revision.clone(),
                staged_revision: revision,
                previous_applied_revision: None,
                context: ApplyContextV1::Participation {
                    base_generation: 8,
                    target_generation: 9,
                    base_marker_hash: hash('4'),
                    mutation_id: key,
                    target_participation_hash: Some(hash('5')),
                    target_marker_hash: hash('6'),
                },
                share_source_manifest_digest: hash('7'),
                changed_path_count: request.entries.len() as u64,
                path_set_digest: request.path_set_digest().unwrap(),
                workspace_apply_request_digest: request.request_hash().unwrap(),
                source_before_observation_digest: before.observation_hash().unwrap(),
                d27_source_binding_bytes: None,
                d27_source_binding_hash: None,
                recovery_parent_identity: roots.recovery.identity,
                recovery_provenance_digest: hash('8'),
                recovery_child: recovery_child(key).unwrap(),
                capacity_plan_bytes: plan.canonical_bytes().unwrap().len() as u64,
                capacity_plan_hash: plan.plan_hash().unwrap(),
                capacity_plan: plan,
                phase: ApplyPhaseV1::Prepared,
            };
            let descriptor_hash = descriptor.descriptor_hash().unwrap();
            let generation = ApplyGenerationV1 {
                schema_version: 1,
                sequence: 0,
                previous_record_hash: Hash::from_hex(&"0".repeat(64)).unwrap(),
                descriptor_hash,
                phase: ApplyPhaseV1::Prepared,
                evidence: ApplyEvidenceV1::Prepared {
                    capacity_plan_hash: descriptor.capacity_plan_hash,
                    request_hash: descriptor.workspace_apply_request_digest,
                    request_bytes: descriptor.capacity_plan.request_bytes,
                    source_before_hash: descriptor.source_before_observation_digest,
                    source_before_bytes: descriptor.capacity_plan.source_before_bytes,
                },
            };
            let envelope = ApplyIntentEnvelopeV1 {
                schema_version: 1,
                descriptor: Some(descriptor),
                descriptor_hash,
                record_hash: generation.record_hash().unwrap(),
                generation,
            };
            envelope.validate().unwrap();
            (request, before, envelope)
        }
        fn active_path(&self, key: Hash) -> PathBuf {
            self.state
                .join("apply-intents-v1/active")
                .join(key.as_hex())
        }
    }
    fn abandoned_generation(zero: &ApplyIntentEnvelopeV1, denied: bool) -> ApplyGenerationV1 {
        let descriptor = zero.descriptor.as_ref().unwrap();
        ApplyGenerationV1 {
            schema_version: 1,
            sequence: 1,
            previous_record_hash: zero.record_hash,
            descriptor_hash: zero.descriptor_hash,
            phase: ApplyPhaseV1::AbandonedBeforeEffect,
            evidence: ApplyEvidenceV1::AbandonedBeforeEffect {
                source_readback_hash: descriptor.source_before_observation_digest,
                child_disposition: ChildDispositionV1::Absent,
                generic_denial_reason: denied
                    .then_some(ManagementDenialReasonV1::ResourceUnavailable),
                generic_generation: denied.then_some(3),
                generic_event_hash: denied.then_some(hash('9')),
                generic_sequence: denied.then_some(11),
            },
        }
    }
    fn abandonment_seal(
        zero: &ApplyIntentEnvelopeV1,
        terminal: &ApplyGenerationV1,
    ) -> ArchiveSealV1 {
        let descriptor = zero.descriptor.as_ref().unwrap();
        let generic = terminal.evidence.generic_evidence();
        ArchiveSealV1 {
            schema_version: 1,
            operation_key: descriptor.operation_key,
            descriptor_hash: zero.descriptor_hash,
            terminal_sequence: terminal.sequence,
            terminal_record_hash: terminal.record_hash().unwrap(),
            terminal_phase: terminal.phase,
            request_hash: descriptor.workspace_apply_request_digest,
            source_before_hash: descriptor.source_before_observation_digest,
            journal_header_hash: None,
            journal_tip_hash: None,
            report_hash: None,
            marker_kind: None,
            marker_hash: None,
            generic_terminal_generation: generic.map(|v| v.0),
            generic_terminal_hash: generic.map(|v| v.1),
            generic_terminal_sequence: generic.map(|v| v.2),
            final_readback_hash: None,
        }
    }

    #[test]
    fn store_inspection_is_read_only_and_generation_zero_reopens_exactly() {
        let fixture = StoreFixture::new();
        let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        assert!(store.active().unwrap().is_none());
        assert_eq!(store.archive_inventory().unwrap(), (0, 0));
        assert!(!fixture.state.join("apply-intents-v1").exists());
        let (request, before, zero) = fixture.admission('1');
        store
            .publish_generation_zero(zero.clone(), &request, &before, None)
            .unwrap();
        let key = zero.descriptor.as_ref().unwrap().operation_key;
        let active = fixture.active_path(key);
        assert_eq!(
            std::fs::read(active.join("request.v1.json")).unwrap(),
            request.canonical_bytes().unwrap()
        );
        assert_eq!(
            std::fs::read(active.join("source-before.v1.json")).unwrap(),
            before.canonical_bytes().unwrap()
        );
        assert_eq!(
            std::fs::read(active.join("g00.v1.json")).unwrap(),
            zero.canonical_bytes().unwrap()
        );
        drop(store);
        let reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        let found = reopened.active().unwrap().unwrap();
        assert_eq!(found.generations, vec![zero]);
        assert_eq!(found.request, request);
        assert_eq!(found.source_before, before);
        assert_eq!(
            std::fs::read_dir(fixture.state.join("apply-intents-v1/staging"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn store_rejects_wrong_state_capacity_and_changed_artifacts_before_creating_root() {
        let fixture = StoreFixture::new();
        let (request, before, zero) = fixture.admission('1');
        let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        let other = StoreFixture::new();
        let (_, _, foreign) = other.admission('1');
        assert!(matches!(
            store.publish_generation_zero(foreign, &request, &before, None),
            Err(ApplyIntentError::Invalid("state capacity identity"))
        ));
        let mut changed = request.clone();
        changed.entries[0].after.as_mut().unwrap().hash = hash('3');
        assert!(
            store
                .publish_generation_zero(zero, &changed, &before, None)
                .is_err()
        );
        assert!(!fixture.state.join("apply-intents-v1").exists());
    }

    #[test]
    fn store_archive_reopens_preserves_directory_identity_and_releases_only_its_slot() {
        for denied in [false, true] {
            let fixture = StoreFixture::new();
            let (request, before, zero) = fixture.admission('1');
            let key = zero.descriptor.as_ref().unwrap().operation_key;
            let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            store
                .publish_generation_zero(zero.clone(), &request, &before, None)
                .unwrap();
            let original = file_identity(&std::fs::metadata(fixture.active_path(key)).unwrap());
            let terminal = abandoned_generation(&zero, denied);
            store.append(key, terminal.clone()).unwrap();
            store.append(key, terminal.clone()).unwrap();
            let seal = abandonment_seal(&zero, &terminal);
            store.archive_terminal(key, &seal).unwrap();
            assert!(store.active().unwrap().is_none());
            let archive = fixture
                .state
                .join("apply-intents-v1/archive")
                .join(key.as_hex());
            assert_eq!(
                file_identity(&std::fs::metadata(&archive).unwrap()),
                original
            );
            drop(store);
            let mut reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            let found = reopened.archived(key).unwrap().unwrap();
            assert_eq!(found.generations.len(), 2);
            assert_eq!(found.latest(), &terminal);
            assert_eq!(found.seal, Some(seal));
            let expected_bytes: u64 = std::fs::read_dir(&archive)
                .unwrap()
                .map(|e| e.unwrap().metadata().unwrap().len())
                .sum();
            assert_eq!(reopened.archive_inventory().unwrap(), (1, expected_bytes));
            assert!(
                reopened
                    .publish_generation_zero(zero, &request, &before, None)
                    .is_err()
            );
            let (_, _, next) = fixture.admission('2');
            reopened
                .publish_generation_zero(next, &request, &before, None)
                .unwrap();
            assert!(reopened.active().unwrap().is_some());
            assert!(reopened.archived(key).unwrap().is_some());
        }
    }

    #[test]
    fn store_append_resumes_same_inode_prefix_and_foreign_prefix_blocks_without_rewrite() {
        for foreign in [false, true] {
            let fixture = StoreFixture::new();
            let (request, before, zero) = fixture.admission('1');
            let key = zero.descriptor.as_ref().unwrap().operation_key;
            let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            store
                .publish_generation_zero(zero.clone(), &request, &before, None)
                .unwrap();
            let terminal = abandoned_generation(&zero, false);
            let envelope = ApplyIntentEnvelopeV1 {
                schema_version: 1,
                descriptor: None,
                descriptor_hash: terminal.descriptor_hash,
                record_hash: terminal.record_hash().unwrap(),
                generation: terminal.clone(),
            };
            let bytes = envelope.canonical_bytes().unwrap();
            let directory = IntentDirectory::open(&fixture.active_path(key)).unwrap();
            let prefix: &[u8] = if foreign {
                b"foreign"
            } else {
                &bytes[..bytes.len() / 2]
            };
            directory
                .write_exact_new(".g01.v1.json.tmp", prefix, MAX_INTENT_GENERATION_BYTES)
                .unwrap();
            let inode = directory
                .entry_identity(&component(OsStr::new(".g01.v1.json.tmp")).unwrap())
                .unwrap();
            drop(store);
            let reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            assert_eq!(
                reopened
                    .active()
                    .unwrap()
                    .unwrap()
                    .pending_generation
                    .as_deref(),
                Some(prefix)
            );
            if foreign {
                assert!(matches!(
                    reopened.append(key, terminal),
                    Err(ApplyIntentError::Invalid("foreign intent prefix"))
                ));
                assert_eq!(
                    directory
                        .read(".g01.v1.json.tmp", MAX_INTENT_GENERATION_BYTES)
                        .unwrap(),
                    prefix
                );
                assert!(!fixture.active_path(key).join("g01.v1.json").exists());
            } else {
                reopened.append(key, terminal.clone()).unwrap();
                assert_eq!(
                    directory
                        .entry_identity(&component(OsStr::new("g01.v1.json")).unwrap())
                        .unwrap(),
                    inode
                );
                assert_eq!(reopened.active().unwrap().unwrap().latest(), &terminal);
            }
        }
    }

    #[test]
    fn store_archive_resumes_exact_seal_prefix_and_retains_rename_collision() {
        for collision in [false, true] {
            let fixture = StoreFixture::new();
            let (request, before, zero) = fixture.admission('1');
            let key = zero.descriptor.as_ref().unwrap().operation_key;
            let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            store
                .publish_generation_zero(zero.clone(), &request, &before, None)
                .unwrap();
            let terminal = abandoned_generation(&zero, true);
            store.append(key, terminal.clone()).unwrap();
            let seal = abandonment_seal(&zero, &terminal);
            let bytes = seal.canonical_bytes().unwrap();
            let directory = IntentDirectory::open(&fixture.active_path(key)).unwrap();
            directory
                .write_exact_new(
                    ".archive-seal.v1.json.tmp",
                    &bytes[..bytes.len() / 2],
                    MAX_INTENT_GENERATION_BYTES,
                )
                .unwrap();
            let inode = directory
                .entry_identity(&component(OsStr::new(".archive-seal.v1.json.tmp")).unwrap())
                .unwrap();
            if collision {
                let archive =
                    IntentDirectory::open(&fixture.state.join("apply-intents-v1/archive")).unwrap();
                archive
                    .child(&key.as_hex(), true)
                    .unwrap()
                    .write_exact_new("sentinel", b"retain", 6)
                    .unwrap();
                assert!(store.archive_terminal(key, &seal).is_err());
                assert_eq!(
                    directory
                        .read(".archive-seal.v1.json.tmp", MAX_INTENT_GENERATION_BYTES)
                        .unwrap(),
                    bytes[..bytes.len() / 2]
                );
                assert_eq!(
                    std::fs::read(archive.path.join(key.as_hex()).join("sentinel")).unwrap(),
                    b"retain"
                );
                let (_, _, next) = fixture.admission('2');
                assert!(
                    store
                        .publish_generation_zero(next, &request, &before, None)
                        .is_err()
                );
            } else {
                drop(store);
                let reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
                reopened.archive_terminal(key, &seal).unwrap();
                let archive = IntentDirectory::open(
                    &fixture
                        .state
                        .join("apply-intents-v1/archive")
                        .join(key.as_hex()),
                )
                .unwrap();
                assert_eq!(
                    archive
                        .entry_identity(&component(OsStr::new("archive-seal.v1.json")).unwrap())
                        .unwrap(),
                    inode
                );
                assert_eq!(reopened.archived(key).unwrap().unwrap().seal, Some(seal));
            }
        }
    }

    #[test]
    fn store_seal_terminal_pointers_and_generic_triple_are_joined_exactly() {
        let fixture = StoreFixture::new();
        let (request, before, zero) = fixture.admission('1');
        let key = zero.descriptor.as_ref().unwrap().operation_key;
        let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        store
            .publish_generation_zero(zero.clone(), &request, &before, None)
            .unwrap();
        let terminal = abandoned_generation(&zero, true);
        store.append(key, terminal.clone()).unwrap();
        let seal = abandonment_seal(&zero, &terminal);
        for field in 0..17 {
            let mut bad = seal.clone();
            match field {
                0 => bad.operation_key = hash('a'),
                1 => bad.descriptor_hash = hash('a'),
                2 => bad.terminal_sequence += 1,
                3 => bad.terminal_record_hash = hash('a'),
                4 => bad.request_hash = hash('a'),
                5 => bad.source_before_hash = hash('a'),
                6 => bad.generic_terminal_generation = Some(4),
                7 => bad.generic_terminal_hash = Some(hash('a')),
                8 => bad.generic_terminal_sequence = Some(12),
                9 => bad.generic_terminal_generation = None,
                10 => bad.schema_version = 2,
                11 => bad.terminal_phase = ApplyPhaseV1::Denied,
                12 => bad.journal_header_hash = Some(hash('a')),
                13 => bad.journal_tip_hash = Some(hash('a')),
                14 => bad.report_hash = Some(hash('a')),
                15 => bad.marker_kind = Some(MarkerKindV1::C12Generation),
                16 => {
                    bad.marker_kind = Some(MarkerKindV1::C12Generation);
                    bad.marker_hash = Some(hash('a'));
                }
                _ => unreachable!(),
            }
            assert!(store.archive_terminal(key, &bad).is_err(), "field {field}");
            assert_eq!(
                std::fs::read_dir(fixture.active_path(key)).unwrap().count(),
                4
            );
        }
        store.archive_terminal(key, &seal).unwrap();
        assert_eq!(store.archived(key).unwrap().unwrap().seal, Some(seal));
    }

    #[test]
    fn store_c12_binding_presence_and_partial_archive_triples_are_refused_without_writes() {
        let fixture = StoreFixture::new();
        let (request, before, zero) = fixture.admission('1');
        let key = zero.descriptor.as_ref().unwrap().operation_key;
        let binding = d27_binding(d27_manifest());
        let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        assert!(matches!(
            store.publish_generation_zero(zero.clone(), &request, &before, Some(&binding)),
            Err(ApplyIntentError::Invalid("binding artifact presence"))
        ));
        assert!(!fixture.state.join("apply-intents-v1").exists());
        store
            .publish_generation_zero(zero.clone(), &request, &before, None)
            .unwrap();
        let terminal = abandoned_generation(&zero, true);
        store.append(key, terminal.clone()).unwrap();
        let seal = abandonment_seal(&zero, &terminal);
        for mask in 1_u8..7 {
            let mut partial = seal.clone();
            partial.generic_terminal_generation = (mask & 1 != 0).then_some(3);
            partial.generic_terminal_hash = (mask & 2 != 0).then_some(hash('9'));
            partial.generic_terminal_sequence = (mask & 4 != 0).then_some(11);
            assert!(
                matches!(
                    store.archive_terminal(key, &partial),
                    Err(ApplyIntentError::Invalid("archive seal fields"))
                ),
                "mask {mask}"
            );
            assert_eq!(
                std::fs::read_dir(fixture.active_path(key)).unwrap().count(),
                4
            );
        }
        let directory = IntentDirectory::open(&fixture.active_path(key)).unwrap();
        let bytes = binding.canonical_bytes().unwrap();
        directory
            .write_exact_new("d27-source-binding.v1.json", &bytes, MAX_ARTIFACT_BYTES)
            .unwrap();
        assert!(store.active().is_err());
        assert_eq!(
            directory
                .read("d27-source-binding.v1.json", MAX_ARTIFACT_BYTES)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn store_reopen_rejects_unknown_entries_gaps_and_changed_saved_capacity() {
        for corruption in 0..3 {
            let fixture = StoreFixture::new();
            let (request, before, zero) = fixture.admission('1');
            let key = zero.descriptor.as_ref().unwrap().operation_key;
            let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            store
                .publish_generation_zero(zero.clone(), &request, &before, None)
                .unwrap();
            let directory = IntentDirectory::open(&fixture.active_path(key)).unwrap();
            match corruption {
                0 => directory.write_exact_new("foreign", b"retain", 6).unwrap(),
                1 => directory
                    .write_exact_new(
                        "g02.v1.json",
                        &zero.canonical_bytes().unwrap(),
                        MAX_INTENT_GENERATION_BYTES,
                    )
                    .unwrap(),
                2 => {
                    let mut saved = zero.clone();
                    saved
                        .descriptor
                        .as_mut()
                        .unwrap()
                        .capacity_plan
                        .request_bytes += 1;
                    let bytes = serde_json::to_vec(&saved).unwrap();
                    std::fs::write(directory.path.join("g00.v1.json"), &bytes).unwrap();
                }
                _ => unreachable!(),
            }
            let names = directory.names(Some(18)).unwrap();
            let bytes: Vec<_> = names
                .iter()
                .map(|n| directory.read(n, MAX_ARTIFACT_BYTES).unwrap())
                .collect();
            drop(store);
            let reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            assert!(reopened.active().is_err());
            assert_eq!(directory.names(Some(18)).unwrap(), names);
            for (name, expected) in names.iter().zip(bytes) {
                assert_eq!(directory.read(name, MAX_ARTIFACT_BYTES).unwrap(), expected);
            }
        }
    }
    #[test]
    fn pinned_file_publication_keeps_collision_bytes_and_rejects_foreign_prefix() {
        let root = tempfile::tempdir().unwrap();
        let directory = IntentDirectory::open(root.path()).unwrap();
        directory.write_exact_new("saved", b"old", 3).unwrap();
        assert!(directory.publish_exact("saved", b"new", 3).is_err());
        assert_eq!(directory.read("saved", 3).unwrap(), b"old");
        directory
            .write_exact_new(".other.tmp", b"foreign", 7)
            .unwrap();
        assert!(directory.publish_exact("other", b"target", 7).is_err());
        assert_eq!(directory.read(".other.tmp", 7).unwrap(), b"foreign");
        assert!(!root.path().join("other").exists());
    }
    #[test]
    fn publication_resumes_exact_prefix_in_place_and_retains_unsafe_entries() {
        let root = tempfile::tempdir().unwrap();
        let directory = IntentDirectory::open(root.path()).unwrap();
        directory.write_exact_new(".saved.tmp", b"ta", 6).unwrap();
        let before = std::fs::metadata(root.path().join(".saved.tmp")).unwrap();
        directory.publish_exact("saved", b"target", 6).unwrap();
        let after = std::fs::metadata(root.path().join("saved")).unwrap();
        assert_eq!(file_identity(&before), file_identity(&after));
        assert_eq!(directory.read("saved", 6).unwrap(), b"target");
        std::os::unix::fs::symlink(root.path().join("saved"), root.path().join(".alias.tmp"))
            .unwrap();
        assert!(directory.publish_exact("alias", b"changed", 7).is_err());
        assert!(
            std::fs::symlink_metadata(root.path().join(".alias.tmp"))
                .unwrap()
                .is_symlink()
        );
        std::fs::hard_link(root.path().join("saved"), root.path().join("hardlink")).unwrap();
        assert!(directory.read("saved", 6).is_err());
    }

    fn d27_zero(
        binding: &D27SourceBindingV1,
        roots: CapacityRootsV1,
        result_id: String,
    ) -> (RequestV1, ObservedSourceV1, ApplyIntentEnvelopeV1) {
        let request = binding.manifest.project_request().unwrap();
        let before = observed(&request);
        let mut prepare = capacity_prepare();
        prepare.inventory =
            crate::management_effect::ManagementInventoryIdV1::parse("D27").unwrap();
        let plan = CapacityPlanV1::build(CapacityAdmissionV1 {
            roots,
            request: &request,
            source_before: &before,
            d27_source_binding: Some(binding),
            prepare: &prepare,
            management_record_limit: u64::MAX,
            management_records_remaining: u64::MAX,
            management_bytes_remaining: u64::MAX,
            inventory_commit_bytes: 8192,
            journal_bytes: 4097,
            report_bytes: 8193,
            archive_seal_bytes: 4096,
            desktop_result_required_bytes: Some(1024 * 1024),
            desktop_records_remaining: Some(u64::MAX),
            desktop_bytes_remaining: Some(u64::MAX),
            archive_count: u64::MAX,
            archive_file_bytes: u64::MAX,
        })
        .unwrap();
        let operation = hash('a');
        let key = d27_operation_key(hash('3'), operation).unwrap();
        let descriptor = ApplyIntentDescriptorV1 {
            schema_version: 1,
            operation_key: key,
            management_operation_id: d27_management_operation_id(operation).unwrap(),
            inventory_id: ApplyInventoryV1::D27,
            project_binding: hash('3'),
            source_identity: request.source_identity,
            agreed_revision: RevisionV1 {
                kind: RevisionKindV1::Sha256,
                value: binding.payload_hash.as_hex(),
            },
            staged_revision: RevisionV1 {
                kind: RevisionKindV1::Sha256,
                value: request.request_hash().unwrap().as_hex(),
            },
            previous_applied_revision: None,
            context: ApplyContextV1::Session {
                base_epoch: u64::MAX,
                session_binding: hash('b'),
                base_session_revision: u64::MAX - 1,
                consume_target_revision: u64::MAX,
                policy_revision: u64::MAX,
                source_operation_digest: operation,
                reserved_result_id_digest: d27_result_id_digest(&result_id).unwrap(),
                reserved_result_id: result_id,
                base_marker_evidence_hash: hash('c'),
                consume_marker_evidence_hash: hash('d'),
                consume_sidecar_offset: u64::MAX,
                consume_sidecar_hash: hash('e'),
                precommit_hash: hash('f'),
            },
            share_source_manifest_digest: binding.manifest_hash,
            changed_path_count: request.entries.len() as u64,
            path_set_digest: request.path_set_digest().unwrap(),
            workspace_apply_request_digest: request.request_hash().unwrap(),
            source_before_observation_digest: before.observation_hash().unwrap(),
            d27_source_binding_bytes: Some(binding.canonical_bytes().unwrap().len() as u64),
            d27_source_binding_hash: Some(binding.binding_hash().unwrap()),
            recovery_parent_identity: binding.manifest.recovery_parent_identity,
            recovery_provenance_digest: binding.manifest.recovery_provenance_digest().unwrap(),
            recovery_child: recovery_child(key).unwrap(),
            capacity_plan_bytes: plan.canonical_bytes().unwrap().len() as u64,
            capacity_plan_hash: plan.plan_hash().unwrap(),
            capacity_plan: plan,
            phase: ApplyPhaseV1::Prepared,
        };
        descriptor.verify_request(&request, &before).unwrap();
        binding
            .verify_artifacts(&descriptor, &request, &before)
            .unwrap();
        let descriptor_hash = descriptor.descriptor_hash().unwrap();
        let generation = ApplyGenerationV1 {
            schema_version: 1,
            sequence: 0,
            previous_record_hash: Hash::from_hex(&"0".repeat(64)).unwrap(),
            descriptor_hash,
            phase: ApplyPhaseV1::Prepared,
            evidence: ApplyEvidenceV1::Prepared {
                capacity_plan_hash: descriptor.capacity_plan_hash,
                request_hash: descriptor.workspace_apply_request_digest,
                request_bytes: descriptor.capacity_plan.request_bytes,
                source_before_hash: descriptor.source_before_observation_digest,
                source_before_bytes: descriptor.capacity_plan.source_before_bytes,
            },
        };
        let envelope = ApplyIntentEnvelopeV1 {
            schema_version: 1,
            descriptor: Some(descriptor),
            descriptor_hash,
            record_hash: generation.record_hash().unwrap(),
            generation,
        };
        envelope.validate().unwrap();
        (request, before, envelope)
    }

    fn maximum_shape_generations() -> [ApplyIntentEnvelopeV1; 2] {
        // Maximal scalar widths and presence, within the closed legal schema.
        // No padding, unknown field or impossible union of context variants.
        let sample = CapacityRootSampleV1 {
            identity: IdentityV1 {
                device: u64::MAX,
                inode: u64::MAX,
            },
            allocation_unit: 4096,
            available_bytes: u64::MAX,
        };
        let mut manifest = d27_manifest();
        manifest.source_identity = sample.identity;
        manifest.recovery_parent_identity = sample.identity;
        let binding = d27_binding(manifest);
        // Each NUL is one UTF-8 byte and canonically escapes to six bytes;
        // ResultId accepts all 128 bytes. The descriptor digest remains exact.
        let (_, _, zero) = d27_zero(
            &binding,
            CapacityRootsV1 {
                state: sample,
                recovery: sample,
                desktop: Some(sample),
            },
            "\0".repeat(polaris_desktop_protocol::ids::MAX_ID_BYTES),
        );
        let generation = ApplyGenerationV1 {
            schema_version: 1,
            sequence: 11,
            previous_record_hash: hash('1'),
            descriptor_hash: zero.descriptor_hash,
            phase: ApplyPhaseV1::AbandonedBeforeEffect,
            evidence: ApplyEvidenceV1::AbandonedBeforeEffect {
                source_readback_hash: hash('2'),
                child_disposition: ChildDispositionV1::RetainedHeaderOnly,
                generic_denial_reason: Some(ManagementDenialReasonV1::ResourceUnavailable),
                generic_generation: Some(u64::MAX),
                generic_event_hash: Some(hash('3')),
                generic_sequence: Some(u64::MAX),
            },
        };
        let abandoned = ApplyIntentEnvelopeV1 {
            schema_version: 1,
            descriptor: None,
            descriptor_hash: zero.descriptor_hash,
            record_hash: generation.record_hash().unwrap(),
            generation,
        };
        [zero, abandoned]
    }

    #[test]
    fn maximum_shape_generation_65536_is_accepted() {
        for (i, shape) in maximum_shape_generations().iter().enumerate() {
            let canonical = shape.canonical_bytes().unwrap();
            assert!(canonical.len() < 8545);
            assert_eq!(
                ApplyIntentEnvelopeV1::from_canonical_bytes(&canonical).unwrap(),
                *shape
            );
            let root = tempfile::tempdir().unwrap();
            let directory = IntentDirectory::open(root.path()).unwrap();
            let name = format!("g{i:02}.v1.json");
            // The approved interpretation tests the raw production size gate
            // separately. A padded buffer is NOT canonical evidence.
            let mut boundary = canonical.clone();
            boundary.resize(MAX_INTENT_GENERATION_BYTES as usize, b' ');
            assert_eq!(
                checked_len(&boundary, MAX_INTENT_GENERATION_BYTES).unwrap(),
                65536
            );
            assert!(matches!(
                ApplyIntentEnvelopeV1::from_canonical_bytes(&boundary),
                Err(ApplyIntentError::Noncanonical)
            ));
            directory
                .publish_exact(&name, &boundary, MAX_INTENT_GENERATION_BYTES)
                .unwrap();
            assert_eq!(
                directory.read(&name, MAX_INTENT_GENERATION_BYTES).unwrap(),
                boundary
            );
            let reopened = IntentDirectory::open(root.path()).unwrap();
            assert_eq!(
                reopened.read(&name, MAX_INTENT_GENERATION_BYTES).unwrap(),
                boundary
            );
            assert_eq!(reopened.names(Some(1)).unwrap(), vec![name]);
            assert_eq!(
                std::fs::metadata(root.path().join(format!("g{i:02}.v1.json")))
                    .unwrap()
                    .mode()
                    & 0o7777,
                0o600
            );
        }
    }

    #[test]
    fn maximum_shape_generation_65537_is_rejected_before_mutation() {
        for shape in maximum_shape_generations() {
            let mut bytes = shape.canonical_bytes().unwrap();
            bytes.resize(MAX_INTENT_GENERATION_BYTES as usize + 1, b' ');
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("private");
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let directory = IntentDirectory::open(&path).unwrap();
            directory.write_exact_new("sentinel", b"retain", 6).unwrap();
            let inode = file_identity(&std::fs::metadata(path.join("sentinel")).unwrap());
            // A raced path would fail an open/verify. TooLarge must win first.
            let retained = root.path().join("retained");
            std::fs::rename(&path, &retained).unwrap();
            std::fs::create_dir(&path).unwrap();
            for action in 0..3 {
                let result = match action {
                    0 => {
                        directory.publish_exact("g00.v1.json", &bytes, MAX_INTENT_GENERATION_BYTES)
                    }
                    1 => directory.write_exact_new(
                        "g00.v1.json",
                        &bytes,
                        MAX_INTENT_GENERATION_BYTES,
                    ),
                    _ => ApplyIntentEnvelopeV1::from_canonical_bytes(&bytes).map(|_| ()),
                };
                assert!(matches!(
                    result,
                    Err(ApplyIntentError::TooLarge {
                        actual: 65537,
                        maximum: 65536
                    })
                ));
            }
            assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
            assert_eq!(std::fs::read_dir(&retained).unwrap().count(), 1);
            assert_eq!(std::fs::read(retained.join("sentinel")).unwrap(), b"retain");
            assert_eq!(
                file_identity(&std::fs::metadata(retained.join("sentinel")).unwrap()),
                inode
            );
        }
    }

    #[test]
    fn d27_store_publication_and_durable_join_reject_each_changed_binding() {
        let fixture = StoreFixture::new();
        let desktop = tempfile::tempdir().unwrap();
        let roots = CapacityRootsV1::from_pinned_files(
            &File::open(&fixture.state).unwrap(),
            &File::open(&fixture.recovery).unwrap(),
            Some(&File::open(desktop.path()).unwrap()),
        )
        .unwrap();
        let mut manifest = d27_manifest();
        manifest.recovery_parent_identity = roots.recovery.identity;
        let binding = d27_binding(manifest);
        let (request, before, zero) = d27_zero(&binding, roots, "result-1".into());
        let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        for field in 0..12 {
            let mut bad = binding.clone();
            match field {
                0 => bad.payload_hash = hash('f'),
                1 => bad.payload_bytes += 1,
                2 => bad.manifest_hash = hash('f'),
                3 => bad.manifest_bytes += 1,
                4 => bad.request_hash = hash('f'),
                5 => bad.request_bytes += 1,
                6 => bad.source_before_hash = hash('f'),
                7 => bad.source_before_bytes += 1,
                8 => bad.manifest.recovery_parent_identity.inode += 1,
                9 => bad.manifest.recovery_parent_path.push_str("-changed"),
                10 => bad.manifest.source_identity.inode += 1,
                11 => bad.manifest.entries[0].after.as_mut().unwrap().inode += 1,
                _ => unreachable!(),
            }
            assert!(
                store
                    .publish_generation_zero(zero.clone(), &request, &before, Some(&bad))
                    .is_err(),
                "field {field}"
            );
            assert!(!fixture.state.join("apply-intents-v1").exists());
        }
        assert!(
            store
                .publish_generation_zero(zero.clone(), &request, &before, None)
                .is_err()
        );
        store
            .publish_generation_zero(zero.clone(), &request, &before, Some(&binding))
            .unwrap();
        let key = zero.descriptor.as_ref().unwrap().operation_key;
        let saved =
            std::fs::read(fixture.active_path(key).join("d27-source-binding.v1.json")).unwrap();
        assert_eq!(saved, binding.canonical_bytes().unwrap());
        drop(store);
        let reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        let inspected = reopened.active().unwrap().unwrap();
        assert_eq!(inspected.d27_source_binding, Some(binding));
        assert_eq!(inspected.generations, vec![zero]);
        assert_eq!(inspected.request, request);
        assert_eq!(inspected.source_before, before);
    }

    #[test]
    fn admission_io_failure_retains_staging_or_published_evidence_and_blocks_next_operation() {
        for point in [
            "admission-staging-created",
            "new-created",
            "new-written",
            "new-data-synced",
            "admission-request-written",
            "admission-before-written",
            "admission-generation-written",
            "admission-staging-synced",
            "admission-renamed",
            "admission-readback",
        ] {
            let fixture = StoreFixture::new();
            let (request, before, zero) = fixture.admission('1');
            let key = zero.descriptor.as_ref().unwrap().operation_key;
            let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            let failure = InjectFailure::at(point, libc::ENOSPC);
            assert!(
                matches!(store.publish_generation_zero(zero.clone(), &request, &before, None), Err(ApplyIntentError::Io(e)) if e.raw_os_error() == Some(libc::ENOSPC))
            );
            failure.assert_reached();
            drop(failure);
            drop(store);
            let mut reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            let root = IntentDirectory::open(&fixture.state.join("apply-intents-v1")).unwrap();
            let staging = root.child("staging", false).unwrap();
            if matches!(point, "admission-renamed" | "admission-readback") {
                assert_eq!(
                    reopened.active().unwrap().unwrap().generations,
                    vec![zero.clone()]
                );
                assert!(staging.names(Some(1)).unwrap().is_empty());
            } else {
                assert!(reopened.active().unwrap().is_none());
                assert_eq!(staging.names(Some(1)).unwrap().len(), 1);
                assert!(!fixture.active_path(key).exists());
            }
            let old_entries = staging.names(Some(1)).unwrap();
            let (_, _, next) = fixture.admission('2');
            assert!(
                reopened
                    .publish_generation_zero(next, &request, &before, None)
                    .is_err()
            );
            assert_eq!(staging.names(Some(1)).unwrap(), old_entries);
        }
    }

    #[test]
    fn seal_and_archive_io_failures_reopen_and_resume_without_freeing_active_early() {
        for errno in [libc::ENOSPC, libc::EDQUOT] {
            for point in [
                "publication-opened",
                "publication-written",
                "publication-data-synced",
                "publication-parent-synced",
                "publication-renamed",
                "publication-readback",
                "publication-final-synced",
                "archive-seal-published",
                "archive-renamed",
                "archive-final-synced",
            ] {
                let fixture = StoreFixture::new();
                let (request, before, zero) = fixture.admission('1');
                let key = zero.descriptor.as_ref().unwrap().operation_key;
                let mut store =
                    ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
                store
                    .publish_generation_zero(zero.clone(), &request, &before, None)
                    .unwrap();
                let terminal = abandoned_generation(&zero, true);
                store.append(key, terminal.clone()).unwrap();
                let seal = abandonment_seal(&zero, &terminal);
                let failure = InjectFailure::at(point, errno);
                assert!(
                    matches!(store.archive_terminal(key, &seal), Err(ApplyIntentError::Io(e)) if e.raw_os_error() == Some(errno)),
                    "{point}"
                );
                failure.assert_reached();
                drop(failure);
                drop(store);
                let mut reopened =
                    ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
                if matches!(point, "archive-renamed" | "archive-final-synced") {
                    assert!(reopened.active().unwrap().is_none());
                    assert_eq!(reopened.archived(key).unwrap().unwrap().seal, Some(seal));
                } else {
                    assert_eq!(reopened.active().unwrap().unwrap().latest(), &terminal);
                    assert!(reopened.archived(key).unwrap().is_none());
                    let (_, _, next) = fixture.admission('2');
                    assert!(
                        reopened
                            .publish_generation_zero(next, &request, &before, None)
                            .is_err()
                    );
                    let directory = IntentDirectory::open(&fixture.active_path(key)).unwrap();
                    let name = if directory
                        .names(Some(18))
                        .unwrap()
                        .contains(&"archive-seal.v1.json".into())
                    {
                        "archive-seal.v1.json"
                    } else {
                        ".archive-seal.v1.json.tmp"
                    };
                    let identity = directory
                        .entry_identity(&component(OsStr::new(name)).unwrap())
                        .unwrap();
                    reopened.archive_terminal(key, &seal).unwrap();
                    let archive = IntentDirectory::open(
                        &fixture
                            .state
                            .join("apply-intents-v1/archive")
                            .join(key.as_hex()),
                    )
                    .unwrap();
                    assert_eq!(
                        archive
                            .entry_identity(&component(OsStr::new("archive-seal.v1.json")).unwrap())
                            .unwrap(),
                        identity
                    );
                    assert_eq!(reopened.archived(key).unwrap().unwrap().seal, Some(seal));
                }
                let (_, _, next) = fixture.admission('2');
                reopened
                    .publish_generation_zero(next, &request, &before, None)
                    .unwrap();
            }
        }
    }

    #[test]
    fn archive_payload_validation_precedes_a_raced_store_open() {
        let fixture = StoreFixture::new();
        let (request, before, zero) = fixture.admission('1');
        let key = zero.descriptor.as_ref().unwrap().operation_key;
        let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        store
            .publish_generation_zero(zero.clone(), &request, &before, None)
            .unwrap();
        let terminal = abandoned_generation(&zero, true);
        store.append(key, terminal.clone()).unwrap();
        let mut seal = abandonment_seal(&zero, &terminal);
        seal.terminal_sequence = MAX_INTENT_GENERATIONS;
        let original = fixture.state.join("apply-intents-v1");
        let retained = fixture.state.join("retained-intents");
        std::fs::rename(&original, &retained).unwrap();
        std::fs::create_dir(&original).unwrap();
        assert!(matches!(
            store.archive_terminal(key, &seal),
            Err(ApplyIntentError::Invalid("archive seal fields"))
        ));
        assert_eq!(std::fs::read_dir(&original).unwrap().count(), 0);
        let saved = std::fs::read(
            retained
                .join("active")
                .join(key.as_hex())
                .join("g01.v1.json"),
        )
        .unwrap();
        assert_eq!(
            ApplyIntentEnvelopeV1::from_canonical_bytes(&saved)
                .unwrap()
                .generation,
            terminal
        );
    }

    #[test]
    fn capacity_seal_bound_and_every_generation_reservation_component_are_validated() {
        let plan = capacity_plan(7);
        let mut exact = plan.clone();
        exact.archive_seal_bytes = MAX_INTENT_GENERATION_BYTES;
        exact.intent_temporary_bytes =
            allocated_bytes(65536, 4096).unwrap() + allocated_bytes(65536, 4096).unwrap();
        exact.state_required_bytes = plan.state_required_bytes
            + 2 * (allocated_bytes(65536, 4096).unwrap()
                - allocated_bytes(plan.archive_seal_bytes, 4096).unwrap());
        exact.formula_hash = exact.recomputed_formula_hash().unwrap();
        assert_eq!(
            CapacityPlanV1::from_canonical_bytes(&exact.canonical_bytes().unwrap()).unwrap(),
            exact
        );
        let mut above = exact.clone();
        above.archive_seal_bytes += 1;
        assert!(matches!(
            above.validate(),
            Err(ApplyIntentError::Invalid("capacity artifact lengths"))
        ));
        // Wrong slot counts/widths and old two-field seal reservations cannot
        // survive merely rehashing the formula/plan or fitting free space.
        for (count, width) in [(11, 65536), (13, 65536), (12, 65535), (12, 65537)] {
            let mut bad = plan.clone();
            bad.intent_final_bytes = allocated_bytes(plan.request_bytes, 4096).unwrap()
                + allocated_bytes(plan.source_before_bytes, 4096).unwrap()
                + count * width;
            bad.formula_hash = bad.recomputed_formula_hash().unwrap();
            assert!(bad.validate().is_err(), "{count} * {width}");
        }
        for field in 0..12 {
            let mut bad = plan.clone();
            match field {
                0 => bad.intent_final_bytes += 1,
                1 => bad.intent_temporary_bytes += 1,
                2 => bad.management_event_journal_required_bytes += 1,
                3 => bad.management_generation_seal_required_bytes += 1,
                4 => bad.generic_effect_bytes += 1,
                5 => bad.state_required_bytes += 1,
                6 => bad.recovery_required_bytes += 1,
                7 => bad.state_directory_overhead_bytes += 1,
                8 => bad.recovery_directory_overhead_bytes += 1,
                9 => bad.free_space_floor_bytes += 1,
                10 => bad.inventory_commit_bytes = 0,
                11 => bad.formula_hash = hash('f'),
                _ => unreachable!(),
            }
            assert!(bad.validate().is_err(), "field {field}");
        }
    }

    #[test]
    fn persisted_capacity_reader_rejects_missing_duplicate_reordered_and_non_utf8_encodings() {
        let plan = capacity_plan(7);
        let raw = plan.canonical_bytes().unwrap();
        let text = String::from_utf8(raw.clone()).unwrap();
        let mut invalid = vec![
            format!("{text}\n").into_bytes(),
            text.replacen("\"schema_version\":1,", "", 1).into_bytes(),
            text.replacen(
                "\"schema_version\":1,",
                "\"schema_version\":1,\"schema_version\":1,",
                1,
            )
            .into_bytes(),
            text.replacen(
                "\"schema_version\":1,",
                "\"unknown\":null,\"schema_version\":1,",
                1,
            )
            .into_bytes(),
            text.replacen(
                "\"state_device\":7,\"recovery_device\":7",
                "\"recovery_device\":7,\"state_device\":7",
                1,
            )
            .into_bytes(),
            text.replacen("\"device\":7,\"inode\":23", "\"inode\":23,\"device\":7", 1)
                .into_bytes(),
            text.replacen("\"schema_version\":1", "\"schema_version\":1.0", 1)
                .into_bytes(),
            text.replacen(
                "\"state_device\":7",
                "\"state_device\":18446744073709551616",
                1,
            )
            .into_bytes(),
        ];
        invalid.push([&[0xef, 0xbb, 0xbf][..], &raw].concat());
        invalid.push(text.encode_utf16().flat_map(u16::to_le_bytes).collect());
        for changed in invalid {
            assert_ne!(changed, raw, "mutation must alter bytes");
            assert!(CapacityPlanV1::from_canonical_bytes(&changed).is_err());
        }
        assert_eq!(CapacityPlanV1::from_canonical_bytes(&raw).unwrap(), plan);
        // Parameter ordering is also committed by the domain hash; a reordered
        // group/root list cannot retain the historical formula commitment.
        let formula = plan.formula_parameters().unwrap();
        let raw = serde_json::to_vec(&formula).unwrap();
        let mut changed = formula.clone();
        changed.device_groups[0].roots.reverse();
        assert_ne!(
            digest(
                b"polaris-capacity-formula-v1\0",
                &serde_json::to_vec(&changed).unwrap()
            ),
            digest(b"polaris-capacity-formula-v1\0", &raw)
        );
        let mut distinct = capacity_plan(8).formula_parameters().unwrap();
        let expected = serde_json::to_vec(&distinct).unwrap();
        distinct.device_groups.reverse();
        assert_ne!(
            digest(
                b"polaris-capacity-formula-v1\0",
                &serde_json::to_vec(&distinct).unwrap()
            ),
            digest(b"polaris-capacity-formula-v1\0", &expected)
        );
    }

    #[test]
    fn terminal_archive_shortage_is_live_and_does_not_change_saved_evidence() {
        let fixture = StoreFixture::new();
        let (request, before, zero) = fixture.admission('1');
        let descriptor = zero.descriptor.as_ref().unwrap();
        let key = descriptor.operation_key;
        let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        store
            .publish_generation_zero(zero.clone(), &request, &before, None)
            .unwrap();
        let terminal = abandoned_generation(&zero, true);
        store.append(key, terminal.clone()).unwrap();
        let seal = abandonment_seal(&zero, &terminal);
        let failure = InjectFailure::at("archive-seal-published", libc::ENOSPC);
        assert!(
            matches!(store.archive_terminal(key, &seal), Err(ApplyIntentError::Io(e)) if e.raw_os_error() == Some(libc::ENOSPC))
        );
        failure.assert_reached();
        drop(failure);
        drop(store);
        let reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
        let current = reopened.active().unwrap().unwrap();
        assert_eq!(current.latest().phase, ApplyPhaseV1::AbandonedBeforeEffect);
        assert_eq!(current.seal, Some(seal.clone()));
        let plan = &current.descriptor().capacity_plan;
        let saved_plan = plan.canonical_bytes().unwrap();
        let saved_generation = std::fs::read(fixture.active_path(key).join("g01.v1.json")).unwrap();
        let saved_seal =
            std::fs::read(fixture.active_path(key).join("archive-seal.v1.json")).unwrap();
        let mut fresh = CapacityRootsV1::from_pinned_files(
            &File::open(&fixture.state).unwrap(),
            &File::open(&fixture.recovery).unwrap(),
            None,
        )
        .unwrap();
        // The seal is already published; only archive directory metadata
        // remains reserved. This status calculation performs no admission.
        let remaining = DIRECTORY_OVERHEAD_BYTES;
        for delta in [-1_i64, 0, 1] {
            fresh.state.available_bytes = (remaining + FREE_SPACE_FLOOR_BYTES)
                .checked_add_signed(delta)
                .unwrap();
            fresh.recovery.available_bytes = fresh.state.available_bytes;
            let rows = plan
                .live_capacity_groups(&fresh, remaining, 0, None)
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].required_remaining_bytes, remaining);
            assert_eq!(rows[0].shortage_bytes, if delta == -1 { 1 } else { 0 });
            assert_eq!(plan.canonical_bytes().unwrap(), saved_plan);
            assert_eq!(
                std::fs::read(fixture.active_path(key).join("g01.v1.json")).unwrap(),
                saved_generation
            );
            assert_eq!(
                std::fs::read(fixture.active_path(key).join("archive-seal.v1.json")).unwrap(),
                saved_seal
            );
        }
        // Status never rewrites terminal outcome; explicit retry reuses seal.
        reopened.archive_terminal(key, &seal).unwrap();
        assert!(reopened.active().unwrap().is_none());
        assert_eq!(
            reopened
                .archived(key)
                .unwrap()
                .unwrap()
                .descriptor()
                .capacity_plan
                .canonical_bytes()
                .unwrap(),
            saved_plan
        );
    }

    #[test]
    fn seal_full_phase_chain_and_unknown_receipts_require_exact_durable_pointers() {
        for unknown in [false, true] {
            let fixture = StoreFixture::new();
            let (request, before, zero) = fixture.admission('1');
            let descriptor = zero.descriptor.as_ref().unwrap();
            let key = descriptor.operation_key;
            let mut store = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            store
                .publish_generation_zero(zero.clone(), &request, &before, None)
                .unwrap();
            let mut evidence = vec![
                ApplyEvidenceV1::RecoveryPrepared {
                    child_identity: descriptor.recovery_parent_identity,
                    journal_header_hash: hash('a'),
                    journal_header_bytes: 128,
                    directory_synced: true,
                    generic_prepared_generation: 0,
                    generic_prepared_hash: hash('b'),
                    generic_prepared_sequence: 1,
                },
                ApplyEvidenceV1::EffectStarted {
                    generic_generation: 0,
                    generic_event_hash: hash('c'),
                    generic_sequence: 2,
                },
                ApplyEvidenceV1::ApplyReported {
                    report_hash: hash('d'),
                    report_bytes: 256,
                    journal_tip_hash: hash('e'),
                    result: ApplyResultV1::Success,
                },
            ];
            if unknown {
                evidence.extend([
                    ApplyEvidenceV1::OutcomeUnknown {
                        reason: ManagementUnknownReasonV1::ReceiptPersistenceFailed,
                        journal_tip_hash: Some(hash('f')),
                        report_hash: Some(hash('d')),
                        marker_hash: Some(hash('1')),
                        generic_generation: 0,
                        generic_event_hash: hash('2'),
                        generic_sequence: 3,
                    },
                    ApplyEvidenceV1::ReconciledAbandon {
                        readback_hash: hash('3'),
                        generic_generation: 0,
                        generic_event_hash: hash('4'),
                        generic_sequence: 4,
                    },
                ]);
            } else {
                evidence.extend([
                    ApplyEvidenceV1::ParticipationCommitted {
                        marker_kind: MarkerKindV1::C12Generation,
                        marker_hash: hash('1'),
                        committed_revision: 9,
                        sidecar_hash: None,
                        sidecar_bytes: None,
                        result_id_digest: None,
                    },
                    ApplyEvidenceV1::Completed {
                        readback_hash: hash('3'),
                        generic_generation: 0,
                        generic_event_hash: hash('4'),
                        generic_sequence: 3,
                    },
                ]);
            }
            let mut previous = zero.generation.clone();
            for (i, evidence) in evidence.into_iter().enumerate() {
                let next = ApplyGenerationV1 {
                    schema_version: 1,
                    sequence: i as u64 + 1,
                    previous_record_hash: previous.record_hash().unwrap(),
                    descriptor_hash: zero.descriptor_hash,
                    phase: evidence.phase(),
                    evidence,
                };
                store.append(key, next.clone()).unwrap();
                previous = next;
            }
            let mut seal = abandonment_seal(&zero, &previous);
            seal.journal_header_hash = Some(hash('a'));
            seal.journal_tip_hash = Some(hash(if unknown { 'f' } else { 'e' }));
            seal.report_hash = Some(hash('d'));
            seal.marker_kind = Some(MarkerKindV1::C12Generation);
            seal.marker_hash = Some(hash('1'));
            seal.final_readback_hash = Some(hash('3'));
            for field in 0..12 {
                let mut bad = seal.clone();
                match field {
                    0 => bad.journal_header_hash = None,
                    1 => bad.journal_header_hash = Some(hash('5')),
                    2 => bad.journal_tip_hash = None,
                    3 => bad.journal_tip_hash = Some(hash('5')),
                    4 => bad.report_hash = None,
                    5 => bad.report_hash = Some(hash('5')),
                    6 => {
                        bad.marker_kind = None;
                        bad.marker_hash = None;
                    }
                    7 => bad.marker_kind = Some(MarkerKindV1::D27Session),
                    8 => bad.marker_hash = Some(hash('5')),
                    9 => bad.final_readback_hash = None,
                    10 => bad.final_readback_hash = Some(hash('5')),
                    11 => bad.generic_terminal_generation = Some(1),
                    _ => unreachable!(),
                }
                assert!(
                    store.archive_terminal(key, &bad).is_err(),
                    "{unknown}, {field}"
                );
                assert_eq!(
                    std::fs::read_dir(fixture.active_path(key)).unwrap().count(),
                    8
                );
            }
            store.archive_terminal(key, &seal).unwrap();
            drop(store);
            let reopened = ApplyIntentStoreV1::inspect(&fixture.state, &fixture.lease).unwrap();
            assert!(reopened.active().unwrap().is_none());
            let archived = reopened.archived(key).unwrap().unwrap();
            assert_eq!(archived.latest(), &previous);
            assert_eq!(archived.seal, Some(seal));
        }
    }

    #[test]
    fn revision_kind_value_and_nested_canonical_schema_are_strict() {
        for (kind, spelling, length) in [
            (RevisionKindV1::GitSha1, "git_sha1", 40),
            (RevisionKindV1::GitSha256, "git_sha256", 64),
            (RevisionKindV1::Sha256, "sha256", 64),
        ] {
            let shape = FinalReadbackV1 {
                schema_version: 1,
                descriptor_hash: hash('1'),
                request_hash: hash('2'),
                source_observation_hash: hash('3'),
                journal_tip_hash: Some(hash('4')),
                report_hash: Some(hash('5')),
                marker_kind: Some(MarkerKindV1::C12Generation),
                marker_hash: Some(hash('6')),
                agreed_revision: RevisionV1 {
                    kind,
                    value: "a".repeat(length),
                },
            };
            let raw = shape.canonical_bytes().unwrap();
            assert_eq!(FinalReadbackV1::from_canonical_bytes(&raw).unwrap(), shape);
            for invalid in [
                String::new(),
                "a".repeat(length - 1),
                "a".repeat(length + 1),
                "A".repeat(length),
                "g".repeat(length),
            ] {
                let mut bad = shape.clone();
                bad.agreed_revision.value = invalid;
                assert!(matches!(
                    bad.canonical_bytes(),
                    Err(ApplyIntentError::Invalid("revision kind or value"))
                ));
                assert!(matches!(
                    FinalReadbackV1::from_canonical_bytes(&serde_json::to_vec(&bad).unwrap()),
                    Err(ApplyIntentError::Invalid("revision kind or value"))
                ));
            }
            let text = String::from_utf8(raw.clone()).unwrap();
            let nested = format!(
                "\"kind\":\"{spelling}\",\"value\":\"{}\"",
                "a".repeat(length)
            );
            assert!(text.contains(&nested));
            for replacement in [
                format!("\"kind\":\"unknown\",\"value\":\"{}\"", "a".repeat(length)),
                format!("\"value\":\"{}\"", "a".repeat(length)),
                format!("\"kind\":\"{spelling}\""),
                format!(
                    "\"kind\":\"{spelling}\",\"kind\":\"{spelling}\",\"value\":\"{}\"",
                    "a".repeat(length)
                ),
                format!(
                    "\"kind\":\"{spelling}\",\"value\":\"{}\",\"unknown\":null",
                    "a".repeat(length)
                ),
                format!(
                    "\"value\":\"{}\",\"kind\":\"{spelling}\"",
                    "a".repeat(length)
                ),
                format!("\"kind\":null,\"value\":\"{}\"", "a".repeat(length)),
                format!("\"kind\":\"{spelling}\",\"value\":null"),
                format!("\"kind\":7,\"value\":\"{}\"", "a".repeat(length)),
                format!("\"kind\":\"{spelling}\",\"value\":[]"),
            ] {
                let bad = text.replacen(&nested, &replacement, 1).into_bytes();
                assert_ne!(bad, raw);
                assert!(
                    FinalReadbackV1::from_canonical_bytes(&bad).is_err(),
                    "{spelling}: {replacement}"
                );
            }
        }
    }

    #[test]
    fn inventory_revision_context_rejects_well_formed_wrong_kinds_and_bindings() {
        let fixture = StoreFixture::new();
        let (_, _, zero) = fixture.admission('1');
        let c12 = zero.descriptor.as_ref().unwrap();
        c12.validate().unwrap();
        let mut git256 = c12.clone();
        git256.agreed_revision = RevisionV1 {
            kind: RevisionKindV1::GitSha256,
            value: "a".repeat(64),
        };
        git256.staged_revision = git256.agreed_revision.clone();
        git256.previous_applied_revision = Some(RevisionV1 {
            kind: RevisionKindV1::GitSha256,
            value: "b".repeat(64),
        });
        let raw = git256.canonical_bytes().unwrap();
        assert_eq!(
            ApplyIntentDescriptorV1::from_canonical_bytes(&raw).unwrap(),
            git256
        );
        for field in 0..3 {
            let mut bad = git256.clone();
            match field {
                0 => {
                    bad.agreed_revision.kind = RevisionKindV1::Sha256;
                    bad.staged_revision.kind = RevisionKindV1::Sha256;
                    bad.previous_applied_revision = None;
                }
                1 => bad.staged_revision.value = "c".repeat(64),
                2 => {
                    bad.previous_applied_revision = Some(RevisionV1 {
                        kind: RevisionKindV1::GitSha1,
                        value: "b".repeat(40),
                    })
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                bad.validate(),
                Err(ApplyIntentError::Invalid("C12 descriptor context"))
            ));
            assert!(
                ApplyIntentDescriptorV1::from_canonical_bytes(&serde_json::to_vec(&bad).unwrap())
                    .is_err()
            );
        }
        let zero = maximum_shape_generations().into_iter().next().unwrap();
        let d27 = zero.descriptor.unwrap();
        d27.validate().unwrap();
        for field in 0..6 {
            let mut bad = d27.clone();
            match field {
                0 => {
                    bad.agreed_revision = RevisionV1 {
                        kind: RevisionKindV1::GitSha1,
                        value: "a".repeat(40),
                    }
                }
                1 => {
                    bad.staged_revision = RevisionV1 {
                        kind: RevisionKindV1::GitSha256,
                        value: "a".repeat(64),
                    }
                }
                2 => bad.staged_revision.value = "f".repeat(64),
                3 => {
                    bad.previous_applied_revision = Some(RevisionV1 {
                        kind: RevisionKindV1::Sha256,
                        value: "a".repeat(64),
                    })
                }
                4 => {
                    let ApplyContextV1::Session {
                        reserved_result_id,
                        reserved_result_id_digest,
                        ..
                    } = &mut bad.context
                    else {
                        unreachable!()
                    };
                    *reserved_result_id = "a".repeat(129);
                    *reserved_result_id_digest = d27_result_id_digest(reserved_result_id).unwrap();
                }
                5 => {
                    let ApplyContextV1::Session {
                        consume_target_revision,
                        ..
                    } = &mut bad.context
                    else {
                        unreachable!()
                    };
                    *consume_target_revision -= 1;
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    bad.validate(),
                    Err(ApplyIntentError::Invalid("D27 descriptor context"))
                ),
                "{field}"
            );
            assert!(
                ApplyIntentDescriptorV1::from_canonical_bytes(&serde_json::to_vec(&bad).unwrap())
                    .is_err()
            );
        }
    }

    #[test]
    fn payload_too_large_does_not_create_any_entry() {
        let root = tempfile::tempdir().unwrap();
        let directory = IntentDirectory::open(root.path()).unwrap();
        assert!(matches!(
            directory.publish_exact("saved", b"1234", 3),
            Err(ApplyIntentError::TooLarge {
                actual: 4,
                maximum: 3
            })
        ));
        assert!(directory.names(Some(1)).unwrap().is_empty());
    }
    #[test]
    fn directory_replacement_is_refused_without_touching_the_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = IntentDirectory::open(&path).unwrap();
        std::fs::rename(&path, root.path().join("retained")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(directory.write_exact_new("saved", b"data", 4).is_err());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(root.path().join("retained"))
                .unwrap()
                .count(),
            0
        );
    }
}
