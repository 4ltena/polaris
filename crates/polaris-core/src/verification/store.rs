//! Private immutable verification records.
//!
//! This is deliberately a body store, not a discovery index.  The trusted
//! workflow/session owner keeps a record reference in its atomically published
//! checkpoint.  A caller therefore cannot use this type to scan another
//! project's verification history or to make an old decision current again.

use super::{
    AllReasonV1, AllTestsScopeV1, CandidateSetV1, PlanDecisionV1, SCHEMA_VERSION,
    StaticCapabilityCatalogV1, StaticCapabilityV1, StaticEvidenceV1, TestDecisionRecordV1,
    TestInventoryV1, TestSelectionV1, VerificationError, VerificationNeedV1,
    VerificationRouteCandidateV1, select_tests_offline, stable_id_hash,
    static_analysis_request_hash,
};
use crate::{
    conversation_state::content_hash,
    workflow::{EvidenceBindingV1, EvidenceRecordV1, ReviewRecordV1, validate_hash},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
const STORE_DIRECTORY: &str = "verification-records-v1";
const MAX_RECORD_BYTES: usize = 512 * 1024;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Every persisted input and decision is tied to the exact project graph and
/// registered analyzer/tool identity that produced it.  These are opaque
/// SHA-256 strings supplied by the trusted owner; equality, not a partial
/// match, is the reuse rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindingV1 {
    pub project_graph_hash: String,
    pub analyzer_hash: String,
    pub tool_hash: String,
    pub source_hash: String,
    pub diff_hash: String,
    /// Exact registered analyzer configuration body, independent of its
    /// executable identity.
    pub config_hash: String,
    /// Exact scope/request identity established before the analyzer ran.
    pub analysis_request_hash: String,
}

impl VerificationBindingV1 {
    pub fn validate(&self) -> io::Result<()> {
        for hash in [
            &self.project_graph_hash,
            &self.analyzer_hash,
            &self.tool_hash,
            &self.source_hash,
            &self.diff_hash,
            &self.config_hash,
            &self.analysis_request_hash,
        ] {
            validate_hash(hash).map_err(invalid_string)?;
        }
        Ok(())
    }

