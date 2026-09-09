//! macOS isolated filesystem, environment, scratch, and ordinary tool compatibility tests.
#![cfg(target_os = "macos")]

use polaris_sandbox::{SandboxMode, SandboxPolicy, run_confined};
use std::path::{Path, PathBuf};

fn isolated_policy(mode: SandboxMode, root: &Path, runtimes: &[PathBuf]) -> SandboxPolicy {
    let home = root.join("scratch-home");
    let tmpdir = root.join("scratch-tmp");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&tmpdir).unwrap();
    SandboxPolicy::isolated(mode, root, runtimes)
        .unwrap()
        .with_isolated_environment(&home, &tmpdir)
        .unwrap()
}

#[test]
fn isolated_environment_does_not_leak_parent_values_to_children_or_grandchildren() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "isolated_environment_subprocess_fixture",
            "--nocapture",
        ])
        .env_clear()
        .env("POLARIS_ISOLATED_ENV_FIXTURE", "1")
        .env("HOME", "DUMMY_PARENT_HOME")
        .env("PATH", "DUMMY_PARENT_PATH")
        .env("LANG", "DUMMY_PARENT_LANG")
        .env("ORDINARY_LABEL", "DUMMY_SECRET_VALUE")
        .env("POLARIS_API_KEY", "DUMMY_SECRET_VALUE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("isolated-env-verified"));
}

