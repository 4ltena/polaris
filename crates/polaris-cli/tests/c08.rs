//! Actual CLI rejection boundaries with disposable HOME, data and project roots.
//! The production entry gate still reads the trusted OS home before parsing;
//! these tests do not write there or reach Keychain, Git or a network remote.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const REMOTE: &str = "https://example.invalid/polaris/c08.git";
const OPERATION_ID: &str = "11111111111111111111111111111111";
#[cfg(unix)]
const JOIN_ARGUMENTS: &[&str] = &[
    "join",
    "--remote",
    REMOTE,
    "--share-id",
    "share-0001",
    "--invitation-id",
    "invite-0001",
    "--expires",
    "4102444800",
    "--account",
    "disposable-c08-fixture",
];

struct Fixture {
    directory: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let home = root.join("home");
        let data = root.join("data");
        let project = root.join("project");
        for path in [&home, &data, &project] {
            fs::create_dir(path).unwrap();
        }
        fs::create_dir(project.join(".polaris")).unwrap();
        fs::write(project.join("keep.txt"), b"local content\n").unwrap();
        Self {
            directory,
            home,
            data,
            project,
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_polaris"))
            .args(["sync"])
            .args(arguments)
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", &self.home)
            .env("POLARIS_DATA_DIR", &self.data)
            .env("TMPDIR", self.directory.path())
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .expect("could not launch the actual CLI")
    }

    fn assert_no_mutation(&self, before: &[(PathBuf, Option<Vec<u8>>)]) {
        assert_eq!(before, tree_snapshot(self.directory.path()).as_slice());
        let state = self
            .data
            .join("state")
            .join(polaris_core::project::project_id(&self.project));
        for artifact in [
            "sync/participation.json",
            "sync/credential-rebind-v1",
            "management-effects-v1",
            "apply-intents-v1",
            "rewind-workspace",
        ] {
            assert!(!state.join(artifact).exists(), "unexpected {artifact}");
        }
        assert!(
            !self
                .home
                .join(".polaris/git-credential-bindings-v1/fingerprint.key")
                .exists()
        );
    }
}

fn tree_snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(
            metadata.is_dir() || metadata.is_file(),
            "unexpected fixture file type: {}",
            path.display()
        );
        let body = metadata.is_file().then(|| fs::read(path).unwrap());
        entries.push((path.strip_prefix(root).unwrap().to_path_buf(), body));
        if metadata.is_dir() {
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

fn message(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(unix)]
fn assert_runtime_refusal(arguments: &[&str], expected: &str) {
    let fixture = Fixture::new();
    let before = tree_snapshot(fixture.directory.path());
    let output = fixture.run(arguments);
    let message = message(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(message.contains(expected), "{message}");
    fixture.assert_no_mutation(&before);
}

#[cfg(unix)]
fn non_tty_refusal() -> &'static str {
    if cfg!(target_os = "macos") {
        "C08の確定にはTTY確認または--yesが必要です。"
    } else {
        "共有参加とcredential rebindはmacOSのData Protection Keychainでのみ利用できます。"
    }
}

#[test]
#[cfg(unix)]
fn c08_normal_join_without_yes_refuses_non_tty_before_mutation() {
    assert_runtime_refusal(JOIN_ARGUMENTS, non_tty_refusal());
}

#[test]
#[cfg(unix)]
fn c08_rebind_without_yes_refuses_non_tty_before_mutation() {
    assert_runtime_refusal(&["join", "--rebind-credential"], non_tty_refusal());
}

#[test]
#[cfg(unix)]
fn c08_recover_without_yes_refuses_non_tty_before_mutation() {
    assert_runtime_refusal(&["join", "--recover", OPERATION_ID], non_tty_refusal());
}

#[test]
#[cfg(unix)]
fn c08_abandon_without_yes_refuses_non_tty_before_mutation() {
    assert_runtime_refusal(&["join", "--abandon", OPERATION_ID], non_tty_refusal());
}

#[test]
fn c08_joined_at_requires_abandon_and_remote() {
    for (arguments, missing) in [
        (
            vec!["join", "--joined-at", "1"],
            vec!["--abandon", "--remote"],
        ),
        (
            vec!["join", "--joined-at", "1", "--abandon", OPERATION_ID],
            vec!["--remote"],
        ),
        (
            vec!["join", "--joined-at", "1", "--remote", REMOTE],
            vec!["--abandon"],
        ),
    ] {
        let fixture = Fixture::new();
        let before = tree_snapshot(fixture.directory.path());
        let output = fixture.run(&arguments);
        let message = message(&output);
        assert_eq!(output.status.code(), Some(2), "{message}");
        assert!(
            message.contains("the following required arguments were not provided"),
            "{message}"
        );
        for flag in missing {
            assert!(message.contains(flag), "{message}");
        }
        fixture.assert_no_mutation(&before);
    }
}

#[test]
#[cfg(unix)]
fn c08_explicit_join_abandon_preserves_fields_until_confirmation() {
    let mut arguments = JOIN_ARGUMENTS.to_vec();
    arguments.extend(["--abandon", OPERATION_ID, "--joined-at", "1"]);
    assert_runtime_refusal(&arguments, non_tty_refusal());

    // The complete request reaches confirmation. Omitting a reconstructed join
    // field must fail before that boundary, proving abandon did not discard it.
    for (flag, diagnostic) in [
        ("--share-id", "--share-idが必要です"),
        ("--invitation-id", "--invitation-idが必要です"),
        ("--expires", "--expiresが必要です"),
        (
            "--joined-at",
            "join中止の再構成には確認時の--joined-atが必要です",
        ),
    ] {
        let mut incomplete = arguments.clone();
        let index = incomplete.iter().position(|value| *value == flag).unwrap();
        incomplete.drain(index..index + 2);
        assert_runtime_refusal(&incomplete, diagnostic);
    }
}
