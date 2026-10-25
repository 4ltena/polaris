//! Host detection from Codex and Claude configs, with secrets bound by HMAC.
use super::*;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;

fn home() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    (temp, home)
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn detector(home: &Path) -> Detector {
    Detector::new(
        home.to_owned(),
        vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
    )
}

fn saved(store: Option<&polaris_mcp::store::Store>) -> polaris_mcp::store::Snapshot {
    store.map_or(
        polaris_mcp::store::Snapshot {
            registry: None,
            secrets: None,
        },
        |store| store.load_snapshot().unwrap(),
    )
}

fn by_name<'a>(found: &'a [Detected], name: &str) -> &'a Detected {
    found.iter().find(|d| d.name == name).unwrap()
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixtureReadRoot {
    Pass,
    Protection,
    Limit,
    Other,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixtureDecision {
    Accept,
    RetryInstall,
    RetryCwd,
    Fatal,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct FixtureInputs {
    structure_ok: bool,
    snapshot_ok: bool,
    path_overlap: bool,
    install_collisions: [bool; 3],
    read_root: FixtureReadRoot,
    cwd_collision: bool,
    cwd_invalid_path: bool,
    registry_absent: bool,
}

#[cfg(target_os = "linux")]
fn classify_fixture(input: FixtureInputs, cwd_refused: bool) -> FixtureDecision {
    if !input.structure_ok || !input.snapshot_ok || input.path_overlap {
        return FixtureDecision::Fatal;
    }
    let collision = input.install_collisions.contains(&true);
    match (input.read_root, collision) {
        (FixtureReadRoot::Protection, true) => return FixtureDecision::RetryInstall,
        (FixtureReadRoot::Pass, false) => {}
        _ => return FixtureDecision::Fatal,
    }
    if cwd_refused {
        if input.cwd_collision && input.cwd_invalid_path && input.registry_absent {
            FixtureDecision::RetryCwd
        } else {
            FixtureDecision::Fatal
        }
    } else {
        FixtureDecision::Accept
    }
}

#[cfg(target_os = "linux")]
fn fixture_decision_table() {
    use FixtureDecision::*;
    let safe = FixtureInputs {
        structure_ok: true,
        snapshot_ok: true,
        path_overlap: false,
        install_collisions: [false; 3],
        read_root: FixtureReadRoot::Pass,
        cwd_collision: false,
        cwd_invalid_path: false,
        registry_absent: true,
    };
    assert_eq!(classify_fixture(safe, false), Accept);
    for index in 0..3 {
        let mut input = FixtureInputs {
            read_root: FixtureReadRoot::Protection,
            ..safe
        };
        input.install_collisions[index] = true;
        assert_eq!(classify_fixture(input, false), RetryInstall);
    }
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                read_root: FixtureReadRoot::Protection,
                ..safe
            },
            false
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                snapshot_ok: false,
                ..safe
            },
            false
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                path_overlap: true,
                ..safe
            },
            false
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                structure_ok: false,
                ..safe
            },
            false
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                read_root: FixtureReadRoot::Limit,
                ..safe
            },
            false
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                read_root: FixtureReadRoot::Other,
                ..safe
            },
            false
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                install_collisions: [true, false, false],
                ..safe
            },
            false
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                cwd_collision: true,
                cwd_invalid_path: true,
                ..safe
            },
            true
        ),
        RetryCwd
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                cwd_collision: false,
                cwd_invalid_path: true,
                ..safe
            },
            true
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                cwd_collision: true,
                cwd_invalid_path: false,
                ..safe
            },
            true
        ),
        Fatal
    );
    assert_eq!(
        classify_fixture(
            FixtureInputs {
                cwd_collision: true,
                cwd_invalid_path: true,
                registry_absent: false,
                ..safe
            },
            true
        ),
        Fatal
    );
}

#[cfg(target_os = "linux")]
fn fixture_read_root(result: Result<(), polaris_mcp::Error>) -> FixtureReadRoot {
    match result {
        Ok(()) => FixtureReadRoot::Pass,
        Err(polaris_mcp::Error::Protection) => FixtureReadRoot::Protection,
        Err(polaris_mcp::Error::Limit) => FixtureReadRoot::Limit,
        Err(_) => FixtureReadRoot::Other,
    }
}