#[test]
#[ignore = "child fixture launched by isolated_environment_does_not_leak_parent_values_to_children_or_grandchildren"]
fn isolated_environment_subprocess_fixture() {
    if std::env::var("POLARIS_ISOLATED_ENV_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    let copy = tempfile::tempdir().unwrap();
    let runtimes: Vec<PathBuf> = ["/System", "/usr/lib", "/bin", "/usr/bin"]
        .into_iter()
        .map(Into::into)
        .collect();
    let policy = isolated_policy(SandboxMode::ReadOnly, copy.path(), &runtimes);
    let script = "test \"$PATH\" = /usr/bin:/bin && test \"$LANG\" = C && test -z \"$ORDINARY_LABEL$POLARIS_API_KEY$POLARIS_ISOLATED_ENV_FIXTURE\" && /bin/sh -c 'test -z \"$ORDINARY_LABEL$POLARIS_API_KEY\" && test \"$HOME\" = \"$1\"' fixture \"$HOME\"";
    let result = run_confined(
        &policy,
        Path::new("/bin/sh"),
        &["-c".into(), script.into()],
        None,
    )
    .unwrap();
    assert_eq!(result.status, 0, "{result:?}");
    assert_eq!(std::env::var("HOME").unwrap(), "DUMMY_PARENT_HOME");
    assert_eq!(
        std::env::var("ORDINARY_LABEL").unwrap(),
        "DUMMY_SECRET_VALUE"
    );
    println!("isolated-env-verified");
}

#[test]
#[ignore = "requires locally installed Apple developer tools; run explicitly on macOS"]
fn isolated_copy_supports_offline_build_and_local_git() {
    fn developer_path(argument: &str, name: Option<&str>) -> PathBuf {
        let mut command = std::process::Command::new("/usr/bin/xcrun");
        command.arg(argument);
        if let Some(name) = name {
            command.arg(name);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "developer tools unavailable");
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
            .canonicalize()
            .unwrap()
    }
    let clang = developer_path("--find", Some("clang"));
    let git = developer_path("--find", Some("git"));
    let sdk = developer_path("--show-sdk-path", None);
    let toolchain = clang.parent().unwrap().parent().unwrap().parent().unwrap();
    let copy = tempfile::tempdir().unwrap();
    std::fs::write(copy.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
    let mut runtime: Vec<PathBuf> = ["/System", "/usr/lib", "/usr/share", "/bin", "/usr/bin"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    runtime.extend([
        toolchain.into(),
        sdk.clone(),
        git.parent().unwrap().into(),
        git.parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("share/git-core"),
    ]);
    let policy = isolated_policy(SandboxMode::WorkspaceWrite, copy.path(), &runtime);
    let build = run_confined(
        &policy,
        &clang,
        &[
            "-isysroot".into(),
            sdk.to_string_lossy().into_owned(),
            "main.c".into(),
            "-o".into(),
            "sample".into(),
        ],
        None,
    )
    .unwrap();
    assert_eq!(build.status, 0, "{build:?}");
    let run = run_confined(
        &policy,
        &copy.path().join("sample").canonicalize().unwrap(),
        &[],
        None,
    )
    .unwrap();
    assert_eq!(run.status, 0, "{run:?}");
    let init = run_confined(
        &policy,
        &git,
        &[
            "init".into(),
            "--quiet".into(),
            "--initial-branch=sandbox-fixture".into(),
            "--template=".into(),
            ".".into(),
        ],
        None,
    )
    .unwrap();
    assert_eq!(init.status, 0, "{init:?}");
    let status = run_confined(
        &policy,
        &git,
        &["status".into(), "--porcelain".into()],
        None,
    )
    .unwrap();
    assert_eq!(status.status, 0, "{status:?}");
    assert!(status.stdout.contains("main.c"));
}

#[test]
fn isolated_copy_allows_shell_edit_but_denies_original_even_full_access() {
    let original = tempfile::tempdir().unwrap();
    let copy = tempfile::tempdir().unwrap();
    std::fs::write(original.path().join("sentinel"), "PRIVATE_SENTINEL_943").unwrap();
    std::fs::write(copy.path().join("input.txt"), "ordinary-source").unwrap();
    let runtime: Vec<PathBuf> = ["/System", "/usr/lib", "/bin", "/usr/bin"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    for mode in [SandboxMode::WorkspaceWrite, SandboxMode::FullAccess] {
        let policy = isolated_policy(mode, copy.path(), &runtime);
        let out = run_confined(&policy, Path::new("/bin/sh"), &[
            "-c".into(),
            "cat input.txt > output.txt && printf 'ordinary-ok\\n'; if cat \"$1\"; then exit 77; fi; test -f output.txt".into(),
            "fixture".into(), original.path().join("sentinel").to_string_lossy().into_owned(),
        ], None).unwrap();
        assert_eq!(out.status, 0, "{out:?}");
        assert!(out.stdout.contains("ordinary-ok"));
        assert!(!out.stdout.contains("PRIVATE_SENTINEL_943"));
        assert!(!out.stderr.contains("PRIVATE_SENTINEL_943"));
        assert_eq!(
            std::fs::read_to_string(copy.path().join("output.txt")).unwrap(),
            "ordinary-source"
        );
    }
}

#[test]
fn isolated_read_only_can_read_copy_but_cannot_write_it() {
    let copy = tempfile::tempdir().unwrap();
    std::fs::write(copy.path().join("input.txt"), "ordinary-source").unwrap();
    let runtime: Vec<PathBuf> = ["/System", "/usr/lib", "/bin", "/usr/bin"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let policy = isolated_policy(SandboxMode::ReadOnly, copy.path(), &runtime);
    let out = run_confined(&policy, Path::new("/bin/sh"), &[
        "-c".into(), "cat input.txt; if echo changed > input.txt; then exit 77; fi; printf scratch > \"$TMPDIR/probe\" && printf scratch > \"$HOME/probe\"".into(),
    ], None).unwrap();
    assert_eq!(out.status, 0, "{out:?}");
    assert_eq!(
        std::fs::read_to_string(copy.path().join("input.txt")).unwrap(),
        "ordinary-source"
    );
    assert_eq!(
        std::fs::read_to_string(copy.path().join("scratch-tmp/probe")).unwrap(),
        "scratch"
    );
    assert_eq!(
        std::fs::read_to_string(copy.path().join("scratch-home/probe")).unwrap(),
        "scratch"
    );
}
