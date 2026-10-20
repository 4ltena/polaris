//! Isolated fixture owners and filesystem evidence for Unity configuration tests.

mod apply;
mod binding;
mod scanner;

use super::UnityLocalV2Preparation;
use crate::{
    management_effect::ManagementEffectStoreV1,
    mcp::local::LocalHost,
    project_mutation::{ProjectMutationLeaseV1, ProjectMutationOwnerV1},
};
use polaris_mcp::{
    registration::{Capabilities, Destination, ProtocolProfile, Registry, Server, Timeouts},
    store::{SecretValue, Store},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const SERVER: &str = "unity-fixture";
const PAIR: [&str; 2] = [
    "node_modules/esbuild/bin/esbuild",
    "node_modules/@esbuild/darwin-arm64/bin/esbuild",
];

/// Protection registrations are process-wide. Every acceptance test gets a
/// child with a synthetic HOME before any host or protection owner is opened.
fn isolated(test: &str) -> bool {
    if std::env::var_os("POLARIS_UNITY_CONFIGURE_CHILD").is_some() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        return false;
    }
    let home = tempfile::tempdir().unwrap();
    // `module_path!` contains the crate name, while libtest's exact names do
    // not. A missing match must never turn this isolation wrapper into a pass.
    let test = test.split_once("::").unwrap().1;
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env("POLARIS_UNITY_CONFIGURE_CHILD", "1")
        .env("HOME", home.path().canonicalize().unwrap())
        .env("POLARIS_DATA_DIR", home.path().join("data"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated acceptance failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
        "the child must execute its exact acceptance test: {test}"
    );
    true
}

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
    state: PathBuf,
    source: PathBuf,
    node: PathBuf,
    host: LocalHost,
    before: Registry,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let path = root.path().canonicalize().unwrap();
        let home = path.join("home");
        let project = path.join("project");
        let state = path.join("state");
        for directory in [&home, &project, &state] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let source = path.join("unity-source");
        make_source(&source);
        let node = path.join("node");
        fs::write(&node, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&node, fs::Permissions::from_mode(0o700)).unwrap();
        let host = LocalHost::open_for_test(&project, &home, true).unwrap();
        let store = host.store().unwrap();
        store
            .update_secret(0, "unity-dummy", Some(SecretValue::new("synthetic-only")))
            .unwrap();
        // The detector's existing Unity shape has only the node pin. Its
        // private cwd is the server root, while the sole argument names the
        // external source entry that configure must bind and copy.
        let old_cwd = store.server_directory(SERVER).unwrap();
        fs::write(old_cwd.join("retained-old-runtime"), b"old").unwrap();
        let current = Server {
            server_id: SERVER.into(),
            enabled_projects: [host.project().into(), "retained-other-project".into()].into(),
            transport: Destination::Stdio {
                command: node.clone(),
                args: vec![source.join("build/index.js").to_str().unwrap().into()],
                cwd: old_cwd,
                sandbox_profile: "read-only".into(),
                secret_env: [("UNITY_DUMMY".into(), "unity-dummy".into())].into(),
                readable_roots: vec![node.clone()],
                executable_sha256: Some(digest(&node)),
                script: None,
                script_sha256: None,
            },
            protocol_profile: ProtocolProfile::Discover2026,
            capabilities: Capabilities {
                catalog_metadata: true,
                resource_read: false,
                prompt_selection: true,
                tool_call: true,
            },
            timeouts: Timeouts {
                catalog_ttl_seconds: 301,
                connect_seconds: 11,
                catalog_seconds: 31,
                operation_seconds: 121,
                connect_retries: 1,
            },
        };
        let other = Server {
            server_id: "retained-unrelated".into(),
            enabled_projects: ["elsewhere".into()].into(),
            transport: Destination::StreamableHttp {
                url: "https://example.invalid/mcp".into(),
                secret_headers: [("authorization".into(), "unity-dummy".into())].into(),
            },
            protocol_profile: ProtocolProfile::Auto2025Compat,
            capabilities: Capabilities::default(),
            timeouts: Timeouts::default(),
        };
        let before = Registry {
            schema_version: 1,
            revision: 1,
            servers: vec![current, other],
        };
        store.save_registry(0, &before).unwrap();
        Self {
            _root: root,
            root: path,
            home,
            project,
            state,
            source,
            node,
            host,
            before,
        }
    }

    fn store(&self) -> &Store {
        self.host.store().unwrap()
    }

    fn prepare(&self) -> UnityLocalV2Preparation {
        UnityLocalV2Preparation::prepare(&self.host, SERVER, &self.source).unwrap()
    }

    fn effects(&self) -> (ProjectMutationLeaseV1, ManagementEffectStoreV1) {
        let owner =
            ProjectMutationOwnerV1::at_store_root(&self.state.join("rewind-workspace")).unwrap();
        let lease = owner.try_mutation().unwrap();
        let effects =
            ManagementEffectStoreV1::open_at_state_dir(&self.project, &self.state, &lease).unwrap();
        (lease, effects)
    }

    fn registry(&self) -> Registry {
        self.store().load_registry().unwrap().unwrap()
    }

    fn replace_registry(&mut self, change: impl FnOnce(&mut Server)) {
        let mut next = self.before.clone();
        next.revision += 1;
        change(&mut next.servers[0]);
        self.store()
            .save_registry(self.before.revision, &next)
            .unwrap();
        self.before = next;
    }
}

fn make_source(root: &Path) {
    fs::create_dir(root).unwrap();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    for directory in [
        "build",
        "node_modules",
        "node_modules/.bin",
        "node_modules/esbuild",
        "node_modules/esbuild/bin",
        "node_modules/@esbuild",
        "node_modules/@esbuild/darwin-arm64",
        "node_modules/@esbuild/darwin-arm64/bin",
    ] {
        fs::create_dir(root.join(directory)).unwrap();
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(root.join("package.json"), b"{}").unwrap();
    fs::write(root.join("package-lock.json"), b"{}").unwrap();
    fs::write(root.join("build/index.js"), b"server").unwrap();
    fs::write(
        root.join("node_modules/.bin/ignored-synthetic-launcher"),
        b"ignored",
    )
    .unwrap();
    let left = root.join(PAIR[0]);
    let file = fs::File::create(&left).unwrap();
    file.set_len(9_817_266).unwrap();
    drop(file);
    fs::set_permissions(&left, fs::Permissions::from_mode(0o700)).unwrap();
    fs::hard_link(&left, root.join(PAIR[1])).unwrap();
}

fn digest(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

#[derive(Debug, Eq, PartialEq)]
struct Observed {
    identity: (u64, u64),
    mode: u32,
    links: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    content: Option<String>,
}

/// Ignores access time only: scans may read files but may not create/chmod,
/// replace, truncate, append, or otherwise alter any fixture artifact.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Observed> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Observed>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        result.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            Observed {
                identity: (metadata.dev(), metadata.ino()),
                mode: metadata.mode(),
                links: metadata.nlink(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
                content: metadata.is_file().then(|| digest(path)),
            },
        );
        if metadata.is_dir() {
            for child in fs::read_dir(path).unwrap() {
                visit(root, &child.unwrap().path(), result);
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}
