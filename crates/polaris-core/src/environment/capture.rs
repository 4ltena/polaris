//! Admit project sources and dependency inventories under protected-path checks.
use polaris_envkit::{
    adapter::InputImage,
    definition::{Definition, ReadInputId},
};
use polaris_tools::path_policy::tree_admission::{
    self as admission, Exclusion, Limits, ObservedTreeEntry, RegisteredSecrets, TreeWalker,
};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

pub struct SourceImage {
    pub target: polaris_envkit::plan::TargetIdentity,
    pub inputs: BTreeMap<ReadInputId, InputImage>,
    pub entries: Vec<ObservedTreeEntry>,
    pub exclusions: Vec<Exclusion>,
}

/// Existing dependency trees are read-only CAS inputs. Reject protected names
/// and registered identities before hashing any file, including renamed aliases.
pub fn installations(
    definition: &Definition,
    source: &Path,
) -> Result<BTreeMap<String, String>, String> {
    installations_with_postimages(definition, source, &BTreeMap::new())
}

/// Newly published images already have a journal-bound inventory captured in
/// the isolated stage. Admit only those exact entries; registered credentials
/// remain forbidden before their bodies are opened.
pub(super) fn installations_with_postimages(
    definition: &Definition,
    source: &Path,
    published: &BTreeMap<String, polaris_envkit::publication::Inventory>,
) -> Result<BTreeMap<String, String>, String> {
    polaris_auth::protection::with_protected_paths(|registered| {
        let secrets =
            RegisteredSecrets::from_auth_registry(registered).map_err(|error| error.to_string())?;
        let paths: std::collections::BTreeSet<_> = definition
            .candidates
            .iter()
            .flat_map(|tuple| tuple.installation_roots.values())
            .collect();
        let mut images = BTreeMap::new();
        for relative in paths {
            let path = source.join(relative);
            let expected = published.get(relative);
            let known_paths: std::collections::BTreeSet<_> = expected
                .into_iter()
                .flat_map(|image| &image.entries)
                .map(|entry| match entry {
                    polaris_envkit::publication::ImageEntry::Directory { path }
                    | polaris_envkit::publication::ImageEntry::File { path, .. }
                    | polaris_envkit::publication::ImageEntry::Link { path, .. } => Path::new(path),
                })
                .collect();
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err("Cannot inspect the existing environment installation".into()),
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                _ => {
                    return Err(
                        "Existing environment installation is not a physical directory".into(),
                    );
                }
            }
            let image = polaris_envkit::publication::inspect_admitted(
                &path,
                &definition.limits,
                &|path, metadata| {
                    let known = path
                        .strip_prefix(source.join(relative))
                        .is_ok_and(|suffix| known_paths.contains(suffix));
                    (!polaris_tools::path_policy::is_denied(path) || known)
                        && !secrets.reserves(path)
                        && metadata.is_none_or(|metadata| !secrets.denies(path, metadata))
                },
            )
            .map_err(|_| {
                "Existing environment installation cannot be safely inventoried".to_string()
            })?;
            if expected.is_some_and(|expected| expected != &image) {
                return Err("Published environment installation changed".into());
            }
            images.insert(
                relative.clone(),
                image.digest().map_err(|error| error.to_string())?,
            );
        }
        Ok(images)
    })
    .map_err(|_| "Authentication protection metadata is unavailable".to_string())?
}

/// Read-only consent capture. Registered authentication stores are excluded
/// while their metadata registry lock is held; no credential bodies are read.
pub fn inspect(definition: &Definition, source: &Path) -> admission::Result<SourceImage> {
    protected_walk(definition, source, None)
}

