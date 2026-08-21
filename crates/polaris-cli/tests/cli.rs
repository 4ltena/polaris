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
