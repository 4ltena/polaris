//! Pins down that subcommands are actually reachable.
//!
//! In M2 Task 8, `--confined-apply` had become unreachable because of the
//! required-argument validation on `--prompt`. Since it's the same shape
//! of trap, this verifies by launching the real binary rather than
//! reading the argument setup by eye.
//!
//! `HOME` is pointed at a temp directory, so this never touches the real
//! `~/.polaris` or `~/.codex`.

#[path = "support/product_binary.rs"]
mod product_binary;

#[test]
fn decision_management_lists_without_provider_startup_or_resource_mutation() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let output = product_binary::command()
        .env("HOME", home.path())
        .env("POLARIS_PROVIDER", "must-not-start-a-provider")
        .current_dir(project.path())
        .args(["decision", "list"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["registered_profiles"], serde_json::json!([]));
    assert!(!home.path().join(".polaris").exists());
    assert!(!project.path().join(".polaris").exists());
}

#[test]
fn decision_verify_refuses_unaccepted_profiles_without_provider_or_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let output = product_binary::command()
        .env("HOME", dir.path())
        .env("POLARIS_DATA_DIR", dir.path().join("data"))
        .env("POLARIS_PROVIDER", "must-not-start-a-provider")
        .current_dir(dir.path())
        .args(["decision", "verify", "unregistered-profile"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("受入登録が未完了") || error.contains("このplatform"));
    assert!(!dir.path().join(".polaris").exists());
    assert!(!error.contains("API_KEY"));
}

