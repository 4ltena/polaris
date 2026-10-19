//! Prepared package downloads bind the approval card before any fetch begins.
use super::*;

#[test]
fn materialise_prepare_binds_package_runtime_home_and_source_before_fetch() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path().canonicalize().unwrap();
    let source = home.join(".codex/config.toml");
    fs::create_dir(source.parent().unwrap()).unwrap();
    let body = "[mcp_servers.docs]\ncommand = 'npx'\nargs = ['-y', '@fixture/docs@1.2.3']\n";
    fs::write(&source, body).unwrap();
    let detector = Detector::new(home.clone(), vec![]);
    let plan = detector
        .prepare_materialise("docs", Some(Path::new("/bin/sh")))
        .unwrap();
    assert_eq!(plan.package, "@fixture/docs");
    assert_eq!(plan.version.as_deref(), Some("1.2.3"));
    assert_eq!(plan.runtime, Path::new("/bin/sh"));
    assert!(
        plan.directory
            .starts_with(home.join(".polaris/mcp/servers"))
    );
    assert!(plan.description().contains("@fixture/docs\nversion: 1.2.3"));
    assert!(
        !home.join(".polaris").exists(),
        "preparation must not fetch or install"
    );

    fs::write(&source, body.replace("1.2.3", "9.9.9")).unwrap();
    assert_eq!(
        detector.materialise_prepared_quiet(plan),
        Err(launcher::Refused::NotALauncher)
    );
    assert!(
        !home.join(".polaris").exists(),
        "source changes must refuse before package execution"
    );

    fs::write(&source, body).unwrap();
    let plan = detector
        .prepare_materialise("docs", Some(Path::new("/bin/sh")))
        .unwrap();
    let other = tempfile::tempdir().unwrap();
    let other_home = other.path().canonicalize().unwrap();
    fs::create_dir(other_home.join(".codex")).unwrap();
    fs::write(other_home.join(".codex/config.toml"), body).unwrap();
    let other_detector = Detector::new(other_home.clone(), vec![]);
    assert_eq!(
        other_detector.materialise_prepared_quiet(plan),
        Err(launcher::Refused::NotALauncher)
    );
    assert!(!other_home.join(".polaris").exists());
}

#[test]
fn materialise_prepare_rejects_credential_arguments_and_unresolved_runtime() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path().canonicalize().unwrap();
    fs::create_dir(home.join(".codex")).unwrap();
    let source = home.join(".codex/config.toml");
    let detector = Detector::new(home.clone(), vec![]);
    fs::write(&source, "[mcp_servers.docs]\ncommand = 'npx'\nargs = ['@fixture/docs', '--token', 'synthetic-credential']\n").unwrap();
    assert!(matches!(
        detector.prepare_materialise("docs", Some(Path::new("/bin/sh"))),
        Err(launcher::Refused::NotALauncher)
    ));
    fs::write(
        &source,
        "[mcp_servers.docs]\ncommand = 'npx'\nargs = ['@fixture/docs', 7]\n",
    )
    .unwrap();
    assert!(matches!(
        detector.prepare_materialise("docs", Some(Path::new("/bin/sh"))),
        Err(launcher::Refused::NotALauncher)
    ));
    fs::write(
        &source,
        "[mcp_servers.docs]\ncommand = 'npx'\nargs = ['@fixture/docs']\n",
    )
    .unwrap();
    assert!(matches!(
        detector.prepare_materialise("docs", Some(Path::new("relative-node"))),
        Err(launcher::Refused::RuntimeNotPinned)
    ));
    assert!(!home.join(".polaris").exists());
}

#[test]
fn materialise_quiet_pins_a_fixture_without_forwarding_package_output() {
    use std::os::unix::fs::PermissionsExt;
    const MARKER: &str = "synthetic-package-output-must-not-reach-the-tui";
    if let Some(mode) = std::env::var_os("POLARIS_MCP_QUIET_FIXTURE_CHILD") {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let detector = Detector::new(home.clone(), vec![]);
        let pinned = if mode == "quiet" || mode == "symlink" {
            let plan = detector
                .prepare_materialise("docs", Some(Path::new("/bin/sh")))
                .unwrap();
            if mode == "symlink" {
                let outside = home.join("outside");
                fs::create_dir(&outside).unwrap();
                fs::create_dir_all(plan.directory.parent().unwrap()).unwrap();
                for directory in [".polaris", ".polaris/mcp", ".polaris/mcp/servers"] {
                    fs::set_permissions(home.join(directory), fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                std::os::unix::fs::symlink(&outside, &plan.directory).unwrap();
                assert_eq!(
                    detector.materialise_prepared_quiet(plan),
                    Err(launcher::Refused::NotSaved)
                );
                assert!(fs::read_dir(outside).unwrap().next().is_none());
                return;
            }
            detector.materialise_prepared_quiet(plan).unwrap()
        } else {
            detector
                .materialise("docs", Some(Path::new("/bin/sh")))
                .unwrap()
        };
        assert_eq!(pinned.package, "@fixture/docs");
        assert_eq!(pinned.version, "1.2.3");
        assert!(pinned.entry.is_file());
        let document: launcher::Pinned = serde_json::from_slice(
            &fs::read(pinned.entry.ancestors().nth(4).unwrap().join("pinned.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(document, pinned);
        return;
    }
    for mode in ["quiet", "inherit", "symlink"] {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        fs::create_dir(home.join(".codex")).unwrap();
        fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = 'npx'\nargs = ['@fixture/docs@1.2.3']\n",
        )
        .unwrap();
        let bin = home.join("fixture-bin");
        fs::create_dir(&bin).unwrap();
        let npm = bin.join("npm");
        fs::write(&npm, format!(r##"#!/bin/sh
set -eu
printf '%s\n' '{MARKER}'
printf '%s\n' '{MARKER}' >&2
/bin/mkdir -p "$3/node_modules/@fixture/docs"
printf '%s' '{{"name":"@fixture/docs","version":"1.2.3","bin":"index.js"}}' > "$3/node_modules/@fixture/docs/package.json"
printf '%s' 'fixture-entry' > "$3/node_modules/@fixture/docs/index.js"
"##)).unwrap();
        fs::set_permissions(&npm, fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["mcp::setup::materialise_tests::materialise_quiet_pins_a_fixture_without_forwarding_package_output", "--exact", "--nocapture"])
            .env("POLARIS_MCP_QUIET_FIXTURE_CHILD", mode)
            .env("HOME", &home).env("PATH", &bin)
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let visible = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(visible.contains(MARKER), mode == "inherit");
    }
}
