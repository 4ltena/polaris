//! Durable role metadata validation without provider resolution.
use super::*;
use polaris_skills::AgentAccess;
use serde_json::json;

fn binding(role: &str) -> SavedRoleBinding {
    SavedRoleBinding {
        role: role.into(),
        runtime: SavedRuntime::Ollama,
        endpoint: Some("http://127.0.0.1:11434".into()),
        model: "fixture:model".into(),
        effort: None,
        observed_tool_support: ToolSupport::Unknown,
    }
}
fn agent(name: &str) -> AgentType {
    AgentType {
        name: name.into(),
        description: String::new(),
        body: String::new(),
        path: "/never-read".into(),
        allowed_tools: vec![],
        access: AgentAccess::Read,
        tier: polaris_skills::Tier::Dwarf,
        tier_min: polaris_skills::Tier::Dwarf,
        wall_seconds: 1,
        max_turns: 1,
        workflow_phase: None,
        continuation: false,
        output_schema: "/never-read/schema.json".into(),
    }
}

#[test]
fn saved_roles_round_trip_normalizes_sorts_and_preserves_observation() {
    for support in [
        ToolSupport::Supported,
        ToolSupport::Unsupported,
        ToolSupport::Unknown,
    ] {
        let mut b = binding("b");
        b.observed_tool_support = support;
        b.runtime = SavedRuntime::LmStudio;
        let saved = SavedRoleBindings::new(vec![b, binding("a")]).unwrap();
        assert_eq!(saved.bindings[0].role, "a");
        assert_eq!(
            saved.bindings[1].endpoint.as_deref(),
            Some("http://127.0.0.1:11434/")
        );
        let wire = serde_json::to_value(&saved).unwrap();
        assert_eq!(wire["bindings"][1]["runtime"], "lm_studio");
        assert!(wire["bindings"][1].get("authorized").is_none());
        let restored: SavedRoleBindings = serde_json::from_value(wire).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored, saved);
        assert_eq!(restored.bindings[1].observed_tool_support, support);
    }
}

#[test]
fn saved_roles_empty_is_explicit_and_catalog_check_is_exact_and_unique() {
    let empty = SavedRoleBindings::new(vec![]).unwrap();
    empty.validate_catalog(&[agent("unconfigured")]).unwrap();
    let saved = SavedRoleBindings::new(vec![binding("reviewer")]).unwrap();
    saved
        .validate_catalog(&[agent("reviewer"), agent("other")])
        .unwrap();
    assert!(saved.validate_catalog(&[]).is_err());
    assert!(saved.validate_catalog(&[agent("Reviewer")]).is_err());
    assert!(
        saved
            .validate_catalog(&[agent("reviewer"), agent("reviewer")])
            .is_err()
    );
}

#[test]
fn saved_roles_bounds_versions_duplicates_and_noncanonical_storage_fail() {
    let limit = (0..MAX_ROLE_BINDINGS)
        .map(|n| binding(&format!("role-{n:02}")))
        .collect();
    SavedRoleBindings::new(limit).unwrap().validate().unwrap();
    assert!(
        SavedRoleBindings::new(
            (0..=MAX_ROLE_BINDINGS)
                .map(|n| binding(&format!("role-{n:02}")))
                .collect()
        )
        .is_err()
    );
    assert!(SavedRoleBindings::new(vec![binding("same"), binding("same")]).is_err());
    let mut saved = SavedRoleBindings::new(vec![binding("a"), binding("b")]).unwrap();
    saved.bindings.swap(0, 1);
    assert!(saved.validate().is_err());
    saved.bindings.swap(0, 1);
    saved.bindings[0].endpoint.as_mut().unwrap().pop();
    assert!(saved.validate().is_err());
    saved.bindings[0].endpoint.as_mut().unwrap().push('/');
    saved.schema_version = 2;
    assert!(matches!(
        saved.validate(),
        Err(StoreError::UnsupportedVersion(2))
    ));
    for (role, model) in [
        ("".into(), "m".into()),
        ("r".into(), "".into()),
        ("r\n".into(), "m".into()),
        ("r".into(), "m\0".into()),
        ("r".repeat(MAX_ROLE_BYTES + 1), "m".into()),
        ("r".into(), "m".repeat(MAX_MODEL_BYTES + 1)),
        ("あ".repeat(43), "m".into()),
    ] {
        let mut b = binding(&role);
        b.model = model;
        assert!(SavedRoleBindings::new(vec![b]).is_err());
    }
    let mut b = binding(&"r".repeat(MAX_ROLE_BYTES));
    b.model = "m".repeat(MAX_MODEL_BYTES);
    SavedRoleBindings::new(vec![b]).unwrap();
}

