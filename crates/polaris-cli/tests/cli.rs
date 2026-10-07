//! Integration tests for the CLI binary. Launches the real process to verify behavior.

#[path = "support/product_binary.rs"]
mod product_binary;

/// Launching without an API key should exit stating plainly that the key
/// is missing. No actual network call is made.
///
/// `HOME` is pointed at a temp directory and `POLARIS_BASE_URL` at an
/// unreachable address so that a developer machine with a real saved key
/// under `~/.polaris/api_key.json` (from `polaris login` or onboarding)
/// can never make this test resolve a real key and send a real, billed
/// request to `https://api.openai.com/v1`.
#[test]
fn reports_missing_api_key() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = product_binary::command()
        .args(["-p", "hello"])
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("could not launch");

    assert!(!out.status.success(), "succeeded despite a missing key");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("POLARIS_API_KEY"),
        "does not report the missing key: {err}"
    );
}

/// A broken subagent type definition (`agents/<type>/SKILL.md` with invalid
/// frontmatter) must be reported as a warning on stderr, not crash the
/// whole process. Discovery runs before the missing-API-key check fails the
/// run, so a dummy key and an unreachable base URL let the process reach
/// discovery and then fail later, on its own terms, at the network call.
#[test]
fn reports_a_broken_agent_type_without_crashing() {
    let home = tempfile::tempdir().expect("temp directory");
    let project = tempfile::tempdir().expect("temp directory");
    std::fs::create_dir(project.path().join(".polaris")).expect("could not create project marker");
    let broken = project.path().join("agents").join("broken-type");
    std::fs::create_dir_all(&broken).expect("could not create agents/broken-type");
    std::fs::write(
        broken.join("SKILL.md"),
        "---\ndescription: missing the required name field\n---\nBody\n",
    )
    .expect("could not write SKILL.md");

    let out = product_binary::command()
        .args(["-p", "hello"])
        .current_dir(project.path())
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env_remove("POLARIS_DATA_DIR")
        .env("POLARIS_API_KEY", "dummy-key")
        .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
        .output()
        .expect("could not launch");

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("agent type skipped: broken-type"),
        "does not report the skipped agent type: {err}"
    );
}

/// If `--help` doesn't mention the three environment variables that decide
/// the endpoint (and their defaults), someone new to the tool can't
/// discover them without reading the source.
#[test]
fn help_mentions_the_three_environment_variables() {
    let out = product_binary::command()
        .arg("--help")
        .output()
        .expect("could not launch");

    assert!(out.status.success(), "--help failed");
    let text = String::from_utf8_lossy(&out.stdout);
    for var in ["POLARIS_API_KEY", "POLARIS_BASE_URL", "POLARIS_MODEL"] {
        assert!(text.contains(var), "--help is missing {var}: {text}");
    }
}

/// A project config alone cannot start the experimental structured10 mode.
#[test]
fn project_config_cannot_start_structured10_without_the_explicit_flag() {
    let home = tempfile::tempdir().expect("temp directory");
    let project = tempfile::tempdir().expect("temp directory");
    std::fs::create_dir_all(project.path().join(".polaris")).unwrap();
    std::fs::write(
        project.path().join(".polaris/config.toml"),
        "history_mode = \"structured10\"\n",
    )
    .unwrap();
    let sessions = home.path().join(".polaris/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("legacy-1.jsonl"),
        "{\"role\":\"user\",\"content\":\"old\"}\n",
    )
    .unwrap();
    // A legacy import also creates a new session.
    for args in [
        &["-p", "hello"][..],
        &["--resume", "legacy-1", "-p", "hello"],
    ] {
        let out = product_binary::command()
            .args(args)
            .current_dir(project.path())
            .env("HOME", home.path().canonicalize().unwrap())
            .env_remove("POLARIS_DATA_DIR")
            .env("POLARIS_API_KEY", "dummy-key")
            .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
            .output()
            .expect("could not launch");
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("--history-mode structured10"),
            "{args:?}: {err}"
        );
        assert!(!home.path().join(".polaris/sessions-v2").exists());
    }
}
