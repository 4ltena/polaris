//! Exercises project memory through the real CLI without model calls.

use std::process::Command;

fn run(home: &std::path::Path, project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_polaris"))
        .env("HOME", home)
        .current_dir(project)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn import_search_get_and_forget_preserve_project_boundary() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let fixture = project.path().join("records.jsonl");
    let canonical = project.path().canonicalize().unwrap();
    let record = serde_json::json!({"project_id":canonical.to_str().unwrap(),"session_id":"session-1","id":"decision-1","role":"user","timestamp":"2026-09-05","source":"fixture#1","text":"採用した色は青色。対象はcache_keyです。"});
    std::fs::write(&fixture, format!("{record}\n")).unwrap();
    let imported = run(
        home.path(),
        project.path(),
        &["memory", "import", fixture.to_str().unwrap()],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let found = run(home.path(), project.path(), &["memory", "search", "青色"]);
    assert!(
        found.status.success(),
        "{}",
        String::from_utf8_lossy(&found.stderr)
    );
    assert!(String::from_utf8_lossy(&found.stdout).contains("decision-1"));
    let rejected = run(
        home.path(),
        foreign.path(),
        &["memory", "import", fixture.to_str().unwrap()],
    );
    assert!(!rejected.status.success());
    let got = run(
        home.path(),
        project.path(),
        &["memory", "get", "session-1", "decision-1"],
    );
    assert!(got.status.success());
    assert!(String::from_utf8_lossy(&got.stdout).contains("historical_evidence"));
    assert!(
        run(
            home.path(),
            project.path(),
            &["memory", "forget", "session-1"]
        )
        .status
        .success()
    );
    assert!(
        !run(
            home.path(),
            project.path(),
            &["memory", "get", "session-1", "decision-1"]
        )
        .status
        .success()
    );
    assert!(
        !run(
            home.path(),
            project.path(),
            &["memory", "import", fixture.to_str().unwrap()]
        )
        .status
        .success()
    );
}

#[test]
fn search_does_not_create_memory_and_invalid_threshold_is_rejected() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    assert!(
        !run(home.path(), project.path(), &["memory", "search", "不存在"])
            .status
            .success()
    );
    assert!(!home.path().join(".polaris").exists());
    assert!(
        !run(
            home.path(),
            project.path(),
            &["--compact-at", "0", "doctor"]
        )
        .status
        .success()
    );
    assert!(
        !run(
            home.path(),
            project.path(),
            &["memory", "forget", "../escape"]
        )
        .status
        .success()
    );
}
