//! Version-2 profile data. These definitions are loaded from bundled resources,
//! never from a project or model. An inert definition grants no effects.
use crate::{
    Artifact, Error,
    catalog::{AdapterId, ProfileId},
    is_digest,
    version::{Constraint, Release},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct ArtifactId(pub String);

impl ArtifactId {
    pub fn of(artifact: &Artifact) -> Self {
        Self(artifact.sha256.clone())
    }
}

macro_rules! id {
    ($name:ident) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
        #[serde(transparent)]
        pub struct $name(pub String);
    };
}
id!(CreatePathId);
id!(PublishTargetId);
id!(ReadInputId);
id!(PrivateStableId);
id!(TemplateId);
id!(StagedImageId);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProgramId {
    Node,
    NpmCli,
    Python,
    Pip,
    Cargo,
    Rustc,
    Go,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HostPrerequisiteId {
    CompilerDriver,
    Linker,
    MacSdk,
    ClangResources,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostPrerequisite {
    pub identity: crate::plan::TargetIdentity,
    /// File bytes, or the bounded inventory of a read-only SDK/resource tree.
    pub sha256: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Program {
    pub artifact: ArtifactId,
    pub runtime: PrivateStableId,
    pub relative: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    TarGzip,
    TarXz,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeComponent {
    pub artifact: ArtifactId,
    pub directory: PrivateStableId,
    pub archive: ArchiveFormat,
    pub strip_prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub path: CreatePathId,
    pub publish: PublishTargetId,
    pub relative: String,
    pub content: String,
    pub preserved_input: Option<ReadInputId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Tuple {
    pub version: String,
    pub artifacts: BTreeMap<ArtifactId, Artifact>,
    pub runtime: Vec<RuntimeComponent>,
    pub programs: BTreeMap<ProgramId, Program>,
    pub templates: BTreeMap<TemplateId, Template>,
    /// Fixed publication roots, such as node_modules or .venv. The owner
    /// creates stage identities/inventories before any project publication.
    pub installation_roots: BTreeMap<PublishTargetId, String>,
    pub build_outputs: BTreeMap<StagedImageId, String>,
    pub dependency_closure_sha256: String,
}

impl Tuple {
    /// A structured compatibility refusal, derived only from the locked
    /// primary package metadata. Ordinary process/build errors are not used.
    pub fn primary_runtime_compatible(&self, profile: ProfileId) -> Result<bool, Error> {
        let Some(package) = profile.primary_package() else {
            return Ok(true);
        };
        let lock = self
            .templates
            .values()
            .find(|template| template.relative == "package-lock.json")
            .ok_or(Error::InvalidProfile)?;
        let lock: serde_json::Value =
            serde_json::from_str(&lock.content).map_err(|_| Error::InvalidProfile)?;
        let primary = lock
            .get("packages")
            .and_then(|packages| packages.get(format!("node_modules/{package}")))
            .ok_or(Error::InvalidProfile)?;
        let Some(range) = primary
            .get("engines")
            .and_then(|engines| engines.get("node"))
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(true);
        };
        if range.len() > 256 {
            return Err(Error::InvalidProfile);
        }
        let version = self
            .programs
            .get(&ProgramId::Node)
            .ok_or(Error::InvalidProfile)?;
        let version =
            semver::Version::parse(&version.version).map_err(|_| Error::InvalidProfile)?;
        let alternatives: Vec<_> = range.split("||").collect();
        if alternatives.len() > 8 {
            return Err(Error::InvalidProfile);
        }
        let mut compatible = false;
        for alternative in alternatives {
            let requirement =
                semver::VersionReq::parse(alternative.trim()).map_err(|_| Error::InvalidProfile)?;
            compatible |= requirement.matches(&version);
        }
        Ok(compatible)
    }
    fn validate_npm(&self, profile: ProfileId) -> Result<(), Error> {
        let manifest = self
            .templates
            .values()
            .find(|template| template.relative == "package.json")
            .ok_or(Error::InvalidProfile)?;
        let lock = self
            .templates
            .values()
            .find(|template| template.relative == "package-lock.json")
            .ok_or(Error::InvalidProfile)?;
        let manifest: serde_json::Value =
            serde_json::from_str(&manifest.content).map_err(|_| Error::InvalidProfile)?;
        let lock: serde_json::Value =
            serde_json::from_str(&lock.content).map_err(|_| Error::InvalidProfile)?;
        if lock["lockfileVersion"] != 3
            || lock["packages"][""]["dependencies"] != manifest["dependencies"]
            || lock["packages"][""]["devDependencies"] != manifest["devDependencies"]
        {
            return Err(Error::InvalidProfile);
        }
        if let Some(package) = profile.primary_package()
            && manifest["dependencies"][package] != self.version
            && manifest["devDependencies"][package] != self.version
        {
            return Err(Error::InvalidProfile);
        }
        let runtime: BTreeSet<_> = self
            .runtime
            .iter()
            .map(|component| &component.artifact)
            .collect();
        let mut used = BTreeSet::new();
        let packages = lock["packages"].as_object().ok_or(Error::InvalidProfile)?;
        // npm ci validates even excluded optional-platform subtrees. Their
        // records must be complete although their native bodies are not fetched.
        for (path, record) in packages {
            for field in ["dependencies", "optionalDependencies"] {
                let Some(dependencies) = record.get(field) else {
                    continue;
                };
                for name in dependencies
                    .as_object()
                    .ok_or(Error::InvalidProfile)?
                    .keys()
                {
                    let mut ancestor = path.as_str();
                    loop {
                        let prefix = if ancestor.is_empty() {
                            String::new()
                        } else {
                            format!("{ancestor}/")
                        };
                        if packages.contains_key(&format!("{prefix}node_modules/{name}")) {
                            break;
                        }
                        if ancestor.is_empty() {
                            return Err(Error::InvalidProfile);
                        }
                        ancestor = ancestor
                            .rsplit_once("/node_modules/")
                            .map_or("", |(parent, _)| parent);
                    }
                }
            }
        }
        for (path, record) in packages {
            if path.is_empty() {
                continue;
            }
            if !relative(path)
                || !path.starts_with("node_modules/")
                || record["link"] == true
                || !record["integrity"]
                    .as_str()
                    .is_some_and(|value| value.starts_with("sha512-"))
            {
                return Err(Error::InvalidProfile);
            }
            let resolved = record["resolved"].as_str().ok_or(Error::InvalidProfile)?;
            if record["optional"] == true
                && (excluded_platform(&record["os"], "darwin")
                    || excluded_platform(&record["cpu"], "arm64"))
            {
                continue;
            }
            let (id, _) = self
                .artifacts
                .iter()
                .find(|(id, artifact)| {
                    !runtime.contains(id)
                        && artifact.url == resolved
                        && record["version"] == artifact.version
                })
                .ok_or(Error::InvalidProfile)?;
            used.insert(id);
        }
        if self
            .artifacts
            .keys()
            .filter(|id| !runtime.contains(id))
            .any(|id| !used.contains(id))
        {
            return Err(Error::InvalidProfile);
        }
        Ok(())
    }

    pub fn closure_digest(&self) -> Result<String, Error> {
        let runtime: BTreeSet<_> = self
            .runtime
            .iter()
            .map(|component| &component.artifact)
            .collect();
        let packages: BTreeMap<_, _> = self
            .artifacts
            .iter()
            .filter(|(id, _)| !runtime.contains(id))
            .collect();
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&packages).map_err(|_| Error::InvalidProfile)?)
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub artifact_bytes: u64,
    pub closure_bytes: u64,
    pub unpacked_bytes: u64,
    pub archive_entries: u32,
    pub processes: u32,
    pub seconds_per_process: u32,
    pub output_bytes_per_stream: u32,
    pub disk_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            artifact_bytes: 512 * 1024 * 1024,
            closure_bytes: 2 * 1024 * 1024 * 1024,
            unpacked_bytes: 8 * 1024 * 1024 * 1024,
            archive_entries: 100_000,
            processes: 128,
            seconds_per_process: 300,
            output_bytes_per_stream: 256 * 1024,
            disk_bytes: 8 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub schema: u32,
    pub revision: u32,
    pub profile: ProfileId,
    pub adapter: AdapterId,
    pub adapter_revision: u32,
    pub platform: String,
    pub origins: Vec<String>,
    pub redirect_origins: Vec<String>,
    pub limits: Limits,
    pub candidates: Vec<Tuple>,
    pub required_host: BTreeSet<HostPrerequisiteId>,
}

impl Definition {
    pub fn validate(&self) -> Result<(), Error> {
        let metadata = self.profile.entry();
        if self.schema != 2
            || self.revision == 0
            || self.adapter != metadata.adapter
            || self.adapter_revision != self.adapter.revision()
            || self.platform != metadata.platform
            || self.candidates.is_empty()
            || self.candidates.len() > 64
            || !self.limits.valid()
            || self.required_host != required_host(self.adapter)
            || self.origins.is_empty()
        {
            return Err(Error::InvalidProfile);
        }
        let allowed = allowed_origins(self.adapter);
        let mut origins = BTreeSet::new();
        for origin in self.origins.iter().chain(&self.redirect_origins) {
            if !allowed.contains(&origin.as_str()) || !origins.insert(origin) {
                return Err(Error::InvalidProfile);
            }
        }
        for candidate in &self.candidates {
            Release::exact(&candidate.version)?;
            candidate.primary_runtime_compatible(self.profile)?;
            if candidate.artifacts.is_empty()
                || candidate.artifacts.len() > 2048
                || candidate.programs.is_empty()
                || candidate.runtime.is_empty()
                || candidate.runtime.len() > 16
                || candidate.templates.len() > 64
                || !is_digest(&candidate.dependency_closure_sha256)
            {
                return Err(Error::InvalidProfile);
            }
            if candidate.closure_digest()? != candidate.dependency_closure_sha256 {
                return Err(Error::InvalidProfile);
            }
            let primary_program = match self.adapter {
                AdapterId::Npm => ProgramId::Node,
                AdapterId::Python => ProgramId::Python,
                AdapterId::Cargo => ProgramId::Rustc,
                AdapterId::Go => ProgramId::Go,
            };
            if let Some(package) = self.profile.primary_package() {
                if !candidate.artifacts.values().any(|artifact| {
                    artifact.name == package && artifact.version == candidate.version
                }) {
                    return Err(Error::InvalidProfile);
                }
            } else if candidate
                .programs
                .get(&primary_program)
                .is_none_or(|program| program.version != candidate.version)
            {
                return Err(Error::InvalidProfile);
            }
            if self.adapter == AdapterId::Npm {
                candidate.validate_npm(self.profile)?;
            }
            let mut bytes = 0u64;
            for (id, artifact) in &candidate.artifacts {
                if id != &ArtifactId::of(artifact) || !is_digest(&id.0) {
                    return Err(Error::InvalidProfile);
                }
                artifact.validate_for_origins(&self.origins, self.limits.artifact_bytes)?;
                bytes = bytes
                    .checked_add(artifact.bytes)
                    .ok_or(Error::InvalidProfile)?;
            }
            if bytes > self.limits.closure_bytes {
                return Err(Error::InvalidProfile);
            }
            let mut directories = BTreeSet::new();
            for component in &candidate.runtime {
                if !candidate.artifacts.contains_key(&component.artifact)
                    || !identifier(&component.directory.0)
                    || !relative(&component.strip_prefix)
                    || !directories.insert((&component.directory, &component.artifact))
                {
                    return Err(Error::InvalidProfile);
                }
            }
            for (id, program) in &candidate.programs {
                if !candidate.runtime.iter().any(|runtime| {
                    runtime.artifact == program.artifact && runtime.directory == program.runtime
                }) || !relative(&program.relative)
                    || program.version.is_empty()
                    || program.version.len() > 64
                {
                    return Err(Error::InvalidProfile);
                }
                if matches!(
                    id,
                    ProgramId::Node | ProgramId::Python | ProgramId::Rustc | ProgramId::Go
                ) && candidate
                    .artifacts
                    .get(&program.artifact)
                    .is_none_or(|artifact| artifact.version != program.version)
                {
                    return Err(Error::InvalidProfile);
                }
            }
            let mut targets = BTreeSet::new();
            let mut creates = BTreeSet::new();
            let mut publications = BTreeSet::new();
            for (id, template) in &candidate.templates {
                if !identifier(&id.0)
                    || !identifier(&template.path.0)
                    || !identifier(&template.publish.0)
                    || !relative(&template.relative)
                    || template.content.len() > 1024 * 1024
                    || !targets.insert(&template.relative)
                    || !creates.insert(&template.path)
                    || !publications.insert(&template.publish)
                    || template
                        .preserved_input
                        .as_ref()
                        .is_some_and(|id| !identifier(&id.0))
                {
                    return Err(Error::InvalidProfile);
                }
            }
            for (id, path) in &candidate.installation_roots {
                if !identifier(&id.0)
                    || !relative(path)
                    || !targets.insert(path)
                    || !publications.insert(id)
                {
                    return Err(Error::InvalidProfile);
                }
            }
            if candidate.build_outputs.len() > 16
                || candidate
                    .build_outputs
                    .iter()
                    .any(|(id, path)| !identifier(&id.0) || !relative(path))
                || (self.adapter == AdapterId::Npm && candidate.build_outputs.is_empty())
            {
                return Err(Error::InvalidProfile);
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, Error> {
        self.validate()?;
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(|_| Error::InvalidProfile)?)
        ))
    }

    pub fn candidates_for(
        &self,
        constraint: &Constraint,
        existing: bool,
    ) -> Result<Vec<usize>, Error> {
        self.validate()?;
        constraint.validate()?;
        if constraint.parser() != self.profile.entry().parser {
            return Err(Error::Unsupported);
        }
        let Some(first) = self.candidates.iter().position(|tuple| {
            Release::exact(&tuple.version).is_ok_and(|version| constraint.matches(version))
        }) else {
            return Err(Error::Unsupported);
        };
        let primary = Release::exact(&self.candidates[first].version)?;
        Ok(self
            .candidates
            .iter()
            .enumerate()
            .skip(first)
            .filter(|(_, tuple)| {
                Release::exact(&tuple.version).is_ok_and(|version| {
                    version == primary
                        || constraint.primary_fallback_allowed(primary, version, !existing)
                })
            })
            .map(|(index, _)| index)
            .take(3)
            .collect())
    }
}

impl Limits {
    fn valid(&self) -> bool {
        let maximum = Self::default();
        self.artifact_bytes > 0
            && self.artifact_bytes <= maximum.artifact_bytes
            && self.closure_bytes > 0
            && self.closure_bytes <= maximum.closure_bytes
            && self.unpacked_bytes > 0
            && self.unpacked_bytes <= maximum.unpacked_bytes
            && self.archive_entries > 0
            && self.archive_entries <= maximum.archive_entries
            && self.processes > 0
            && self.processes <= maximum.processes
            && self.seconds_per_process > 0
            && self.seconds_per_process <= maximum.seconds_per_process
            && self.output_bytes_per_stream > 0
            && self.output_bytes_per_stream <= maximum.output_bytes_per_stream
            && self.disk_bytes > 0
            && self.disk_bytes <= maximum.disk_bytes
    }
}

pub(crate) fn required_host(adapter: AdapterId) -> BTreeSet<HostPrerequisiteId> {
    if adapter == AdapterId::Cargo {
        [
            HostPrerequisiteId::CompilerDriver,
            HostPrerequisiteId::Linker,
            HostPrerequisiteId::MacSdk,
            HostPrerequisiteId::ClangResources,
        ]
        .into_iter()
        .collect()
    } else {
        BTreeSet::new()
    }
}

/// Compiled allowlists are not broadened by profile data. Individual profiles
/// must narrow these to the exact origins used by their pinned closure.
fn allowed_origins(adapter: AdapterId) -> &'static [&'static str] {
    match adapter {
        AdapterId::Npm => &["https://nodejs.org", "https://registry.npmjs.org"],
        AdapterId::Python => &[
            "https://github.com",
            "https://release-assets.githubusercontent.com",
            "https://www.python.org",
            "https://files.pythonhosted.org",
        ],
        AdapterId::Cargo => &["https://static.rust-lang.org", "https://static.crates.io"],
        AdapterId::Go => &[
            "https://go.dev",
            "https://dl.google.com",
            "https://proxy.golang.org",
            "https://storage.googleapis.com",
        ],
    }
}

pub(crate) fn relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && Path::new(value).as_os_str() == value
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub(crate) fn excluded_platform(value: &serde_json::Value, platform: &str) -> bool {
    let Some(values) = value.as_array() else {
        return false;
    };
    let Some(values) = values
        .iter()
        .map(|value| value.as_str())
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    values.iter().any(|value| *value == format!("!{platform}"))
        || (values.iter().any(|value| !value.starts_with('!')) && !values.contains(&platform))
}
