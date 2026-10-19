//! Registered cwd admission, absent secrets, native confinement and scratch lifetime.
use super::*;
use polaris_mcp::registration::{Capabilities, ProtocolProfile};
use polaris_sandbox::SandboxMode;
use std::{collections::BTreeSet, future::pending, os::unix::fs::symlink, path::Path};

const TEST_HOME: &str = "/var/empty/polaris-test-home";

fn limits() -> Limits {
    Limits {
        max_entries: 32,
        max_files: 16,
        max_file_bytes: 8192,
        max_total_bytes: 32768,
        max_depth: 4,
    }
}
fn directory(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}
struct FixtureTemp {
    root: TempDir,
    _retained: Vec<TempDir>,
}
impl FixtureTemp {
    fn path(&self) -> &Path {
        self.root.path()
    }
}
fn fixture_identity_collision(paths: &[PathBuf]) -> bool {
    polaris_auth::protection::with_protected_paths(|registered| {
        paths.iter().any(|path| {
            let metadata = fs::metadata(path).unwrap();
            registered
                .identities()
                .contains(&polaris_auth::protection::ProtectedIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                })
        })
    })
    .unwrap()
}
fn fixture() -> (FixtureTemp, PathBuf, Registry) {
    let mut retained = Vec::new();
    for generation in 1..=32 {
        let (root, cwd, registry) = fixture_candidate();
        if fixture_identity_collision(&[root.path().to_owned(), cwd.clone(), cwd.join("data.txt")])
        {
            eprintln!(
                "runtime cwd fixture generation={generation} retired_identity_collision=true"
            );
            retained.push(root);
            continue;
        }
        return (
            FixtureTemp {
                root,
                _retained: retained,
            },
            cwd,
            registry,
        );
    }
    panic!("runtime cwd fixture exhausted 32 private tree candidates")
}
fn fixture_candidate() -> (TempDir, PathBuf, Registry) {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let cwd = directory(&base, "server");
    fs::write(cwd.join("data.txt"), "ordinary input").unwrap();
    let registry = Registry {
        schema_version: 1,
        revision: 7,
        servers: vec![Server {
            server_id: "local".into(),
            enabled_projects: BTreeSet::from(["project".into()]),
            transport: Destination::Stdio {
                command: Path::new("/bin/sh").canonicalize().unwrap(),
                args: vec![],
                cwd: cwd.clone(),
                sandbox_profile: "read-only".into(),
                secret_env: BTreeMap::new(),
                readable_roots: vec![],
                executable_sha256: None,
                script: None,
                script_sha256: None,
            },
            protocol_profile: ProtocolProfile::Discover2026,
            capabilities: Capabilities {
                catalog_metadata: true,
                resource_read: true,
                tool_call: true,
                prompt_selection: false,
            },
            timeouts: Default::default(),
        }],
    };
    (temp, cwd, registry)
}
fn runtimes() -> Vec<PathBuf> {
    let mut roots = vec!["/bin".into(), "/usr/bin".into(), "/usr/lib".into()];
    #[cfg(target_os = "macos")]
    roots.push("/System/Library".into());
    #[cfg(target_os = "linux")]
    roots.push("/lib".into());
    roots
}
fn reason(config: Result<Option<RuntimeConfig>, polaris_mcp::Error>) -> Option<polaris_mcp::Error> {
    config.unwrap().unwrap().quarantined().get("local").copied()
}
fn prepare(registry: Registry) -> Result<RuntimeConfig, polaris_mcp::Error> {
    RuntimeConfig::prepare_registered(
        Snapshot {
            registry: Some(registry),
            secrets: None,
        },
        "project".into(),
        Path::new(TEST_HOME),
        &runtimes(),
        &[],
        limits(),
    )
    .map(Option::unwrap)
}

