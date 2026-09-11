//! Pins down that subcommands are actually reachable.
//!
//! In M2 Task 8, `--confined-apply` had become unreachable because of the
//! required-argument validation on `--prompt`. Since it's the same shape
//! of trap, this verifies by launching the real binary rather than
//! reading the argument setup by eye.
//!
//! `HOME` is pointed at a temp directory, so this never touches the real
//! `~/.polaris` or `~/.codex`.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_polaris")
}

/// `login` parses successfully without `--prompt`. Stopping at `--help`
/// means it never opens a browser or reaches the network.
#[test]
fn the_login_subcommand_is_reachable_without_a_prompt() {
    let out = Command::new(bin())
        .args(["login", "--help"])
        .output()
        .expect("could not launch");
    assert!(
        out.status.success(),
        "login --help failed. stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `logout` runs to completion without `--prompt`. It doesn't fail even
/// when not logged in.
#[test]
fn the_logout_subcommand_runs_without_a_prompt() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(bin())
        .arg("logout")
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .output()
        .expect("could not launch");
    assert!(
        out.status.success(),
        "logout failed. stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `logout` deletes only its own store. It doesn't touch
/// `~/.codex/auth.json`.
#[test]
fn logout_never_touches_the_codex_store() {
    let home = tempfile::tempdir().expect("temp directory");
    let codex = home.path().join(".codex");
    std::fs::create_dir_all(&codex).expect("cannot create");
    let codex_auth = codex.join("auth.json");
    std::fs::write(&codex_auth, b"{\"sentinel\":true}").expect("cannot write");

    let polaris_dir = home.path().join(".polaris");
    std::fs::create_dir_all(&polaris_dir).expect("cannot create");
    let polaris_auth = polaris_dir.join("auth.json");
    std::fs::write(
        &polaris_auth,
        b"{\"access_token\":\"a\",\"refresh_token\":\"r\",\"account_id\":\"x\"}",
    )
    .expect("cannot write");

    let out = Command::new(bin())
        .arg("logout")
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .output()
        .expect("could not launch");
    assert!(out.status.success(), "logout failed");

    assert!(!polaris_auth.exists(), "did not delete its own store");
    assert_eq!(
        std::fs::read(&codex_auth).expect("cannot read"),
        b"{\"sentinel\":true}",
        "touched the codex store"
    );
}

/// Omitting `--prompt` no longer fails at argument parsing: it now means
/// "enter the TUI" (see `polaris_tui::run`). Under the test harness,
/// stdout/stdin are not a real terminal, so the TUI path's own
/// non-interactive guard refuses to start instead of hanging — this must
/// not silently succeed, and it must not be confused with the old
/// "--prompt is required" error (which no longer exists).
#[test]
fn the_normal_path_without_a_prompt_refuses_the_tui_on_a_non_interactive_terminal() {
    // Provider construction now runs unconditionally, even without a
    // prompt, so a real key must be present or the process would fail
    // there instead of reaching the TUI's own guard.
    let out = Command::new(bin())
        .env("POLARIS_PROVIDER", "openai")
        .env("POLARIS_API_KEY", "sk-test-not-a-real-key")
        .output()
        .expect("could not launch");
    assert!(!out.status.success(), "succeeded with no instruction");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("non-interactive"),
        "does not say why it refused to start: {stderr}"
    );
}

/// Acceptance criterion 5. The default run, with `POLARIS_PROVIDER`
/// unset, still takes the openai path as before. Scolding about the
/// missing key in openai's own words confirms it hasn't strayed onto the
/// codex path.
///
/// `HOME` is pointed at a temp directory and `POLARIS_BASE_URL` at an
/// unreachable address so that a developer machine with a real saved key
/// under `~/.polaris/api_key.json` can never make this test resolve a
/// real key and send a real, billed request to the OpenAI API.
#[test]
fn the_default_provider_is_still_openai() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(bin())
        .args(["-p", "x"])
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("could not launch");
    assert!(!out.status.success(), "succeeded with no key");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("POLARIS_API_KEY"),
        "did not take the openai path: {stderr}"
    );
}

