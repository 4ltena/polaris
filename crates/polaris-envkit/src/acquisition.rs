//! Shared schema-2 transfer capability. The management owner persists Started
//! before obtaining this capability and retains its project lease throughout.
use crate::{
    Artifact, Error,
    adapter::{self, ResolvedPlan, Snapshot},
    definition::Definition,
    plan::TargetIdentity,
    transfer::{TransferError, fetch_one},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Deliberately not serializable: tool arguments and saved plans are data,
/// never authorization to make an HTTP request.
pub struct Acquisition {
    index: usize,
    plan_sha256: String,
    private_root: TargetIdentity,
    artifacts: Vec<Artifact>,
    redirects: Vec<String>,
}

impl Acquisition {
    pub fn accepted(
        definition: &Definition,
        plan: &ResolvedPlan,
        fresh: &Snapshot,
        accepted_sha256: &str,
    ) -> Result<Self, Error> {
        adapter::revalidate(definition, plan, fresh)?;
        let digest = plan.digest(definition)?;
        if digest != accepted_sha256 {
            return Err(Error::StalePlan);
        }
        let candidate_index = plan.candidates.first().ok_or(Error::StalePlan)?.index;
        let tuple = definition
            .candidates
            .get(candidate_index)
            .ok_or(Error::StalePlan)?;
        Ok(Self {
            index: candidate_index,
            plan_sha256: digest,
            private_root: fresh.private_root.clone(),
            artifacts: tuple.artifacts.values().cloned().collect(),
            redirects: definition.redirect_origins.clone(),
        })
    }

    pub fn plan_sha256(&self) -> &str {
        &self.plan_sha256
    }

    pub fn candidate_index(&self) -> usize {
        self.index
    }

    /// Only a measured compatibility refusal can advance a candidate, and the
    /// transition is durable before a new transfer capability becomes usable.
    pub fn fallback(
        &self,
        journal: &mut crate::journal::Journal,
        definition: &Definition,
        plan: &ResolvedPlan,
        fresh: &Snapshot,
        failure: crate::Failure,
    ) -> Result<Option<Self>, Error> {
        adapter::revalidate(definition, plan, fresh)?;
        if self.plan_sha256 != plan.digest(definition)?
            || journal.progress().plan_sha256 != self.plan_sha256
            || journal.progress().candidate != self.index
            || journal.progress().stage != crate::Stage::Started
        {
            return Err(Error::StalePlan);
        }
        if !matches!(
            failure,
            crate::Failure::RuntimeIncompatible | crate::Failure::PrimaryIncompatible
        ) {
            return Ok(None);
        }
        let position = plan
            .candidates
            .iter()
            .position(|candidate| candidate.index == self.index)
            .ok_or(Error::StalePlan)?;
        let Some(next) = plan.candidates.get(position + 1) else {
            return Ok(None);
        };
        let before = &definition.candidates[self.index];
        let after = &definition.candidates[next.index];
        let allowed = match failure {
            crate::Failure::RuntimeIncompatible => {
                before.version == after.version
                    && before.dependency_closure_sha256 == after.dependency_closure_sha256
                    && before.templates == after.templates
                    && before.installation_roots == after.installation_roots
                    && before.build_outputs == after.build_outputs
            }
            crate::Failure::PrimaryIncompatible => {
                plan.normalized_version.primary_fallback_allowed(
                    crate::version::Release::exact(&before.version)?,
                    crate::version::Release::exact(&after.version)?,
                    !fresh.existing_project,
                )
            }
            _ => false,
        };
        if !allowed {
            return Ok(None);
        }
        journal.record_schema2_fallback(next.index, failure)?;
        Ok(Some(Self {
            index: next.index,
            plan_sha256: self.plan_sha256.clone(),
            private_root: self.private_root.clone(),
            artifacts: after.artifacts.values().cloned().collect(),
            redirects: self.redirects.clone(),
        }))
    }

    /// The cache must already exist as a private child of the operation root.
    /// No model-selected path, registry, proxy or credential is accepted.
    #[cfg(unix)]
    pub async fn fetch(
        &self,
        mut cancelled: impl FnMut() -> bool + Send,
    ) -> Result<Vec<PathBuf>, TransferError> {
        let root = Path::new(&self.private_root.canonical_path);
        let cache = root.join("artifacts");
        private_directory(root, Some(&self.private_root))?;
        let cache_identity = private_directory(&cache, None)?;
        let mut result = Vec::with_capacity(self.artifacts.len());
        for artifact in &self.artifacts {
            private_directory(root, Some(&self.private_root))?;
            private_directory(&cache, Some(&cache_identity))?;
            result.push(fetch_one(artifact, &self.redirects, &cache, &mut cancelled).await?);
        }
        private_directory(root, Some(&self.private_root))?;
        private_directory(&cache, Some(&cache_identity))?;
        Ok(result)
    }
}

#[cfg(unix)]
fn private_directory(
    path: &Path,
    expected: Option<&TargetIdentity>,
) -> Result<TargetIdentity, TransferError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = fs::symlink_metadata(path).map_err(|_| TransferError::Cache)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || path.canonicalize().map_err(|_| TransferError::Cache)? != path
    {
        return Err(TransferError::Cache);
    }
    let identity = TargetIdentity {
        canonical_path: path.to_str().ok_or(TransferError::Cache)?.into(),
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    if expected.is_some_and(|expected| expected != &identity) {
        return Err(Error::StalePlan.into());
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn exact_existing_project_only_changes_runtime_after_a_durable_typed_refusal() {
        use crate::{
            adapter::InputImage,
            catalog::ProfileId,
            definition::{ArtifactId, ProgramId},
        };
        use sha2::{Digest, Sha256};
        use std::{collections::BTreeMap, os::unix::fs::MetadataExt};
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let project = root.join("project");
        let private = root.join("private");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&private).unwrap();
        let identity = |path: &Path| {
            let metadata = fs::metadata(path).unwrap();
            TargetIdentity {
                canonical_path: path.to_str().unwrap().into(),
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        };
        let mut definition = crate::bundled::load(ProfileId::Nextjs).unwrap();
        let mut alternative = definition.candidates[0].clone();
        let original = alternative.runtime[0].artifact.clone();
        let mut artifact = alternative.artifacts.remove(&original).unwrap();
        artifact.version = "24.22.2".into();
        artifact.sha256 = "e".repeat(64);
        let changed = ArtifactId::of(&artifact);
        alternative.artifacts.insert(changed.clone(), artifact);
        alternative.runtime[0].artifact = changed.clone();
        for program in alternative.programs.values_mut() {
            if program.artifact == original {
                program.artifact = changed.clone();
            }
        }
        alternative
            .programs
            .get_mut(&ProgramId::Node)
            .unwrap()
            .version = "24.22.2".into();
        definition.candidates.push(alternative);
        let inputs = definition.candidates[0]
            .templates
            .values()
            .filter_map(|template| {
                template.preserved_input.clone().map(|id| {
                    (
                        id,
                        InputImage {
                            relative: template.relative.clone(),
                            sha256: format!("{:x}", Sha256::digest(template.content.as_bytes())),
                        },
                    )
                })
            })
            .collect();
        let snapshot = Snapshot {
            runner_binary_sha256: "a".repeat(64),
            host_os_revision: "d".repeat(64),
            target: identity(&project),
            private_root: identity(&private),
            platform: "darwin-arm64".into(),
            existing_project: true,
            inputs,
            installations: BTreeMap::new(),
            host_prerequisites: BTreeMap::new(),
        };
        let constraint = definition
            .profile
            .entry()
            .parser
            .parse(Some(&definition.candidates[0].version))
            .unwrap();
        let plan = adapter::resolve(&definition, &constraint, &snapshot).unwrap();
        assert_eq!(plan.candidates.len(), 2);
        let digest = plan.digest(&definition).unwrap();
        let first = Acquisition::accepted(&definition, &plan, &snapshot, &digest).unwrap();
        assert_eq!(first.candidate_index(), 0);
        let mut journal = crate::journal::Journal::create(&private, &digest, 0).unwrap();
        journal.advance(crate::Stage::Started, None).unwrap();
        assert!(
            first
                .fallback(
                    &mut journal,
                    &definition,
                    &plan,
                    &snapshot,
                    crate::Failure::Build
                )
                .unwrap()
                .is_none()
        );
        let next = first
            .fallback(
                &mut journal,
                &definition,
                &plan,
                &snapshot,
                crate::Failure::RuntimeIncompatible,
            )
            .unwrap()
            .unwrap();
        assert_eq!(next.candidate_index(), 1);
        assert_eq!(journal.progress().candidate, 1);
        drop(journal);
        let recovered = crate::journal::Journal::open(&private, &digest).unwrap();
        assert_eq!(recovered.progress().candidate, 1);
        // A process restart does not reconstruct transfer authority or replay.
        assert_ne!(
            Acquisition::accepted(&definition, &plan, &snapshot, &digest)
                .unwrap()
                .candidate_index(),
            recovered.progress().candidate
        );
    }
    #[cfg(unix)]
    #[test]
    fn cache_identity_replacement_and_public_permissions_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let original = private_directory(&root, None).unwrap();
        assert!(private_directory(&root, Some(&original)).is_ok());
        let mut stale = original;
        stale.inode += 1;
        assert!(private_directory(&root, Some(&stale)).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(&root, None).is_err());
    }
}