#[test]
fn decision_verify_gui_frame_reports_failure_without_claiming_verification() {
    let dir = tempfile::tempdir().unwrap();
    let output = product_binary::command()
        .env("HOME", dir.path())
        .env("POLARIS_PROVIDER", "must-not-start-a-provider")
        .current_dir(dir.path())
        .args([
            "decision",
            "verify",
            "unregistered-profile",
            "--stdio-result",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let frame: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(frame["type"], "verification");
    assert_eq!(frame["profile"], "unregistered-profile");
    assert!(matches!(
        frame["status"].as_str(),
        Some("unverified_profile" | "unsupported_platform")
    ));
    assert!(frame["generation"].is_null());
    assert!(!dir.path().join(".polaris").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn decision_management_never_imports_an_unaccepted_profile_even_with_yes() {
    for (operation, repair_plan) in [
        ("plan", false),
        ("plan", true),
        ("import", false),
        ("repair", false),
    ] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut command = product_binary::command();
        command
            .env("HOME", home.path())
            .env("POLARIS_PROVIDER", "must-not-start-a-provider")
            .current_dir(project.path())
            .args([
                "decision",
                operation,
                "clef-flash-q4-cpu-v1",
                "--worker",
                "/absent/worker",
                "--model",
                "/absent/model",
            ]);
        if operation != "plan" {
            command.arg("--yes");
        }
        if repair_plan {
            command.arg("--repair");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("受入登録が未完了"));
        assert!(output.stdout.is_empty());
        assert!(!home.path().join(".polaris").exists());
        assert!(!project.path().join(".polaris").exists());
    }
}

#[test]
fn decision_management_help_exposes_explicit_import_sources_and_confirmation() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let output = product_binary::command()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args(["decision", "import", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for field in ["<PROFILE>", "--worker", "--model", "--yes"] {
        assert!(help.contains(field), "{help}");
    }
    assert!(!home.path().join(".polaris").exists());
}

#[test]
fn decision_download_has_explicit_confirmation_and_no_arbitrary_source_override() {
    let root = tempfile::tempdir().unwrap();
    let help = product_binary::command()
        .env("HOME", root.path())
        .current_dir(root.path())
        .args(["decision", "download", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    for field in ["<PROFILE>", "--yes", "--repair"] {
        assert!(help.contains(field), "{help}");
    }
    assert!(!help.contains("--worker"));
    for arguments in [
        vec!["decision", "download", "fixture", "--worker", "/arbitrary"],
        vec![
            "decision",
            "download",
            "fixture",
            "--yes",
            "--stdio-confirm",
        ],
    ] {
        let output = product_binary::command()
            .env("HOME", root.path())
            .current_dir(root.path())
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    assert!(!root.path().join(".polaris").exists());
}

#[test]
fn decision_download_never_uses_unaccepted_sources_even_with_explicit_yes() {
    for arguments in [
        vec!["decision", "plan-download", "unregistered-profile"],
        vec!["decision", "download", "unregistered-profile", "--yes"],
        vec![
            "decision",
            "download",
            "unregistered-profile",
            "--repair",
            "--stdio-confirm",
        ],
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = product_binary::command()
            .env("HOME", root.path())
            .env("POLARIS_PROVIDER", "must-not-start-a-provider")
            .current_dir(root.path())
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("受入登録が未完了") || error.contains("このplatform"));
        assert!(!root.path().join(".polaris").exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn decision_management_pipe_requires_its_own_confirmation_and_cannot_bypass_acceptance() {
    for yes in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut command = product_binary::command();
        command
            .env("HOME", home.path())
            .current_dir(project.path())
            .args([
                "decision",
                "import",
                "clef-flash-q4-cpu-v1",
                "--worker",
                "/absent/worker",
                "--model",
                "/absent/model",
                "--stdio-confirm",
            ]);
        if yes {
            command.arg("--yes");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(if yes { 2 } else { 1 }));
        if !yes {
            assert!(String::from_utf8_lossy(&output.stderr).contains("受入登録が未完了"));
        }
        assert!(output.stdout.is_empty());
        assert!(!home.path().join(".polaris").exists());
        assert!(!project.path().join(".polaris").exists());
    }
}

#[cfg(unix)]
#[test]
fn mcp_configure_exposes_the_fixed_preset_and_rejects_unconfirmed_non_tty_without_state() {
    use std::process::Stdio;

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let help = product_binary::command()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args(["mcp", "configure", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help_text = String::from_utf8_lossy(&help.stdout);
    for field in ["unity-local-v2", "--preset", "--source", "--yes"] {
        assert!(help_text.contains(field), "{help_text}");
    }
    let output = product_binary::command()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args([
            "mcp",
            "configure",
            "unity-fixture",
            "--preset",
            "unity-local-v2",
            "--source",
        ])
        .arg(project.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("構成の確認には対話端末が必要"));
    assert!(!home.path().join(".polaris").exists());
    assert!(!project.path().join(".polaris").exists());
}

#[cfg(unix)]
#[test]
fn mcp_configure_never_creates_an_absent_store_even_with_explicit_yes() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let output = product_binary::command()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args([
            "mcp",
            "configure",
            "unity-fixture",
            "--preset",
            "unity-local-v2",
            "--yes",
            "--source",
        ])
        .arg(project.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
    assert!(!home.path().join(".polaris").exists());
    assert!(!project.path().join(".polaris").exists());
}

#[cfg(unix)]
#[test]
fn mcp_configure_rejects_an_unknown_preset_before_store_access() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let output = product_binary::command()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args([
            "mcp",
            "configure",
            "unity-fixture",
            "--preset",
            "arbitrary-command",
            "--yes",
            "--source",
        ])
        .arg(project.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unity-local-v2"));
    assert!(!home.path().join(".polaris").exists());
    assert!(!project.path().join(".polaris").exists());
}

#[cfg(unix)]
#[test]
fn common_tests_runs_a_valid_bounded_plan_or_reports_unavailable() {
    let workspace = tempfile::tempdir().expect("temp directory");
    let plan = workspace.path().join("common-tests.json");
    std::fs::write(
        &plan,
        serde_json::json!({
            "version": 1,
            "argv": ["/usr/bin/true"],
            "cases": [{"case": "exits_with", "status": 0}]
        })
        .to_string(),
    )
    .expect("write plan");

    let out = product_binary::command()
        .current_dir(workspace.path())
        .args(["common-tests", plan.to_str().expect("utf-8 plan path")])
        .output()
        .expect("could not launch common-tests");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"result\": \"passed\"") || stdout.contains("\"result\": \"unavailable\""),
        "common-tests did not return a typed execution result: {stdout}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.success(),
        stdout.contains("\"result\": \"passed\""),
        "only a passed report may exit successfully"
    );
}

#[cfg(unix)]
#[test]
fn common_tests_rejects_a_plan_larger_than_the_decode_limit() {
    let workspace = tempfile::tempdir().expect("temp directory");
    let plan = workspace.path().join("oversized-common-tests.json");
    std::fs::write(
        &plan,
        vec![b'x'; polaris_core::common_tests::MAX_PLAN_BYTES + 1],
    )
    .expect("write plan");
    let out = product_binary::command()
        .current_dir(workspace.path())
        .args(["common-tests", plan.to_str().expect("utf-8 plan path")])
        .output()
        .expect("could not launch common-tests");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("計画を読めません"));
}

#[cfg(unix)]
#[test]
fn common_tests_never_returns_a_host_home_sentinel() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("host home");
    let sentinel = home.path().join("credential-sentinel");
    std::fs::write(&sentinel, "HOST_HOME_SECRET").expect("write sentinel");
    let plan = workspace.path().join("read-home.json");
    std::fs::write(
        &plan,
        serde_json::json!({
            "version": 1,
            "argv": ["/bin/cat", sentinel],
            "cases": [{"case": "exits_with", "status": 0}]
        })
        .to_string(),
    )
    .expect("write plan");
    let out = product_binary::command()
        .current_dir(workspace.path())
        .args(["common-tests", plan.to_str().expect("utf-8 plan path")])
        .output()
        .expect("could not launch common-tests");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("HOST_HOME_SECRET"),
        "host HOME was readable: {stdout}"
    );
    assert!(
        stdout.contains("\"result\": \"failed\"") || stdout.contains("\"result\": \"unavailable\""),
        "host HOME read unexpectedly passed: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn common_tests_never_exposes_workspace_secrets_to_a_case_program() {
    let workspace = tempfile::tempdir().expect("workspace");
    let secret = workspace.path().join(".env");
    std::fs::write(&secret, "WORKSPACE_SECRET_SENTINEL=never-visible").expect("write secret");
    let plan = workspace.path().join("read-workspace-secret.json");
    std::fs::write(
        &plan,
        serde_json::json!({
            "version": 1,
            "argv": ["/bin/cat", secret],
            "cases": [{"case": "exits_with", "status": 0}]
        })
        .to_string(),
    )
    .expect("write plan");

    let out = product_binary::command()
        .current_dir(workspace.path())
        .args(["common-tests", plan.to_str().expect("utf-8 plan path")])
        .output()
        .expect("could not launch common-tests");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("WORKSPACE_SECRET_SENTINEL")
            && !stderr.contains("WORKSPACE_SECRET_SENTINEL"),
        "case program read the real workspace secret: {stdout}; {stderr}"
    );
    assert!(
        !out.status.success(),
        "a plan that targets the real workspace must not pass: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn common_tests_rejects_an_invalid_plan_before_execution() {
    let workspace = tempfile::tempdir().expect("temp directory");
    let plan = workspace.path().join("invalid-common-tests.json");
    std::fs::write(
        &plan,
        r#"{"version":1,"argv":["/usr/bin/true"],"cases":[],"extra":true}"#,
    )
    .expect("write invalid plan");

    let out = product_binary::command()
        .current_dir(workspace.path())
        .args(["common-tests", plan.to_str().expect("utf-8 plan path")])
        .output()
        .expect("could not launch common-tests");
    assert!(!out.status.success(), "invalid plan unexpectedly succeeded");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("計画の形式が不正です"),
        "invalid schema was not rejected explicitly: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn hidden_static_component_manifest_returns_only_the_typed_fixed_contract() {
    let component = tempfile::tempdir().expect("component");
    std::fs::write(
        component.path().join("Cargo.toml"),
        "[package]\nname = \"static-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");
    let clean = product_binary::command()
        .current_dir(component.path())
        .arg("static-component-manifest")
        .output()
        .expect("launch static checker");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let clean: serde_json::Value = serde_json::from_slice(&clean.stdout).expect("typed JSON");
    assert_eq!(clean["schema_version"], 1);
    assert_eq!(clean["state"], "satisfied");
    assert_eq!(clean["findings"], serde_json::json!([]));

    std::fs::write(component.path().join("Cargo.toml"), "[workspace]\n").expect("replace manifest");
    let violated = product_binary::command()
        .current_dir(component.path())
        .arg("static-component-manifest")
        .output()
        .expect("launch static checker");
    assert_eq!(violated.status.code(), Some(1));
    let violated: serde_json::Value = serde_json::from_slice(&violated.stdout).expect("typed JSON");
    assert_eq!(violated["state"], "violated");
    assert_eq!(
        violated["findings"][0]["rule_id"],
        "component-cargo-manifest-v1"
    );

    let widened = product_binary::command()
        .current_dir(component.path())
        .args(["static-component-manifest", "--path", "/tmp/other"])
        .output()
        .expect("launch widened static checker");
    assert!(!widened.status.success());
}

/// `login` parses successfully without `--prompt`. Stopping at `--help`
/// means it never opens a browser or reaches the network.
#[test]
fn the_login_subcommand_is_reachable_without_a_prompt() {
    let out = product_binary::command()
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
#[cfg(unix)]
#[test]
fn the_logout_subcommand_runs_without_a_prompt() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = product_binary::command()
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
#[cfg(unix)]
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

    let out = product_binary::command()
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
#[cfg(unix)]
#[test]
fn the_normal_path_without_a_prompt_refuses_the_tui_on_a_non_interactive_terminal() {
    // Provider construction now runs unconditionally, even without a
    // prompt, so a real key must be present or the process would fail
    // there instead of reaching the TUI's own guard.
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let out = product_binary::command()
        .current_dir(workspace.path())
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
#[cfg(unix)]
#[test]
fn the_default_provider_is_still_openai() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = product_binary::command()
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
#[cfg(unix)]
#[test]
fn a_logged_out_codex_run_names_the_login_command() {
    let home = tempfile::tempdir().expect("temp directory");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let out = product_binary::command()
        .current_dir(workspace.path())
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
#[cfg(unix)]
#[test]
fn an_unknown_provider_name_fails_fast() {
    let out = product_binary::command()
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
#[cfg(unix)]
#[test]
fn a_saved_api_key_file_is_used_when_the_env_var_is_absent() {
    let home = tempfile::tempdir().expect("temp directory");
    let key_path = home.path().join(".polaris").join("api_key.json");
    std::fs::create_dir_all(key_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&key_path, r#"{"key":"sk-from-file"}"#).expect("write key file");

    let out = product_binary::command()
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
#[cfg(unix)]
#[test]
fn tui_mode_with_no_key_anywhere_reaches_onboarding() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = product_binary::command()
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

#[cfg(unix)]
#[test]
fn logged_out_codex_tui_reaches_onboarding_without_a_provider_override() {
    let home = tempfile::tempdir().unwrap();
    let output = product_binary::command()
        .env("HOME", home.path().canonicalize().unwrap())
        .env("POLARIS_PROVIDER", "codex")
        .env_remove("POLARIS_API_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to start onboarding"));
}

#[cfg(unix)]
#[test]
fn registered_startup_distinguishes_logout_ready_reauth_and_in_progress() {
    const CHILD: &str = "POLARIS_REGISTERED_STARTUP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "registered_startup_distinguishes_logout_ready_reauth_and_in_progress",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path().canonicalize().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use polaris_auth::accounts::store::{AccountState, AccountStore};
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    polaris_auth::protection::use_trusted_home(&home).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = AccountStore::open().unwrap();
    let reservation = store.begin_registration(None).unwrap();
    store.cancel_registration(&reservation, false).unwrap();
    let run = |provider: Option<&str>, args: &[&str]| {
        let mut command = product_binary::command();
        command
            .current_dir(workspace.path())
            .env("HOME", &home)
            .env_remove("POLARIS_API_KEY")
            .env_remove("POLARIS_PROVIDER")
            .args(args);
        if let Some(provider) = provider {
            command.env("POLARIS_PROVIDER", provider);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        String::from_utf8(output.stderr).unwrap()
    };
    for provider in [None, Some("codex")] {
        assert!(run(provider, &[]).contains("refusing to start onboarding"));
    }
    assert!(run(Some("codex"), &["-p", "synthetic"]).contains("使用できる登録がありません"));
    assert!(
        !run(Some("codex"), &["--account", &reservation.registration_id]).contains("onboarding")
    );
    assert_eq!(
        store.snapshot().unwrap().accounts[0].state,
        AccountState::Disconnected
    );
    assert!(!home.join(".polaris/auth.json").exists());

    let reservation = store.begin_registration(None).unwrap();
    store.mark_exchange_pending(&reservation).unwrap();
    let (generation, _) = store
        .complete_registration(
            &reservation,
            polaris_auth::Credentials {
                access_token: "synthetic-only".into(),
                refresh_token: "synthetic-only".into(),
                account_id: "synthetic-account".into(),
                expires_at: Some(u64::MAX),
            },
        )
        .unwrap();
    assert!(run(None, &[]).contains("refusing to start the TUI"));
    store
        .begin_reauthentication(&reservation.registration_id, generation)
        .unwrap();
    let in_progress = run(None, &[]);
    assert!(!in_progress.contains("onboarding"));
    // With no live owner lease, a new process settles the abandoned reservation.
    let generation = store.snapshot().unwrap().accounts[0].generation;
    let reauth = store
        .begin_reauthentication(&reservation.registration_id, generation)
        .unwrap();
    store.mark_exchange_pending(&reauth).unwrap();
    store.cancel_registration(&reauth, true).unwrap();
    let message = run(None, &[]);
    assert!(
        message.contains(&format!(
            "polaris account reauth {}",
            reservation.registration_id
        )),
        "{message}"
    );
    assert!(!message.contains("onboarding"));
    assert_eq!(
        store.snapshot().unwrap().accounts[0].state,
        AccountState::ReauthRequired
    );
}

/// A user who already signed in with ChatGPT (via `polaris login` or
/// onboarding's own ChatGPT option) must not be sent back to onboarding on
/// the next launch just because `POLARIS_PROVIDER` isn't set. TUI mode
/// with saved codex credentials, no openai key anywhere, and no explicit
/// `POLARIS_PROVIDER` must reach the *chat loop's* own non-interactive
/// guard (proving it picked the codex provider), not onboarding's guard.
#[cfg(unix)]
#[test]
fn saved_codex_credentials_are_used_without_an_explicit_provider() {
    let home = tempfile::tempdir().expect("temp directory");
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let polaris_dir = home.path().join(".polaris");
    std::fs::create_dir_all(&polaris_dir).expect("cannot create");
    std::fs::write(
        polaris_dir.join("auth.json"),
        b"{\"access_token\":\"a\",\"refresh_token\":\"r\",\"account_id\":\"x\"}",
    )
    .expect("cannot write");

    let out = product_binary::command()
        .current_dir(workspace.path())
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
#[cfg(unix)]
#[test]
fn exec_with_an_argument_reaches_the_one_shot_path() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = product_binary::command()
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
#[cfg(unix)]
#[test]
fn exec_with_no_argument_reads_the_prompt_from_stdin() {
    use std::io::Write as _;
    use std::process::Stdio;

    let home = tempfile::tempdir().expect("temp directory");
    let mut child = product_binary::command()
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
#[cfg(unix)]
#[test]
fn sandbox_runs_a_command_under_confinement_and_reports_its_output() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = product_binary::command()
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
#[cfg(unix)]
#[test]
fn sandbox_propagates_the_childs_exit_code() {
    let home = tempfile::tempdir().expect("temp directory");
    let out = product_binary::command()
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
    let out = product_binary::command()
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
        stdout.contains("Decision capacity (read-only):"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Execution fit: not verified here"),
        "{stdout}"
    );
    assert!(stdout.contains("Hosted Jev/remote System One"), "{stdout}");
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    assert!(
        stdout.contains("unified memory (not dedicated VRAM)"),
        "{stdout}"
    );
    #[cfg(not(windows))]
    assert!(
        stdout.contains("not found"),
        "missing credentials: {stdout}"
    );
    #[cfg(windows)]
    {
        assert!(stdout.contains(
            "Can't determine the credentials path: authentication path protection is unavailable"
        ));
        assert!(stdout.contains(
            "Can't determine the API key path: authentication path protection is unavailable"
        ));
    }
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
#[cfg(unix)]
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

    let out = product_binary::command()
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
        let out = product_binary::command()
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

/// 明示指定は登録がある場合だけ意味を持ち、別providerとは併用できない。
#[cfg(unix)]
#[test]
fn an_explicit_account_is_refused_without_a_registration_or_outside_codex() {
    let home = tempfile::tempdir().expect("temp directory");
    let home = home.path().canonicalize().expect("physical home path");
    let run = |provider: &str| {
        product_binary::command()
            .args(["-p", "x", "--account", "reg-missing"])
            .env("HOME", &home)
            .env("POLARIS_PROVIDER", provider)
            .env("POLARIS_API_KEY", "synthetic-only")
            .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
            .output()
            .expect("could not launch")
    };
    let openai = run("openai");
    assert!(!openai.status.success());
    assert!(
        String::from_utf8_lossy(&openai.stderr).contains("--accountはcodexのみ"),
        "{}",
        String::from_utf8_lossy(&openai.stderr)
    );
    let codex = run("codex");
    assert!(!codex.status.success());
    assert!(
        String::from_utf8_lossy(&codex.stderr).contains("polaris account login"),
        "{}",
        String::from_utf8_lossy(&codex.stderr)
    );
}
