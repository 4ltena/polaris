//! Deterministic registry destination and pinned-path migration mapping tests.
use super::*;
use std::collections::BTreeSet;

use crate::registration::{Capabilities, ProtocolProfile, Server, Timeouts};

const HOME: &str = "/logical-home";
const OLD: &str = "/logical-home/.polaris/mcp/servers/alpha";
const NEW: &str = "/logical-home/.polaris/mcp-v2/servers/alpha";

fn server() -> Server {
    Server {
        server_id: "alpha".into(),
        enabled_projects: BTreeSet::from(["opaque-project".into()]),
        transport: Destination::Stdio {
            command: PathBuf::from(format!("{OLD}/bin/node")),
            args: vec![
                format!("{OLD}/site/main.js"),
                "--mode".into(),
                "fast".into(),
            ],
            cwd: PathBuf::from(OLD),
            sandbox_profile: "read-only".into(),
            secret_env: BTreeMap::from([("API_TOKEN".into(), "token_reference".into())]),
            readable_roots: vec![PathBuf::from(OLD)],
            executable_sha256: Some("a".repeat(64)),
            script: Some(PathBuf::from(format!("{OLD}/site/main.js"))),
            script_sha256: Some("b".repeat(64)),
        },
        protocol_profile: ProtocolProfile::Auto2025Compat,
        capabilities: Capabilities {
            catalog_metadata: true,
            resource_read: true,
            prompt_selection: false,
            tool_call: false,
        },
        timeouts: Timeouts {
            catalog_ttl_seconds: 400,
            connect_seconds: 11,
            catalog_seconds: 40,
            operation_seconds: 121,
            connect_retries: 3,
        },
    }
}

fn registry(server: Server) -> Registry {
    Registry {
        schema_version: 1,
        revision: 73,
        servers: vec![server],
    }
}

fn pin(server: &Server) -> PinnedPathsV1 {
    let Destination::Stdio {
        command, script, ..
    } = &server.transport
    else {
        panic!("stdio fixture")
    };
    PinnedPathsV1 {
        schema_version: 1,
        package: "@example/stdio-server".into(),
        version: "2.7.9+build.4".into(),
        runtime: command.clone(),
        entry: script.as_ref().unwrap().clone(),
    }
}

fn pins(server: &Server) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([(
        server.server_id.clone(),
        serde_json::to_vec(&pin(server)).unwrap(),
    )])
}

fn map(
    source: &Registry,
    pins: &BTreeMap<String, Vec<u8>>,
) -> Result<ManagedPathTransformV2, Error> {
    transform_managed_paths_v2(source, Path::new(HOME), pins)
}

fn http_server() -> Server {
    let mut result = server();
    result.server_id = "http".into();
    result.transport = Destination::StreamableHttp {
        url: "https://example.com/mcp".into(),
        secret_headers: BTreeMap::from([("authorization".into(), "http_reference".into())]),
    };
    result
}

#[test]
fn exact_managed_paths_preserve_relative_components_and_pin() {
    let original = server();
    let source = registry(original.clone());
    let result = map(&source, &pins(&original)).unwrap();
    let Destination::Stdio {
        command,
        args,
        cwd,
        readable_roots,
        script,
        executable_sha256,
        script_sha256,
        ..
    } = &result.registry.servers[0].transport
    else {
        panic!("stdio")
    };
    assert_eq!(cwd, Path::new(NEW));
    assert_eq!(command, Path::new(&format!("{NEW}/bin/node")));
    assert_eq!(
        script.as_deref(),
        Some(Path::new(&format!("{NEW}/site/main.js")))
    );
    assert_eq!(
        args,
        &[
            format!("{NEW}/site/main.js"),
            "--mode".into(),
            "fast".into()
        ]
    );
    assert_eq!(readable_roots, &[PathBuf::from(NEW)]);
    assert_eq!(executable_sha256.as_deref(), Some("a".repeat(64).as_str()));
    assert_eq!(script_sha256.as_deref(), Some("b".repeat(64).as_str()));
    let mapped = &result.pins["alpha"];
    assert_eq!(mapped.runtime, *command);
    assert_eq!(Some(&mapped.entry), script.as_ref());
    assert_eq!(mapped.package, pin(&original).package);
    assert_eq!(mapped.version, pin(&original).version);
}

