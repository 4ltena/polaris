//! Two local devices exchanging through a bare repository; no network.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("polaris")
}

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
        let output = Command::new(bin())
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
    let (ok, message) = first.run(&["push"]);
    assert!(ok, "{message}");
    assert!(message.contains("送信しました: 1件"), "{message}");

    let second = Device::new();
    second.join(&remote);
    let (ok, message) = second.run(&["pull"]);
    assert!(ok, "{message}");
    assert!(message.contains("受け取りました: 1件"), "{message}");
    // A credential file is never shared.
    let (_, status) = second.run(&["status"]);
    assert!(!status.contains("SECRET"), "{status}");

    // Both devices change the same line from the same base.
    fs::create_dir_all(second.root.join("src")).unwrap();
    assert!(second.run(&["apply"]).0);
    second.write("src/main.rs", "fn main() { second() }\n");
    assert!(second.run(&["push"]).0);
    first.write("src/main.rs", "fn main() { first() }\n");
    let (ok, message) = first.run(&["pull"]);
    assert!(ok, "{message}");
    assert!(message.contains("双方の内容を残しました"), "{message}");
    assert!(message.contains("src/main.rs"), "{message}");

    // The project keeps what this device wrote; nothing was overwritten.
    assert_eq!(
        fs::read_to_string(first.root.join("src/main.rs")).unwrap(),
        "fn main() { first() }\n"
    );
    let (_, status) = first.run(&["status"]);
    assert!(status.contains("解消待ち: src/main.rs"), "{status}");

    // With something unresolved, applying is refused outright.
    let (ok, message) = first.run(&["apply"]);
    assert!(!ok && message.contains("解消待ちが1件"), "{message}");

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

#[test]
fn applying_needs_an_agreed_state_and_never_removes_what_was_never_shared() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());

    let first = Device::new();
    assert!(first.join(&remote).0);
    first.write("src/main.rs", "fn main() {}\n");
    assert!(first.run(&["push"]).0);

    // Joining alone agrees to nothing, so applying must not empty the project.
    let second = Device::new();
    assert!(second.join(&remote).0);
    second.write("src/own.rs", "手元のもの\n");
    let (ok, message) = second.run(&["apply"]);
    assert!(
        !ok && message.contains("まだ合意した内容がありません"),
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
    assert!(ok && message.contains("反映済み"), "{message}");
    assert_eq!(
        fs::read_to_string(second.root.join("notes.md")).unwrap(),
        "反映のあとに書いた\n"
    );
    assert_eq!(
        fs::read_to_string(second.root.join("src/own.rs")).unwrap(),
        "書き換えた\n"
    );
}

#[test]
fn sending_is_refused_while_received_work_has_not_reached_the_project() {
    let bare = tempfile::tempdir().unwrap();
    let remote = remote(&bare.path().canonicalize().unwrap());

    let first = Device::new();
    assert!(first.join(&remote).0);
    first.write("README.md", "最初の版\n");
    assert!(first.run(&["push"]).0);

    let second = Device::new();
    assert!(second.join(&remote).0);
    second.write("src/own.rs", "手元のもの\n");
    assert!(second.run(&["pull"]).0);

    // Sending now would record the received file as a removal.
    let (ok, message) = second.run(&["push"]);
    assert!(!ok && message.contains("`polaris sync apply`"), "{message}");

    assert!(second.run(&["apply"]).0);
    let (ok, message) = second.run(&["push"]);
    assert!(ok, "{message}");

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
    let (ok, message) = device.run(&["push"]);
    assert!(ok, "{message}");
    assert!(message.contains("送信しました: 1件"), "{message}");

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
