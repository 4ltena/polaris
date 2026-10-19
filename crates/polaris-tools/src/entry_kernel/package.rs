//! Closed inventory of the actual macOS package used for migration admission.
use super::*;
use std::ffi::CStr;

const PACKAGE_MANIFEST: &str = "mcp-entry-binaries-v2.json";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageManifest {
    protocol_version: u32,
    entries: Vec<PackageEntry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageEntry {
    kind: ProductionEntryKindV2,
    relative_path: String,
    sha256: String,
}

impl ProductionEntryManifestV2 {
    /// Converts copied-byte package evidence into live physical identities.
    /// It neither creates a gate nor authorizes a protocol activation.
    pub fn from_macos_package(app: &Path) -> Result<Self, EntryGateErrorV2> {
        let paths = [
            app.to_owned(),
            app.join("Contents"),
            app.join("Contents/MacOS"),
            app.join("Contents/Helpers"),
            app.join("Contents/Resources"),
        ];
        let directories: Vec<_> = paths
            .iter()
            .map(|path| {
                let file = physical_directory(path)?;
                owned_directory(&file, false)?;
                let before = file.metadata().map_err(storage)?;
                Ok((file, before))
            })
            .collect::<Result<_, EntryGateErrorV2>>()?;
        let resources = &directories[4].0;
        let mut file = child(resources, PACKAGE_MANIFEST, libc::O_RDONLY).map_err(storage)?;
        let before = file.metadata().map_err(storage)?;
        if !before.is_file()
            || before.nlink() != 1
            || before.uid() != unsafe { libc::geteuid() }
            || before.mode() & 0o7022 != 0
            || before.len() > MAX_GATE_BYTES
        {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_GATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(storage)?;
        let manifest: PackageManifest = decode(&bytes)?;
        if manifest.protocol_version != 2 || !(3..=4).contains(&manifest.entries.len()) {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let mut expected = BTreeSet::new();
        let mut entries = Vec::new();
        for entry in manifest.entries {
            let relative = match entry.kind {
                ProductionEntryKindV2::Cli => "Contents/Helpers/polaris",
                ProductionEntryKindV2::DesktopApp => "Contents/MacOS/PolarisDesktop",
                ProductionEntryKindV2::DesktopService => "Contents/Helpers/polaris-desktop-service",
                ProductionEntryKindV2::ExecutionHelper => {
                    "Contents/Helpers/polaris-execution-helper"
                }
            };
            if entry.relative_path != relative
                || !valid_digest(&entry.sha256)
                || !expected.insert(relative.to_owned())
            {
                return Err(EntryGateErrorV2::InvalidGate);
            }
            let binary = ProductionBinaryIdentityV2::inspect(&app.join(relative))?;
            if binary.sha256 != entry.sha256 {
                return Err(EntryGateErrorV2::Identity);
            }
            entries.push(ProductionEntryIdentityV2 {
                kind: entry.kind,
                binary,
            });
        }
        if ![
            "Contents/Helpers/polaris",
            "Contents/MacOS/PolarisDesktop",
            "Contents/Helpers/polaris-desktop-service",
        ]
        .iter()
        .all(|path| expected.contains(*path))
        {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let mut actual = BTreeSet::new();
        for (index, prefix) in [(2, "Contents/MacOS"), (3, "Contents/Helpers")] {
            for name in directory_names(&directories[index].0)? {
                actual.insert(format!("{prefix}/{name}"));
            }
        }
        if actual != expected {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let after = file.metadata().map_err(storage)?;
        let named = child(resources, PACKAGE_MANIFEST, libc::O_RDONLY)
            .map_err(storage)?
            .metadata()
            .map_err(storage)?;
        if bytes.len() as u64 != before.len()
            || !stable_binary(&before, &after)
            || !stable_binary(&before, &named)
        {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let runtime_copy_source_hashes = entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.kind,
                    ProductionEntryKindV2::Cli | ProductionEntryKindV2::ExecutionHelper
                )
            })
            .map(|entry| entry.binary.sha256.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let result = Self {
            protocol_version: 2,
            entries,
            runtime_copy_source_hashes,
        };
        result.validate()?;
        #[cfg(test)]
        LATE_PACKAGE_ENTRY.with(|slot| {
            if let Some(path) = slot.borrow_mut().take() {
                std::fs::write(path, b"late unlisted entry").unwrap();
            }
        });
        for (path, (held, before)) in paths.iter().zip(&directories) {
            let named = physical_directory(path)?;
            if !stable_binary(before, &held.metadata().map_err(storage)?)
                || !stable_binary(before, &named.metadata().map_err(storage)?)
            {
                return Err(EntryGateErrorV2::Identity);
            }
        }
        Ok(result)
    }
}

fn directory_names(file: &File) -> Result<BTreeSet<String>, EntryGateErrorV2> {
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(EntryGateErrorV2::Storage);
    }
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(EntryGateErrorV2::Storage);
    }
    let result = (|| {
        let mut names = BTreeSet::new();
        loop {
            unsafe {
                *libc::__error() = 0;
            }
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                if unsafe { *libc::__error() } != 0 {
                    return Err(EntryGateErrorV2::Storage);
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if [b".".as_slice(), b"..".as_slice()].contains(&name.to_bytes()) {
                continue;
            }
            let name = name.to_str().map_err(|_| EntryGateErrorV2::InvalidGate)?;
            if names.len() >= MAX_ENTRIES || !names.insert(name.to_owned()) {
                return Err(EntryGateErrorV2::InvalidGate);
            }
        }
        Ok(names)
    })();
    let closed = unsafe { libc::closedir(dir) };
    if closed != 0 {
        return Err(EntryGateErrorV2::Storage);
    }
    result
}

#[cfg(test)]
thread_local! {
    static LATE_PACKAGE_ENTRY: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn test_add_after_inventory(app: &Path) {
    LATE_PACKAGE_ENTRY
        .with(|slot| *slot.borrow_mut() = Some(app.join("Contents/Helpers/unlisted")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[cfg(target_os = "macos")]
    fn package_fixture(with_helper: bool) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("PolarisDesktop.app");
        for child in ["MacOS", "Helpers", "Resources"] {
            std::fs::create_dir_all(app.join("Contents").join(child)).unwrap();
        }
        let mut entries = Vec::new();
        for (kind, relative) in [
            ("cli", "Contents/Helpers/polaris"),
            ("desktop_app", "Contents/MacOS/PolarisDesktop"),
            (
                "desktop_service",
                "Contents/Helpers/polaris-desktop-service",
            ),
            (
                "execution_helper",
                "Contents/Helpers/polaris-execution-helper",
            ),
        ] {
            if !with_helper && kind == "execution_helper" {
                continue;
            }
            let path = app.join(relative);
            std::fs::write(&path, format!("synthetic {kind}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let identity = ProductionBinaryIdentityV2::inspect(&path).unwrap();
            entries.push(
            serde_json::json!({"kind": kind,"relative_path": relative,"sha256": identity.sha256}),
        );
        }
        std::fs::write(
            app.join("Contents/Resources/mcp-entry-binaries-v2.json"),
            serde_json::to_vec(&serde_json::json!({"protocol_version":2,"entries":entries}))
                .unwrap(),
        )
        .unwrap();
        (root, app)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn packaged_entries_cover_all_actual_binaries_and_optional_helper() {
        for helper in [false, true] {
            let (_root, app) = package_fixture(helper);
            let manifest = ProductionEntryManifestV2::from_macos_package(&app).unwrap();
            assert_eq!(manifest.entries.len(), if helper { 4 } else { 3 });
            assert_eq!(
                manifest.runtime_copy_source_hashes.len(),
                if helper { 2 } else { 1 }
            );
            manifest.validate().unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn packaged_entries_reject_extra_missing_or_modified_binary() {
        for fault in ["extra", "missing", "modified"] {
            let (_root, app) = package_fixture(true);
            let path = app.join("Contents/Helpers/polaris");
            match fault {
                "extra" => {
                    std::fs::write(app.join("Contents/Helpers/unlisted"), b"unlisted").unwrap()
                }
                "missing" => std::fs::remove_file(path).unwrap(),
                _ => std::fs::write(path, b"changed bytes").unwrap(),
            }
            assert!(ProductionEntryManifestV2::from_macos_package(&app).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn packaged_entries_reject_path_alias_unknown_field_and_manifest_link() {
        for fault in ["alias", "unknown", "hardlink"] {
            let (_root, app) = package_fixture(true);
            let path = app.join("Contents/Resources/mcp-entry-binaries-v2.json");
            if fault == "hardlink" {
                std::fs::hard_link(&path, app.join("manifest-alias")).unwrap();
            } else {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if fault == "alias" {
                    value["entries"][0]["relative_path"] = "Contents/Helpers/./polaris".into();
                } else {
                    value["ignored"] = true.into();
                }
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            assert!(ProductionEntryManifestV2::from_macos_package(&app).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn packaged_entries_reject_directory_change_after_final_binary_readback() {
        let (_root, app) = package_fixture(true);
        test_add_after_inventory(&app);
        assert!(ProductionEntryManifestV2::from_macos_package(&app).is_err());
        assert!(app.join("Contents/Helpers/unlisted").is_file());
    }
}