#[test]
fn managed_cwd_requires_exact_own_server_directory() {
    for cwd in [
        "/logical-home/.polaris/mcp",
        "/logical-home/.polaris/mcp/servers",
        "/logical-home/.polaris/mcp/servers/beta",
        "/logical-home/.polaris/mcp/servers/alpha/site",
        "/logical-home/.polaris/mcp/other",
    ] {
        let mut source = registry(server());
        let Destination::Stdio { cwd: field, .. } = &mut source.servers[0].transport else {
            unreachable!()
        };
        *field = PathBuf::from(cwd);
        source.validate().unwrap();
        assert!(map(&source, &BTreeMap::new()).is_err(), "{cwd}");
    }
}

#[test]
fn managed_cwd_accepts_only_exact_own_runtime_version_child() {
    let operation = "0123456789abcdef0123456789abcdef";
    let old_version = format!("{OLD}/runtime-{operation}");
    let new_version = format!("{NEW}/runtime-{operation}");
    let mut original = server();
    let Destination::Stdio {
        cwd,
        command,
        script,
        args,
        readable_roots,
        ..
    } = &mut original.transport
    else {
        unreachable!()
    };
    *cwd = PathBuf::from(&old_version);
    *script = Some(PathBuf::from(format!("{old_version}/build/index.js")));
    args[0] = format!("{old_version}/build/index.js");
    *readable_roots = vec![PathBuf::from(&old_version)];
    *command = PathBuf::from("/usr/local/bin/node");
    let held_pins = pins(&original);
    let transformed = map(&registry(original), &held_pins).unwrap();
    let Destination::Stdio {
        cwd,
        script,
        args,
        readable_roots,
        ..
    } = &transformed.registry.servers[0].transport
    else {
        unreachable!()
    };
    assert_eq!(cwd, Path::new(&new_version));
    assert_eq!(
        script.as_deref(),
        Some(Path::new(&format!("{new_version}/build/index.js")))
    );
    assert_eq!(args[0], format!("{new_version}/build/index.js"));
    assert_eq!(readable_roots, &[PathBuf::from(new_version)]);
    assert_eq!(
        transformed.pins["alpha"].runtime,
        PathBuf::from("/usr/local/bin/node")
    );
    assert_eq!(
        transformed.pins["alpha"].entry,
        PathBuf::from(format!("{NEW}/runtime-{operation}/build/index.js"))
    );

    for cwd in [
        format!("{OLD}/runtime-0123456789ABCDEF0123456789ABCDEF"),
        format!("{OLD}/runtime-0123456789abcdef0123456789abcdeg"),
        format!("{OLD}/runtime-0123456789abcdef0123456789abcde"),
        format!("{OLD}/runtime-{operation}/build"),
        format!("/logical-home/.polaris/mcp/servers/beta/runtime-{operation}"),
    ] {
        let mut source = registry(server());
        let Destination::Stdio { cwd: field, .. } = &mut source.servers[0].transport else {
            unreachable!()
        };
        *field = PathBuf::from(&cwd);
        source.validate().unwrap();
        assert!(map(&source, &BTreeMap::new()).is_err(), "{cwd}");
    }
}