/// Copy to the operation owner's already-created private stage. This method
/// never owns or removes the destination; unknown outcomes remain recoverable.
/// The caller retains the project lease and compares the resulting image with
/// its approved read-only capture before starting an offline child.
pub fn copy(
    definition: &Definition,
    plan: &polaris_envkit::adapter::ResolvedPlan,
    fresh: &polaris_envkit::adapter::Snapshot,
    candidate: usize,
    stage_identity: &polaris_envkit::plan::TargetIdentity,
) -> admission::Result<SourceImage> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let source = Path::new(&plan.snapshot.target.canonical_path);
    let failed = || admission::error(source, admission::ErrorKind::SourceChanged);
    polaris_envkit::adapter::revalidate(definition, plan, fresh).map_err(|_| failed())?;
    let layout =
        polaris_envkit::invocation::Layout::for_plan(plan, candidate).map_err(|_| failed())?;
    let stage = layout.stage();
    if stage_identity.canonical_path != stage.to_str().ok_or_else(failed)? {
        return Err(failed());
    }
    verify_identity(&plan.snapshot.target)?;
    verify_identity(&plan.snapshot.private_root)?;
    verify_identity(stage_identity)?;
    let (directory, _) = admission::open_root_with_ancestors(&stage)?;
    let metadata = admission::io(&stage, directory.metadata())?;
    if stage.starts_with(source)
        || source.starts_with(&stage)
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || admission::io(&stage, std::fs::read_dir(&stage))?
            .next()
            .is_some()
    {
        return Err(failed());
    }
    let before = protected_walk(definition, source, None)?;
    if before.target != plan.snapshot.target || before.inputs != plan.snapshot.inputs {
        return Err(failed());
    }
    let image = protected_walk(definition, source, Some(&stage))?;
    verify_identity(&plan.snapshot.target)?;
    verify_identity(&plan.snapshot.private_root)?;
    verify_identity(stage_identity)?;
    if image.target != plan.snapshot.target || image.inputs != plan.snapshot.inputs {
        return Err(failed());
    }
    Ok(image)
}

fn verify_identity(expected: &polaris_envkit::plan::TargetIdentity) -> admission::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let path = Path::new(&expected.canonical_path);
    let (opened, _) = admission::open_root_with_ancestors(path)?;
    let metadata = admission::io(path, opened.metadata())?;
    if (metadata.dev(), metadata.ino()) != (expected.device, expected.inode) {
        return Err(admission::error(path, admission::ErrorKind::SourceChanged));
    }
    Ok(())
}

fn protected_walk(
    definition: &Definition,
    source: &Path,
    destination: Option<&Path>,
) -> admission::Result<SourceImage> {
    definition
        .validate()
        .map_err(|_| admission::error(source, admission::ErrorKind::InvalidPath))?;
    polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered)?;
        walk(definition, source, destination, &secrets)
    })
    .map_err(|_| admission::error(source, admission::ErrorKind::ProtectionUnavailable))?
}