#[cfg(target_os = "linux")]
fn install_preflight(
    home: &Path,
    generation: usize,
    role: &str,
    discarded_install: usize,
    discarded_cwd: usize,
) -> FixtureDecision {
    let install = home.join("tools/example");
    let bin = install.join("bin");
    let script = bin.join("server");
    let nodes = [&install, &bin, &script];
    let metadata = nodes.map(|path| fs::symlink_metadata(path).ok());
    let euid = unsafe { libc::geteuid() };
    let directory_ok = |index: usize| {
        metadata[index].as_ref().is_some_and(|m| {
            m.is_dir()
                && !m.file_type().is_symlink()
                && m.uid() == euid
                && m.mode() & 0o022 == 0
                && m.nlink() >= 1
        })
    };
    let entries_are = |path: &Path, expected: &Path| {
        fs::read_dir(path)
            .ok()
            .and_then(|iter| {
                iter.map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<Vec<_>>>()
                    .ok()
            })
            .is_some_and(|entries| entries == [expected])
    };
    let diagnostic = home.join(".polaris/mcp/servers/__fixture_preflight__");
    let structure_ok = directory_ok(0)
        && directory_ok(1)
        && metadata[2].as_ref().is_some_and(|m| {
            m.is_file()
                && !m.file_type().is_symlink()
                && m.uid() == euid
                && m.mode() & 0o7777 == 0o755
                && m.nlink() == 1
        })
        && entries_are(&install, &bin)
        && entries_are(&bin, &script)
        && nodes.iter().all(|path| {
            path.canonicalize().ok().as_deref() == Some(path.as_path())
                && !polaris_tools::path_policy::is_denied(path)
        })
        && diagnostic.starts_with(home)
        && !diagnostic.exists()
        && !install.starts_with(&diagnostic)
        && !diagnostic.starts_with(&install);
    let snapshot = polaris_auth::protection::with_protected_paths(|registered| {
        let overlap = registered
            .paths()
            .iter()
            .any(|path| path.starts_with(&install));
        let matches = metadata.each_ref().map(|m| {
            m.as_ref().is_some_and(|m| {
                registered
                    .identities()
                    .contains(&polaris_auth::protection::ProtectedIdentity {
                        dev: m.dev(),
                        ino: m.ino(),
                    })
            })
        });
        (overlap, matches)
    });
    let snapshot_ok = snapshot.is_ok();
    let (path_overlap, install_collisions) = snapshot.unwrap_or((false, [false; 3]));
    let read_root = fixture_read_root(crate::mcp::runtime::resources::admit_read_root(
        &install,
        home,
        &diagnostic,
        &[],
    ));
    let decision = classify_fixture(
        FixtureInputs {
            structure_ok,
            snapshot_ok,
            path_overlap,
            install_collisions,
            read_root,
            cwd_collision: false,
            cwd_invalid_path: false,
            registry_absent: true,
        },
        false,
    );
    eprintln!(
        "mcp setup fixture role={role} generation={generation} install_collisions={install_collisions:?} read_root={read_root:?} cwd_class=NotChecked decision={decision:?} discarded_install={discarded_install} discarded_cwd={discarded_cwd}"
    );
    decision
}

#[test]
fn codex_http_headers_become_bound_secret_references_and_env_names_are_not_read() {
    let (_temp, home) = home();
    write(
        &home.join(".codex/config.toml"),
        r#"
[mcp_servers.docs]
url = "https://docs.example.com/mcp"
http_headers = { authorization = "Bearer synthetic-token" }

[mcp_servers.env_only]
url = "https://env.example.com/mcp"
bearer_token_env_var = "DOCS_TOKEN"

[mcp_servers.oauth]
url = "https://oauth.example.com/mcp"
auth = "oauth"
"#,
    );
    let detector = detector(&home);
    let found = detector.detect("project", &saved(None));
    let docs = by_name(&found, "docs");
    assert_eq!(docs.transport, "streamable_http");
    assert_eq!(docs.reason, None);
    let proposal = docs.proposal.as_ref().unwrap();
    let Destination::StreamableHttp {
        url,
        secret_headers,
    } = &proposal.server.transport
    else {
        panic!("http");
    };
    assert_eq!(url, "https://docs.example.com/mcp");
    let reference = &secret_headers["authorization"];
    assert!(reference.starts_with("docs-"));
    assert_eq!(
        proposal.server.enabled_projects,
        ["project".to_owned()].into()
    );
    assert_eq!(detector.secret_values(proposal).unwrap()[0].0, *reference);
    assert!(!format!("{found:?}").contains("synthetic-token"));
    assert_eq!(
        by_name(&found, "env_only").reason,
        Some(Reason::EnvironmentReference)
    );
    assert_eq!(
        by_name(&found, "oauth").reason,
        Some(Reason::UnsupportedAuth)
    );
    assert_ne!(
        found[0].id,
        detector.detect("project", &saved(None))[0].id,
        "opaque per detection"
    );
    polaris_auth::protection::with_protected_paths(|registered| {
        assert!(
            registered
                .paths()
                .contains(&home.join(".codex/config.toml"))
        );
    })
    .unwrap();
}