    /// Build the only binding accepted for a registered static adapter.  The
    /// analyzer and tool identities come from the host capability; callers
    /// provide only the exact registered configuration and request hashes.
    pub fn for_registered_static(
        capability: &StaticCapabilityV1,
        project_graph_hash: String,
        source_hash: String,
        diff_hash: String,
        config_hash: String,
        analysis_request_hash: String,
    ) -> io::Result<Self> {
        capability.validate().map_err(verification_error)?;
        let analyzer_hash = content_hash(&serde_json::to_vec(capability)?);
        let value = Self {
            project_graph_hash,
            analyzer_hash,
            tool_hash: capability.executable_hash.clone(),
            source_hash,
            diff_hash,
            config_hash,
            analysis_request_hash,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRecordKindV1 {
    Need,
    Inventory,
    Candidates,
    StaticEvidence,
    JevDecision,
    PlanDecision,
    Review,
    Evidence,
}

/// A reference is only useful when its binding is still exact.  It contains
/// no absolute path and never grants enumeration authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationRecordRefV1 {
    pub kind: VerificationRecordKindV1,
    pub content_hash: String,
    pub binding: VerificationBindingV1,
}

impl VerificationRecordRefV1 {
    pub fn validate(&self) -> io::Result<()> {
        validate_hash(&self.content_hash).map_err(invalid_string)?;
        self.binding.validate()
    }
}

#[derive(Debug, Clone)]
pub struct VerificationStore {
    project_directory: PathBuf,
}

impl VerificationStore {
    #[cfg(unix)]
    pub fn open(data_root: &Path, project_id: &str) -> io::Result<Self> {
        if project_id.trim().is_empty() || project_id.contains('\0') {
            return Err(invalid("verification project IDが不正です"));
        }
        let root = data_root.join(STORE_DIRECTORY);
        ensure_private_directory(&root)?;
        let project_directory = root.join(content_hash(project_id.as_bytes()));
        ensure_private_directory(&project_directory)?;
        Ok(Self { project_directory })
    }

    #[cfg(not(unix))]
    pub fn open(_data_root: &Path, _project_id: &str) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "このplatformではverification保存の保護を確認できません",
        ))
    }

    pub fn put_need(
        &self,
        binding: &VerificationBindingV1,
        need: &VerificationNeedV1,
    ) -> io::Result<VerificationRecordRefV1> {
        need.validate().map_err(verification_error)?;
        self.put(VerificationRecordKindV1::Need, binding, need)
    }

    pub fn load_need(
        &self,
        reference: &VerificationRecordRefV1,
        expected: &VerificationBindingV1,
    ) -> io::Result<VerificationNeedV1> {
        let value: VerificationNeedV1 =
            self.load(VerificationRecordKindV1::Need, reference, expected)?;
        value.validate().map_err(verification_error)?;
        Ok(value)
    }

    pub fn put_inventory(
        &self,
        binding: &VerificationBindingV1,
        inventory: &TestInventoryV1,
    ) -> io::Result<VerificationRecordRefV1> {
        inventory.validate().map_err(verification_error)?;
        self.put(VerificationRecordKindV1::Inventory, binding, inventory)
    }

    pub fn load_inventory(
        &self,
        reference: &VerificationRecordRefV1,
        expected: &VerificationBindingV1,
    ) -> io::Result<TestInventoryV1> {
        let value: TestInventoryV1 =
            self.load(VerificationRecordKindV1::Inventory, reference, expected)?;
        value.validate().map_err(verification_error)?;
        Ok(value)
    }

    pub fn put_candidates(
        &self,
        binding: &VerificationBindingV1,
        inventory: &TestInventoryV1,
        candidates: &CandidateSetV1,
    ) -> io::Result<VerificationRecordRefV1> {
        inventory.validate().map_err(verification_error)?;
        candidates
            .validate_against(inventory)
            .map_err(verification_error)?;
        self.put(VerificationRecordKindV1::Candidates, binding, candidates)
    }

    pub fn load_candidates(
        &self,
        reference: &VerificationRecordRefV1,
        expected: &VerificationBindingV1,
        inventory: &TestInventoryV1,
    ) -> io::Result<CandidateSetV1> {
        inventory.validate().map_err(verification_error)?;
        let value: CandidateSetV1 =
            self.load(VerificationRecordKindV1::Candidates, reference, expected)?;
        value
            .validate_against(inventory)
            .map_err(verification_error)?;
        Ok(value)
    }

    /// Persist static evidence only after the caller supplies the registered
    /// analyzer capability.  This prevents a forged `analyzer_id` or tool hash
    /// from being made durable under a legitimate binding.
    pub(crate) fn put_static_evidence(
        &self,
        binding: &VerificationBindingV1,
        capability: &StaticCapabilityV1,
        evidence: &StaticEvidenceV1,
    ) -> io::Result<VerificationRecordRefV1> {
        validate_static_binding(binding, capability, evidence)?;
        self.put(VerificationRecordKindV1::StaticEvidence, binding, evidence)
    }

    /// Persist only evidence whose capability resolves from a host-owned
    /// catalog.  Production callers use this rather than accepting a
    /// capability object bundled with adapter output.
    pub fn put_registered_static_evidence(
        &self,
        binding: &VerificationBindingV1,
        catalog: &StaticCapabilityCatalogV1,
        evidence: &StaticEvidenceV1,
    ) -> io::Result<VerificationRecordRefV1> {
        let capability = catalog
            .resolve(&evidence.analyzer_id)
            .ok_or_else(|| invalid("static analyzer is not registered"))?;
        self.put_static_evidence(binding, capability, evidence)
    }

    pub fn load_static_evidence(
        &self,
        reference: &VerificationRecordRefV1,
        expected: &VerificationBindingV1,
        capability: &StaticCapabilityV1,
    ) -> io::Result<StaticEvidenceV1> {
        let value = self.load(
            VerificationRecordKindV1::StaticEvidence,
            reference,
            expected,
        )?;
        validate_static_binding(expected, capability, &value)?;
        Ok(value)
    }

    pub fn load_registered_static_evidence(
        &self,
        reference: &VerificationRecordRefV1,
        expected: &VerificationBindingV1,
        catalog: &StaticCapabilityCatalogV1,
    ) -> io::Result<StaticEvidenceV1> {
        let value: StaticEvidenceV1 = self.load(
            VerificationRecordKindV1::StaticEvidence,
            reference,
            expected,
        )?;
        let capability = catalog
            .resolve(&value.analyzer_id)
            .ok_or_else(|| invalid("static analyzer is not registered"))?;
        validate_static_binding(expected, capability, &value)?;
        Ok(value)
    }

    pub fn put_jev_decision(
        &self,
        binding: &VerificationBindingV1,
        inventory: &TestInventoryV1,
        candidates: &CandidateSetV1,
        decision: &TestDecisionRecordV1,
    ) -> io::Result<VerificationRecordRefV1> {
        validate_decision(binding, inventory, candidates, decision)?;
        self.put(VerificationRecordKindV1::JevDecision, binding, decision)
    }

    pub fn load_jev_decision(
        &self,
        reference: &VerificationRecordRefV1,
        expected: &VerificationBindingV1,
        inventory: &TestInventoryV1,
        candidates: &CandidateSetV1,
    ) -> io::Result<TestDecisionRecordV1> {
        let value = self.load(VerificationRecordKindV1::JevDecision, reference, expected)?;
        validate_decision(expected, inventory, candidates, &value)?;
        Ok(value)
    }

    pub fn put_plan_decision(
        &self,
        binding: &VerificationBindingV1,
        candidates: &[VerificationRouteCandidateV1],
        decision: &PlanDecisionV1,
    ) -> io::Result<VerificationRecordRefV1> {
        binding.validate()?;
        decision
            .validate_against(candidates)
            .map_err(verification_error)?;
        if decision.diff_hash != binding.diff_hash {
            return Err(invalid("plan decision bindingが一致しません"));
        }
        self.put(VerificationRecordKindV1::PlanDecision, binding, decision)
    }

    pub fn load_plan_decision(
        &self,
        reference: &VerificationRecordRefV1,
        expected: &VerificationBindingV1,
        candidates: &[VerificationRouteCandidateV1],
    ) -> io::Result<PlanDecisionV1> {
        let value: PlanDecisionV1 =
            self.load(VerificationRecordKindV1::PlanDecision, reference, expected)?;
        value
            .validate_against(candidates)
            .map_err(verification_error)?;
        if value.diff_hash != expected.diff_hash {
            return Err(invalid("plan decision bindingが一致しません"));
        }
        Ok(value)
    }

    /// Trusted-owner entry: a review is only stored when its exact workflow
    /// binding matches the current source/diff observed by this verification
    /// run. This is crate-visible so model/tool code cannot publish reviews.
    pub(crate) fn put_review_record(
        &self,
        verification_binding: &VerificationBindingV1,
        current: &EvidenceBindingV1,
        review: &ReviewRecordV1,
    ) -> io::Result<VerificationRecordRefV1> {
        verification_binding.validate()?;
        current.validate().map_err(invalid_string)?;
        review.validate().map_err(invalid_string)?;
        if review.binding != *current
            || current.source_hash != verification_binding.source_hash
            || current.diff_hash != verification_binding.diff_hash
        {
            return Err(invalid(
                "review record bindingがcurrent verificationと一致しません",
            ));
        }
        self.put(
            VerificationRecordKindV1::Review,
            verification_binding,
            review,
        )
    }

    pub(crate) fn load_review_record(
        &self,
        reference: &VerificationRecordRefV1,
        verification_binding: &VerificationBindingV1,
        current: &EvidenceBindingV1,
    ) -> io::Result<ReviewRecordV1> {
        verification_binding.validate()?;
        current.validate().map_err(invalid_string)?;
        let review: ReviewRecordV1 = self.load(
            VerificationRecordKindV1::Review,
            reference,
            verification_binding,
        )?;
        review.validate().map_err(invalid_string)?;
        if review.binding != *current
            || current.source_hash != verification_binding.source_hash
            || current.diff_hash != verification_binding.diff_hash
        {
            return Err(invalid(
                "review record bindingがcurrent verificationと一致しません",
            ));
        }
        Ok(review)
    }

    /// Final, owner-sealed evidence is retained as a private immutable record
    /// so a terminal checkpoint can be reconstructed after restart.  It is
    /// deliberately bound to the same observed source/diff as the runner.
    pub(crate) fn put_final_evidence_record(
        &self,
        verification_binding: &VerificationBindingV1,
        evidence: &EvidenceRecordV1,
    ) -> io::Result<VerificationRecordRefV1> {
        verification_binding.validate()?;
        evidence.validate().map_err(invalid_string)?;
        if evidence.binding.source_hash != verification_binding.source_hash
            || evidence.binding.diff_hash != verification_binding.diff_hash
        {
            return Err(invalid(
                "final evidence bindingがverification bindingと一致しません",
            ));
        }
        self.put(
            VerificationRecordKindV1::Evidence,
            verification_binding,
            evidence,
        )
    }

    pub(crate) fn load_final_evidence_record(
        &self,
        reference: &VerificationRecordRefV1,
        verification_binding: &VerificationBindingV1,
        current: &EvidenceBindingV1,
    ) -> io::Result<EvidenceRecordV1> {
        let evidence: EvidenceRecordV1 = self.load(
            VerificationRecordKindV1::Evidence,
            reference,
            verification_binding,
        )?;
        evidence.validate().map_err(invalid_string)?;
        if evidence.binding != *current
            || current.source_hash != verification_binding.source_hash
            || current.diff_hash != verification_binding.diff_hash
        {
            return Err(invalid(
                "final evidence bindingがcurrent verificationと一致しません",
            ));
        }
        Ok(evidence)
    }

    /// Applies an already-persisted Jev decision without any network access.
    /// Any missing, stale, corrupt, or invalid record becomes `All`, preserving
    /// the same fail-closed semantics as `select_tests_offline`.
    #[expect(
        clippy::too_many_arguments,
        reason = "selection inputs are explicit for deterministic offline replay"
    )]
    pub fn select_tests_offline(
        &self,
        decision: Option<&VerificationRecordRefV1>,
        expected: &VerificationBindingV1,
        inventory: &TestInventoryV1,
        candidates: &CandidateSetV1,
        question_hash: &str,
        cutoff: u8,
        all_scope: AllTestsScopeV1,
    ) -> TestSelectionV1 {
        let all = |reason| TestSelectionV1::All {
            scope: all_scope,
            reason,
        };
        if expected.validate().is_err() {
            return all(AllReasonV1::HashMismatch);
        }
        let record = match decision {
            Some(reference) => {
                match self.load_jev_decision(reference, expected, inventory, candidates) {
                    Ok(record) => Some(record),
                    Err(_) => return all(AllReasonV1::HashMismatch),
                }
            }
            None => None,
        };
        select_tests_offline(
            record.as_ref(),
            inventory,
            candidates,
            question_hash,
            &expected.diff_hash,
            cutoff,
            all_scope,
        )
    }

    fn put<T: Serialize + DeserializeOwned + PartialEq + Clone>(
        &self,
        kind: VerificationRecordKindV1,
        binding: &VerificationBindingV1,
        payload: &T,
    ) -> io::Result<VerificationRecordRefV1> {
        binding.validate()?;
        let payload_bytes = serde_json::to_vec(payload)?;
        let envelope = StoredRecordV1 {
            schema_version: SCHEMA_VERSION,
            kind,
            binding: binding.clone(),
            payload_hash: content_hash(&payload_bytes),
            payload: payload.clone(),
        };
        let bytes = serde_json::to_vec(&envelope)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(invalid("verification recordが上限を超えました"));
        }
        let content_hash = content_hash(&bytes);
        self.publish_exact(&content_hash, &bytes, &envelope)?;
        Ok(VerificationRecordRefV1 {
            kind,
            content_hash,
            binding: binding.clone(),
        })
    }

    fn load<T: DeserializeOwned + Serialize + PartialEq>(
        &self,
        kind: VerificationRecordKindV1,
        reference: &VerificationRecordRefV1,
        expected: &VerificationBindingV1,
    ) -> io::Result<T> {
        reference.validate()?;
        expected.validate()?;
        if reference.kind != kind || reference.binding != *expected {
            return Err(invalid("verification record bindingが一致しません"));
        }
        let bytes = read_private_regular_file(&self.object_path(&reference.content_hash)?)?;
        if content_hash(&bytes) != reference.content_hash {
            return Err(invalid("verification record content hashが一致しません"));
        }
        let envelope: StoredRecordV1<T> = serde_json::from_slice(&bytes)?;
        validate_envelope(&envelope, kind, expected)?;
        Ok(envelope.payload)
    }

    fn publish_exact<T: Serialize + DeserializeOwned + PartialEq>(
        &self,
        content_hash: &str,
        expected: &[u8],
        envelope: &StoredRecordV1<T>,
    ) -> io::Result<()> {
        let path = self.object_path(content_hash)?;
        if path.exists() {
            return self.check_existing(&path, expected, envelope);
        }
        let temporary = self.project_directory.join(format!(
            ".pending-{}-{}",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = private_create_new(&temporary)?;
        if let Err(error) = (|| -> io::Result<()> {
            file.write_all(expected)?;
            file.sync_all()?;
            Ok(())
        })() {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {
                fs::remove_file(&temporary)?;
                sync_directory(&self.project_directory)?;
                self.check_existing(&path, expected, envelope)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temporary);
                self.check_existing(&path, expected, envelope)
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    fn check_existing<T: Serialize + DeserializeOwned + PartialEq>(
        &self,
        path: &Path,
        expected: &[u8],
        envelope: &StoredRecordV1<T>,
    ) -> io::Result<()> {
        let bytes = read_private_regular_file(path)?;
        if bytes != expected {
            return Err(invalid("同じverification hashの本文が一致しません"));
        }
        let existing: StoredRecordV1<T> = serde_json::from_slice(&bytes)?;
        if &existing != envelope {
            return Err(invalid("保存済みverification recordが一致しません"));
        }
        Ok(())
    }

    fn object_path(&self, content_hash: &str) -> io::Result<PathBuf> {
        validate_hash(content_hash).map_err(invalid_string)?;
        Ok(self.project_directory.join(format!("{content_hash}.json")))
    }

    #[cfg(test)]
    fn object_path_for_test(&self, reference: &VerificationRecordRefV1) -> PathBuf {
        self.object_path(&reference.content_hash).unwrap()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecordV1<T> {
    schema_version: u32,
    kind: VerificationRecordKindV1,
    binding: VerificationBindingV1,
    payload_hash: String,
    payload: T,
}

fn validate_envelope<T: Serialize>(
    envelope: &StoredRecordV1<T>,
    kind: VerificationRecordKindV1,
    expected: &VerificationBindingV1,
) -> io::Result<()> {
    if envelope.schema_version != SCHEMA_VERSION
        || envelope.kind != kind
        || envelope.binding != *expected
        || content_hash(&serde_json::to_vec(&envelope.payload)?) != envelope.payload_hash
    {
        return Err(invalid("verification record envelopeが不正です"));
    }
    Ok(())
}

fn validate_static_binding(
    binding: &VerificationBindingV1,
    capability: &StaticCapabilityV1,
    evidence: &StaticEvidenceV1,
) -> io::Result<()> {
    binding.validate()?;
    evidence.validate().map_err(verification_error)?;
    capability.validate().map_err(verification_error)?;
    let capability_hash = content_hash(&serde_json::to_vec(capability)?);
    if capability.schema_version != SCHEMA_VERSION
        || binding.analyzer_hash != capability_hash
        || binding.tool_hash != capability.executable_hash
        || evidence.analyzer_id != capability.analyzer_id
        || evidence.tool_hash != binding.tool_hash
        || evidence.source_hash != binding.source_hash
        || evidence.diff_hash != binding.diff_hash
        || evidence.config_hash != binding.config_hash
        || static_analysis_request_hash(evidence) != binding.analysis_request_hash
        || evidence
            .project_graph_basis
            .as_ref()
            .is_some_and(|basis| basis.graph_hash != binding.project_graph_hash)
    {
        return Err(invalid("static evidence bindingが一致しません"));
    }
    Ok(())
}

fn validate_decision(
    binding: &VerificationBindingV1,
    inventory: &TestInventoryV1,
    candidates: &CandidateSetV1,
    decision: &TestDecisionRecordV1,
) -> io::Result<()> {
    binding.validate()?;
    inventory.validate().map_err(verification_error)?;
    candidates
        .validate_against(inventory)
        .map_err(verification_error)?;
    decision.validate().map_err(verification_error)?;
    if decision.diff_hash != binding.diff_hash
        || decision.inventory_hash != inventory.inventory_hash
        || decision.candidate_hash != stable_id_hash(&candidates.ids)
        || decision
            .answers
            .iter()
            .map(|answer| &answer.candidate_id)
            .collect::<std::collections::BTreeSet<_>>()
            != candidates.ids.iter().collect()
    {
        return Err(invalid("Jev decision bindingが一致しません"));
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    if path.exists() {
        return validate_private_directory(path);
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {
            sync_directory(
                path.parent()
                    .ok_or_else(|| invalid("verification保存先が不正です"))?,
            )?;
            validate_private_directory(path)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_private_directory(path)
        }
        Err(error) => Err(error),
    }
}

fn private_create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    validate_private_regular_file(path)?;
    Ok(file)
}

fn read_private_regular_file(path: &Path) -> io::Result<Vec<u8>> {
    validate_private_regular_file(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.len() > MAX_RECORD_BYTES as u64 {
        return Err(invalid("verification recordが上限を超えました"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_RECORD_BYTES as u64 {
        return Err(invalid("verification recordが上限を超えました"));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.read_to_end(&mut bytes)?;
    validate_private_regular_file(path)?;
    Ok(bytes)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid("verification保存先directoryが不正です"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(invalid("verification保存先の所有者または権限が不正です"));
        }
    }
    Ok(())
}

fn validate_private_regular_file(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid("verification recordが通常fileではありません"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(invalid(
                "verification recordの所有者、権限またはlink数が不正です",
            ));
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_string(message: String) -> io::Error {
    invalid(&message)
}

fn verification_error(error: VerificationError) -> io::Error {
    invalid(&error.to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn hash(value: &str) -> String {
        content_hash(value.as_bytes())
    }
    fn binding() -> VerificationBindingV1 {
        VerificationBindingV1 {
            project_graph_hash: hash("graph"),
            analyzer_hash: hash("analyzer"),
            tool_hash: hash("tool"),
            source_hash: hash("source"),
            diff_hash: hash("diff"),
            config_hash: hash("config"),
            analysis_request_hash: hash("request"),
        }
    }
    fn entry(id: &str) -> super::super::TestEntryV1 {
        super::super::TestEntryV1 {
            id: id.into(),
            framework: "rust".into(),
            package: "core".into(),
            target: "lib".into(),
            exact_name: format!("suite::{id}"),
            source_location: Some(super::super::SourceLocationV1 {
                path: "src/lib.rs".into(),
                line: 1,
            }),
            dynamic_or_unknown: false,
            cost: super::super::TestCostClassV1::Fast,
            symbols: BTreeSet::new(),
            common_case_ids: BTreeSet::new(),
        }
    }
    fn inventory() -> TestInventoryV1 {
        TestInventoryV1::complete_rust(vec![entry("one")]).unwrap()
    }
    fn candidates() -> CandidateSetV1 {
        CandidateSetV1 {
            ids: vec!["one".into()],
            mandatory_ids: BTreeSet::new(),
            inventory_complete: true,
        }
    }
    fn decision(
        inventory: &TestInventoryV1,
        candidates: &CandidateSetV1,
        binding: &VerificationBindingV1,
    ) -> TestDecisionRecordV1 {
        TestDecisionRecordV1 {
            schema_version: SCHEMA_VERSION,
            record_id: "decision-1".into(),
            provider: "jev".into(),
            model: "test".into(),
            question_hash: hash("question"),
            diff_hash: binding.diff_hash.clone(),
            inventory_hash: inventory.inventory_hash.clone(),
            candidate_hash: stable_id_hash(&candidates.ids),
            answers: vec![super::super::TestScoreAnswerV1 {
                candidate_id: "one".into(),
                score: 3.0,
                probabilities: [
                    ("0".into(), 0.0),
                    ("1".into(), 0.0),
                    ("2".into(), 0.0),
                    ("3".into(), 1.0),
                ]
                .into_iter()
                .collect(),
                confidence: Some(1.0),
            }],
            usage: Default::default(),
        }
    }

    fn capability(tool_hash: String) -> StaticCapabilityV1 {
        StaticCapabilityV1 {
            schema_version: SCHEMA_VERSION,
            analyzer_id: "analyzer-a".into(),
            executable_hash: tool_hash,
            version: "1".into(),
            output_schema_hash: hash("schema"),
            rules: vec![super::super::StaticRuleCapabilityV1 {
                rule_id: "rule-a".into(),
                obligation_kinds: ["boundary".into()].into_iter().collect(),
                maximum_scope: super::super::AnalysisScopeV1::File,
                clean_proves_satisfied: false,
                requires_project_graph: false,
            }],
        }
    }

    #[test]
    fn needs_and_static_evidence_are_content_addressed_and_capability_bound() {
        let root = tempfile::tempdir().unwrap();
        let mut binding = binding();
        let registered_capability = capability(binding.tool_hash.clone());
        binding.analyzer_hash = content_hash(&serde_json::to_vec(&registered_capability).unwrap());
        let store = VerificationStore::open(root.path(), "project-a").unwrap();
        let need = VerificationNeedV1 {
            schema_version: SCHEMA_VERSION,
            requirement_id: "requirement-a".into(),
            task_id: "task-a".into(),
            criterion_id: "criterion-a".into(),
            observable_failure: "a valid result is not persisted".into(),
            symbols: vec![],
            evidence_kinds: [super::super::EvidenceKindV1::Static].into_iter().collect(),
        };
        let need_ref = store.put_need(&binding, &need).unwrap();
        assert_eq!(store.load_need(&need_ref, &binding).unwrap(), need);
        let evidence = StaticEvidenceV1 {
            schema_version: SCHEMA_VERSION,
            evidence_id: "evidence-a".into(),
            obligation_id: "criterion-a".into(),
            obligation_kind: "boundary".into(),
            analyzer_id: registered_capability.analyzer_id.clone(),
            rule_id: "rule-a".into(),
            tool_hash: binding.tool_hash.clone(),
            config_hash: hash("config"),
            source_hash: binding.source_hash.clone(),
            diff_hash: binding.diff_hash.clone(),
            scope: super::super::AnalysisScopeV1::File,
            scope_completeness: super::super::ScopeCompletenessV1::Complete,
            project_graph_basis: None,
            state: super::super::StaticEvidenceStateV1::Unknown,
            findings: vec![],
            error_kind: None,
            duration_ms: None,
            cache_hit: None,
        };
        binding.analysis_request_hash = super::super::static_analysis_request_hash(&evidence);
        let reference = store
            .put_static_evidence(&binding, &registered_capability, &evidence)
            .unwrap();
        assert_eq!(
            store
                .load_static_evidence(&reference, &binding, &registered_capability)
                .unwrap(),
            evidence
        );
        let different_tool = capability(hash("other-tool"));
        assert!(
            store
                .load_static_evidence(&reference, &binding, &different_tool)
                .is_err()
        );
        let mut different_schema = registered_capability.clone();
        different_schema.output_schema_hash = hash("other-schema");
        assert!(
            store
                .load_static_evidence(&reference, &binding, &different_schema)
                .is_err()
        );

        let mut changed_config = binding.clone();
        changed_config.config_hash = hash("other-config");
        assert!(
            store
                .load_static_evidence(&reference, &changed_config, &registered_capability)
                .is_err()
        );
        let mut changed_scope = binding.clone();
        changed_scope.analysis_request_hash = hash("other-scope");
        assert!(
            store
                .load_static_evidence(&reference, &changed_scope, &registered_capability)
                .is_err()
        );
        let mut changed_graph = binding.clone();
        changed_graph.project_graph_hash = hash("other-graph");
        assert!(
            store
                .load_static_evidence(&reference, &changed_graph, &registered_capability)
                .is_err()
        );
    }

    #[test]
    fn records_survive_restart_and_bind_to_exact_hashes() {
        let root = tempfile::tempdir().unwrap();
        let binding = binding();
        let inventory = inventory();
        let candidates = candidates();
        let first = VerificationStore::open(root.path(), "project-a").unwrap();
        let inventory_ref = first.put_inventory(&binding, &inventory).unwrap();
        let candidates_ref = first
            .put_candidates(&binding, &inventory, &candidates)
            .unwrap();
        let decision_ref = first
            .put_jev_decision(
                &binding,
                &inventory,
                &candidates,
                &decision(&inventory, &candidates, &binding),
            )
            .unwrap();
        let reopened = VerificationStore::open(root.path(), "project-a").unwrap();
        assert_eq!(
            reopened.load_inventory(&inventory_ref, &binding).unwrap(),
            inventory
        );
        assert_eq!(
            reopened
                .load_candidates(&candidates_ref, &binding, &inventory)
                .unwrap(),
            candidates
        );
        assert!(
            matches!(reopened.select_tests_offline(Some(&decision_ref), &binding, &inventory, &candidates, &hash("question"), 2, AllTestsScopeV1::Workspace), TestSelectionV1::Exact { ids } if ids == ["one"])
        );
        let mut stale = binding.clone();
        stale.diff_hash = hash("other");
        assert!(reopened.load_inventory(&inventory_ref, &stale).is_err());
        assert!(matches!(
            reopened.select_tests_offline(
                Some(&decision_ref),
                &stale,
                &inventory,
                &candidates,
                &hash("question"),
                2,
                AllTestsScopeV1::Workspace
            ),
            TestSelectionV1::All {
                reason: AllReasonV1::HashMismatch,
                ..
            }
        ));
    }

    #[test]
    fn tamper_and_invalid_candidate_membership_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let binding = binding();
        let inventory = inventory();
        let candidates = candidates();
        let store = VerificationStore::open(root.path(), "project-a").unwrap();
        let reference = store.put_inventory(&binding, &inventory).unwrap();
        let path = store.object_path_for_test(&reference);
        fs::remove_file(&path).unwrap();
        let mut replacement = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        replacement.write_all(b"{}").unwrap();
        drop(replacement);
        assert!(store.load_inventory(&reference, &binding).is_err());
        assert!(matches!(
            store.select_tests_offline(
                None,
                &binding,
                &inventory,
                &candidates,
                &hash("question"),
                2,
                AllTestsScopeV1::Package
            ),
            TestSelectionV1::All {
                reason: AllReasonV1::MissingJudge,
                ..
            }
        ));
        let invalid = CandidateSetV1 {
            ids: vec!["unknown".into()],
            mandatory_ids: BTreeSet::new(),
            inventory_complete: true,
        };
        assert!(
            store
                .put_candidates(&binding, &inventory, &invalid)
                .is_err()
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests;