#[test]
fn command_script_and_readable_roots_cannot_cross_server_tree() {
    let mut command = registry(server());
    let Destination::Stdio {
        command: field,
        readable_roots,
        ..
    } = &mut command.servers[0].transport
    else {
        unreachable!()
    };
    *field = PathBuf::from("/logical-home/.polaris/mcp/servers/beta/node");
    readable_roots.clear();
    command.validate().unwrap();
    assert!(map(&command, &BTreeMap::new()).is_err());

    let mut script = registry(server());
    let Destination::Stdio {
        script: field,
        args,
        readable_roots,
        ..
    } = &mut script.servers[0].transport
    else {
        unreachable!()
    };
    *field = Some(PathBuf::from(
        "/logical-home/.polaris/mcp/servers/beta/main.js",
    ));
    args[0] = field.as_ref().unwrap().to_str().unwrap().into();
    readable_roots.clear();
    script.validate().unwrap();
    assert!(map(&script, &BTreeMap::new()).is_err());

    let mut roots = registry(server());
    let Destination::Stdio { readable_roots, .. } = &mut roots.servers[0].transport else {
        unreachable!()
    };
    *readable_roots = vec![PathBuf::from("/logical-home/.polaris/mcp/servers")];
    roots.validate().unwrap();
    assert!(map(&roots, &BTreeMap::new()).is_err());
}

#[test]
fn nonnormalized_home_and_stored_paths_are_rejected() {
    let source = registry(server());
    for home in [
        "relative",
        "/logical-home/",
        "/logical-home/./",
        "/logical-home/../other",
    ] {
        assert!(transform_managed_paths_v2(&source, Path::new(home), &BTreeMap::new()).is_err());
    }
    for path in [
        "/logical-home/.polaris/mcp/servers/alpha/../beta",
        "/logical-home/.polaris/mcp/servers/alpha//bin/node",
        "/logical-home/.polaris/mcp/servers/alpha/./bin/node",
    ] {
        let mut source = registry(server());
        let Destination::Stdio {
            command,
            readable_roots,
            ..
        } = &mut source.servers[0].transport
        else {
            unreachable!()
        };
        *command = PathBuf::from(path);
        readable_roots.clear();
        assert!(map(&source, &BTreeMap::new()).is_err(), "{path}");
    }
}

#[test]
fn external_runtime_and_external_cwd_remain_exactly_unchanged() {
    let mut original = server();
    let Destination::Stdio { command, cwd, .. } = &mut original.transport else {
        unreachable!()
    };
    *command = PathBuf::from("/usr/local/bin/node");
    *cwd = PathBuf::from("/external/workspace");
    let result = map(&registry(original.clone()), &pins(&original)).unwrap();
    let Destination::Stdio {
        command,
        cwd,
        script,
        ..
    } = &result.registry.servers[0].transport
    else {
        unreachable!()
    };
    assert_eq!(command, Path::new("/usr/local/bin/node"));
    assert_eq!(cwd, Path::new("/external/workspace"));
    assert_eq!(
        script.as_deref(),
        Some(Path::new(&format!("{NEW}/site/main.js")))
    );
    assert_eq!(result.pins["alpha"].runtime, *command);
}

#[test]
fn wholly_external_stdio_destination_is_preserved() {
    let mut original = server();
    let Destination::Stdio {
        command,
        args,
        cwd,
        readable_roots,
        script,
        script_sha256,
        ..
    } = &mut original.transport
    else {
        unreachable!()
    };
    *command = PathBuf::from("/external/runtime/node");
    *cwd = PathBuf::from("/external/workspace");
    *script = Some(PathBuf::from("/external/server/main.js"));
    *args = vec!["/external/server/main.js".into(), "opaque-value".into()];
    *readable_roots = vec![
        PathBuf::from("/external/runtime"),
        PathBuf::from("/external/server"),
    ];
    *script_sha256 = Some("c".repeat(64));
    let source = registry(original);
    let result = map(&source, &BTreeMap::new()).unwrap();
    assert_eq!(result.registry, source);
    assert!(
        result.projection.servers[0]
            .fields
            .iter()
            .all(|field| field.old_value_hash == field.new_value_hash)
    );
}

#[test]
fn component_similar_sibling_is_external_without_string_prefix_replacement() {
    let mut original = server();
    let Destination::Stdio {
        command,
        args,
        cwd,
        readable_roots,
        script,
        script_sha256,
        ..
    } = &mut original.transport
    else {
        unreachable!()
    };
    *command = PathBuf::from("/logical-home/.polaris/mcp-backup/bin/node");
    *cwd = PathBuf::from("/logical-home/.polaris/mcp-backup");
    args.clear();
    *script = None;
    *script_sha256 = None;
    *readable_roots = vec![PathBuf::from("/logical-home/.polaris/mcp-backup")];
    let source = registry(original);
    assert_eq!(map(&source, &BTreeMap::new()).unwrap().registry, source);
}