#[test]
fn saved_roles_endpoint_rejects_credentials_remote_and_implicit_hosts() {
    for endpoint in [
        "http://localhost:11434",
        "http://user:secret@127.0.0.1:11434",
        "http://192.0.2.1:11434",
        "http://127.0.0.1:11434/v1",
        "http://127.0.0.1?key=secret",
        "http://127.0.0.1#fragment",
        "http://127.1",
        "file:///tmp/model",
    ] {
        let mut b = binding("r");
        b.endpoint = Some(endpoint.into());
        assert!(SavedRoleBindings::new(vec![b]).is_err(), "{endpoint}");
    }
    let mut b = binding("r");
    b.endpoint = Some("http://[::1]:1234".into());
    let saved = SavedRoleBindings::new(vec![b]).unwrap();
    assert_eq!(
        saved.bindings[0].endpoint.as_deref(),
        Some("http://[::1]:1234/")
    );
}

#[test]
fn saved_roles_wire_rejects_unknown_missing_and_fabricated_capabilities() {
    let saved = SavedRoleBindings::new(vec![binding("r")]).unwrap();
    let wire = serde_json::to_value(saved).unwrap();
    let mut changed = wire.clone();
    changed["authorized"] = json!(true);
    assert!(serde_json::from_value::<SavedRoleBindings>(changed).is_err());
    let mut changed = wire.clone();
    changed["bindings"][0]["local_execution"] = json!(true);
    assert!(serde_json::from_value::<SavedRoleBindings>(changed).is_err());
    for (field, value) in [
        ("runtime", json!("cloud")),
        ("observed_tool_support", json!(true)),
        ("observed_tool_support", json!("verified")),
    ] {
        let mut changed = wire.clone();
        changed["bindings"][0][field] = value;
        assert!(serde_json::from_value::<SavedRoleBindings>(changed).is_err());
    }
    let mut changed = wire;
    changed["bindings"][0]
        .as_object_mut()
        .unwrap()
        .remove("observed_tool_support");
    assert!(serde_json::from_value::<SavedRoleBindings>(changed).is_err());
}

#[test]
fn saved_roles_deserialization_requires_validation_before_publication() {
    let saved = SavedRoleBindings::new(vec![binding("r")]).unwrap();
    let wire = serde_json::to_value(&saved).unwrap();
    for invalid in [
        json!({"schema_version": 1, "bindings": vec![wire["bindings"][0].clone(); 33]}),
        json!({"schema_version": 9, "bindings": []}),
        json!({"schema_version": 1, "bindings": [wire["bindings"][0].clone(), wire["bindings"][0].clone()]}),
    ] {
        let restored: SavedRoleBindings = serde_json::from_value(invalid).unwrap();
        assert!(restored.validate().is_err());
        assert!(restored.validate_catalog(&[agent("r")]).is_err());
    }
    let mut invalid = wire;
    invalid["bindings"][0]["endpoint"] = json!("http://127.0.0.1:11434");
    let restored: SavedRoleBindings = serde_json::from_value(invalid).unwrap();
    assert!(restored.validate().is_err());
}

fn tier_binding(tier: SavedTier) -> SavedTierBinding {
    SavedTierBinding {
        tier,
        runtime: SavedRuntime::Account,
        endpoint: None,
        model: "gpt-5.4-mini".into(),
        effort: Some("low".into()),
    }
}

