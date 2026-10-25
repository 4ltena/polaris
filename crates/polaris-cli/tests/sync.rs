//! Two local devices exchanging through a bare repository; no network.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "support/product_binary.rs"]
mod product_binary;

struct Device {
    _data: tempfile::TempDir,
    _project: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Device {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        // A project marker so the root resolves to this directory.
        fs::create_dir_all(root.join(".polaris")).unwrap();
        Self {
            data: data.path().canonicalize().unwrap(),
            root,
            _data: data,
            _project: project,
        }
    }

    fn run(&self, arguments: &[&str]) -> (bool, String) {
        let output = product_binary::command()
            .args(["sync"])
            .args(arguments)
            .current_dir(&self.root)
            .env("POLARIS_DATA_DIR", &self.data)
            .env("HOME", &self.data)
            .output()
            .expect("could not launch");
        (
            output.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        )
    }

    /// Synthetic legacy participation for local pull/apply/leave fixtures.
    /// This does not exercise or prove the production C08 join contract.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn join(&self, remote: &str) -> (bool, String) {
        use polaris_core::sync::{
            participation,
            share::{ShareLink, ShareManifest},
        };
        use std::os::unix::fs::PermissionsExt;

        assert!(remote.starts_with("file://"), "fixture must stay local");
        let state = self
            .data
            .join("state")
            .join(polaris_core::project::project_id(&self.root));
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        // participation::join acquires the shared project lease under this
        // dedicated state root; no process-wide HOME/data override is needed.
        match participation::join(
            &state,
            ShareLink {
                remote: remote.into(),
                share_id: "share-0001".into(),
                invitation_id: "invite-0001".into(),
                expires_unix_seconds: 4102444800,
            },
            ShareManifest {
                version: 1,
                excluded: Vec::new(),
            },
            1,
        ) {
            Ok(_) => (true, "synthetic legacy participation fixture".into()),
            Err(error) => (false, error.to_string()),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn join(&self, remote: &str) -> (bool, String) {
        self.run(&[
            "join",
            "--remote",
            remote,
            "--share-id",
            "share-0001",
            "--invitation-id",
            "invite-0001",
            "--expires",
            "4102444800",
        ])
    }

    /// Seed only a disposable file:// remote for receive/apply fixtures.
    /// This neither calls nor proves the production C10 sender.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn seed_received_fixture(&self) -> polaris_core::sync::repository::Staged {
        use polaris_core::sync::{participation, repository::SyncRepository, share::SyncPaths};
        let state = self
            .data
            .join("state")
            .join(polaris_core::project::project_id(&self.root));
        let held = participation::read(&state)
            .expect("synthetic joined sender")
            .participation()
            .unwrap()
            .clone();
        assert!(
            held.share.remote.starts_with("file://"),
            "fixture must stay local"
        );
        let repository = SyncRepository::open(SyncPaths::under(&state)).unwrap();
        let staged = repository.stage_from(&self.root, &held.manifest).unwrap();
        repository.commit("local receive fixture").unwrap();
        repository.send(&held.share.remote).unwrap();
        #[cfg(target_os = "macos")]
        {
            // Preserve the legacy sender's baseline for subsequent conflict
            // fixtures, without inventing C10 authority or a send receipt.
            let mut held = held;
            held.agreed_commit = repository.head();
            held.applied_commit = held.agreed_commit.clone();
            participation::write(&state, &held).unwrap();
        }
        staged
    }

