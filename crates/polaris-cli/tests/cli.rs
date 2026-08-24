//! Integration tests for the CLI binary. Launches the real process to verify behavior.

use std::process::Command;

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
    let exe = env!("CARGO_BIN_EXE_polaris");
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(exe)
        .args(["-p", "hello"])
        .env("HOME", home.path())
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
    let exe = env!("CARGO_BIN_EXE_polaris");
    let home = tempfile::tempdir().expect("temp directory");
    let project = tempfile::tempdir().expect("temp directory");
    let broken = project.path().join("agents").join("broken-type");
    std::fs::create_dir_all(&broken).expect("could not create agents/broken-type");
    std::fs::write(
        broken.join("SKILL.md"),
        "---\ndescription: missing the required name field\n---\nBody\n",
    )
    .expect("could not write SKILL.md");

    let out = Command::new(exe)
        .args(["-p", "hello"])
        .current_dir(project.path())
        .env("HOME", home.path())
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
    let exe = env!("CARGO_BIN_EXE_polaris");
    let out = Command::new(exe)
        .arg("--help")
        .output()
        .expect("could not launch");

    assert!(out.status.success(), "--help failed");
    let text = String::from_utf8_lossy(&out.stdout);
    for var in ["POLARIS_API_KEY", "POLARIS_BASE_URL", "POLARIS_MODEL"] {
        assert!(text.contains(var), "--help is missing {var}: {text}");
    }
}