#[test]
fn an_account_model_has_no_endpoint_and_a_local_one_must_have_one() {
    // An account model reached through the run's own registration has no
    // origin to record; recording one would claim an endpoint nobody
    // authorized. A local runtime without one has nowhere to send.
    let mut remote = binding("a");
    remote.runtime = SavedRuntime::Account;
    remote.endpoint = None;
    assert!(SavedRoleBindings::new(vec![remote.clone()]).is_ok());

    remote.endpoint = Some("http://127.0.0.1:11434".into());
    assert!(matches!(
        SavedRoleBindings::new(vec![remote]),
        Err(StoreError::Corrupt("role endpoint presence"))
    ));

    let mut local = binding("a");
    local.endpoint = None;
    assert!(matches!(
        SavedRoleBindings::new(vec![local]),
        Err(StoreError::Corrupt("role endpoint presence"))
    ));
}

#[test]
fn an_effort_nobody_accepts_is_refused_where_it_is_typed() {
    // Pinned as a literal, not read back out of `EFFORTS`: the loop below
    // would pass just as happily against a set that had lost `max` or
    // regained `ultra`.
    assert_eq!(EFFORTS, ["low", "medium", "high", "xhigh", "max"]);
    let mut held = binding("a");
    for effort in EFFORTS {
        held.effort = Some(effort.into());
        assert!(
            SavedRoleBindings::new(vec![held.clone()]).is_ok(),
            "{effort}"
        );
    }
    held.effort = None;
    assert!(SavedRoleBindings::new(vec![held.clone()]).is_ok());
    // Caught when it is saved rather than at the first request, which is
    // where the settings screen can still say so.
    for refused in ["ultra", "HIGH", "", "extra high"] {
        held.effort = Some(refused.into());
        assert!(
            matches!(
                SavedRoleBindings::new(vec![held.clone()]),
                Err(StoreError::Corrupt("effort"))
            ),
            "{refused}"
        );
    }
}

#[test]
fn weights_are_stored_once_each_and_in_order() {
    let saved = SavedTierBindings::new(vec![
        tier_binding(SavedTier::Supergiant),
        tier_binding(SavedTier::Dwarf),
    ])
    .expect("two different weights");
    assert_eq!(
        saved.bindings.iter().map(|b| b.tier).collect::<Vec<_>>(),
        vec![SavedTier::Dwarf, SavedTier::Supergiant],
        "stored in weight order so the file reads the same every time"
    );
    // Two entries for one weight would leave which model it buys up to
    // iteration order.
    assert!(matches!(
        SavedTierBindings::new(vec![
            tier_binding(SavedTier::Dwarf),
            tier_binding(SavedTier::Dwarf)
        ]),
        Err(StoreError::Corrupt("tier order or duplicate"))
    ));
    assert!(SavedTierBindings::default().validate().is_ok());
}

#[test]
fn the_stored_weight_and_the_declared_one_are_the_same_four() {
    for tier in polaris_skills::Tier::ALL {
        assert_eq!(polaris_skills::Tier::from(SavedTier::from(tier)), tier);
    }
    // The stored spelling is what the configuration keys off, so it has to
    // match the frontmatter's.
    for (tier, name) in [
        (SavedTier::Dwarf, "dwarf"),
        (SavedTier::Subgiant, "subgiant"),
        (SavedTier::Giant, "giant"),
        (SavedTier::Supergiant, "supergiant"),
    ] {
        assert_eq!(json!(tier), json!(name));
        assert_eq!(polaris_skills::Tier::from(tier).name(), name);
    }
}

/// A profile saved before the weights were renamed still loads. The three
/// old spellings were written to disk by shipped builds, so dropping them
/// would silently reset a stored role to a default weight rather than
/// failing where someone could see it.
#[test]
fn the_spellings_written_before_the_rename_still_load() {
    for (stored, tier) in [
        ("low", SavedTier::Dwarf),
        ("medium", SavedTier::Subgiant),
        ("ultra", SavedTier::Supergiant),
    ] {
        assert_eq!(
            serde_json::from_value::<SavedTier>(json!(stored)).expect(stored),
            tier
        );
    }
}