#[test]
fn exact_script_arguments_only_are_rewritten_without_reordering() {
    let mut original = server();
    let Destination::Stdio { args, .. } = &mut original.transport else {
        unreachable!()
    };
    *args = vec![
        "--before".into(),
        format!("{OLD}/site/main.js"),
        "after".into(),
        format!("{OLD}/site/main.js"),
    ];
    let result = map(&registry(original), &BTreeMap::new()).unwrap();
    let Destination::Stdio { args, .. } = &result.registry.servers[0].transport else {
        unreachable!()
    };
    assert_eq!(
        args,
        &[
            "--before",
            &format!("{NEW}/site/main.js"),
            "after",
            &format!("{NEW}/site/main.js")
        ]
    );
    let indexes: Vec<_> = result.projection.servers[0]
        .fields
        .iter()
        .filter_map(|field| match field.field {
            ManagedPathFieldV2::ScriptArgument { index } => Some(index),
            _ => None,
        })
        .collect();
    assert_eq!(indexes, [1, 3]);
}

#[test]
fn opaque_arguments_containing_old_root_block_instead_of_guessing() {
    for argument in [
        format!("--entry={OLD}/site/main.js"),
        format!("{OLD}/site/./main.js"),
        "/logical-home/.polaris/mcp/servers/beta/main.js".into(),
        format!("prefix {OLD}/site/main.js suffix"),
    ] {
        let mut original = server();
        let Destination::Stdio { args, .. } = &mut original.transport else {
            unreachable!()
        };
        args.push(argument.clone());
        assert!(
            map(&registry(original), &BTreeMap::new()).is_err(),
            "{argument}"
        );
    }
}

#[test]
fn pin_schema_fields_and_bytes_are_closed_and_bounded() {
    let original = server();
    let source = registry(original.clone());
    let mut value = serde_json::to_value(pin(&original)).unwrap();
    value["schema_version"] = 2.into();
    assert!(
        map(
            &source,
            &BTreeMap::from([("alpha".into(), serde_json::to_vec(&value).unwrap())])
        )
        .is_err()
    );
    value["schema_version"] = 1.into();
    value["unknown"] = true.into();
    assert!(
        map(
            &source,
            &BTreeMap::from([("alpha".into(), serde_json::to_vec(&value).unwrap())])
        )
        .is_err()
    );
    assert!(
        map(
            &source,
            &BTreeMap::from([("alpha".into(), vec![b' '; MAX_JSON_BYTES + 1])])
        )
        .is_err()
    );
    assert!(map(&source, &BTreeMap::from([("alpha".into(), b"{}".to_vec())])).is_err());
    for field in ["package", "version"] {
        let mut value = serde_json::to_value(pin(&original)).unwrap();
        value[field] = "".into();
        assert!(
            map(
                &source,
                &BTreeMap::from([("alpha".into(), serde_json::to_vec(&value).unwrap())])
            )
            .is_err()
        );
    }
}

#[test]
fn pin_runtime_entry_and_exact_argument_must_match_registry() {
    let original = server();
    let source = registry(original.clone());
    for (field, changed) in [
        ("runtime", "/usr/local/bin/other-node".to_owned()),
        ("entry", format!("{OLD}/site/other.js")),
    ] {
        let mut value = serde_json::to_value(pin(&original)).unwrap();
        value[field] = changed.into();
        assert!(
            map(
                &source,
                &BTreeMap::from([("alpha".into(), serde_json::to_vec(&value).unwrap())])
            )
            .is_err()
        );
    }
    let mut alias = original.clone();
    let Destination::Stdio { args, .. } = &mut alias.transport else {
        unreachable!()
    };
    args[0] = format!("{OLD}/site/./main.js");
    // The registration parser compares Path components, while migration requires exact bytes.
    alias.validate().unwrap();
    assert!(map(&registry(alias), &pins(&original)).is_err());
}