#[test]
fn nonempty_registered_cwd_keeps_its_path_and_scratch_lives_with_config_clones() {
    let (_temp, cwd, registry) = fixture();
    let config = prepare(registry).unwrap();
    assert_eq!(config.server_ids(), ["local"]);
    assert!(config.secrets.is_none());
    let policy = &config.policies["local"];
    let boundary = policy.isolated_boundary().unwrap();
    assert_eq!(boundary.workspace, cwd);
    assert_eq!(
        fs::read_to_string(cwd.join("data.txt")).unwrap(),
        "ordinary input"
    );
    let environment = boundary.environment.as_ref().unwrap();
    assert!(!environment.home.starts_with(&cwd));
    assert!(!policy.contains(&cwd));
    let scratch = environment.home.parent().unwrap().to_owned();
    let retained = config.clone();
    drop(config);
    assert!(scratch.join("home").is_dir() && scratch.join("tmp").is_dir());
    fs::write(cwd.join("allowed-output"), "child output").unwrap();
    retained.stdio_resources["local"].revalidate().unwrap();
    drop(retained);
    assert!(!scratch.exists());
    assert!(cwd.join("allowed-output").exists());
}

#[test]
fn registered_cwd_rejects_unsafe_entries_instead_of_silently_filtering_them() {
    for bad in [
        "secret", "symlink", "hardlink", "git", "writable", "oversize",
    ] {
        let (temp, cwd, registry) = fixture();
        match bad {
            "secret" => fs::write(cwd.join(".env"), "synthetic-only").unwrap(),
            "symlink" => symlink("data.txt", cwd.join("alias")).unwrap(),
            "hardlink" => fs::hard_link(cwd.join("data.txt"), cwd.join("alias")).unwrap(),
            "git" => {
                directory(&cwd, ".git");
            }
            "writable" => {
                fs::set_permissions(cwd.join("data.txt"), Permissions::from_mode(0o666)).unwrap()
            }
            "oversize" => fs::write(cwd.join("large"), vec![b'x'; 8193]).unwrap(),
            _ => unreachable!(),
        }
        assert_eq!(
            prepare(registry).unwrap().quarantined().get("local"),
            Some(&polaris_mcp::Error::Protection),
            "{bad}"
        );
        assert!(cwd.exists() && temp.path().exists());
    }
}

#[test]
fn absent_protected_path_and_replaced_cwd_or_ancestor_are_rejected() {
    let (_temp, cwd, registry) = fixture();
    let config = prepare(registry).unwrap();
    drop(polaris_auth::protection::protect_store(&cwd.join("reserved.json")).unwrap());
    assert!(!cwd.join("reserved.json").exists());
    assert!(config.stdio_resources["local"].revalidate().is_err());
    {
        let (_temp, cwd, registry) = fixture();
        let config = prepare(registry).unwrap();
        let moved = cwd.with_file_name("moved");
        fs::rename(&cwd, &moved).unwrap();
        fs::create_dir(&cwd).unwrap();
        fs::set_permissions(&cwd, Permissions::from_mode(0o700)).unwrap();
        assert!(config.stdio_resources["local"].revalidate().is_err());
    }
}

#[test]
fn replaced_ancestor_and_scratch_or_executable_are_rejected() {
    for changed in ["ancestor", "scratch", "command"] {
        let (temp, cwd, mut registry) = fixture();
        let base = temp.path().canonicalize().unwrap();
        let parent = directory(&base, "container");
        let nested = parent.join("server");
        fs::rename(&cwd, &nested).unwrap();
        let command = nested.join("entry.sh");
        fs::write(&command, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&command, Permissions::from_mode(0o700)).unwrap();
        if let Destination::Stdio {
            cwd,
            command: entry,
            ..
        } = &mut registry.servers[0].transport
        {
            *cwd = nested.clone();
            *entry = command.clone();
        }
        let config = prepare(registry).unwrap();
        let resource = &config.stdio_resources["local"];
        match changed {
            "ancestor" => {
                let moved = base.join("moved-parent");
                fs::rename(&parent, &moved).unwrap();
                directory(&base, "container");
                fs::rename(moved.join("server"), &nested).unwrap();
            }
            "scratch" => {
                let home = &resource.scratch_directories[1].0;
                fs::rename(home, home.with_file_name("old-home")).unwrap();
                fs::create_dir(home).unwrap();
                fs::set_permissions(home, Permissions::from_mode(0o700)).unwrap();
            }
            "command" => fs::write(&command, "#!/bin/sh\nexit 1\n").unwrap(),
            _ => unreachable!(),
        }
        assert!(resource.revalidate().is_err(), "{changed}");
    }
}