#[test]
fn stdio_launchers_credentials_and_env_shebangs_are_classified() {
    let make_fixture = || {
        let (temp, home) = home();
        let tool = home.join("tools/example/bin/server");
        write(&tool, "#!/usr/bin/env sh\nexit 0\n");
        let split = home.join("tools/split/server");
        write(&split, "#!/usr/bin/env -S sh -e\nexit 0\n");
        let long = home.join("tools/long/server");
        write(&long, &format!("#!/bin/sh {}\n", "a".repeat(600)));
        for path in [&tool, &split, &long] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        write(
            &home.join(".codex/config.toml"),
            &format!(
                r#"
[mcp_servers.script]
command = "{}"
args = ["--verbose"]
env = {{ SERVER_KEY = "synthetic-value" }}

[mcp_servers.launcher]
command = "npx"
args = ["-y", "example-server"]

[mcp_servers.leaky]
command = "{}"
args = ["--api-key=synthetic-inline"]

[mcp_servers.header]
command = "{}"
args = ["--header", "PRIVATE-TOKEN: glpat-synthetic"]

[mcp_servers.direct_env]
command = "/usr/bin/env"
args = ["sh"]

[mcp_servers.split]
command = "{}"

[mcp_servers.long]
command = "{}"
"#,
                tool.display(),
                tool.display(),
                tool.display(),
                split.display(),
                long.display()
            ),
        );
        (temp, home, tool)
    };
    #[cfg(target_os = "linux")]
    let (_retained, _temp, home, tool, found) = {
        fixture_decision_table();
        let mut retained = Vec::new();
        let mut accepted = None;
        for generation in 1..=32 {
            let (temp, home, tool) = make_fixture();
            let detector = detector(&home);
            match install_preflight(&home, generation, "classification", retained.len(), 0) {
                FixtureDecision::Accept => {
                    let found = detector.detect("project", &saved(None));
                    let script = by_name(&found, "script").proposal.as_ref().unwrap();
                    let Destination::Stdio {
                        readable_roots,
                        cwd,
                        ..
                    } = &script.server.transport
                    else {
                        panic!("stdio");
                    };
                    assert_eq!(readable_roots, &[home.join("tools/example")]);
                    assert!(cwd.starts_with(home.join(".polaris/mcp/servers")));
                    assert_eq!(
                        fixture_read_root(crate::mcp::runtime::resources::admit_read_root(
                            &home.join("tools/example"),
                            &home,
                            cwd,
                            &[],
                        )),
                        FixtureReadRoot::Pass
                    );
                    eprintln!(
                        "mcp setup fixture role=classification generation={generation} cwd_class=Pass accepted=1 discarded_install={} discarded_cwd=0",
                        retained.len()
                    );
                    accepted = Some((temp, home, tool, found));
                    break;
                }
                FixtureDecision::RetryInstall if generation < 32 => retained.push(temp),
                decision => {
                    panic!("classification fixture generation={generation} decision={decision:?}")
                }
            }
        }
        let (temp, home, tool, found) =
            accepted.expect("classification fixture exhausted 32 generations");
        (retained, temp, home, tool, found)
    };
    #[cfg(not(target_os = "linux"))]
    let (_temp, home, tool, found) = {
        let (temp, home, tool) = make_fixture();
        let found = detector(&home).detect("project", &saved(None));
        (temp, home, tool, found)
    };
    let script = by_name(&found, "script");
    assert_eq!(script.reason, None);
    let proposal = script.proposal.as_ref().unwrap();
    let Destination::Stdio {
        command,
        args,
        readable_roots,
        executable_sha256,
        script,
        script_sha256,
        sandbox_profile,
        secret_env,
        cwd,
    } = &proposal.server.transport
    else {
        panic!("stdio");
    };
    assert_eq!(*command, Path::new("/bin/sh").canonicalize().unwrap());
    assert_eq!(args[0], tool.canonicalize().unwrap().to_str().unwrap());
    assert_eq!(args[1], "--verbose");
    assert_eq!(readable_roots, &[home.join("tools/example")]);
    assert!(executable_sha256.is_some());
    assert_eq!(
        script.as_deref(),
        Some(tool.canonicalize().unwrap().as_path())
    );
    assert!(script_sha256.is_some());
    assert_eq!(sandbox_profile, "read-only");
    assert!(secret_env["SERVER_KEY"].starts_with("script-"));
    assert!(cwd.starts_with(home.join(".polaris/mcp/servers")));
    assert_eq!(
        by_name(&found, "launcher").reason,
        Some(Reason::LauncherNotSupported)
    );
    assert_eq!(
        by_name(&found, "leaky").reason,
        Some(Reason::CredentialInArguments)
    );
    assert_eq!(
        by_name(&found, "header").reason,
        Some(Reason::CredentialInArguments)
    );
    assert_eq!(by_name(&found, "direct_env").reason, Some(Reason::Invalid));
    assert_eq!(by_name(&found, "split").reason, Some(Reason::Invalid));
    assert_eq!(by_name(&found, "long").reason, Some(Reason::Invalid));
}