#[test]
fn managed_pin_entry_cannot_be_an_external_or_wrong_server_script() {
    for entry in [
        "/external/main.js",
        "/logical-home/.polaris/mcp/servers/beta/main.js",
    ] {
        let mut original = server();
        let Destination::Stdio {
            script,
            args,
            readable_roots,
            ..
        } = &mut original.transport
        else {
            unreachable!()
        };
        *script = Some(PathBuf::from(entry));
        args[0] = entry.into();
        readable_roots.clear();
        original.validate().unwrap();
        assert!(map(&registry(original.clone()), &pins(&original)).is_err());
    }
}

#[test]
fn unknown_http_and_excess_pins_are_not_silently_dropped() {
    let source = registry(server());
    assert!(
        map(
            &source,
            &BTreeMap::from([("unregistered".into(), b"{}".to_vec())])
        )
        .is_err()
    );
    let http = http_server();
    let http_source = registry(http);
    assert!(
        map(
            &http_source,
            &BTreeMap::from([("http".into(), b"{}".to_vec())])
        )
        .is_err()
    );
    let excess: BTreeMap<_, _> = (0..=MAX_SERVERS)
        .map(|index| (format!("server_{index}"), b"{}".to_vec()))
        .collect();
    assert!(map(&source, &excess).is_err());
}

#[test]
fn revision_http_and_all_nonpath_fields_are_preserved() {
    let original = server();
    let mut source = registry(original.clone());
    source.servers.push(http_server());
    let result = map(&source, &pins(&original)).unwrap();
    assert_eq!(result.registry.schema_version, source.schema_version);
    assert_eq!(result.registry.revision, source.revision);
    assert_eq!(result.registry.servers[1], source.servers[1]);
    let mapped = &result.registry.servers[0];
    assert_eq!(mapped.server_id, original.server_id);
    assert_eq!(mapped.enabled_projects, original.enabled_projects);
    assert_eq!(mapped.protocol_profile, original.protocol_profile);
    assert_eq!(mapped.capabilities, original.capabilities);
    assert_eq!(mapped.timeouts, original.timeouts);
    let Destination::Stdio {
        sandbox_profile,
        secret_env,
        ..
    } = &mapped.transport
    else {
        unreachable!()
    };
    let Destination::Stdio {
        sandbox_profile: old_profile,
        secret_env: old_env,
        ..
    } = &original.transport
    else {
        unreachable!()
    };
    assert_eq!(sandbox_profile, old_profile);
    assert_eq!(secret_env, old_env);
    assert!(result.projection.servers[1].fields.is_empty());
    assert_eq!(
        result.projection.servers[1].old_destination_hash,
        result.projection.servers[1].new_destination_hash
    );
    let projection = serde_json::to_string(&result.projection).unwrap();
    for raw in [
        HOME,
        "fast",
        "API_TOKEN",
        "token_reference",
        "@example/stdio-server",
        "https://example.com/mcp",
    ] {
        assert!(!projection.contains(raw), "{raw}");
    }
}

#[test]
fn destination_field_and_document_payload_hash_mapping_is_deterministic() {
    let original = server();
    let source = registry(original.clone());
    let result = map(&source, &pins(&original)).unwrap();
    let repeated = map(&source, &pins(&original)).unwrap();
    assert_eq!(result.projection, repeated.projection);
    let mapping = &result.projection.servers[0];
    assert_eq!(
        mapping.old_destination_hash,
        original.destination_hash().unwrap()
    );
    assert_eq!(
        mapping.new_destination_hash,
        result.registry.servers[0].destination_hash().unwrap()
    );
    assert_ne!(mapping.old_destination_hash, mapping.new_destination_hash);
    let cwd = mapping
        .fields
        .iter()
        .find(|field| field.field == ManagedPathFieldV2::Cwd)
        .unwrap();
    assert_eq!(
        cwd.old_value_hash,
        canonical_hash(&serde_json::json!(OLD)).unwrap()
    );
    assert_eq!(
        cwd.new_value_hash,
        canonical_hash(&serde_json::json!(NEW)).unwrap()
    );
    let exact_doc_hash = |registry: &Registry| {
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&Some(registry)).unwrap())
        )
    };
    assert_eq!(
        result.projection.source_registry_payload_hash,
        exact_doc_hash(&source)
    );
    assert_eq!(
        result.projection.new_registry_payload_hash,
        exact_doc_hash(&result.registry)
    );
    assert_ne!(
        result.projection.source_registry_payload_hash,
        result.projection.new_registry_payload_hash
    );
}

