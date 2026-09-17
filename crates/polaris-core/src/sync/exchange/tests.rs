//! Two local devices and a bare repository standing in for the remote.
use super::*;
use crate::sync::repository::SyncRepository;
use crate::sync::share::{ShareManifest, SyncPaths};
use std::fs;
use std::path::PathBuf;

struct Device {
    _state: tempfile::TempDir,
    _project: tempfile::TempDir,
    root: PathBuf,
    repository: SyncRepository,
}

impl Device {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let repository = SyncRepository::open(SyncPaths::under(state.path())).unwrap();
        Self {
            _state: state,
            _project: project,
            root,
            repository,
        }
    }

    fn write(&self, name: &str, body: &str) {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn commit(&self, message: &str) -> Option<String> {
        self.repository
            .stage_from(&self.root, &ShareManifest::default())
            .unwrap();
        self.repository.commit(message).unwrap()
    }
}

fn remote() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().canonicalize().unwrap().join("sync.git");
    let status = std::process::Command::new("git")
        .args(["init", "--bare", "--quiet", "--initial-branch=polaris"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    (directory, path.to_string_lossy().into_owned())
}

#[test]
fn a_change_from_the_other_device_merges_without_touching_the_project() {
    let (_remote_directory, remote) = remote();
    let first = Device::new();
    first.write("src/main.rs", "fn main() {}\n");
    first.commit("first").unwrap();
    first.repository.send(&remote).unwrap();

    let second = Device::new();
    second
        .repository
        .receive(&remote)
        .unwrap()
        .expect("commits");
    let exchange = second.repository.merge_received().unwrap();
    assert!(!exchange.unresolved);
    assert!(exchange.merged_commit.is_some());
    // The project directory is untouched: applying is a separate step.
    assert!(!second.root.join("src").exists());

    // A change on a different file merges mechanically.
    second.write("src/main.rs", "fn main() {}\n");
    second.write("README.md", "説明\n");
    second.commit("second").unwrap();
    second.repository.send(&remote).unwrap();
    first.repository.receive(&remote).unwrap();
    let exchange = first.repository.merge_received().unwrap();
    assert!(!exchange.unresolved, "{exchange:?}");
    assert!(
        exchange
            .changes
            .iter()
            .any(|change| change.path == "README.md")
    );
}

#[test]
fn edits_to_the_same_lines_are_kept_from_both_sides_and_nothing_is_merged() {
    let (_remote_directory, remote) = remote();
    let first = Device::new();
    first.write("shared.txt", "one\n");
    first.commit("base").unwrap();
    first.repository.send(&remote).unwrap();

    let second = Device::new();
    second.repository.receive(&remote).unwrap();
    second.repository.merge_received().unwrap();
    // Both devices edit the same line from the same base.
    second.write("shared.txt", "two\n");
    // The second device's project only has the file it just wrote, which is
    // what a real device would hold after applying.
    second.commit("second").unwrap();
    second.repository.send(&remote).unwrap();

    first.write("shared.txt", "three\n");
    first.commit("first-edit").unwrap();
    first.repository.receive(&remote).unwrap();
    let exchange = first.repository.merge_received().unwrap();
    assert!(exchange.unresolved);
    assert_eq!(exchange.merged_commit, None, "nothing is merged");
    assert_eq!(
        exchange.needs_attention(),
        vec![&Change {
            path: "shared.txt".into(),
            resolution: Resolution::BothKept {
                reason: Kept::Overlapping
            },
        }]
    );
    // The local branch did not move, so the earlier work is still there.
    assert!(first.repository.head().is_some());
}

#[test]
fn a_deletion_against_an_edit_is_never_resolved_by_taking_one_side() {
    let (_remote_directory, remote) = remote();
    let first = Device::new();
    first.write("notes.md", "元の内容\n");
    first.write("keep.md", "残す\n");
    first.commit("base").unwrap();
    first.repository.send(&remote).unwrap();

    let second = Device::new();
    second.repository.receive(&remote).unwrap();
    second.repository.merge_received().unwrap();
    second.write("keep.md", "残す\n");
    // The second device removed the file the first one is editing.
    second.commit("deleted").unwrap();
    second.repository.send(&remote).unwrap();

    first.write("notes.md", "編集した内容\n");
    first.commit("edited").unwrap();
    first.repository.receive(&remote).unwrap();
    let exchange = first.repository.merge_received().unwrap();
    assert!(exchange.unresolved);
    assert_eq!(
        exchange.needs_attention(),
        vec![&Change {
            path: "notes.md".into(),
            resolution: Resolution::BothKept {
                reason: Kept::DeleteVersusEdit
            },
        }]
    );
}
