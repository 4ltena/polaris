//! Copied runtime isolation, limits, and fixed executable search paths.
use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn copy_limits() -> Limits {
    Limits {
        max_entries: 64,
        max_files: 32,
        max_file_bytes: 1024,
        max_total_bytes: 4096,
        max_depth: 4,
    }
}

fn package(root: &Path) -> RelocatablePackage {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir(root.join("lib")).unwrap();
    fs::write(root.join("bin/dummy"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(root.join("bin/dummy"), Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("lib/data"), b"dummy runtime data").unwrap();
    RelocatablePackage {
        root: root.into(),
        bin_dirs: vec![PathBuf::from("bin")],
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    helper: PathBuf,
    plan: ToolchainPlan,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("ordinary"), b"source content").unwrap();
        let helper = root.join("helper");
        fs::write(&helper, b"dummy helper; never executed").unwrap();
        fs::set_permissions(&helper, Permissions::from_mode(0o700)).unwrap();
        let plan = ToolchainPlan {
            packages: vec![package(&root.join("package"))],
            limits: RuntimeLimits {
                max_packages: 4,
                max_bin_dirs: 8,
                copy: copy_limits(),
            },
        };
        Self {
            _temp: temp,
            root,
            source,
            helper,
            plan,
        }
    }

    fn prepare(&self, mode: SandboxMode) -> Result<PreparedWorkspace> {
        prepare_with_toolchains_locked(
            &self.source,
            &self.helper,
            mode,
            &[PathBuf::from("/usr/lib")],
            copy_limits(),
            &ProtectedPathsSnapshot::default(),
            Some(&self.plan),
        )
    }

    fn stage(&self, secrets: &RegisteredSecrets) -> Result<StagedToolchains> {
        stage(
            Some(&self.plan),
            &self.source,
            &ProtectedPathsSnapshot::default(),
            secrets,
        )
    }
}

#[test]
fn copies_are_readonly_in_all_modes_inherited_and_owned_until_drop() {
    for mode in [
        SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite,
        SandboxMode::FullAccess,
    ] {
        let fixture = Fixture::new();
        let prepared = fixture.prepare(mode).unwrap();
        let runtime = prepared._toolchains.roots().next().unwrap();
        let runtime_parent = runtime.parent().unwrap().to_owned();
        let original = &fixture.plan.packages[0].root;
        let boundary = prepared.policy().isolated_boundary().unwrap();
        assert!(boundary.readable_roots.contains(&runtime));
        for root in &boundary.readable_roots {
            assert!(!overlaps(root, &fixture.source));
            assert!(!overlaps(root, original));
        }
        assert!(!prepared.policy().contains(&runtime));
        assert_eq!(
            fs::read(runtime.join("lib/data")).unwrap(),
            b"dummy runtime data"
        );
        assert_eq!(fs::metadata(&runtime).unwrap().mode() & 0o7777, 0o500);
        assert_eq!(
            fs::metadata(runtime.join("bin")).unwrap().mode() & 0o7777,
            0o500
        );
        assert_eq!(
            fs::metadata(runtime.join("bin/dummy")).unwrap().mode() & 0o7777,
            0o500
        );
        assert_eq!(
            fs::metadata(runtime.join("lib/data")).unwrap().mode() & 0o7777,
            0o400
        );
        assert_ne!(
            fs::metadata(runtime.join("lib/data")).unwrap().ino(),
            fs::metadata(original.join("lib/data")).unwrap().ino()
        );
        let child = prepared
            .policy()
            .restrict(SandboxMode::ReadOnly, &[])
            .unwrap();
        assert_eq!(
            child.isolated_boundary().unwrap().readable_roots,
            boundary.readable_roots
        );
        assert!(!child.contains(&runtime));
        assert!(
            child
                .restrict(SandboxMode::WorkspaceWrite, std::slice::from_ref(&runtime))
                .is_err()
        );
        assert!(
            prepared
                .snapshot()
                .manifest
                .iter()
                .all(|e| e.relative_path == Path::new("ordinary"))
        );
        drop(prepared);
        assert!(!runtime_parent.exists());
        assert!(original.join("bin/dummy").exists());
        assert_eq!(
            fs::metadata(original.join("bin/dummy")).unwrap().mode() & 0o777,
            0o755
        );
    }
}

#[test]
fn modifying_or_removing_original_does_not_change_the_copy() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare(SandboxMode::FullAccess).unwrap();
    let copied = prepared._toolchains.roots().next().unwrap();
    let original = &fixture.plan.packages[0].root;
    fs::write(original.join("lib/data"), b"replacement").unwrap();
    fs::remove_file(original.join("bin/dummy")).unwrap();
    assert_eq!(
        fs::read(copied.join("lib/data")).unwrap(),
        b"dummy runtime data"
    );
    assert_eq!(
        fs::read(copied.join("bin/dummy")).unwrap(),
        b"#!/bin/sh\nexit 0\n"
    );
}