fn unregistered(
    source: Option<&Registry>,
    id: &str,
    pin: &PinnedPathsV1,
) -> Result<UnregisteredPinTransformV2, Error> {
    transform_unregistered_pin_v2(
        source,
        id,
        &serde_json::to_vec(pin).unwrap(),
        Path::new(HOME),
    )
}

#[test]
fn unregistered_pin_maps_own_tree_and_projects_only_typed_field_hashes() {
    let original = pin(&server());
    let result = unregistered(None, "alpha", &original).unwrap();
    assert_eq!(result.server_id, "alpha");
    assert_eq!(result.pin.runtime, PathBuf::from(format!("{NEW}/bin/node")));
    assert_eq!(
        result.pin.entry,
        PathBuf::from(format!("{NEW}/site/main.js"))
    );
    assert_eq!(result.pin.package, original.package);
    assert_eq!(result.pin.version, original.version);
    assert_eq!(result.pin.schema_version, 1);
    assert_eq!(result.fields.len(), 2);
    assert_eq!(result.fields[0].field, ManagedPathFieldV2::PinRuntime);
    assert_eq!(result.fields[1].field, ManagedPathFieldV2::PinEntry);
    assert_eq!(
        result.fields[1].old_value_hash,
        canonical_hash(&serde_json::json!(original.entry)).unwrap()
    );
    assert_eq!(
        result.fields[1].new_value_hash,
        canonical_hash(&serde_json::json!(result.pin.entry)).unwrap()
    );
    let projection = serde_json::to_string(&result.fields).unwrap();
    assert!(!projection.contains(HOME));
    assert!(!projection.contains("@example"));
}

#[test]
fn unregistered_pin_preserves_external_normalized_runtime_and_entry() {
    let mut original = pin(&server());
    original.runtime = PathBuf::from("/usr/local/bin/node");
    original.entry = PathBuf::from("/logical-home/.polaris/mcp-sibling/site/main.js");
    let result = unregistered(None, "alpha", &original).unwrap();
    assert!(result.pin == original);
    assert!(
        result
            .fields
            .iter()
            .all(|field| field.old_value_hash == field.new_value_hash)
    );
}

#[test]
fn forty_unregistered_materialized_pins_are_independent_of_registry_limit() {
    let source = registry(server());
    let original = source.clone();
    let mut ids = BTreeSet::new();
    for index in 0..40 {
        let id = format!("materialized_{index}");
        let old = format!("{HOME}/.polaris/mcp/servers/{id}");
        let new = format!("{HOME}/.polaris/mcp-v2/servers/{id}");
        let mut held = pin(&server());
        held.runtime = PathBuf::from("/usr/local/bin/node");
        held.entry = PathBuf::from(format!("{old}/site/main.js"));
        let result = unregistered(Some(&source), &id, &held).unwrap();
        assert_eq!(result.pin.runtime, held.runtime);
        assert_eq!(
            result.pin.entry,
            PathBuf::from(format!("{new}/site/main.js"))
        );
        ids.insert(result.server_id);
    }
    assert_eq!(ids.len(), 40);
    assert_eq!(source, original);
}

#[test]
fn unregistered_pin_rejects_source_registered_stdio_and_http_ids() {
    let mut source = registry(server());
    source.servers.push(http_server());
    let held = pin(&server());
    assert!(unregistered(Some(&source), "alpha", &held).is_err());
    let mut held_http = held.clone();
    held_http.entry = PathBuf::from(format!("{HOME}/.polaris/mcp/servers/http/site/main.js"));
    assert!(unregistered(Some(&source), "http", &held_http).is_err());
}

