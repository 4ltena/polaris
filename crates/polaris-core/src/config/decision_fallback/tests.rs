//! User-only routing policy, destination validation and secret-free preflight display.
use crate::config::*;

const BODY: &str = r#"
[decision]
enabled = true
backend = "clef"
model = "flash-cpu"
[decision.fallback]
purposes = ["routing"]
max_attempts = 2
attempt_timeout_ms = 100
total_timeout_ms = 200
[[decision.fallback.targets]]
backend = "jev"
model = "fixture-model"
base_url = "https://example.invalid/private-path"
"#;

#[test]
fn fallback_is_user_owned_and_both_config_loaders_agree() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.toml");
    let project = dir.path().join("project.toml");
    std::fs::write(&user, BODY).unwrap();
    let full = try_load_from(Some(&user), None).unwrap().decision;
    let only = try_load_decision_from(Some(&user), None).unwrap();
    assert_eq!(full, only);
    let fallback = only.fallback.unwrap();
    assert_eq!(fallback.targets.len(), 1);
    assert_eq!(fallback.targets[0].backend, DecisionBackend::Jev);
    assert_eq!(fallback.max_attempts, 2);
    std::fs::write(&project, BODY).unwrap();
    assert!(matches!(
        try_load_from(Some(&user), Some(&project)),
        Err(ConfigError::UserOnlyDecisionFallback)
    ));
    assert!(matches!(
        try_load_decision_from(Some(&user), Some(&project)),
        Err(ConfigError::UserOnlyDecisionFallback)
    ));
    std::fs::write(&project, "[decision]\nlocal_only = true\n").unwrap();
    assert!(
        try_load_decision_from(Some(&user), Some(&project))
            .unwrap()
            .local_only
    );
}

#[test]
fn fallback_rejects_unsupported_purposes_limits_and_unsafe_destinations() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.toml");
    for (from, to) in [
        ("[\"routing\"]", "[\"finalization\"]"),
        ("max_attempts = 2", "max_attempts = 0"),
        ("max_attempts = 2", "max_attempts = 3"),
        ("attempt_timeout_ms = 100", "attempt_timeout_ms = 0"),
        ("attempt_timeout_ms = 100", "attempt_timeout_ms = 201"),
        ("total_timeout_ms = 200", "total_timeout_ms = 300001"),
        (
            "https://example.invalid/private-path",
            "http://example.invalid/v1",
        ),
        (
            "https://example.invalid/private-path",
            "https://user:dummy@example.invalid/v1",
        ),
        (
            "https://example.invalid/private-path",
            "https://example.invalid/v1?key=dummy",
        ),
        (
            "https://example.invalid/private-path",
            "https://example.invalid/v1#dummy",
        ),
        (
            "https://example.invalid/private-path",
            "http://localhost/v1",
        ),
    ] {
        std::fs::write(&user, BODY.replace(from, to)).unwrap();
        assert!(try_load_from(Some(&user), None).is_err(), "{to}");
        assert!(try_load_decision_from(Some(&user), None).is_err(), "{to}");
    }
    for endpoint in [
        "https://example.invalid/v1",
        "http://127.0.0.1:8181/v1",
        "http://[::1]:8181/v1",
    ] {
        std::fs::write(
            &user,
            BODY.replace("https://example.invalid/private-path", endpoint),
        )
        .unwrap();
        assert!(
            try_load_decision_from(Some(&user), None)
                .unwrap()
                .fallback
                .is_some()
        );
    }
}

#[test]
fn fallback_disclosure_has_destinations_and_scope_without_paths_or_control_injection() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(&user, BODY.replace("fixture-model", "fixture\\nmodel")).unwrap();
    let config = try_load_decision_from(Some(&user), None).unwrap();
    let lines = config.fallback.as_ref().unwrap().disclosure(&config);
    let text = lines.join("\n");
    assert!(text.contains("https://example.invalid"));
    assert!(text.contains("routing") && text.contains("state and questions"));
    assert!(!text.contains("private-path"));
    assert!(lines.iter().all(|line| !line.contains('\n')));
    assert!(text.contains("fixture\\nmodel"));
}