fn walk(
    definition: &Definition,
    source: &Path,
    destination: Option<&Path>,
    secrets: &RegisteredSecrets,
) -> admission::Result<SourceImage> {
    let (root, ancestors) = admission::open_root_with_ancestors(source)?;
    use std::os::unix::fs::MetadataExt;
    let metadata = admission::io(source, root.metadata())?;
    let target = polaris_envkit::plan::TargetIdentity {
        canonical_path: source
            .to_str()
            .ok_or_else(|| admission::error(source, admission::ErrorKind::InvalidPath))?
            .into(),
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    let mut walker = TreeWalker {
        secrets,
        private_owner: None,
        limits: Limits {
            max_entries: 20_000,
            max_files: 10_000,
            max_file_bytes: 16 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_depth: 64,
        },
        entries: 0,
        files: 0,
        bytes: 0,
        root_path: source,
        policy_root: source,
        destination,
        selection: None,
        manifest: Vec::new(),
        exclusions: Vec::new(),
        deadline: Some(Instant::now() + Duration::from_secs(30)),
        exclude_build_caches: true,
    };
    walker.walk(&root, Path::new(""), 0)?;
    let (_, fresh) = admission::open_root_with_ancestors(source)?;
    if ancestors != fresh {
        return Err(admission::error(
            source,
            admission::ErrorKind::SourceChanged,
        ));
    }
    // Walk order depends on the filesystem. IDs are deterministic and the
    // manifest/lock IDs match the fixed template's preserved-input IDs.
    walker
        .manifest
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    let mut inputs = BTreeMap::new();
    for entry in &walker.manifest {
        let Some(hash) = entry.sha256 else { continue };
        let relative = entry
            .relative_path
            .to_str()
            .ok_or_else(|| admission::error(source, admission::ErrorKind::InvalidPath))?;
        let preserved: std::collections::BTreeSet<_> = definition
            .candidates
            .iter()
            .flat_map(|tuple| tuple.templates.values())
            .filter(|template| template.relative == relative)
            .filter_map(|template| template.preserved_input.clone())
            .collect();
        if preserved.len() > 1 {
            return Err(admission::error(source, admission::ErrorKind::InvalidPath));
        }
        let id = preserved
            .into_iter()
            .next()
            .unwrap_or_else(|| ReadInputId(format!("source_{}", inputs.len())));
        let image = InputImage {
            relative: relative.into(),
            sha256: hash.iter().map(|byte| format!("{byte:02x}")).collect(),
        };
        if inputs.insert(id, image).is_some() {
            return Err(admission::error(source, admission::ErrorKind::InvalidPath));
        }
    }
    Ok(SourceImage {
        target,
        inputs,
        entries: walker.manifest,
        exclusions: walker.exclusions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_envkit::{bundled, catalog::ProfileId};
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn generated_installation_requires_exact_prior_inventory_for_protected_names() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().canonicalize().unwrap();
        let venv = source.join(".venv");
        fs::create_dir(&venv).unwrap();
        fs::write(venv.join("cacert.pem"), "synthetic public bundle").unwrap();
        let definition = bundled::load(ProfileId::Python).unwrap();
        let image = polaris_envkit::publication::inspect(&venv, &definition.limits).unwrap();
        let published = BTreeMap::from([(".venv".into(), image)]);
        assert!(installations(&definition, &source).is_err());
        assert!(installations_with_postimages(&definition, &source, &published).is_ok());
        fs::write(venv.join("cacert.pem"), "changed").unwrap();
        assert!(installations_with_postimages(&definition, &source, &published).is_err());
        fs::write(venv.join("cacert.pem"), "synthetic public bundle").unwrap();
        fs::write(venv.join(".env"), "SYNTHETIC_ONLY").unwrap();
        assert!(installations_with_postimages(&definition, &source, &published).is_err());
    }

    #[test]
    fn source_capture_preserves_profile_ids_and_omits_secrets_links_and_build_cache() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().canonicalize().unwrap();
        fs::write(
            source.join("requirements.lock"),
            "# no third-party packages\n",
        )
        .unwrap();
        fs::write(source.join("main.py"), "print('envkit ready')\n").unwrap();
        fs::write(source.join(".env.local"), "SYNTHETIC_ONLY=excluded\n").unwrap();
        fs::write(source.join(".npmrc"), "synthetic-only\n").unwrap();
        fs::write(source.join("registered-private"), "synthetic-only\n").unwrap();
        fs::create_dir(source.join("node_modules")).unwrap();
        fs::write(source.join("node_modules/ignored"), "cache\n").unwrap();
        fs::create_dir(source.join(".venv")).unwrap();
        fs::write(source.join(".venv/pyvenv.cfg"), "synthetic runtime\n").unwrap();
        symlink("main.py", source.join("alias.py")).unwrap();
        let secrets = RegisteredSecrets::new([source.join("registered-private")]).unwrap();
        let definition = bundled::load(ProfileId::Python).unwrap();
        let first = walk(&definition, &source, None, &secrets).unwrap();
        assert_eq!(first.inputs.len(), 2);
        assert_eq!(
            first.inputs[&ReadInputId("requirements".into())].relative,
            "requirements.lock"
        );
        let second = walk(&definition, &source, None, &secrets).unwrap();
        assert_eq!(first.inputs, second.inputs);
        assert!(
            first
                .exclusions
                .iter()
                .any(|entry| entry.relative_path == Path::new(".env.local"))
        );
        fs::write(source.join("main.py"), "print('changed')\n").unwrap();
        assert_ne!(
            first.inputs,
            walk(&definition, &source, None, &secrets).unwrap().inputs
        );
    }
}