    fn write(&self, name: &str, body: &str) {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
}

fn remote(directory: &Path) -> String {
    let path = directory.join("sync.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet", "--initial-branch=polaris"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    // A local bare repository stands in for the shared remote.
    format!("file://{}", path.to_string_lossy())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
type TreeSnapshotEntry = (PathBuf, bool, u64, u64, u32, u64, i64, i64, Vec<u8>);

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tree_snapshot(root: &Path) -> Vec<TreeSnapshotEntry> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<TreeSnapshotEntry>) {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path).unwrap();
        let is_dir = metadata.is_dir();
        let body = if is_dir {
            Vec::new()
        } else {
            fs::read(path).unwrap()
        };
        entries.push((
            path.strip_prefix(root).unwrap().to_path_buf(),
            is_dir,
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            body,
        ));
        if is_dir {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sync_apply_uses_shared_owner_and_preserves_local_files() {
    let unjoined = Device::new();
    unjoined.write("keep.txt", "unchanged\n");
    let before = tree_snapshot(&unjoined.root);
    let (ok, message) = unjoined.run(&["apply"]);
    assert!(
        !ok && message.contains("has not joined a share"),
        "{message}"
    );
    assert_eq!(before, tree_snapshot(&unjoined.root));
    let state = unjoined
        .data
        .join("state")
        .join(polaris_core::project::project_id(&unjoined.root));
    assert!(!state.join("apply-intents-v1").exists());
    assert!(!state.join("management-effects-v1").exists());
    assert!(!state.join("sync/recovery").exists());

    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());
    let sender = Device::new();
    assert!(sender.join(&remote).0);
    sender.write("incoming.txt", "remote content\n");
    sender.seed_received_fixture();
    let receiver = Device::new();
    assert!(receiver.join(&remote).0);
    let (ok, message) = receiver.run(&["pull"]);
    assert!(ok, "{message}");
    receiver.write("keep.txt", "local content\n");
    let (ok, message) = receiver.run(&["apply"]);
    assert!(ok && message.contains("Applied"), "{message}");
    assert!(!message.contains("disconnected"));
    assert_eq!(
        fs::read(receiver.root.join("incoming.txt")).unwrap(),
        b"remote content\n"
    );
    assert_eq!(
        fs::read(receiver.root.join("keep.txt")).unwrap(),
        b"local content\n"
    );
    let state = receiver
        .data
        .join("state")
        .join(polaris_core::project::project_id(&receiver.root));
    let held = polaris_core::sync::participation::read(&state)
        .unwrap()
        .participation()
        .unwrap()
        .clone();
    assert_eq!(held.agreed_commit, held.applied_commit);
    assert!(held.agreed_commit.is_some());
    assert_eq!(
        fs::read_dir(state.join("apply-intents-v1/active"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(state.join("apply-intents-v1/archive"))
            .unwrap()
            .count(),
        1
    );
    run_owner_fixture(&receiver, "inspect");
    let before = tree_snapshot(&receiver.root);
    let (ok, message) = receiver.run(&["apply"]);
    assert!(ok && message.contains("already been applied"), "{message}");
    assert_eq!(before, tree_snapshot(&receiver.root));
    assert_eq!(
        fs::read_dir(state.join("apply-intents-v1/archive"))
            .unwrap()
            .count(),
        1
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sync_staging_is_refused_while_shared_owner_holds_the_lease() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());
    let device = Device::new();
    assert!(device.join(&remote).0);
    device.write("main.rs", "unchanged\n");
    let before = tree_snapshot(&device.root);
    let state = device
        .data
        .join("state")
        .join(polaris_core::project::project_id(&device.root));
    let participation = fs::read(state.join("sync/participation.json")).unwrap();
    run_owner_fixture(&device, "lease");
    assert_eq!(before, tree_snapshot(&device.root));
    assert_eq!(
        participation,
        fs::read(state.join("sync/participation.json")).unwrap()
    );
    assert!(!state.join("sync/recovery").exists());
    assert!(!state.join("apply-intents-v1").exists());
}

#[cfg(target_os = "linux")]
fn run_owner_fixture(device: &Device, role: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "sync_owner_fixture_child",
            "--nocapture",
        ])
        .current_dir(&device.root)
        .env("POLARIS_DATA_DIR", &device.data)
        .env("HOME", &device.data)
        .env("POLARIS_SYNC_FIXTURE_ROLE", role)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sync_refuses_unsupported_root_encoding_before_staging() {
    use std::os::unix::ffi::OsStringExt;
    for root_kind in ["project", "state"] {
        let mut device = Device::new();
        let component = std::ffi::OsString::from_vec(b"unsupported-\xff".to_vec());
        if root_kind == "project" {
            device.root = device.root.join(component);
            fs::create_dir_all(device.root.join(".polaris")).unwrap();
        } else {
            device.data = device.data.join(component);
            fs::create_dir(&device.data).unwrap();
        }
        device.write("keep.txt", "unchanged\n");
        let before = tree_snapshot(&device.root);
        let (ok, message) = device.run(&["apply"]);
        assert!(!ok, "{message}");
        assert_eq!(
            message.trim(),
            format!(
                "{{\"schema_version\":1,\"code\":\"root_encoding_unsupported\",\"root\":\"{root_kind}\"}}"
            )
        );
        assert_eq!(before, tree_snapshot(&device.root));
        let state = device
            .data
            .join("state")
            .join(polaris_core::project::project_id(&device.root));
        for artifact in [
            "sync",
            "rewind-workspace",
            "apply-intents-v1",
            "management-effects-v1",
        ] {
            assert!(!state.join(artifact).exists(), "unexpected {artifact}");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "launched with explicit fixture data root by the sync integration tests"]
fn sync_owner_fixture_child() {
    let project = std::env::current_dir().unwrap();
    let owner = polaris_core::apply_owner::ApplyOwnerV1::acquire(&project).unwrap();
    match std::env::var("POLARIS_SYNC_FIXTURE_ROLE").unwrap().as_str() {
        "inspect" => match owner.inspect_management().unwrap() {
            polaris_core::management_effect::ManagementEffectInspectOpenV1::Present(inspected) => {
                assert!(inspected.effects().any(|effect| effect.request.inventory.to_string() == "C12" && matches!(effect.state, polaris_core::management_effect::ManagementEffectStateV1::Completed { .. })));
            }
            _ => panic!("the CLI bypassed the shared effect owner"),
        },
        "lease" => {
            let state = polaris_core::project::state_dir(&project).unwrap();
            let before = tree_snapshot(&project);
            let participation = fs::read(state.join("sync/participation.json")).unwrap();
            for command in ["push", "pull", "apply", "leave"] {
                let output = product_binary::command()
                    .args(["sync", command])
                    .current_dir(&project)
                    .env(
                        "POLARIS_DATA_DIR",
                        std::env::var_os("POLARIS_DATA_DIR").unwrap(),
                    )
                    .env("HOME", std::env::var_os("HOME").unwrap())
                    .output()
                    .unwrap();
                let message = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let refused = if command == "push" {
                    // C10 checks platform support before acquiring the send
                    // owner. Receive/apply/leave still exercise the held lease.
                    message.starts_with("c10_unavailable_platform:")
                } else {
                    message.contains("Cannot start the sharing operation")
                };
                assert!(!output.status.success() && refused, "{command}: {message}");
                assert_eq!(before, tree_snapshot(&project));
                assert_eq!(
                    participation,
                    fs::read(state.join("sync/participation.json")).unwrap()
                );
            }
        }
        role => panic!("unknown fixture role {role}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sync_apply_refuses_deferred_directories_without_intent_or_source_effect() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());
    let sender = Device::new();
    assert!(sender.join(&remote).0);
    sender.write("new-directory/main.rs", "remote content\n");
    sender.seed_received_fixture();
    let receiver = Device::new();
    assert!(receiver.join(&remote).0);
    assert!(receiver.run(&["pull"]).0);
    let state = receiver
        .data
        .join("state")
        .join(polaris_core::project::project_id(&receiver.root));
    let management = state.join("management-effects-v1");
    let effects_before = management.exists().then(|| tree_snapshot(&management));
    let before = tree_snapshot(&receiver.root);
    let (ok, message) = receiver.run(&["apply"]);
    assert!(!ok && message.contains("new directories"), "{message}");
    assert_eq!(before, tree_snapshot(&receiver.root));
    assert!(!state.join("apply-intents-v1").exists());
    assert_eq!(
        effects_before,
        management.exists().then(|| tree_snapshot(&management)),
        "refused apply must preserve the effects already published by join"
    );
    let held = polaris_core::sync::participation::read(&state)
        .unwrap()
        .participation()
        .unwrap()
        .clone();
    assert_ne!(held.agreed_commit, held.applied_commit);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sync_apply_refuses_cross_device_recovery_before_creation() {
    use std::os::unix::fs::MetadataExt;
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());
    let sender = Device::new();
    assert!(sender.join(&remote).0);
    sender.write("main.rs", "remote content\n");
    sender.seed_received_fixture();
    let mut receiver = Device::new();
    let data = tempfile::Builder::new()
        .prefix("cli-sync-cross-device-")
        .tempdir_in("/dev/shm")
        .unwrap();
    receiver.data = data.path().canonicalize().unwrap();
    receiver._data = data;
    assert_ne!(
        fs::metadata(&receiver.root).unwrap().dev(),
        fs::metadata(&receiver.data).unwrap().dev(),
        "the Linux tmpfs recovery fixture needs distinct devices"
    );
    let (joined, message) = receiver.join(&remote);
    assert!(joined, "cross-device legacy fixture setup: {message}");
    let (ok, message) = receiver.run(&["pull"]);
    assert!(ok, "{message}");
    let state = receiver
        .data
        .join("state")
        .join(polaris_core::project::project_id(&receiver.root));
    let management = state.join("management-effects-v1");
    let effects_before = management.exists().then(|| tree_snapshot(&management));
    let before = tree_snapshot(&receiver.root);
    let participation = fs::read(state.join("sync/participation.json")).unwrap();
    let (ok, message) = receiver.run(&["apply"]);
    assert!(!ok && message.contains("same filesystem"), "{message}");
    assert_eq!(before, tree_snapshot(&receiver.root));
    assert_eq!(
        participation,
        fs::read(state.join("sync/participation.json")).unwrap()
    );
    assert!(!state.join("sync/recovery").exists());
    assert!(!state.join("apply-intents-v1").exists());
    assert_eq!(
        effects_before,
        management.exists().then(|| tree_snapshot(&management)),
        "refused apply must preserve the effects already published by join"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sync_apply_refuses_symlink_recovery_before_publication() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());
    let sender = Device::new();
    assert!(sender.join(&remote).0);
    sender.write("main.rs", "remote content\n");
    sender.seed_received_fixture();
    let receiver = Device::new();
    assert!(receiver.join(&remote).0);
    assert!(receiver.run(&["pull"]).0);
    let state = receiver
        .data
        .join("state")
        .join(polaris_core::project::project_id(&receiver.root));
    let foreign = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(foreign.path(), state.join("sync/recovery")).unwrap();
    let management = state.join("management-effects-v1");
    let effects_before = management.exists().then(|| tree_snapshot(&management));
    let before = tree_snapshot(&receiver.root);
    let participation = fs::read(state.join("sync/participation.json")).unwrap();
    let foreign_before = tree_snapshot(foreign.path());
    let (ok, message) = receiver.run(&["apply"]);
    assert!(!ok && message.contains("Recovered"), "{message}");
    assert_eq!(before, tree_snapshot(&receiver.root));
    assert_eq!(foreign_before, tree_snapshot(foreign.path()));
    assert_eq!(
        participation,
        fs::read(state.join("sync/participation.json")).unwrap()
    );
    assert_eq!(
        fs::read_link(state.join("sync/recovery")).unwrap(),
        foreign.path()
    );
    assert!(!state.join("apply-intents-v1").exists());
    assert_eq!(
        effects_before,
        management.exists().then(|| tree_snapshot(&management)),
        "refused apply must preserve the effects already published by join"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn two_devices_share_a_project_and_keep_both_versions_of_a_clash() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());

    let first = Device::new();
    assert!(!first.run(&["status"]).0, "not joined yet");
    let (ok, message) = first.join(&remote);
    assert!(ok, "{message}");

    first.write("src/main.rs", "fn main() {}\n");
    first.write(".env", "SECRET=1\n");
    #[cfg(target_os = "macos")]
    assert_eq!(first.seed_received_fixture().files, 1);
    #[cfg(not(target_os = "macos"))]
    {
        let (ok, message) = first.run(&["push"]);
        assert!(ok, "{message}");
        assert!(message.contains("Sent: 1 items"), "{message}");
    }

    let second = Device::new();
    second.join(&remote);
    let (ok, message) = second.run(&["pull"]);
    assert!(ok, "{message}");
    assert!(message.contains("Received: 1"), "{message}");
    // A credential file is never shared.
    let (_, status) = second.run(&["status"]);
    assert!(!status.contains("SECRET"), "{status}");

    // Both devices change the same line from the same base.
    fs::create_dir_all(second.root.join("src")).unwrap();
    assert!(second.run(&["apply"]).0);
    second.write("src/main.rs", "fn main() { second() }\n");
    #[cfg(target_os = "macos")]
    second.seed_received_fixture();
    #[cfg(not(target_os = "macos"))]
    assert!(second.run(&["push"]).0);
    first.write("src/main.rs", "fn main() { first() }\n");
    let (ok, message) = first.run(&["pull"]);
    assert!(ok, "{message}");
    assert!(message.contains("Both versions retained"), "{message}");
    assert!(message.contains("src/main.rs"), "{message}");

    // The project keeps what this device wrote; nothing was overwritten.
    assert_eq!(
        fs::read_to_string(first.root.join("src/main.rs")).unwrap(),
        "fn main() { first() }\n"
    );
    let (_, status) = first.run(&["status"]);
    assert!(status.contains("Unresolved: src/main.rs"), "{status}");

    // With something unresolved, applying is refused outright.
    let (ok, message) = first.run(&["apply"]);
    assert!(!ok && message.contains("1 unresolved items"), "{message}");

    // A device with nothing unresolved takes the shared content into its
    // project, and what it never shared stays where it is.
    let third = Device::new();
    third.join(&remote);
    third.write("local-note.md", "手元のメモ\n");
    // The directory the shared file lives in already exists here.
    fs::create_dir_all(third.root.join("src")).unwrap();
    assert!(third.run(&["pull"]).0);
    let (ok, message) = third.run(&["apply"]);
    assert!(ok, "{message}");
    assert!(
        fs::read_to_string(third.root.join("src/main.rs"))
            .unwrap()
            .contains("main"),
        "{message}"
    );
    assert_eq!(
        fs::read_to_string(third.root.join("local-note.md")).unwrap(),
        "手元のメモ\n"
    );

    assert!(first.run(&["leave"]).0);
    assert!(!first.run(&["status"]).0);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn applying_needs_an_agreed_state_and_never_removes_what_was_never_shared() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());

    let first = Device::new();
    assert!(first.join(&remote).0);
    first.write("src/main.rs", "fn main() {}\n");
    #[cfg(target_os = "macos")]
    first.seed_received_fixture();
    #[cfg(not(target_os = "macos"))]
    assert!(first.run(&["push"]).0);

    // Joining alone agrees to nothing, so applying must not empty the project.
    let second = Device::new();
    assert!(second.join(&remote).0);
    second.write("src/own.rs", "手元のもの\n");
    let (ok, message) = second.run(&["apply"]);
    assert!(
        !ok && message.contains("No agreed content yet"),
        "{message}"
    );
    assert!(second.root.join("src/own.rs").exists(), "{message}");

    // After receiving, the shared file arrives and this device's own file and
    // its later edits are left alone.
    assert!(second.run(&["pull"]).0);
    let (ok, message) = second.run(&["apply"]);
    assert!(ok, "{message}");
    assert_eq!(
        fs::read_to_string(second.root.join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    second.write("notes.md", "反映のあとに書いた\n");
    second.write("src/own.rs", "書き換えた\n");

    // The same agreed state is never applied a second time.
    let (ok, message) = second.run(&["apply"]);
    assert!(ok && message.contains("already been applied"), "{message}");
    assert_eq!(
        fs::read_to_string(second.root.join("notes.md")).unwrap(),
        "反映のあとに書いた\n"
    );
    assert_eq!(
        fs::read_to_string(second.root.join("src/own.rs")).unwrap(),
        "書き換えた\n"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn sending_is_refused_while_received_work_has_not_reached_the_project() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());

    let first = Device::new();
    assert!(first.join(&remote).0);
    first.write("README.md", "最初の版\n");
    #[cfg(target_os = "macos")]
    first.seed_received_fixture();
    #[cfg(not(target_os = "macos"))]
    assert!(first.run(&["push"]).0);

    let second = Device::new();
    assert!(second.join(&remote).0);
    second.write("src/own.rs", "手元のもの\n");
    assert!(second.run(&["pull"]).0);

    // Sending now would record the received file as a removal.
    let (ok, message) = second.run(&["push"]);
    #[cfg(target_os = "macos")]
    assert!(
        !ok && message.trim() == "c10_permission_unavailable",
        "{message}"
    );
    #[cfg(not(target_os = "macos"))]
    assert!(!ok && message.contains("`polaris sync apply`"), "{message}");

    assert!(second.run(&["apply"]).0);
    #[cfg(target_os = "macos")]
    {
        let source = tree_snapshot(&second.root);
        let state = tree_snapshot(&second.data);
        let remote_before = tree_snapshot(bare.path());
        let (ok, message) = second.run(&["push"]);
        assert!(
            !ok && message.trim() == "c10_permission_unavailable",
            "{message}"
        );
        assert_eq!(source, tree_snapshot(&second.root));
        assert_eq!(state, tree_snapshot(&second.data));
        assert_eq!(remote_before, tree_snapshot(bare.path()));
        second.seed_received_fixture();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let (ok, message) = second.run(&["push"]);
        assert!(ok, "{message}");
    }

    // The other device still has what it sent.
    assert!(first.run(&["pull"]).0);
    let (ok, message) = first.run(&["apply"]);
    assert!(ok, "{message}");
    assert_eq!(
        fs::read_to_string(first.root.join("README.md")).unwrap(),
        "最初の版\n"
    );
}

#[test]
fn every_env_variant_and_generated_output_stays_on_this_device() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());

    let device = Device::new();
    assert!(device.join(&remote).0);
    device.write("package.json", "{}\n");
    for secret in [".env", ".env.local", ".env.production.local", ".envrc"] {
        device.write(secret, "OPENAI_API_KEY=sk-real\n");
    }
    device.write(".next/build-manifest.json", "{}\n");
    device.write("coverage/lcov.info", "TN:\n");
    #[cfg(target_os = "linux")]
    {
        let before = tree_snapshot(&device.root);
        let (ok, message) = device.run(&["push"]);
        assert!(
            !ok && message.starts_with("c10_unavailable_platform:"),
            "{message}"
        );
        assert_eq!(before, tree_snapshot(&device.root));
        assert_eq!(device.seed_received_fixture().files, 1);
    }
    #[cfg(target_os = "macos")]
    {
        let before = tree_snapshot(&device.root);
        let state = tree_snapshot(&device.data);
        let remote_before = tree_snapshot(bare.path());
        let (ok, message) = device.run(&["push"]);
        assert!(
            !ok && message.trim() == "c10_permission_unavailable",
            "{message}"
        );
        assert_eq!(before, tree_snapshot(&device.root));
        assert_eq!(state, tree_snapshot(&device.data));
        assert_eq!(remote_before, tree_snapshot(bare.path()));
        assert_eq!(device.seed_received_fixture().files, 1);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let (ok, message) = device.run(&["push"]);
        assert!(ok, "{message}");
        assert!(message.contains("Sent: 1 items"), "{message}");
    }

    // Read the shared history back from the remote itself.
    let listing = Command::new("git")
        .args(["--git-dir"])
        .arg(bare.path().canonicalize().unwrap().join("sync.git"))
        .args(["ls-tree", "-r", "--name-only", "polaris"])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert_eq!(listing.trim(), "package.json", "{listing}");
}
