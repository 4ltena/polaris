//! Configuration provenance and strict compatible target boundaries.
use crate::config::*;
const BODY: &str = r#"
[decision]
enabled = true
backend = "system-one"
[decision.system_one]
endpoint = "https://example.invalid/private-path"
model = "fixture-model"
credential_env = "POLARIS_SYSTEM_ONE_FIXTURE"
"#;
#[test]
fn system_one_user_association_is_atomic_and_both_loaders_agree() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.toml");
    let project = dir.path().join("project.toml");
    std::fs::write(&user, BODY).unwrap();
    let loaded = try_load_decision_from(Some(&user), None).unwrap();
    assert_eq!(loaded, try_load_from(Some(&user), None).unwrap().decision);
    assert_eq!(loaded.backend, DecisionBackend::SystemOne);
    assert_eq!(
        loaded.system_one.unwrap().origin().as_deref(),
        Some("https://example.invalid")
    );
    std::fs::write(&project, BODY).unwrap();
    assert!(matches!(
        try_load_from(Some(&user), Some(&project)),
        Err(ConfigError::UserOnlySystemOne)
    ));
    assert!(matches!(
        try_load_decision_from(Some(&user), Some(&project)),
        Err(ConfigError::UserOnlySystemOne)
    ));
    std::fs::write(&project, "[decision]\nmodel = 'replacement'\n").unwrap();
    assert!(try_load_decision_from(Some(&user), Some(&project)).is_err());
    std::fs::write(&project, "[decision]\nlocal_only = true\n").unwrap();
    assert!(
        try_load_decision_from(Some(&user), Some(&project))
            .unwrap()
            .local_only
    );
}
#[test]
fn system_one_refuses_ambiguous_or_unsafe_profiles_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.toml");
    for body in [
        BODY.replace("POLARIS_SYSTEM_ONE_FIXTURE", "HOME"),
        BODY.replace("POLARIS_SYSTEM_ONE_FIXTURE", "POLARIS_SYSTEM_ONE_"),
        BODY.replace(
            "https://example.invalid/private-path",
            "http://127.0.0.1/api",
        ),
        BODY.replace(
            "https://example.invalid/private-path",
            "https://example.invalid/api?key=dummy",
        ),
        BODY.replace(
            "backend = \"system-one\"",
            "backend = \"system-one\"\nbase_url = 'https://other.invalid'",
        ),
        BODY.replace("model = \"fixture-model\"", "model = ''"),
    ] {
        std::fs::write(&user, body).unwrap();
        assert!(try_load_decision_from(Some(&user), None).is_err());
        assert!(try_load_from(Some(&user), None).is_err());
    }
    let loopback = BODY.replace(
        "https://example.invalid/private-path",
        "http://127.0.0.1/api",
    ) + "allow_loopback_http = true\n";
    std::fs::write(&user, loopback).unwrap();
    assert!(try_load_decision_from(Some(&user), None).is_ok());
}
#[test]
fn system_one_fallback_discloses_origin_and_model_without_path_or_reference() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.toml");
    let body = BODY.to_owned()
        + r#"
[decision.fallback]
purposes = ["routing"]
max_attempts = 2
attempt_timeout_ms = 100
total_timeout_ms = 200
[[decision.fallback.targets]]
backend = "system-one"
[decision.fallback.targets.system_one]
endpoint = "https://other.invalid/private-target"
model = "other-model"
credential_env = "POLARIS_SYSTEM_ONE_OTHER"
"#;
    std::fs::write(&user, body).unwrap();
    let config = try_load_decision_from(Some(&user), None).unwrap();
    let lines = config
        .fallback
        .as_ref()
        .unwrap()
        .disclosure(&config)
        .join("\n");
    assert!(lines.contains("https://other.invalid"));
    assert!(lines.contains("other-model"));
    assert!(!lines.contains("private-"));
    assert!(!lines.contains("POLARIS_SYSTEM_ONE_"));
}