#[test]
fn claude_servers_include_projects_reject_sse_and_rebinding_needs_the_same_value() {
    let (_temp, home) = home();
    let path = home.join(".claude.json");
    let body = |token: &str, extra: &str| {
        format!(
            r#"{{"numStartups":3,{extra}"mcpServers":{{"search":{{"type":"http","url":"https://search.example.com/mcp","headers":{{"Authorization":"Bearer {token}"}}}},"legacy":{{"type":"sse","url":"https://legacy.example.com/sse"}}}},"projects":{{"/work/app":{{"mcpServers":{{"local-docs":{{"type":"http","url":"https://local.example.com/mcp"}}}}}}}}}}"#
        )
    };
    write(&path, &body("synthetic-a", ""));
    let detector = detector(&home);
    let found = detector.detect("project", &saved(None));
    assert_eq!(
        by_name(&found, "legacy").reason,
        Some(Reason::UnsupportedTransport)
    );
    assert_eq!(by_name(&found, "local-docs").reason, None);
    let search = by_name(&found, "search").proposal.clone().unwrap();
    write(&path, &body("synthetic-a", r#""tipsHistory":{"x":1},"#));
    assert_eq!(detector.secret_values(&search).unwrap().len(), 1);
    write(&path, &body("synthetic-b", ""));
    assert!(matches!(
        detector.secret_values(&search),
        Err(polaris_mcp::Error::Auth)
    ));
}

#[test]
fn placeholders_home_bins_and_scoped_names_are_distinguished() {
    let (_temp, home) = home();
    let tool = home.join("bin/server");
    write(&tool, "exit 0\n");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    write(
        &home.join(".claude.json"),
        &format!(
            r#"{{"mcpServers":{{"search":{{"type":"http","url":"https://a.example.com/mcp"}},"placeholder":{{"type":"http","url":"https://c.example.com/mcp","headers":{{"Authorization":"Bearer ${{TOKEN}}"}}}},"home-bin":{{"command":"{}"}},"shimmed":{{"command":"shimmed"}}}},"projects":{{"/work/app":{{"mcpServers":{{"search":{{"type":"http","url":"https://b.example.com/mcp"}}}}}}}}}}"#,
            tool.display()
        ),
    );
    let shims = home.join(".local/share/mise/shims");
    fs::create_dir_all(&shims).unwrap();
    std::os::unix::fs::symlink(&tool, shims.join("shimmed")).unwrap();
    let found = Detector::new(
        home.clone(),
        vec![shims, PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
    )
    .detect("project", &saved(None));
    let ids: Vec<_> = found
        .iter()
        .filter(|d| d.name == "search")
        .map(|d| d.proposal.as_ref().unwrap().server.server_id.clone())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        by_name(&found, "placeholder").reason,
        Some(Reason::EnvironmentReference)
    );
    assert_eq!(
        by_name(&found, "home-bin").reason,
        Some(Reason::UnsupportedInstallLocation)
    );
    assert_eq!(
        by_name(&found, "shimmed").reason,
        Some(Reason::LauncherNotSupported)
    );
}

fn limits() -> crate::isolated_workspace::Limits {
    crate::isolated_workspace::Limits {
        max_entries: 32,
        max_files: 16,
        max_file_bytes: 8192,
        max_total_bytes: 32768,
        max_depth: 4,
    }
}

#[test]
fn approved_proposals_save_bound_secrets_before_the_registry_and_replace_by_id() {
    let (_temp, home) = home();
    let config = home.join(".codex/config.toml");
    let body = |header: &str, token: &str| {
        format!(
            "[mcp_servers.docs]\nurl = \"https://docs.example.com/mcp\"\nhttp_headers = {{ \"{header}\" = \"Bearer {token}\" }}\n"
        )
    };
    write(&config, &body("authorization", "synthetic-a"));
    let detector = detector(&home);
    let proposal = detector.detect("project", &saved(None))[0]
        .proposal
        .clone()
        .unwrap();
    let description = proposal.description("docs");
    assert!(description.contains("Endpoint: https://docs.example.com/mcp"));
    assert!(description.contains("mcp_servers.docs.http_headers.authorization"));
    assert!(description.contains("Network: used"));
    assert!(!description.contains("synthetic-a"));
    let store = polaris_mcp::store::Store::open(&home).unwrap();
    let admission = Admission {
        store: &store,
        home: &home,
        runtime_roots: &[],
        excluded_roots: &[],
        limits: limits(),
    };
    assert_eq!(detector.register(&proposal, &admission).unwrap(), 1);
    let reference = proposal.secrets[0].reference.clone();
    let snapshot = store.load_snapshot().unwrap();
    assert_eq!(
        snapshot.registry.unwrap().servers,
        std::slice::from_ref(&proposal.server)
    );
    assert_eq!(
        snapshot.secrets.unwrap().resolve(&reference).unwrap(),
        "Bearer synthetic-a"
    );
    assert!(
        !fs::read_to_string(home.join(".polaris/mcp/servers.json"))
            .unwrap()
            .contains("synthetic-a")
    );

    let current = |store| detector.detect("project", &saved(Some(store)));
    assert_eq!(
        current(&store)[0].reason,
        Some(Reason::AlreadyRegistered),
        "an unchanged saved server is reported, not proposed"
    );

    write(&config, &body("authorization", "synthetic-b"));
    assert_eq!(
        detector.register(&proposal, &admission),
        Err(polaris_mcp::Error::Auth)
    );
    let stale = current(&store)[0].proposal.clone().unwrap();
    assert!(stale.replaces && stale.description("docs").contains("Replacement"));
    assert_ne!(
        stale.secrets[0].reference, reference,
        "live references stay untouched"
    );
    let mut registry = store.load_registry().unwrap().unwrap();
    registry.servers[0].capabilities.tool_call = false;
    registry.servers[0].enabled_projects.insert("other".into());
    registry.revision = 2;
    store.save_registry(1, &registry).unwrap();
    assert_eq!(
        detector.register(&stale, &admission),
        Err(polaris_mcp::Error::RevisionConflict),
        "a settings change after detection needs a new detection"
    );

    write(&config, &body("x-api-key", "synthetic-c"));
    let renamed = current(&store)[0].proposal.clone().unwrap();
    assert_eq!(renamed.server.server_id, proposal.server.server_id);
    assert!(!renamed.server.capabilities.tool_call);
    assert!(renamed.server.enabled_projects.contains("other"));
    assert!(
        renamed
            .description("docs")
            .contains("Disabled operations: tool calls")
    );
    assert_eq!(detector.register(&renamed, &admission).unwrap(), 3);
    let secrets = store.load_secrets().unwrap().unwrap();
    assert_eq!(
        secrets.references().collect::<Vec<_>>(),
        [renamed.secrets[0].reference.as_str()]
    );
    assert_eq!(store.load_registry().unwrap().unwrap().servers.len(), 1);
}

#[test]
fn approval_text_escapes_source_names_and_names_every_saved_field() {
    let (_temp, home) = home();
    write(
        &home.join(".codex/config.toml"),
        "[mcp_servers.\"x\\n認証: なし\"]\nurl = \"https://docs.example.com/mcp\"\nbearer_token = \"synthetic\"\n",
    );
    let detected = detector(&home).detect("project", &saved(None));
    let proposal = detected[0].proposal.as_ref().unwrap();
    let description = proposal.description(&detected[0].name);
    assert!(
        !description
            .lines()
            .any(|line| line == "Authentication: none" || line.starts_with("Authentication: none"))
    );
    assert!(description.contains("x\\n認証: なし"));
    for field in [
        "with a leading \"Bearer \" as authorization header",
        "Transport: discover_2026",
        "Timeouts: connection",
        "Target project: project",
    ] {
        assert!(description.contains(field), "{field}");
    }
}

#[test]
fn stdio_proposals_get_a_private_cwd_and_pass_startup_admission_before_saving() {
    let make_fixture = || {
        let (temp, home) = home();
        let tool = home.join("tools/example/bin/server");
        write(&tool, "#!/bin/sh\nexit 0\n");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        write(
            &home.join(".claude.json"),
            &format!(
                r#"{{"mcpServers":{{"local":{{"command":"{}"}}}}}}"#,
                tool.display()
            ),
        );
        (temp, home)
    };
    #[cfg(target_os = "linux")]
    let runtime_roots = ["/bin", "/usr/bin", "/usr/lib"].map(PathBuf::from);
    #[cfg(not(target_os = "linux"))]
    let runtime_roots = ["/bin", "/usr/bin", "/usr/lib", "/System/Library"].map(PathBuf::from);
    #[cfg(target_os = "linux")]
    let (_retained, _temp, home, detector, proposal, store) = {
        fixture_decision_table();
        let mut retained = Vec::new();
        let mut discarded_install = 0;
        let mut discarded_cwd = 0;
        let mut accepted = None;
        for generation in 1..=32 {
            let (temp, home) = make_fixture();
            let detector = detector(&home);
            match install_preflight(
                &home,
                generation,
                "registration",
                discarded_install,
                discarded_cwd,
            ) {
                FixtureDecision::RetryInstall if generation < 32 => {
                    discarded_install += 1;
                    retained.push(temp);
                    continue;
                }
                FixtureDecision::Accept => {}
                decision => panic!(
                    "registration fixture generation={generation} install decision={decision:?}"
                ),
            }
            let proposal = detector.detect("project", &saved(None))[0]
                .proposal
                .clone()
                .unwrap();
            let Destination::Stdio {
                readable_roots,
                cwd,
                ..
            } = &proposal.server.transport
            else {
                panic!("stdio");
            };
            assert_eq!(readable_roots, &[home.join("tools/example")]);
            assert!(cwd.starts_with(home.join(".polaris/mcp/servers")));
            assert_eq!(
                fixture_read_root(crate::mcp::runtime::resources::admit_read_root(
                    &home.join("tools/example"),
                    &home,
                    cwd,
                    &[],
                )),
                FixtureReadRoot::Pass
            );
            let store = polaris_mcp::store::Store::open(&home).unwrap();
            let admission = Admission {
                store: &store,
                home: &home,
                runtime_roots: &runtime_roots,
                excluded_roots: &[],
                limits: limits(),
            };
            let description = proposal.description("local");
            assert!(description.contains("Readable locations"));
            assert!(description.contains("Network: not used"));
            match detector.register(&proposal, &admission) {
                Ok(1) => {
                    let cwd_meta = fs::symlink_metadata(cwd).unwrap();
                    let euid = unsafe { libc::geteuid() };
                    assert!(cwd_meta.is_dir() && !cwd_meta.file_type().is_symlink());
                    assert_eq!(cwd_meta.uid(), euid);
                    assert_eq!(cwd_meta.mode() & 0o7777, 0o700);
                    assert_eq!(cwd.canonicalize().unwrap(), *cwd);
                    eprintln!(
                        "mcp setup fixture role=registration generation={generation} cwd_class=Pass accepted=1 discarded_install={discarded_install} discarded_cwd={discarded_cwd}"
                    );
                    accepted = Some((temp, home, detector, proposal, store));
                    break;
                }
                Err(polaris_mcp::Error::Protection) => {
                    let cwd = store.server_directory(&proposal.server.server_id).unwrap();
                    let metadata = fs::symlink_metadata(&cwd).unwrap();
                    let euid = unsafe { libc::geteuid() };
                    let cwd_ok = metadata.is_dir()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == euid
                        && metadata.mode() & 0o7777 == 0o700;
                    let snapshot = polaris_auth::protection::with_protected_paths(|registered| {
                        let overlap = registered
                            .paths()
                            .iter()
                            .any(|path| path.starts_with(&cwd) || cwd.starts_with(path));
                        let collision = registered.identities().contains(
                            &polaris_auth::protection::ProtectedIdentity {
                                dev: metadata.dev(),
                                ino: metadata.ino(),
                            },
                        );
                        (overlap, collision)
                    });
                    let snapshot_ok = snapshot.is_ok();
                    let (overlap, collision) = snapshot.unwrap_or((true, false));
                    let invalid_path = matches!(
                        crate::isolated_workspace::RegisteredDirectory::open(&cwd, limits()),
                        Err(error) if error.kind == crate::isolated_workspace::ErrorKind::InvalidPath
                    );
                    let registry_absent = store.load_registry().unwrap().is_none();
                    let read_root =
                        fixture_read_root(crate::mcp::runtime::resources::admit_read_root(
                            &home.join("tools/example"),
                            &home,
                            &cwd,
                            &[],
                        ));
                    let decision = classify_fixture(
                        FixtureInputs {
                            structure_ok: cwd_ok,
                            snapshot_ok,
                            path_overlap: overlap,
                            install_collisions: [false; 3],
                            read_root,
                            cwd_collision: collision,
                            cwd_invalid_path: invalid_path,
                            registry_absent,
                        },
                        true,
                    );
                    eprintln!(
                        "mcp setup fixture role=registration generation={generation} cwd_class=Protection cwd_collision={collision} read_root={read_root:?} decision={decision:?} accepted=0 discarded_install={discarded_install} discarded_cwd={discarded_cwd}"
                    );
                    if decision != FixtureDecision::RetryCwd || generation == 32 {
                        panic!(
                            "registration fixture generation={generation} cwd decision={decision:?}"
                        );
                    }
                    discarded_cwd += 1;
                    retained.push(temp);
                }
                result => panic!("registration fixture generation={generation} result={result:?}"),
            }
        }
        let (temp, home, detector, proposal, store) =
            accepted.expect("registration fixture exhausted 32 generations");
        (retained, temp, home, detector, proposal, store)
    };
    #[cfg(not(target_os = "linux"))]
    let (_temp, home, detector, proposal, store) = {
        let (temp, home) = make_fixture();
        let detector = detector(&home);
        let proposal = detector.detect("project", &saved(None))[0]
            .proposal
            .clone()
            .unwrap();
        let store = polaris_mcp::store::Store::open(&home).unwrap();
        (temp, home, detector, proposal, store)
    };
    let admission = Admission {
        store: &store,
        home: &home,
        runtime_roots: &runtime_roots,
        excluded_roots: &[],
        limits: limits(),
    };
    let description = proposal.description("local");
    assert!(description.contains("Readable locations"));
    assert!(description.contains("Network: not used"));
    #[cfg(not(target_os = "linux"))]
    assert_eq!(detector.register(&proposal, &admission).unwrap(), 1);
    let Destination::Stdio { cwd, .. } = &proposal.server.transport else {
        panic!("stdio");
    };
    assert_eq!(cwd.metadata().unwrap().permissions().mode() & 0o777, 0o700);

    fs::write(home.join("tools/example/unsafe"), "x").unwrap();
    fs::set_permissions(
        home.join("tools/example/unsafe"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    assert_eq!(
        detector.register(&proposal, &admission),
        Err(polaris_mcp::Error::Protection)
    );
    assert_eq!(store.load_registry().unwrap().unwrap().revision, 1);
}

#[test]
fn known_app_servers_are_proposed_with_their_profiles_shown_on_the_card() {
    let (_temp, home) = home();
    let tool = home.join("tools/resolve/ResolveMCP");
    write(&tool, "#!/bin/sh\nexit 0\n");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    write(
        &home.join(".claude.json"),
        &format!(
            r#"{{"mcpServers":{{"resolve":{{"command":"{}"}},"unreal":{{"type":"http","url":"http://127.0.0.1:8000/mcp"}}}}}}"#,
            tool.display()
        ),
    );
    let found = detector(&home).detect("project", &saved(None));
    let resolve = by_name(&found, "resolve").proposal.clone().unwrap();
    assert_eq!(
        resolve.server.protocol_profile,
        ProtocolProfile::Auto2024Compat
    );
    let Destination::Stdio {
        sandbox_profile, ..
    } = &resolve.server.transport
    else {
        panic!("stdio");
    };
    assert_eq!(sandbox_profile, "loopback");
    let card = resolve.description("resolve");
    assert!(card.contains("App: DaVinci Resolve"));
    assert!(card.contains("Network: Uses only connections and listeners within this Mac"));
    assert!(card.contains("Transport: auto_2024_compat"));
    let unreal = by_name(&found, "unreal").proposal.clone().unwrap();
    let card = unreal.description("unreal");
    assert!(card.contains("App: Unreal Engine"));
    assert!(card.contains("localhost is not a trust boundary"));
}

#[test]
fn an_unregistrable_source_does_not_leave_the_other_unprotected() {
    let (_temp, home) = home();
    write(&home.join(".codex"), "not a directory");
    write(&home.join(".claude.json"), "{}");
    assert!(protect_sources(&home).is_err());
    polaris_auth::protection::with_protected_paths(|registered| {
        assert!(registered.paths().contains(&home.join(".claude.json")));
    })
    .unwrap();
}

#[tokio::test]
async fn local_setup_needs_the_external_permission_and_a_gate_answer() {
    use crate::{
        approval::{ApprovalPolicy, Approver, Decision, Gate},
        mcp::operation::ExternalCeiling,
    };
    use serde_json::json;
    struct Answers {
        decision: Decision,
        asked: usize,
    }
    impl Approver for Answers {
        fn ask(&mut self, reason: &str) -> Decision {
            assert!(reason.contains("Endpoint: https://docs.example.com/mcp"));
            self.asked += 1;
            self.decision
        }
    }
    let (_temp, home) = home();
    write(
        &home.join(".codex/config.toml"),
        "[mcp_servers.docs]\nurl = \"https://docs.example.com/mcp\"\n",
    );
    for (ceiling, policy, decision, saved) in [
        (
            ExternalCeiling::Denied,
            ApprovalPolicy::Always,
            Decision::Allow,
            false,
        ),
        (
            ExternalCeiling::Allowed,
            ApprovalPolicy::Never,
            Decision::Allow,
            false,
        ),
        (
            ExternalCeiling::Allowed,
            ApprovalPolicy::Always,
            Decision::Deny,
            false,
        ),
        (
            ExternalCeiling::Allowed,
            ApprovalPolicy::Always,
            Decision::Allow,
            true,
        ),
    ] {
        let tool = SetupTool::new(
            home.clone(),
            vec![],
            vec![],
            limits(),
            "project".into(),
            Authorization::Local(ceiling),
        );
        let mut gate = Gate::new(policy);
        let mut answers = Answers { decision, asked: 0 };
        let detected: serde_json::Value = serde_json::from_str(
            &tool
                .call(
                    &json!({"op":"detect"}),
                    std::future::pending(),
                    &mut gate,
                    &mut answers,
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let args = json!({"op":"register","candidate":detected["candidates"][0]["candidate"]});
        let result = tool
            .call(&args, std::future::pending(), &mut gate, &mut answers)
            .await;
        assert_eq!(result.is_ok(), saved, "{result:?}");
        if !saved {
            assert_eq!(result, Err(json!({"error":"denied"}).to_string()));
            assert!(!polaris_mcp::store::Store::exists(&home));
        }
        assert_eq!(
            answers.asked,
            usize::from(ceiling == ExternalCeiling::Allowed && policy == ApprovalPolicy::Always)
        );
    }
    let store = polaris_mcp::store::Store::open(&home).unwrap();
    assert_eq!(store.load_registry().unwrap().unwrap().revision, 1);
}

#[test]
fn project_and_source_configs_never_become_candidates() {
    let (_temp, home) = home();
    let project = home.join("work/app");
    let planted = "[mcp_servers.planted]\nurl = \"https://planted.example.com/mcp\"\n";
    write(
        &project.join(".mcp.json"),
        r#"{"mcpServers":{"planted":{"type":"http","url":"https://planted.example.com/mcp"}}}"#,
    );
    write(&project.join(".codex/config.toml"), planted);
    write(&project.join(".polaris/config.toml"), planted);
    let found = detector(&home).detect(project.to_str().unwrap(), &saved(None));
    assert!(found.is_empty(), "{found:?}");
}

/// A launcher is refused because the digest would be the launcher's, and the
/// code it fetches can change between launches. Once the package has been
/// fetched into the directory polaris keeps for that server, none of that is
/// true any more: the pin names an interpreter and a file, and every check
/// below applies to them unchanged.
#[test]
fn a_launcher_with_a_pin_registers_as_the_file_that_was_pinned() {
    let (_temp, home) = home();
    write(
        &home.join(".codex/config.toml"),
        "[mcp_servers.docs]\ncommand = \"npx\"\nargs = [\"-y\", \"@scope/docs-mcp\", \"--root\", \"/tmp\"]\n",
    );
    let server_id = sanitized_id(
        "docs",
        &format!(
            "{}\n{}",
            home.join(".codex/config.toml").display(),
            ["mcp_servers", "docs"].join("\n")
        ),
    );
    let directory = home.join(".polaris/mcp/servers").join(&server_id);
    fs::create_dir_all(&directory).unwrap();
    let entry = directory.join("node_modules/.bin/docs-mcp");
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(&entry, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        directory.join("pinned.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "package": "@scope/docs-mcp",
            "version": "4.5.6",
            "runtime": "/bin/sh",
            "entry": entry,
        }))
        .unwrap(),
    )
    .unwrap();

    let found = detector(&home).detect("project", &saved(None));
    let held = by_name(&found, "docs");
    assert_eq!(held.reason, None, "{:?}", held.reason);
    let proposal = held.proposal.as_ref().expect("a proposal");
    match &proposal.server.transport {
        polaris_mcp::registration::Destination::Stdio {
            command,
            args,
            script,
            script_sha256,
            readable_roots,
            ..
        } => {
            // The interpreter runs, and what it runs is the pinned file —
            // followed by the arguments the server itself asked for.
            assert_eq!(*command, Path::new("/bin/sh").canonicalize().unwrap());
            assert_eq!(
                args,
                &[
                    entry.to_str().unwrap().to_owned(),
                    "--root".to_owned(),
                    "/tmp".to_owned()
                ]
            );
            // The interpreter's digest is shared by every server that uses it
            // and says nothing about this one. The pinned file is what differs,
            // so it is digested, shown at approval and re-checked at launch.
            assert_eq!(script.as_deref(), Some(entry.as_path()));
            assert!(script_sha256.is_some(), "the pinned file is digested");
            // Nothing is offered as a readable root: the interpreter is under a
            // trusted runtime root, and the pinned file is inside the server's
            // own directory, which is the sandbox workspace already. A root
            // that overlaps the workspace is refused, and would take the whole
            // server down with it.
            assert!(readable_roots.is_empty(), "{readable_roots:?}");
        }
        other => panic!("expected stdio, got {other:?}"),
    }
}

/// A readable root is a whole directory, and one entry in it decides the lot:
/// on 2026-09-18 every node server on this machine was refused because
/// `/usr/local/bin` holds a world-writable binary and symlinks that leave the
/// tree. The executable itself is admissible in both cases, so registration
/// falls back to it rather than proposing a root a run will refuse — which is
/// tighter than the directory would have been anyway.
#[test]
fn an_install_tree_a_run_would_refuse_falls_back_to_the_executable() {
    let (_temp, home) = home();
    let mut interpreters = Vec::new();
    for directory in ["clean", "shared"] {
        let interpreter = home.join("tools").join(directory).join("interp");
        write(&interpreter, "\u{7f}ELF not a script\n");
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o755)).unwrap();
        interpreters.push(interpreter);
    }
    // The one entry that costs the whole directory its admission.
    let junk = home.join("tools/shared/anyone-may-write");
    write(&junk, "");
    fs::set_permissions(&junk, fs::Permissions::from_mode(0o666)).unwrap();
    write(
        &home.join(".codex/config.toml"),
        &format!(
            "[mcp_servers.clean]\ncommand = \"{}\"\n\n[mcp_servers.shared]\ncommand = \"{}\"\n",
            interpreters[0].display(),
            interpreters[1].display(),
        ),
    );

    let found = detector(&home).detect("project", &saved(None));
    let root_of = |name: &str| match &by_name(&found, name)
        .proposal
        .as_ref()
        .unwrap_or_else(|| panic!("{name} has no proposal"))
        .server
        .transport
    {
        polaris_mcp::registration::Destination::Stdio { readable_roots, .. } => readable_roots
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("{name} has no readable root")),
        other => panic!("expected stdio, got {other:?}"),
    };
    // An admissible tree is still offered whole: a program that loads a file
    // beside itself keeps working.
    assert_eq!(root_of("clean"), home.join("tools/clean"));
    assert_eq!(root_of("shared"), interpreters[1]);
}

/// Without a pin the refusal stands. Nothing here weakens the rule; it only
/// stops applying once the thing being launched is a file polaris holds.
#[test]
fn a_launcher_without_a_pin_is_still_refused() {
    let (_temp, home) = home();
    write(
        &home.join(".codex/config.toml"),
        "[mcp_servers.docs]\ncommand = \"npx\"\nargs = [\"-y\", \"@scope/docs-mcp\"]\n",
    );
    let found = detector(&home).detect("project", &saved(None));
    assert_eq!(
        by_name(&found, "docs").reason,
        Some(Reason::LauncherNotSupported)
    );
}

/// A pin for a different version than the command named is not an answer.
#[test]
fn a_pin_for_another_version_leaves_the_launcher_refused() {
    let (_temp, home) = home();
    write(
        &home.join(".codex/config.toml"),
        "[mcp_servers.docs]\ncommand = \"npx\"\nargs = [\"-y\", \"@scope/docs-mcp@1.0.0\"]\n",
    );
    let server_id = sanitized_id(
        "docs",
        &format!(
            "{}\n{}",
            home.join(".codex/config.toml").display(),
            ["mcp_servers", "docs"].join("\n")
        ),
    );
    let directory = home.join(".polaris/mcp/servers").join(&server_id);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("pinned.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "package": "@scope/docs-mcp",
            "version": "4.5.6",
            "runtime": "/bin/sh",
            "entry": directory.join("server"),
        }))
        .unwrap(),
    )
    .unwrap();
    let found = detector(&home).detect("project", &saved(None));
    assert_eq!(
        by_name(&found, "docs").reason,
        Some(Reason::LauncherNotSupported)
    );
}

/// A person who is told "設定を読み取れません" has nowhere to look. Each way
/// an entry can be unreadable names the field that could not be read.
#[test]
fn an_unreadable_entry_names_the_field_that_could_not_be_read() {
    let cases: [(&str, Reason); 5] = [
        ("[mcp_servers.a]\nargs = [\"x\"]\n", Reason::CommandMissing),
        (
            "[mcp_servers.a]\ncommand = \"/bin/sh\"\nargs = [1]\n",
            Reason::ArgumentNotText,
        ),
        ("[mcp_servers.a]\ntype = \"http\"\n", Reason::CommandMissing),
        (
            "[mcp_servers.a]\nurl = \"not a url\"\n",
            Reason::UrlNotUsable,
        ),
        // The shape that stopped computer-use: a command relative to
        // something this cannot name.
        (
            "[mcp_servers.a]\ncommand = \"./relative/server\"\ncwd = \".\"\n",
            Reason::CommandNotAbsolute,
        ),
    ];
    for (body, expected) in cases {
        let (_temp, home) = home();
        write(&home.join(".codex/config.toml"), body);
        let found = detector(&home).detect("project", &saved(None));
        assert_eq!(by_name(&found, "a").reason, Some(expected), "{body}");
    }
}
