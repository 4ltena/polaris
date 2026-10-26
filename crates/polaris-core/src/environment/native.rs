//! Read-only discovery of the existing Apple toolchain. No SDK or compiler is
//! installed and no global developer-directory or toolchain setting is changed.
use polaris_envkit::{
    catalog::ProfileId,
    definition::{HostPrerequisite, HostPrerequisiteId, Limits},
    plan::TargetIdentity,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub fn capture(
    profile: ProfileId,
) -> Result<BTreeMap<HostPrerequisiteId, HostPrerequisite>, String> {
    capture_with_cancel(profile, || false)
}

pub fn capture_with_cancel(
    profile: ProfileId,
    mut cancelled: impl FnMut() -> bool,
) -> Result<BTreeMap<HostPrerequisiteId, HostPrerequisite>, String> {
    if cancelled() {
        return Err("Native prerequisite capture cancelled".into());
    }
    if profile != ProfileId::Rust {
        return Ok(BTreeMap::new());
    }
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err("Rust environment setup requires macOS arm64".into());
    }
    let mut probe = NativeProbe::new()?;
    let compiler = physical(&probe.query(
        Path::new("/usr/bin/xcrun"),
        &["--sdk", "macosx", "--find", "clang"],
        &mut cancelled,
    )?)?;
    let linker = physical(&probe.query(
        Path::new("/usr/bin/xcrun"),
        &["--sdk", "macosx", "--find", "ld"],
        &mut cancelled,
    )?)?;
    let sdk = physical(&probe.query(
        Path::new("/usr/bin/xcrun"),
        &["--sdk", "macosx", "--show-sdk-path"],
        &mut cancelled,
    )?)?;
    let resources = physical(&probe.query(&compiler, &["-print-resource-dir"], &mut cancelled)?)?;
    let compiler_version = probe
        .query(&compiler, &["--version"], &mut cancelled)?
        .lines()
        .next()
        .ok_or("Missing compiler version")?
        .to_owned();
    let linker_version = probe
        .query(&linker, &["-v"], &mut cancelled)?
        .lines()
        .next()
        .ok_or("Missing linker version")?
        .to_owned();
    let sdk_version = probe.query(
        Path::new("/usr/bin/xcrun"),
        &["--sdk", "macosx", "--show-sdk-version"],
        &mut cancelled,
    )?;
    let mut result = BTreeMap::new();
    for (id, path, version) in [
        (
            HostPrerequisiteId::CompilerDriver,
            compiler,
            compiler_version.clone(),
        ),
        (HostPrerequisiteId::Linker, linker, linker_version),
        (HostPrerequisiteId::MacSdk, sdk, sdk_version),
        (
            HostPrerequisiteId::ClangResources,
            resources,
            compiler_version,
        ),
    ] {
        if cancelled() {
            return Err("Native prerequisite capture cancelled".into());
        }
        use std::os::unix::fs::MetadataExt;
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| "Cannot inspect the native toolchain")?;
        let inventory = polaris_envkit::publication::inspect_read_only(&path, &Limits::default())
            .map_err(|_| "Cannot bind the existing native toolchain inventory")?;
        result.insert(
            id,
            HostPrerequisite {
                identity: TargetIdentity {
                    canonical_path: path.to_str().ok_or("Invalid native toolchain path")?.into(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
                sha256: inventory
                    .digest()
                    .map_err(|_| "Cannot bind the native toolchain")?,
                version,
            },
        );
    }
    Ok(result)
}

/// Revalidate pinned bytes and identities without executing discovery again.
/// Unchanged compiler/SDK bytes retain their measured version; a changed
/// input stops the plan instead of silently selecting another toolchain.
pub fn refresh(
    expected: &BTreeMap<HostPrerequisiteId, HostPrerequisite>,
    mut cancelled: impl FnMut() -> bool,
) -> Result<BTreeMap<HostPrerequisiteId, HostPrerequisite>, String> {
    use std::os::unix::fs::MetadataExt;
    for prerequisite in expected.values() {
        if cancelled() {
            return Err("Native prerequisite validation cancelled".into());
        }
        let path = Path::new(&prerequisite.identity.canonical_path);
        let metadata =
            fs::symlink_metadata(path).map_err(|_| "Native prerequisite is unavailable")?;
        if (metadata.dev(), metadata.ino())
            != (prerequisite.identity.device, prerequisite.identity.inode)
            || metadata.file_type().is_symlink()
            || polaris_envkit::publication::inspect_read_only(path, &Limits::default())
                .and_then(|image| image.digest())
                .map_err(|_| "Native prerequisite inventory changed")?
                != prerequisite.sha256
        {
            return Err("Native prerequisite identity changed".into());
        }
    }
    Ok(expected.clone())
}

fn physical(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if !path.is_absolute() || value.chars().any(char::is_control) {
        return Err("Invalid native toolchain path".into());
    }
    path.canonicalize()
        .map_err(|_| "The existing Apple toolchain is unavailable".into())
}

struct NativeProbe {
    _temporary: tempfile::TempDir,
    workspace: PathBuf,
    developer: PathBuf,
    policy: polaris_sandbox::SandboxPolicy,
    pending: Vec<polaris_sandbox::PendingCleanup>,
}
impl NativeProbe {
    fn new() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        let selection = std::env::var_os("POLARIS_ENVKIT_APPLE_TOOLCHAIN");
        let selected = select_developer(selection.as_deref(), || {
            fs::read_link("/private/var/db/xcode_select_link")
        })?;
        let developer = selected
            .canonicalize()
            .map_err(|_| "The existing Apple toolchain is unavailable")?;
        if developer.parent().is_none() || !developer.is_dir() {
            return Err("Invalid Apple developer directory".into());
        }
        let temporary = tempfile::Builder::new()
            .prefix("polaris-native-probe-")
            .tempdir()
            .map_err(|_| "Cannot prepare native probe scratch")?;
        let root = temporary
            .path()
            .canonicalize()
            .map_err(|_| "Cannot bind native probe scratch")?;
        let workspace = root.join("workspace");
        let scratch = root.join("scratch");
        let home = scratch.join("home");
        let tmp = scratch.join("tmp");
        for path in [&workspace, &scratch, &home, &tmp] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|_| "Cannot prepare native probe scratch")?;
        }
        let mut readable = vec![developer.clone()];
        // xcrun consults the selected Xcode bundle's Info.plist and invokes
        // xcodebuild, whose bundled frameworks live beside Developer.
        if developer
            .file_name()
            .is_some_and(|name| name == "Developer")
            && let Some(contents) = developer.parent()
            && contents.file_name().is_some_and(|name| name == "Contents")
            && contents.parent().is_some_and(|bundle| {
                bundle
                    .extension()
                    .is_some_and(|extension| extension == "app")
            })
        {
            readable.push(contents.to_path_buf());
        }
        for path in ["/bin", "/usr/bin", "/usr/lib", "/System/Library"] {
            readable.push(
                Path::new(path)
                    .canonicalize()
                    .map_err(|_| "Cannot bind native system roots")?,
            );
        }
        let policy = polaris_sandbox::SandboxPolicy::isolated_registered(
            polaris_sandbox::SandboxMode::ReadOnly,
            &workspace,
            &readable,
        )
        .and_then(|policy| policy.with_isolated_private_environment(&scratch, &home, &tmp))
        .and_then(|policy| {
            policy.with_isolated_fixed_values(BTreeMap::from([(
                "DEVELOPER_DIR".into(),
                developer.to_string_lossy().into_owned(),
            )]))
        })
        .map_err(|_| "Native probe isolation is unavailable")?;
        Ok(Self {
            _temporary: temporary,
            workspace,
            developer,
            policy,
            pending: Vec::new(),
        })
    }

    fn query(
        &mut self,
        program: &Path,
        arguments: &[&str],
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<String, String> {
        use polaris_sandbox::ControlledEnd;
        let program = program
            .canonicalize()
            .map_err(|_| "Native probe executable is unavailable")?;
        if !program.starts_with(&self.developer) && program != Path::new("/usr/bin/xcrun") {
            return Err("Native probe exceeds the selected toolchain scope".into());
        }
        let before =
            fs::symlink_metadata(&program).map_err(|_| "Cannot bind native probe executable")?;
        let started = std::time::Instant::now();
        let mut stopped = false;
        let mut outcome = polaris_sandbox::run_confined_controlled_in_dir(
            &self.policy,
            &program,
            &arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>(),
            None,
            Some(&self.workspace),
            || {
                stopped |= cancelled() || started.elapsed() >= std::time::Duration::from_secs(15);
                stopped
            },
        )
        .map_err(|_| "Native probe isolation is unavailable")?;
        if let Some(pending) = outcome.pending.take() {
            self.pending.push(pending);
        }
        // Retain the owner and its scratch until ordinary process-group cleanup
        // is confirmed. A stopped or uncertain child is never reported ready.
        while !self.pending.is_empty() {
            let mut index = 0;
            while index < self.pending.len() {
                match self.pending[index].try_stop_and_reap() {
                    Ok(Some(_)) => {
                        drop(self.pending.swap_remove(index));
                    }
                    _ => index += 1,
                }
            }
            if !self.pending.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        if stopped || outcome.end != ControlledEnd::Exited {
            return Err("Native prerequisite probe cancelled or timed out".into());
        }
        if !polaris_tools::path_policy::tree_admission::unchanged(
            &before,
            &fs::symlink_metadata(&program).map_err(|_| "Native executable changed")?,
        ) || !outcome.status.is_some_and(|status| status.success())
            || outcome.problem.is_some()
            || outcome.stdout_truncated
            || outcome.stderr_truncated
            || !outcome.stdout_eof
            || !outcome.stderr_eof
            || outcome.stdout.len() > 4096
            || outcome.stderr.len() > 4096
        {
            let detail: String = outcome
                .stderr
                .chars()
                .filter(|character| !character.is_control() || *character == '\n')
                .take(1024)
                .collect();
            return Err(format!(
                "Cannot query the existing Apple toolchain ({}): {}",
                program.display(),
                detail.trim()
            ));
        }
        let text = if outcome.stdout.is_empty() {
            outcome.stderr
        } else {
            outcome.stdout
        };
        let text = text.trim();
        if text.is_empty()
            || text.len() > 1024
            || text
                .chars()
                .any(|character| character.is_control() && character != '\n')
        {
            return Err("Invalid Apple toolchain response".into());
        }
        Ok(text.to_owned())
    }
}

/// Explicit finite selection; never retry a failed Xcode probe with another SDK.
fn select_developer(
    selection: Option<&std::ffi::OsStr>,
    selected: impl FnOnce() -> std::io::Result<PathBuf>,
) -> Result<PathBuf, String> {
    match selection {
        Some(value) if value == "command-line-tools" => {
            Ok(PathBuf::from("/Library/Developer/CommandLineTools"))
        }
        Some(_) => {
            Err("Invalid POLARIS_ENVKIT_APPLE_TOOLCHAIN; use command-line-tools or unset it".into())
        }
        None => match selected() {
            Ok(path) => Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(PathBuf::from("/Library/Developer/CommandLineTools"))
            }
            Err(_) => Err("Cannot inspect the selected Apple developer directory".into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_clt_does_not_consult_or_modify_global_selection() {
        assert_eq!(
            select_developer(Some(std::ffi::OsStr::new("command-line-tools")), || {
                panic!("explicit selection must not read the global selector")
            })
            .unwrap(),
            PathBuf::from("/Library/Developer/CommandLineTools")
        );
        assert_eq!(
            select_developer(None, || Ok(PathBuf::from(
                "/Applications/Xcode.app/Contents/Developer"
            )))
            .unwrap(),
            PathBuf::from("/Applications/Xcode.app/Contents/Developer")
        );
        assert!(
            select_developer(Some(std::ffi::OsStr::new("/tmp/custom")), || {
                panic!("invalid selection must stop")
            })
            .is_err()
        );
        assert!(
            select_developer(None, || Err(std::io::ErrorKind::PermissionDenied.into())).is_err()
        );
    }
}