#[test]
fn disabled_servers_are_not_prepared_but_enabled_stdio_never_disappears() {
    let (_temp, cwd, mut registry) = fixture();
    registry.servers[0].enabled_projects = BTreeSet::from(["elsewhere".into()]);
    fs::write(cwd.join(".env"), "synthetic").unwrap();
    let missing = RuntimeConfig::prepare_registered(
        Snapshot {
            registry: Some(registry.clone()),
            secrets: None,
        },
        "project".into(),
        Path::new(TEST_HOME),
        &runtimes(),
        &[],
        limits(),
    )
    .unwrap();
    assert!(missing.is_none());
    registry.servers[0]
        .enabled_projects
        .insert("project".into());
    assert_eq!(
        prepare(registry).unwrap().quarantined().get("local"),
        Some(&polaris_mcp::Error::Protection)
    );
}

#[test]
fn source_overlap_and_unlisted_command_runtime_do_not_expand_access() {
    let (_temp, cwd, registry) = fixture();
    assert_eq!(
        reason(RuntimeConfig::prepare_registered(
            Snapshot {
                registry: Some(registry.clone()),
                secrets: None
            },
            "project".into(),
            Path::new(TEST_HOME),
            &runtimes(),
            std::slice::from_ref(&cwd),
            limits()
        )),
        Some(polaris_mcp::Error::Protection)
    );
    assert_eq!(
        reason(RuntimeConfig::prepare_registered(
            Snapshot {
                registry: Some(registry),
                secrets: None
            },
            "project".into(),
            Path::new(TEST_HOME),
            &[],
            &[],
            limits()
        )),
        Some(polaris_mcp::Error::Protection)
    );
}

#[tokio::test]
async fn missing_secrets_are_valid_only_when_no_reference_requires_them() {
    let (_temp, _cwd, mut registry) = fixture();
    registry.servers[0].transport = Destination::StreamableHttp {
        url: "https://example.invalid/mcp".into(),
        secret_headers: BTreeMap::new(),
    };
    let config = prepare(registry.clone()).unwrap();
    assert!(config.secrets.is_none() && config.stdio_resources.is_empty());
    assert!(Connection::prepare(&registry.servers[0], "project", None, None, || true).is_ok());
    if let Destination::StreamableHttp { secret_headers, .. } = &mut registry.servers[0].transport {
        secret_headers.insert("authorization".into(), "missing".into());
    }
    assert_eq!(
        prepare(registry.clone())
            .unwrap()
            .quarantined()
            .get("local"),
        Some(&polaris_mcp::Error::Auth)
    );
    assert!(matches!(
        Connection::prepare(&registry.servers[0], "project", None, None, || true),
        Err(polaris_mcp::Error::Auth)
    ));
}