/// Acceptance criterion 4. Pointing at codex while logged out produces an
/// error naming `polaris login`. It's not an HTTP error — this stops
/// before reaching the network, so no real API call is made.
#[test]
fn a_logged_out_codex_run_names_the_login_command() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(bin())
        .args(["-p", "a few lines"])
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env("POLARIS_PROVIDER", "codex")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("could not launch");
    assert!(
        !out.status.success(),
        "succeeded despite not being logged in"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("polaris login"),
        "does not name what needs to be done: {stderr}"
    );
    assert!(
        !stderr.contains("status "),
        "came out as an HTTP error: {stderr}"
    );
}

/// An unknown provider name fails at startup. Noticing only after
/// running, via "the model isn't responding", is too late.
#[test]
fn an_unknown_provider_name_fails_fast() {
    let out = Command::new(bin())
        .args(["-p", "x"])
        .env("POLARIS_PROVIDER", "nonesuch")
        .env("POLARIS_API_KEY", "dummy")
        .output()
        .expect("could not launch");
    assert!(!out.status.success(), "succeeded with an unknown provider");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nonesuch"),
        "did not print the name: {stderr}"
    );
}

/// The api_key.json fallback: with no POLARIS_API_KEY env var but a
/// previously-saved key file, one-shot mode should succeed in building
/// the provider (it will still fail later when the fake key is rejected
/// by a real network call, but that's not what this test checks — it
/// only checks that key *resolution* used the file instead of failing at
/// "POLARIS_API_KEY is not set").
#[test]
fn a_saved_api_key_file_is_used_when_the_env_var_is_absent() {
    let home = tempfile::tempdir().expect("temp directory");
    let key_path = home.path().join(".polaris").join("api_key.json");
    std::fs::create_dir_all(key_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&key_path, r#"{"key":"sk-from-file"}"#).expect("write key file");

    let out = Command::new(bin())
        .args(["-p", "x"])
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
        .output()
        .expect("could not launch");

    // It must NOT fail with the "not set" message — it should get past
    // key resolution and fail later (e.g. a connection error to the
    // deliberately-unreachable base URL), proving the file was read.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("POLARIS_API_KEY is not set"),
        "did not use the saved key file: {stderr}"
    );
}

/// TUI mode with no API key anywhere (env, file) must actually reach
/// onboarding's own code path, not just fail with the old
/// "POLARIS_API_KEY is not set" message. We can't complete onboarding
/// without a real terminal, but we CAN prove the code path was reached:
/// onboarding's own non-interactive-terminal guard fires with a distinct
/// message from the chat loop's own guard, so seeing THAT specific
/// message (not the chat loop's "refusing to start the TUI...") proves
/// main.rs's onboarding wiring was actually exercised, not skipped.
#[test]
fn tui_mode_with_no_key_anywhere_reaches_onboarding() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(bin())
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("could not launch");

    assert!(
        !out.status.success(),
        "succeeded with no credentials at all"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to start onboarding"),
        "did not reach onboarding's own guard (got the chat loop's guard instead, or something else): {stderr}"
    );
}

/// A user who already signed in with ChatGPT (via `polaris login` or
/// onboarding's own ChatGPT option) must not be sent back to onboarding on
/// the next launch just because `POLARIS_PROVIDER` isn't set. TUI mode
/// with saved codex credentials, no openai key anywhere, and no explicit
/// `POLARIS_PROVIDER` must reach the *chat loop's* own non-interactive
/// guard (proving it picked the codex provider), not onboarding's guard.
#[test]
fn saved_codex_credentials_are_used_without_an_explicit_provider() {
    let home = tempfile::tempdir().expect("temp directory");
    let polaris_dir = home.path().join(".polaris");
    std::fs::create_dir_all(&polaris_dir).expect("cannot create");
    std::fs::write(
        polaris_dir.join("auth.json"),
        b"{\"access_token\":\"a\",\"refresh_token\":\"r\",\"account_id\":\"x\"}",
    )
    .expect("cannot write");

    let out = Command::new(bin())
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("could not launch");

    assert!(
        !out.status.success(),
        "succeeded on a non-interactive terminal"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to start the TUI"),
        "did not reach the chat loop's own guard, so it did not pick the codex provider \
         (got onboarding's guard instead, or something else): {stderr}"
    );
}