#[test]
fn unregistered_pin_foreign_managed_tree_and_root_control_paths_block() {
    for foreign in [
        "/logical-home/.polaris/mcp/servers/beta/site/main.js",
        "/logical-home/.polaris/mcp/servers/alpha-other/site/main.js",
        "/logical-home/.polaris/mcp/servers.json.tmp",
        "/logical-home/.polaris/mcp/runtime/node",
    ] {
        for runtime in [false, true] {
            let mut held = pin(&server());
            if runtime {
                held.runtime = PathBuf::from(foreign);
            } else {
                held.entry = PathBuf::from(foreign);
            }
            assert!(unregistered(None, "alpha", &held).is_err(), "{foreign}");
        }
    }
}

#[test]
fn unregistered_pin_closed_schema_rejects_incomplete_unknown_and_oversized_wire() {
    let held = pin(&server());
    let mut schema = serde_json::to_value(&held).unwrap();
    schema["schema_version"] = 9.into();
    let mut unknown = serde_json::to_value(&held).unwrap();
    unknown["temporary"] = true.into();
    for bytes in [
        serde_json::to_vec(&schema).unwrap(),
        serde_json::to_vec(&unknown).unwrap(),
        br#"{"schema_version":1,"package":"unfinished"#.to_vec(),
        vec![b' '; MAX_JSON_BYTES + 1],
    ] {
        assert!(transform_unregistered_pin_v2(None, "alpha", &bytes, Path::new(HOME)).is_err());
    }
}

#[test]
fn unregistered_pin_validates_id_source_registry_home_and_path_spelling() {
    let held = pin(&server());
    for id in ["", "..", "alpha/beta", "alpha.tmp", "bad\0id"] {
        assert!(unregistered(None, id, &held).is_err(), "{id}");
    }
    let mut invalid_source = registry(server());
    invalid_source.revision = 0;
    assert!(unregistered(Some(&invalid_source), "materialized", &held).is_err());
    let bytes = serde_json::to_vec(&held).unwrap();
    for home in ["/logical-home/", "/logical-home/../other", "relative"] {
        assert!(transform_unregistered_pin_v2(None, "alpha", &bytes, Path::new(home)).is_err());
    }
    for path in [
        "/logical-home/.polaris/mcp/servers/alpha/site/../main.js",
        "/logical-home/.polaris/mcp/servers/alpha//site/main.js",
        "/logical-home/.polaris/mcp/servers/alpha/./site/main.js",
    ] {
        let mut invalid = held.clone();
        invalid.entry = PathBuf::from(path);
        assert!(unregistered(None, "alpha", &invalid).is_err(), "{path}");
    }
}

#[test]
fn unregistered_pin_preserves_opaque_package_text_and_legal_payload_suffixes() {
    let mut held = pin(&server());
    held.package = format!("package-text:{OLD}/opaque");
    held.version = "7.0.0+kept.exactly".into();
    held.runtime = PathBuf::from(format!("{OLD}/bin/node.tmp"));
    held.entry = PathBuf::from(format!("{OLD}/site/main.partial"));
    let bytes = serde_json::to_vec_pretty(&held).unwrap();
    let result = transform_unregistered_pin_v2(None, "alpha", &bytes, Path::new(HOME)).unwrap();
    assert_eq!(result.pin.package, held.package);
    assert_eq!(result.pin.version, held.version);
    assert_eq!(
        result.pin.runtime,
        PathBuf::from(format!("{NEW}/bin/node.tmp"))
    );
    assert_eq!(
        result.pin.entry,
        PathBuf::from(format!("{NEW}/site/main.partial"))
    );
    let decoded: PinnedPathsV1 =
        serde_json::from_slice(&serde_json::to_vec(&result.pin).unwrap()).unwrap();
    assert!(decoded == result.pin);
}
