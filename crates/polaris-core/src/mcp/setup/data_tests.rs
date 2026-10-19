use super::*;
use polaris_mcp::registration::Capabilities;
use std::collections::BTreeSet;

fn proposal() -> Proposal {
    Proposal {
        server: Server {
            server_id: "fixture".into(),
            enabled_projects: BTreeSet::from(["project".into()]),
            transport: Destination::StreamableHttp {
                url: "https://fixture.invalid/mcp".into(),
                secret_headers: BTreeMap::from([(
                    "authorization".into(),
                    "credential_reference".into(),
                )]),
            },
            protocol_profile: Default::default(),
            capabilities: Capabilities {
                catalog_metadata: true,
                ..Default::default()
            },
            timeouts: Default::default(),
        },
        secrets: vec![SecretBinding {
            reference: "credential_reference".into(),
            source: PathBuf::from("fixture-source"),
            key_path: vec!["auth".into(), "bearer_token".into()],
            mac: [221; 32],
        }],
        expected_registry: 7,
        expected_secrets: 9,
        replaces: false,
    }
}

#[test]
fn setup_data_hash_binds_saved_fields_and_excludes_mac() {
    let original = proposal();
    original.server.validate().unwrap();
    let encoded = serde_json::to_vec(&original.server).unwrap();
    assert_eq!(
        serde_json::from_slice::<Server>(&encoded).unwrap(),
        original.server
    );
    let hash = original.payload_hash().unwrap();
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(original.clone().payload_hash().unwrap(), hash);
    let mut changed = original.clone();
    changed.secrets[0].mac = [1; 32];
    assert_eq!(changed.payload_hash().unwrap(), hash);
    let mut changes = vec![original.clone(); 7];
    changes[0].server.server_id = "other".into();
    changes[1].expected_registry += 1;
    changes[2].expected_secrets += 1;
    changes[3].secrets[0].reference = "other_reference".into();
    changes[4].secrets[0].source = PathBuf::from("other-source");
    changes[5].secrets[0].key_path.push("other_key".into());
    changes[6].replaces = true;
    for changed in changes {
        assert_ne!(changed.payload_hash().unwrap(), hash);
    }
}

#[test]
fn setup_data_description_preserves_fields_and_debug_hides_binding_details() {
    let proposal = proposal();
    let description = proposal.description("fixture\nname");
    for visible in [
        "fixture\\nname",
        "https://fixture.invalid/mcp",
        "fixture-source",
        "auth.bearer_token",
        "credential_reference",
        "discover_2026",
        "カタログの取得",
        "対象プロジェクト: project",
    ] {
        assert!(description.contains(visible), "missing {visible}");
    }
    assert!(!description.contains("fixture\nname"));
    assert!(!description.contains(&hex(&proposal.secrets[0].mac)));
    assert_eq!(
        format!("{proposal:?}"),
        "Proposal(fixture, [SecretBinding(credential_reference)])"
    );
    let mut replacement = proposal;
    replacement.replaces = true;
    assert!(replacement.description("fixture").contains("置き換え:"));
}

#[test]
fn setup_data_hmac_matches_public_fixture_and_hex_encodes_bytes() {
    assert_eq!(hex(&[0, 15, 16, 255]), "000f10ff");
    assert_eq!(
        hex(&hmac(&[7; 32], b"public fixture")),
        "7a8c150ef0005334b02b6d0449c231709987e1835c3ea74367f95b5f6b9e57de"
    );
    assert_ne!(
        hmac(&[7; 32], b"public fixture"),
        hmac(&[8; 32], b"public fixture")
    );
}

#[test]
fn setup_data_ids_are_bounded_and_bound_to_identity() {
    let id = sanitized_id("Fixture / MCP", "source-one");
    assert!(id.starts_with("Fixture___MCP-"));
    assert_eq!(id, sanitized_id("Fixture / MCP", "source-one"));
    assert_ne!(id, sanitized_id("Fixture / MCP", "source-two"));
    let bounded = sanitized_id(&"x".repeat(100), "source-one");
    assert_eq!(bounded.len(), 64);
    assert!(
        bounded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    );
}

#[test]
fn setup_data_credentials_distinguish_safe_arguments_and_urls() {
    assert!(!credential_like(&["--port".into(), "8141".into()]));
    for args in [
        vec!["--token".into(), "public-fixture".into()],
        vec!["--api-key=public-fixture".into()],
        vec!["Authorization: Bearer public-fixture".into()],
        vec!["ghp_public_fixture".into()],
    ] {
        assert!(credential_like(&args));
    }
    assert!(!url_credential_like("https://fixture.invalid/mcp"));
    assert!(url_credential_like(
        "https://fixture-user:fixture-value@fixture.invalid/mcp"
    ));
}