#[test]
fn scratch_is_not_created_in_source_runtime_or_protected_directories() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let source = directory(&base, "source");
    let runtime = directory(&base, "runtime");
    let protected = directory(&base, "protected");
    drop(polaris_auth::protection::protect_store(&protected.join("secret.json")).unwrap());
    for parent in [&source, &runtime, &protected] {
        assert!(
            prepare_scratch(
                parent,
                std::slice::from_ref(&source),
                std::slice::from_ref(&runtime)
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
    }
    let safe = prepare_scratch(&base, &[source], &[runtime]).unwrap();
    assert!(safe.path().is_dir());
}

#[tokio::test]
async fn runtime_revalidates_registered_cwd_before_native_spawn() {
    let (_temp, cwd, mut registry) = fixture();
    if let Destination::Stdio { args, .. } = &mut registry.servers[0].transport {
        *args = vec!["-c".into(), "touch started".into()];
    }
    let config = prepare(registry).unwrap();
    use polaris_desktop_protocol::{
        ids::{AttemptId, RunId, SessionId},
        request::RunTarget,
    };
    let target = RunTarget {
        run_id: RunId::new("run").unwrap(),
        attempt_id: AttemptId::new("attempt").unwrap(),
    };
    let mut owner = crate::execution_owner::RunExecution::new(
        &target,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let port = owner
        .configure_mcp(
            &target,
            SessionId::new("session").unwrap(),
            crate::mcp::operation::ExternalCeiling::Allowed,
            crate::approval::ApprovalPolicy::Always,
        )
        .unwrap();
    let mut runtime = RunRuntime::new(config, port).unwrap();
    fs::write(cwd.join(".env"), "synthetic").unwrap();
    assert!(matches!(
        runtime.ensure_catalog("local", pending()).await,
        Err(Error::Transport(polaris_mcp::Error::Protection))
    ));
    assert!(!cwd.join("started").exists());
    assert!(runtime.connections.is_empty());
    runtime.shutdown().await.unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn loopback_profile_is_read_only_with_loopback_network() {
    for (profile, loopback) in [("read-only", false), ("loopback", true)] {
        let (_temp, _cwd, mut registry) = fixture();
        if let Destination::Stdio {
            sandbox_profile, ..
        } = &mut registry.servers[0].transport
        {
            *sandbox_profile = profile.into();
        }
        let config = prepare(registry).unwrap_or_else(|e| panic!("{profile}: {e:?}"));
        let policy = &config.policies["local"];
        assert_eq!(policy.mode(), polaris_sandbox::SandboxMode::ReadOnly);
        assert_eq!(
            policy.isolated_boundary().unwrap().loopback_network,
            loopback
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn registered_native_child_keeps_cwd_and_private_environment_permissions() {
    use std::io::Read;
    for writable in [false, true] {
        let (_temp, cwd, mut registry) = fixture();
        if let Destination::Stdio {
            sandbox_profile, ..
        } = &mut registry.servers[0].transport
        {
            *sandbox_profile = if writable {
                "workspace-write"
            } else {
                "read-only"
            }
            .into();
        }
        let config = prepare(registry).unwrap();
        let retained = config.clone();
        let policy = &config.policies["local"];
        let environment = policy
            .isolated_boundary()
            .unwrap()
            .environment
            .as_ref()
            .unwrap();
        let home = environment.home.clone();
        let tmp = environment.tmpdir.clone();
        let args = vec!["-c".into(),
            "printf '%s\\n' \"$PWD\" \"$HOME\" \"$TMPDIR\"; cat data.txt; printf home > \"$HOME/ok\"; printf tmp > \"$TMPDIR/ok\"; printf source > source-output".into()];
        let (mut child, mut pipes) = polaris_sandbox::ManagedStdio::spawn_authorized(
            policy,
            &Path::new("/bin/sh").canonicalize().unwrap(),
            &args,
            &cwd,
            &BTreeMap::new(),
            || config.stdio_resources["local"].revalidate().is_ok(),
        )
        .unwrap()
        .unwrap();
        drop(pipes.stdin);
        drop(config);
        let end = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(end) = child.wait_and_poll().await {
                    break end;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(end, polaris_sandbox::ControlledEnd::Exited);
        assert!(child.take_pending().is_none());
        let mut output = String::new();
        pipes.stdout.read_to_string(&mut output).unwrap();
        assert_eq!(
            output,
            format!(
                "{}\n{}\n{}\nordinary input",
                cwd.display(),
                home.display(),
                tmp.display()
            )
        );
        assert_eq!(fs::read_to_string(home.join("ok")).unwrap(), "home");
        assert_eq!(fs::read_to_string(tmp.join("ok")).unwrap(), "tmp");
        assert_eq!(cwd.join("source-output").exists(), writable);
        let container = home.parent().unwrap().to_owned();
        drop(child);
        drop(retained);
        assert!(!container.exists());
    }
}

#[test]
fn unsafe_servers_are_quarantined_without_disabling_the_rest() {
    let (_temp, cwd, mut registry) = fixture();
    fs::write(cwd.join(".env"), "synthetic").unwrap();
    let mut remote = registry.servers[0].clone();
    remote.server_id = "remote".into();
    remote.transport = Destination::StreamableHttp {
        url: "https://example.com/mcp".into(),
        secret_headers: BTreeMap::new(),
    };
    registry.servers.push(remote);
    let config = prepare(registry).unwrap();
    assert_eq!(config.server_ids(), ["remote"]);
    assert_eq!(
        config.quarantined().get("local"),
        Some(&polaris_mcp::Error::Protection)
    );
}

fn pinned_tool(base: &Path) -> (PathBuf, PathBuf, String) {
    let root = base.join("toolchain");
    assert!(!root.exists());
    for generation in 1..=32 {
        let candidate = tempfile::tempdir_in(base).unwrap();
        let bin = directory(candidate.path(), "bin");
        fs::set_permissions(candidate.path(), Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();
        let command = bin.join("server");
        fs::write(&command, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&command, Permissions::from_mode(0o755)).unwrap();
        let module = candidate.path().join("module.js");
        fs::write(&module, "export {}").unwrap();
        fs::set_permissions(&module, Permissions::from_mode(0o644)).unwrap();
        if fixture_identity_collision(&[candidate.path().to_owned(), bin, command, module]) {
            eprintln!(
                "runtime tool fixture generation={generation} retired_identity_collision=true"
            );
            // These synthetic siblings remain outside the admitted tool/CWD.
            // The outer fixture removes them when its temporary root drops.
            let _ = candidate.keep();
            continue;
        }
        fs::rename(candidate.path(), &root).unwrap();
        let command = root.join("bin/server");
        let digest = crate::conversation_state::content_hash(&fs::read(&command).unwrap());
        return (root, command, digest);
    }
    panic!("runtime tool fixture exhausted 32 candidates")
}

fn with_tool(registry: &mut Registry, command: &Path, roots: Vec<PathBuf>, digest: Option<String>) {
    if let Destination::Stdio {
        command: entry,
        readable_roots,
        executable_sha256,
        ..
    } = &mut registry.servers[0].transport
    {
        *entry = command.to_owned();
        *readable_roots = roots;
        *executable_sha256 = digest;
    }
}

#[test]
fn readable_roots_admit_only_owned_unwritable_trees_and_pinned_executables() {
    let (temp, _cwd, mut registry) = fixture();
    let base = temp.path().canonicalize().unwrap();
    let (root, command, digest) = pinned_tool(&base);
    with_tool(
        &mut registry,
        &command,
        vec![root.clone()],
        Some(digest.clone()),
    );
    let config = prepare(registry.clone()).unwrap();
    assert_eq!(config.server_ids(), ["local"]);
    let boundary = config.policies["local"].isolated_boundary().unwrap();
    assert!(boundary.readable_roots.contains(&root));

    let quarantined = |registry: Registry| {
        prepare(registry)
            .unwrap()
            .quarantined()
            .get("local")
            .copied()
    };
    let mut unpinned = registry.clone();
    with_tool(
        &mut unpinned,
        &command,
        vec![root.clone()],
        Some("0".repeat(64)),
    );
    assert_eq!(quarantined(unpinned), Some(polaris_mcp::Error::Protection));
    let mut outside = registry.clone();
    with_tool(&mut outside, &command, vec![], Some(digest.clone()));
    assert_eq!(quarantined(outside), Some(polaris_mcp::Error::Protection));

    fs::set_permissions(root.join("module.js"), Permissions::from_mode(0o664)).unwrap();
    assert_eq!(
        quarantined(registry.clone()),
        Some(polaris_mcp::Error::Protection)
    );
    fs::set_permissions(root.join("module.js"), Permissions::from_mode(0o644)).unwrap();
    symlink(base.join("server"), root.join("escape")).unwrap();
    assert_eq!(
        quarantined(registry.clone()),
        Some(polaris_mcp::Error::Protection)
    );
    fs::remove_file(root.join("escape")).unwrap();
    symlink(root.join("module.js"), root.join("inside")).unwrap();
    assert_eq!(quarantined(registry.clone()), None);

    let alias = base.join("toolchain-alias");
    symlink(&root, &alias).unwrap();
    let mut aliased = registry.clone();
    with_tool(
        &mut aliased,
        &alias.join("bin/server"),
        vec![alias],
        Some(digest.clone()),
    );
    assert_eq!(quarantined(aliased), Some(polaris_mcp::Error::Protection));
    drop(polaris_auth::protection::protect_store(&root.join("secret.json")).unwrap());
    assert_eq!(quarantined(registry), Some(polaris_mcp::Error::Protection));
}

#[test]
fn registration_bounds_pinned_fields() {
    let (_temp, _cwd, mut registry) = fixture();
    let command = Path::new("/bin/sh").canonicalize().unwrap();
    for (roots, digest) in [
        (vec![PathBuf::from("relative")], None),
        (
            (0..9).map(|i| PathBuf::from(format!("/root{i}"))).collect(),
            None,
        ),
        (vec![], Some("not-hex".to_owned())),
        (vec![PathBuf::from("/opt/other")], Some("0".repeat(64))),
    ] {
        with_tool(&mut registry, &command, roots, digest);
        assert_eq!(
            registry.validate(),
            Err(polaris_mcp::Error::InvalidRegistration)
        );
    }
}

#[test]
fn read_roots_refuse_home_and_overlapping_trees_and_scripts_are_pinned() {
    let (temp, _cwd, mut registry) = fixture();
    let base = temp.path().canonicalize().unwrap();
    let (root, command, digest) = pinned_tool(&base);
    with_tool(&mut registry, &command, vec![root.clone()], Some(digest));
    let reason = |registry: &Registry, home: &Path, excluded: &[PathBuf]| {
        RuntimeConfig::prepare_registered(
            Snapshot {
                registry: Some(registry.clone()),
                secrets: None,
            },
            "project".into(),
            home,
            &runtimes(),
            excluded,
            limits(),
        )
        .unwrap()
        .unwrap()
        .quarantined()
        .get("local")
        .copied()
    };
    let denied = Some(polaris_mcp::Error::Protection);
    let home = Path::new(TEST_HOME);
    assert_eq!(reason(&registry, home, &[]), None);
    assert_eq!(reason(&registry, &base, &[]), denied);
    assert_eq!(reason(&registry, &root.join("bin"), &[]), denied);
    assert_eq!(reason(&registry, home, std::slice::from_ref(&root)), denied);
    assert_eq!(reason(&registry, home, &[root.join("bin")]), denied);

    let script = root.join("module.js");
    let script_digest = crate::conversation_state::content_hash(&fs::read(&script).unwrap());
    let mut interpreted = registry.clone();
    if let Destination::Stdio {
        args,
        script: entry,
        script_sha256,
        ..
    } = &mut interpreted.servers[0].transport
    {
        *args = vec![script.to_str().unwrap().into()];
        *entry = Some(script.clone());
        *script_sha256 = Some(script_digest);
    }
    interpreted.validate().unwrap();
    assert_eq!(reason(&interpreted, home, &[]), None);
    fs::write(&script, "export const changed = 1").unwrap();
    assert_eq!(reason(&interpreted, home, &[]), denied);
}

#[derive(Debug, Eq, PartialEq)]
struct InventoryEntry {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    group: u32,
    links: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    contents: Option<Vec<u8>>,
    link_target: Option<PathBuf>,
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, InventoryEntry> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let metadata = path.symlink_metadata().unwrap();
        let contents = metadata.is_file().then(|| fs::read(&path).unwrap());
        let link_target = metadata
            .file_type()
            .is_symlink()
            .then(|| fs::read_link(&path).unwrap());
        if metadata.is_dir() {
            pending.extend(
                fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
        entries.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            InventoryEntry {
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                owner: metadata.uid(),
                group: metadata.gid(),
                links: metadata.nlink(),
                bytes: metadata.len(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
                contents,
                link_target,
            },
        );
    }
    entries
}

fn no_exec(
    registry: &Registry,
    excluded: &[PathBuf],
) -> Result<Vec<StdioAdmission>, polaris_mcp::Error> {
    RuntimeConfig::validate_servers_no_exec(
        &registry.servers,
        "project",
        Path::new(TEST_HOME),
        &runtimes(),
        excluded,
        limits(),
    )
}

fn scratch_preparations() -> usize {
    SCRATCH_PREPARATIONS.with(|count| count.get())
}

#[test]
fn no_exec_admission_keeps_owned_inventory_and_never_prepares_scratch() {
    let (temp, _cwd, mut registry) = fixture();
    let base = temp.path().canonicalize().unwrap();
    let (root, command, digest) = pinned_tool(&base);
    with_tool(&mut registry, &command, vec![root.clone()], Some(digest));
    let script = root.join("module.js");
    let script_digest = crate::conversation_state::content_hash(&fs::read(&script).unwrap());
    if let Destination::Stdio {
        args,
        script: entry,
        script_sha256,
        ..
    } = &mut registry.servers[0].transport
    {
        *args = vec![script.to_str().unwrap().into()];
        *entry = Some(script);
        *script_sha256 = Some(script_digest);
    }
    let before = inventory(&base);
    let scratch_before = scratch_preparations();
    for (profile, mode, loopback) in [
        ("read-only", SandboxMode::ReadOnly, false),
        ("loopback", SandboxMode::ReadOnly, true),
        ("workspace-write", SandboxMode::WorkspaceWrite, false),
        ("full-access", SandboxMode::FullAccess, false),
    ] {
        if let Destination::Stdio {
            sandbox_profile, ..
        } = &mut registry.servers[0].transport
        {
            *sandbox_profile = profile.into();
        }
        let admissions = no_exec(&registry, &[]).unwrap();
        assert_eq!(admissions.len(), 1);
        assert_eq!(admissions[0].pinned_file_count(), 2);
        admissions[0].revalidate().unwrap();
        let (policy, _) = admit_stdio(
            &registry.servers[0],
            Path::new(TEST_HOME),
            &runtimes(),
            &[],
            limits(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(policy.mode(), mode);
        let boundary = policy.isolated_boundary().unwrap();
        assert_eq!(boundary.loopback_network, loopback);
        assert!(boundary.environment.is_none());
    }
    assert_eq!(inventory(&base), before);
    assert_eq!(scratch_preparations(), scratch_before);
}

#[test]
fn no_exec_rejects_digest_resources_and_selected_exclusions_without_writes() {
    for failure in ["digest", "cwd", "excluded"] {
        let (temp, cwd, mut registry) = fixture();
        let base = temp.path().canonicalize().unwrap();
        let (root, command, digest) = pinned_tool(&base);
        with_tool(&mut registry, &command, vec![root], Some(digest));
        let excluded = match failure {
            "digest" => {
                if let Destination::Stdio {
                    executable_sha256, ..
                } = &mut registry.servers[0].transport
                {
                    *executable_sha256 = Some("0".repeat(64));
                }
                vec![]
            }
            "cwd" => {
                fs::write(cwd.join(".env"), "synthetic-only").unwrap();
                vec![]
            }
            "excluded" => vec![cwd],
            _ => unreachable!(),
        };
        let before = inventory(&base);
        let scratch_before = scratch_preparations();
        assert_eq!(
            no_exec(&registry, &excluded).err(),
            Some(polaris_mcp::Error::Protection),
            "{failure}"
        );
        assert_eq!(inventory(&base), before, "{failure}");
        assert_eq!(scratch_preparations(), scratch_before, "{failure}");
    }
    let (temp, cwd, mut registry) = fixture();
    let base = temp.path().canonicalize().unwrap();
    let (root, command, digest) = pinned_tool(&base);
    with_tool(&mut registry, &command, vec![root], Some(digest));
    registry.servers[0].enabled_projects = BTreeSet::from(["elsewhere".into()]);
    assert_eq!(
        no_exec(&registry, std::slice::from_ref(&cwd))
            .unwrap()
            .len(),
        1
    );
    fs::write(cwd.join(".env"), "synthetic-only").unwrap();
    let before = inventory(&base);
    let scratch_before = scratch_preparations();
    // Other projects omit these exclusions but still undergo resource admission.
    assert_eq!(
        no_exec(&registry, std::slice::from_ref(&cwd)).err(),
        Some(polaris_mcp::Error::Protection)
    );
    assert_eq!(inventory(&base), before);
    assert_eq!(scratch_preparations(), scratch_before);
}

#[test]
fn no_exec_retains_witnesses_and_revalidates_early_servers_after_iteration() {
    for changed in ["command", "cwd"] {
        for retained in [true, false] {
            let (temp, cwd, mut registry) = fixture();
            let base = temp.path().canonicalize().unwrap();
            let (root, command, digest) = pinned_tool(&base);
            with_tool(&mut registry, &command, vec![root], Some(digest));
            let change = || match changed {
                "command" => fs::write(&command, "#!/bin/sh\nexit 1\n").unwrap(),
                "cwd" => fs::write(cwd.join(".env"), "synthetic-only").unwrap(),
                _ => unreachable!(),
            };
            let scratch_before = scratch_preparations();
            if retained {
                let admissions = no_exec(&registry, &[]).unwrap();
                change();
                assert_eq!(
                    admissions[0].revalidate(),
                    Err(polaris_mcp::Error::Protection)
                );
            } else {
                let mut remote = registry.servers[0].clone();
                remote.server_id = "remote".into();
                remote.transport = Destination::StreamableHttp {
                    url: "https://example.invalid/mcp".into(),
                    secret_headers: BTreeMap::new(),
                };
                let servers = std::iter::once(&registry.servers[0])
                    .chain(std::iter::once(&remote).inspect(|_| change()));
                let admitted = RuntimeConfig::validate_servers_no_exec(
                    servers,
                    "project",
                    Path::new(TEST_HOME),
                    &runtimes(),
                    &[],
                    limits(),
                );
                assert_eq!(admitted.err(), Some(polaris_mcp::Error::Protection));
            }
            assert_eq!(scratch_preparations(), scratch_before);
        }
    }
}

#[test]
fn no_exec_http_has_no_resource_witnesses_or_filesystem_effects() {
    let (temp, cwd, mut registry) = fixture();
    let base = temp.path().canonicalize().unwrap();
    fs::write(cwd.join(".env"), "synthetic-only").unwrap();
    registry.servers[0].transport = Destination::StreamableHttp {
        url: "https://example.invalid/mcp".into(),
        secret_headers: BTreeMap::new(),
    };
    let before = inventory(&base);
    let scratch_before = scratch_preparations();
    assert!(no_exec(&registry, &[base.clone()]).unwrap().is_empty());
    assert_eq!(inventory(&base), before);
    assert_eq!(scratch_preparations(), scratch_before);
}