/// `exec` with an explicit prompt argument must reach the same one-shot
/// path as `--prompt` — proven the same way `the_default_provider_is_still_openai`
/// proves it: no key anywhere, and the failure is the openai arm's own
/// "not set" message, not a `clap` parse error or the TUI path.
#[test]
fn exec_with_an_argument_reaches_the_one_shot_path() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(bin())
        .args(["exec", "do the thing"])
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("could not launch");
    assert!(!out.status.success(), "succeeded with no key");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("POLARIS_API_KEY"),
        "did not take the one-shot path: {stderr}"
    );
}

/// `exec` with no argument reads the instruction from stdin, matching
/// `codex exec`'s own convention. Piping input in proves the stdin branch
/// ran (an empty pipe would otherwise block forever waiting on the real
/// terminal, so a successful non-hanging exit here is itself part of what
/// this proves).
#[test]
fn exec_with_no_argument_reads_the_prompt_from_stdin() {
    use std::io::Write as _;
    use std::process::Stdio;

    let home = tempfile::tempdir().expect("temp directory");
    let mut child = Command::new(bin())
        .arg("exec")
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not launch");
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(b"do the thing from stdin")
        .expect("could not write to stdin");
    let out = child.wait_with_output().expect("could not wait");

    assert!(!out.status.success(), "succeeded with no key");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("POLARIS_API_KEY"),
        "did not take the one-shot path: {stderr}"
    );
}

/// `sandbox` runs the given command under confinement and reports its
/// output and exit code, without going through the agent loop at all (no
/// credentials needed).
#[test]
fn sandbox_runs_a_command_under_confinement_and_reports_its_output() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(bin())
        .args(["sandbox", "--", "echo", "hello-from-sandbox"])
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .output()
        .expect("could not launch");
    assert!(
        out.status.success(),
        "sandboxed echo failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "hello-from-sandbox"
    );
}

/// `sandbox` propagates the child's real exit code (not a flat 0/1), so a
/// caller scripting around it can tell success from a specific failure.
#[test]
fn sandbox_propagates_the_childs_exit_code() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(bin())
        .args(["sandbox", "--", "sh", "-c", "exit 7"])
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .output()
        .expect("could not launch");
    assert_eq!(out.status.code(), Some(7));
}

/// `doctor` runs to completion with no credentials at all, reporting
/// absence rather than failing outright — it's a diagnostic, not a
/// precondition check.
#[test]
fn doctor_runs_successfully_with_no_credentials() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = Command::new(bin())
        .arg("doctor")
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("could not launch");
    assert!(
        out.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("not found"),
        "missing credentials: {stdout}"
    );
    assert!(
        stdout.contains("would default to: openai"),
        "missing provider resolution: {stdout}"
    );
}

/// `doctor` reports the codex provider as the default once ChatGPT
/// credentials are saved and no openai key exists anywhere — the exact
/// scenario `saved_codex_credentials_are_used_without_an_explicit_provider`
/// pins down at the process level; here `doctor` must describe the same
/// decision it's about to make.
#[test]
fn doctor_reports_codex_as_default_when_only_codex_credentials_are_saved() {
    let home = tempfile::tempdir().expect("temp directory");
    let polaris_dir = home.path().join(".polaris");
    std::fs::create_dir_all(&polaris_dir).expect("cannot create");
    std::fs::write(
        polaris_dir.join("auth.json"),
        b"{\"access_token\":\"a\",\"refresh_token\":\"r\",\"account_id\":\"x\"}",
    )
    .expect("cannot write");

    let out = Command::new(bin())
        .arg("doctor")
        .env(
            "HOME",
            home.path().canonicalize().expect("physical home path"),
        )
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("could not launch");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("would default to: codex"),
        "did not report codex as the default: {stdout}"
    );
}

/// `completion` generates a non-empty script for every shell clap_complete
/// supports, and never touches credentials or the network.
#[test]
fn completion_generates_a_script_for_every_supported_shell() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let out = Command::new(bin())
            .args(["completion", shell])
            .output()
            .unwrap_or_else(|e| panic!("could not launch for {shell}: {e}"));
        assert!(
            out.status.success(),
            "completion generation failed for {shell}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !out.stdout.is_empty(),
            "empty completion script for {shell}"
        );
    }
}
