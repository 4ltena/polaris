//! A01: the SDK stays a pinned client with no server, child-process or macro
//! support, and the workspace keeps its own reqwest line.
use std::collections::BTreeSet;

#[test]
fn rmcp_is_pinned_to_client_features_without_child_process_or_server_support() {
    let manifest = include_str!("../Cargo.toml");
    let workspace = include_str!("../../../Cargo.toml");
    let lock = include_str!("../../../Cargo.lock");
    let rmcp = manifest
        .lines()
        .find(|line| line.starts_with("rmcp = "))
        .expect("rmcp dependency");
    assert!(rmcp.contains("version = \"=3.3.0\"") && rmcp.contains("default-features = false"));
    let features: BTreeSet<&str> = rmcp
        .split("features = [")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .expect("feature list")
        .split(',')
        .map(|feature| feature.trim().trim_matches('"'))
        .collect();
    assert_eq!(
        features,
        BTreeSet::from([
            "client",
            "reqwest",
            "transport-async-rw",
            "transport-streamable-http-client-reqwest",
        ])
    );
    let package = lock
        .split("[[package]]")
        .find(|package| package.contains("name = \"rmcp\""))
        .expect("locked rmcp");
    assert!(package.contains("version = \"3.3.0\""));
    for name in ["process-wrap", "rmcp-macros", "schemars"] {
        assert!(!lock.contains(&format!("name = \"{name}\"")), "{name}");
    }
    assert!(
        workspace
            .lines()
            .any(|line| line.starts_with("reqwest = { version = \"0.12\""))
    );
}