#[test]
fn rejects_every_exclusion_instead_of_publishing_an_incomplete_package() {
    for kind in ["secret", "symlink", "hardlink", "fifo", "git"] {
        let fixture = Fixture::new();
        let root = &fixture.plan.packages[0].root;
        match kind {
            "secret" => fs::write(root.join(".env"), b"dummy only").unwrap(),
            "symlink" => symlink(&fixture.source, root.join("alias")).unwrap(),
            "hardlink" => fs::hard_link(root.join("lib/data"), root.join("alias")).unwrap(),
            "fifo" => {
                let name = CString::new(root.join("pipe").as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            "git" => fs::create_dir(root.join(".git")).unwrap(),
            _ => unreachable!(),
        }
        assert!(matches!(
            fixture.prepare(SandboxMode::FullAccess),
            Err(PrepareError::RuntimeDenied)
        ));
    }
}

#[test]
fn rejects_registered_custom_names_and_old_single_link_identities() {
    for moved in [false, true] {
        let fixture = Fixture::new();
        let registered = fixture.plan.packages[0].root.join("ordinary-credential");
        fs::write(&registered, b"synthetic credential").unwrap();
        let secrets = RegisteredSecrets::new([registered.clone()]).unwrap();
        if moved {
            fs::rename(&registered, registered.with_file_name("innocent-name")).unwrap();
        }
        assert!(matches!(
            fixture.stage(&secrets),
            Err(PrepareError::RuntimeDenied)
        ));
    }
}

#[test]
fn bounds_are_aggregate_across_packages() {
    for resource in [
        "packages", "bins", "entries", "files", "bytes", "file", "depth",
    ] {
        let mut fixture = Fixture::new();
        fixture
            .plan
            .packages
            .push(package(&fixture.root.join("second")));
        match resource {
            "packages" => fixture.plan.limits.max_packages = 1,
            "bins" => fixture.plan.limits.max_bin_dirs = 1,
            "entries" => fixture.plan.limits.copy.max_entries = 7,
            "files" => fixture.plan.limits.copy.max_files = 3,
            "bytes" => fixture.plan.limits.copy.max_total_bytes = 50,
            "file" => fixture.plan.limits.copy.max_file_bytes = 1,
            "depth" => fixture.plan.limits.copy.max_depth = 0,
            _ => unreachable!(),
        }
        assert!(
            matches!(
                fixture.prepare(SandboxMode::ReadOnly),
                Err(PrepareError::Limit)
            ),
            "{resource}"
        );
    }
}

#[test]
fn rejects_source_ancestry_root_alias_and_unbounded_bin_paths() {
    for root_kind in ["source", "ancestor", "root", "alias"] {
        let mut fixture = Fixture::new();
        fixture.plan.packages[0].root = match root_kind {
            "source" => fixture.source.clone(),
            "ancestor" => fixture.root.clone(),
            "root" => PathBuf::from("/"),
            "alias" => {
                let alias = fixture.root.join("alias");
                symlink(&fixture.plan.packages[0].root, &alias).unwrap();
                alias
            }
            _ => unreachable!(),
        };
        assert!(
            fixture.prepare(SandboxMode::ReadOnly).is_err(),
            "{root_kind}"
        );
    }
    for bin in [
        "",
        ".",
        "../source",
        "/bin",
        "bin:other",
        "missing",
        "lib/data",
    ] {
        let mut fixture = Fixture::new();
        fixture.plan.packages[0].bin_dirs = vec![PathBuf::from(bin)];
        assert!(fixture.prepare(SandboxMode::ReadOnly).is_err(), "{bin}");
    }
}

#[test]
fn a_later_package_failure_cleans_earlier_sealed_copies() {
    // Exercise the same owner unwind as stage(): previously sealed packages
    // must become removable even after a later operation returns an error.
    let fixture = Fixture::new();
    let staged = fixture.stage(&RegisteredSecrets::new([]).unwrap()).unwrap();
    let parents: Vec<_> = staged
        .roots()
        .map(|p| p.parent().unwrap().to_owned())
        .collect();
    let failed = (|| -> Result<()> {
        let _owner = staged;
        let mut bad = fixture.plan.clone();
        bad.packages[0].bin_dirs = vec![PathBuf::from("missing")];
        stage(
            Some(&bad),
            &fixture.source,
            &ProtectedPathsSnapshot::default(),
            &RegisteredSecrets::new([]).unwrap(),
        )?;
        Ok(())
    })();
    assert!(failed.is_err());
    assert!(parents.iter().all(|p| !p.exists()));
}
