//! Read-only preview and descriptor-bound source/registration consistency checks.

use super::*;
use std::os::unix::fs::symlink;

#[test]
fn preparation_and_cancel_are_read_only_and_retain_the_bound_card() {
    if isolated(concat!(
        module_path!(),
        "::preparation_and_cancel_are_read_only_and_retain_the_bound_card"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let before = snapshot(&fixture.root);
    let preparation = fixture.prepare();
    let preview = preparation.preview();
    assert_eq!(preparation.preset(), "unity-local-v2");
    assert_eq!(preview.server_id, SERVER);
    assert_eq!(preview.project_id, fixture.host.project());
    assert_eq!(preview.source, fixture.source);
    assert_eq!(
        preview.old_cwd,
        fixture.home.join(".polaris/mcp/servers").join(SERVER)
    );
    assert_eq!(preview.node, fixture.node);
    assert_eq!(preview.entry, fixture.source.join("build/index.js"));
    assert_eq!(preview.node_sha256, digest(&fixture.node));
    assert_eq!(preview.entry_sha256, digest(&preview.entry));
    assert_eq!(preview.hardlink_pair, PAIR.map(PathBuf::from));
    assert_eq!(
        preview.hardlink_pair_sha256,
        digest(&fixture.source.join(PAIR[0]))
    );
    assert_eq!(
        preview.fixed_exclusions,
        [PathBuf::from("node_modules/.bin")]
    );
    assert_eq!(preview.selected_files, 5);
    assert_eq!(preview.selected_bytes, 2 * 9_817_266 + 10);
    assert_eq!(preview.registry_before_revision, 1);
    assert_eq!(preview.registry_after_revision, 2);
    assert_eq!(preview.protocol_profile, ProtocolProfile::Auto2024Compat);
    assert_eq!(preview.sandbox_profile, "loopback");
    let server_parent = fixture.home.join(".polaris/mcp/servers").join(SERVER);
    assert_eq!(preview.new_cwd.parent(), Some(server_parent.as_path()));
    let name = preview.new_cwd.file_name().unwrap().to_str().unwrap();
    assert_eq!(name.len(), "runtime-".len() + 32);
    assert!(
        name.strip_prefix("runtime-")
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    );
    assert!(!preview.new_cwd.exists());
    assert!(!fixture.state.join("management-effects-v1").exists());
    assert!(!fixture.state.join("rewind-workspace").exists());
    assert_eq!(snapshot(&fixture.root), before);
    drop(preparation);
    assert_eq!(
        snapshot(&fixture.root),
        before,
        "cancelling only drops descriptors"
    );
    assert_eq!(fixture.registry(), fixture.before);
}

#[test]
fn preparation_refuses_missing_store_and_existing_lock_without_creating_them() {
    if isolated(concat!(
        module_path!(),
        "::preparation_refuses_missing_store_and_existing_lock_without_creating_them"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let absent_home = fixture.root.join("absent-home");
    fs::create_dir(&absent_home).unwrap();
    let host = LocalHost::open_for_test(&fixture.project, &absent_home, false).unwrap();
    let before = snapshot(&fixture.root);
    assert!(UnityLocalV2Preparation::prepare(&host, SERVER, &fixture.source).is_err());
    assert_eq!(snapshot(&fixture.root), before);
    assert!(!absent_home.join(".polaris").exists());

    let lock = fixture.home.join(".polaris/mcp/owner.lock");
    fs::remove_file(&lock).unwrap();
    let before = snapshot(&fixture.root);
    assert!(UnityLocalV2Preparation::prepare(&fixture.host, SERVER, &fixture.source).is_err());
    assert!(!lock.exists());
    assert_eq!(snapshot(&fixture.root), before);
}

#[test]
fn preparation_requires_the_registered_source_entry_and_node_binding() {
    if isolated(concat!(
        module_path!(),
        "::preparation_requires_the_registered_source_entry_and_node_binding"
    )) {
        return;
    }
    let mut fixtures = Vec::new();
    for case in [
        "unknown",
        "http",
        "disabled",
        "extra-arg",
        "entry",
        "node-digest",
        "entry-digest",
        "source-alias",
        "wrong-source",
    ] {
        let mut fixture = Fixture::new();
        let mut source = fixture.source.clone();
        let mut server = SERVER;
        match case {
            "unknown" => server = "missing",
            "http" => fixture.replace_registry(|server| {
                server.transport = Destination::StreamableHttp {
                    url: "https://example.invalid/other".into(),
                    secret_headers: BTreeMap::new(),
                };
            }),
            "disabled" => fixture.replace_registry(|server| {
                server.enabled_projects.clear();
            }),
            "extra-arg" => fixture.replace_registry(|server| {
                if let Destination::Stdio { args, .. } = &mut server.transport {
                    args.push("--ambiguous".into());
                }
            }),
            "entry" => {
                let other = fixture.source.join("build/other.js");
                fs::write(&other, b"other").unwrap();
                let digest = digest(&other);
                fixture.replace_registry(|server| {
                    if let Destination::Stdio {
                        args,
                        script,
                        script_sha256,
                        ..
                    } = &mut server.transport
                    {
                        *args = vec![other.to_str().unwrap().into()];
                        *script = Some(other);
                        *script_sha256 = Some(digest);
                    }
                });
            }
            "node-digest" => fixture.replace_registry(|server| {
                if let Destination::Stdio {
                    executable_sha256, ..
                } = &mut server.transport
                {
                    *executable_sha256 = Some("a".repeat(64));
                }
            }),
            "entry-digest" => {
                let entry = fixture.source.join("build/index.js");
                fixture.replace_registry(|server| {
                    if let Destination::Stdio {
                        script,
                        script_sha256,
                        ..
                    } = &mut server.transport
                    {
                        *script = Some(entry);
                        *script_sha256 = Some("b".repeat(64));
                    }
                });
            }
            "source-alias" => {
                source = fixture.root.join("source-alias");
                symlink(&fixture.source, &source).unwrap();
            }
            "wrong-source" => {
                source = fixture.root.join("other-source");
                make_source(&source);
            }
            _ => unreachable!(),
        }
        let before = snapshot(&fixture.root);
        assert!(
            UnityLocalV2Preparation::prepare(&fixture.host, server, &source).is_err(),
            "case {case}"
        );
        assert_eq!(snapshot(&fixture.root), before, "case {case}");
        fixtures.push(fixture);
    }
}

#[test]
fn preparation_also_accepts_a_matching_existing_optional_entry_pin() {
    if isolated(concat!(
        module_path!(),
        "::preparation_also_accepts_a_matching_existing_optional_entry_pin"
    )) {
        return;
    }
    let mut fixture = Fixture::new();
    let entry = fixture.source.join("build/index.js");
    let sha256 = digest(&entry);
    fixture.replace_registry(|server| {
        if let Destination::Stdio {
            script,
            script_sha256,
            ..
        } = &mut server.transport
        {
            *script = Some(entry.clone());
            *script_sha256 = Some(sha256.clone());
        }
    });
    let before = snapshot(&fixture.root);
    let preparation = fixture.prepare();
    assert_eq!(preparation.preview().entry, entry);
    assert_eq!(preparation.preview().entry_sha256, sha256);
    assert_eq!(snapshot(&fixture.root), before);
}

#[test]
fn approval_bound_source_inputs_are_rechecked_before_any_effect_or_copy() {
    if isolated(concat!(
        module_path!(),
        "::approval_bound_source_inputs_are_rechecked_before_any_effect_or_copy"
    )) {
        return;
    }
    let mut fixtures = Vec::new();
    for case in [
        "package-bytes",
        "package-swap",
        "lock-added",
        "lock-removed",
        "lock-swap",
        "entry-swap",
        "node-swap",
        "pair-bytes",
        "pair-symlink",
        "source-swap",
    ] {
        let fixture = Fixture::new();
        let preparation = fixture.prepare();
        let destination = preparation.preview().new_cwd.clone();
        match case {
            "package-bytes" => fs::write(fixture.source.join("package.json"), b"[]").unwrap(),
            "package-swap" | "lock-swap" | "entry-swap" | "node-swap" => {
                let path = match case {
                    "package-swap" => fixture.source.join("package.json"),
                    "lock-swap" => fixture.source.join("package-lock.json"),
                    "entry-swap" => fixture.source.join("build/index.js"),
                    _ => fixture.node.clone(),
                };
                let bytes = fs::read(&path).unwrap();
                let mode = fs::metadata(&path).unwrap().permissions();
                fs::rename(&path, path.with_extension("retired")).unwrap();
                fs::write(&path, bytes).unwrap();
                fs::set_permissions(path, mode).unwrap();
            }
            "lock-added" => fs::write(fixture.source.join("yarn.lock"), b"added").unwrap(),
            "lock-removed" => fs::remove_file(fixture.source.join("package-lock.json")).unwrap(),
            "pair-bytes" => {
                use std::io::Write;
                fs::OpenOptions::new()
                    .write(true)
                    .open(fixture.source.join(PAIR[0]))
                    .unwrap()
                    .write_all(b"changed")
                    .unwrap();
            }
            "pair-symlink" => {
                let right = fixture.source.join(PAIR[1]);
                fs::remove_file(&right).unwrap();
                symlink(fixture.source.join(PAIR[0]), right).unwrap();
            }
            "source-swap" => {
                fs::rename(&fixture.source, fixture.root.join("retired-source")).unwrap();
                make_source(&fixture.source);
            }
            _ => unreachable!(),
        }
        let (lease, effects) = fixture.effects();
        let before = snapshot(&fixture.root);
        assert!(
            preparation
                .apply(&fixture.host, &effects, &lease, &fixture.project)
                .is_err(),
            "case {case}"
        );
        assert_eq!(fixture.registry(), fixture.before, "case {case}");
        assert!(effects.effects().unwrap().is_empty(), "case {case}");
        assert!(!destination.exists(), "case {case}");
        assert_eq!(snapshot(&fixture.root), before, "case {case}");
        fixtures.push(fixture);
    }
}
