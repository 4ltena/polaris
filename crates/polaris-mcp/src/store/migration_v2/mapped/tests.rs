//! Mapped staging views and inventoried payload identity regression tests.
use super::*;
use crate::store::migration_v2::{
    projection::ProjectionTreeV2, records::tests::ObservationFixture,
};
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

struct Fixture {
    source: ObservationFixture,
    record: StagingRecordV2,
    inventory: CompleteTreeProjectionV2,
    stage: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::with_executable(false)
    }
    fn with_executable(executable: bool) -> Self {
        let source = ObservationFixture::new(&"a".repeat(64));
        let mut record = source.stage();
        let (home, _, _, _) = source.binding.coordinates().unwrap();
        let stage = home
            .join(".polaris")
            .join(record.retired.binding.layout.staging.component());
        for path in [
            &stage,
            &stage.join("servers"),
            &stage.join("servers/example"),
        ] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let script = stage.join("servers/example/index.js");
        std::fs::write(&script, b"stage payload").unwrap();
        std::fs::set_permissions(
            &script,
            std::fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
        )
        .unwrap();
        let fd = child(
            &source.binding.parent,
            &record.retired.binding.layout.staging.component(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .unwrap();
        record.staging_anchor = EntryAnchorV2::of(&fd.metadata().unwrap());
        record.canonical_parent_anchor =
            EntryAnchorV2::of(&source.binding.parent.metadata().unwrap());
        let inventory =
            CompleteTreeProjectionV2::inspect(&fd, ProjectionTreeV2::ActivationV2).unwrap();
        Self {
            source,
            record,
            inventory,
            stage,
        }
    }
    fn view(&self) -> MappedStagingViewV2<'_> {
        MappedStagingViewV2::inspect_descriptor(
            self.source.binding.observation().unwrap(),
            &self.record,
            &self.inventory,
        )
        .unwrap()
    }
}

#[test]
fn fixed_mapper_reads_the_inventoried_stage_inode_with_final_root_absent() {
    let fixture = Fixture::new();
    let view = fixture.view();
    assert!(!view.logical_root().exists());
    assert!(view.contains(&view.logical_root().join("servers/example/index.js")));
    assert!(!view.contains(&fixture.stage));
    let mut file = view
        .open_readonly(&view.logical_root().join("servers/example/index.js"))
        .unwrap();
    let expected = std::fs::metadata(fixture.stage.join("servers/example/index.js")).unwrap();
    let actual = file.metadata().unwrap();
    assert_eq!(
        (actual.dev(), actual.ino()),
        (expected.dev(), expected.ino())
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"stage payload");
    assert!(
        view.open_readonly(view.logical_root())
            .unwrap()
            .metadata()
            .unwrap()
            .is_dir()
    );
    assert!(
        view.open_readonly(&view.logical_root().join("servers/example"))
            .unwrap()
            .metadata()
            .unwrap()
            .is_dir()
    );
    view.revalidate().unwrap();
    assert!(!view.logical_root().exists());
}

#[test]
fn mapped_descriptor_observation_closes_parent_replacement_and_record_anchor_drift() {
    let fixture = Fixture::new();
    let (home, _, _, _) = fixture.source.binding.coordinates().unwrap();
    let namespace =
        MigrationParentObservationV2::inspect(home, &fixture.source.binding.parent).unwrap();
    let mut invalid = fixture.record.clone();
    invalid.canonical_parent_anchor.inode ^= 1;
    assert!(
        MappedStagingViewV2::inspect_descriptor(namespace, &invalid, &fixture.inventory).is_err()
    );
    let namespace =
        MigrationParentObservationV2::inspect(home, &fixture.source.binding.parent).unwrap();
    let view =
        MappedStagingViewV2::inspect_descriptor(namespace, &fixture.record, &fixture.inventory)
            .unwrap();
    let parent = home.join(".polaris");
    std::fs::rename(&parent, home.join("retained-parent")).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(parent.join("sentinel"), b"foreign parent").unwrap();
    assert!(view.verify_current().is_err());
    assert!(view.open_readonly(view.logical_root()).is_err());
    assert_eq!(
        std::fs::read(parent.join("sentinel")).unwrap(),
        b"foreign parent"
    );
}

#[test]
fn fixed_mapper_requires_activation_roles_even_for_the_same_stage_inode() {
    let fixture = Fixture::new();
    fixture.view().revalidate().unwrap();
    let fd = child(
        &fixture.source.binding.parent,
        &fixture.record.retired.binding.layout.staging.component(),
        libc::O_RDONLY | libc::O_DIRECTORY,
    )
    .unwrap();
    let source_inventory =
        CompleteTreeProjectionV2::inspect(&fd, ProjectionTreeV2::SourceV1).unwrap();
    assert!(
        MappedStagingViewV2::inspect_descriptor(
            fixture.source.binding.observation().unwrap(),
            &fixture.record,
            &source_inventory,
        )
        .is_err()
    );
    fixture.view().revalidate().unwrap();
}

#[test]
fn fixed_mapper_directory_uses_shared_walker_and_named_fd_binding() {
    let fixture = Fixture::new();
    let view = fixture.view();
    let limits = Limits {
        max_entries: 8,
        max_files: 4,
        max_file_bytes: 1024,
        max_total_bytes: 4096,
        max_depth: 4,
    };
    let path = view.logical_root().join("servers/example");
    let witness = view.registered_directory(&path, limits).unwrap();
    assert_eq!(witness.path(), fixture.stage.join("servers/example"));
    witness.revalidate().unwrap();
    assert!(!view.logical_root().exists());
    assert!(
        view.registered_directory(
            &path,
            Limits {
                max_entries: 0,
                ..limits
            }
        )
        .is_err()
    );
    let cwd = fixture.stage.join("servers/example");
    std::fs::rename(&cwd, fixture.stage.join("servers/retired-example")).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(witness.revalidate().is_err());
    assert!(view.revalidate().is_err());
}

#[test]
fn fixed_mapper_refuses_misses_foreign_paths_aliases_and_final_fallback() {
    let fixture = Fixture::new();
    let view = fixture.view();
    for path in [
        view.logical_root().join("servers/example/missing.js"),
        fixture.stage.join("servers/example/index.js"),
        view.logical_root()
            .join("servers/example/../example/index.js"),
        PathBuf::from(format!(
            "{}//servers/example/index.js",
            view.logical_root().display()
        )),
    ] {
        assert!(view.open_readonly(&path).is_err());
    }
    // A competing final tree must not become a fallback or an ambiguous choice.
    std::fs::create_dir(view.logical_root()).unwrap();
    std::fs::set_permissions(view.logical_root(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        view.open_readonly(&view.logical_root().join("servers/example/index.js"))
            .is_err()
    );
    assert!(
        MappedStagingViewV2::inspect_descriptor(
            fixture.source.binding.observation().unwrap(),
            &fixture.record,
            &fixture.inventory
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(fixture.stage.join("servers/example/index.js")).unwrap(),
        b"stage payload"
    );
}

#[test]
fn fixed_mapper_rejects_named_stage_replacement_and_same_inode_payload_drift() {
    let fixture = Fixture::new();
    let view = fixture.view();
    let script = fixture.stage.join("servers/example/index.js");
    std::fs::write(&script, b"foreign bytes").unwrap();
    assert!(view.revalidate().is_err());
    assert!(
        view.open_readonly(&view.logical_root().join("servers/example/index.js"))
            .is_err()
    );
    let fixture = Fixture::new();
    let view = fixture.view();
    std::fs::rename(&fixture.stage, fixture.stage.with_extension("foreign")).unwrap();
    std::fs::create_dir(&fixture.stage).unwrap();
    std::fs::set_permissions(&fixture.stage, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(view.revalidate().is_err());
    assert!(view.open_readonly(view.logical_root()).is_err());
}

#[test]
fn fixed_mapper_rejects_foreign_namespace_record_and_links_without_repair() {
    let fixture = Fixture::new();
    let other = ObservationFixture::new(&"a".repeat(64));
    assert!(
        MappedStagingViewV2::inspect_descriptor(
            other.binding.observation().unwrap(),
            &fixture.record,
            &fixture.inventory
        )
        .is_err()
    );
    let view = fixture.view();
    let script = fixture.stage.join("servers/example/index.js");
    std::fs::hard_link(&script, fixture.stage.join("servers/example/link.js")).unwrap();
    assert!(
        view.open_readonly(&view.logical_root().join("servers/example/index.js"))
            .is_err()
    );
    assert_eq!(std::fs::metadata(&script).unwrap().nlink(), 2);
    let fixture = Fixture::new();
    let view = fixture.view();
    let script = fixture.stage.join("servers/example/index.js");
    std::fs::remove_file(&script).unwrap();
    std::os::unix::fs::symlink("unreadable-target", &script).unwrap();
    assert!(
        view.open_readonly(&view.logical_root().join("servers/example/index.js"))
            .is_err()
    );
    assert!(script.symlink_metadata().unwrap().file_type().is_symlink());
}

#[test]
fn mapped_pin_uses_shared_digest_and_executable_rules_without_final_lookup() {
    use sha2::{Digest, Sha256};
    let fixture = Fixture::with_executable(true);
    let view = fixture.view();
    let path = view.logical_root().join("servers/example/index.js");
    let digest = format!("{:x}", Sha256::digest(b"stage payload"));
    let pin = view.pinned_file(&path, Some(&digest)).unwrap();
    crate::admission::require_executable(&pin.2).unwrap();
    view.revalidate_pin(&pin).unwrap();
    assert!(!view.logical_root().exists());
    assert!(view.pinned_file(&path, Some(&"0".repeat(64))).is_err());
    assert!(view.pinned_file(view.logical_root(), None).is_err());
    assert!(
        view.pinned_file(&fixture.stage.join("servers/example/index.js"), None)
            .is_err()
    );
    assert!(
        view.pinned_file(&view.logical_root().join("servers/example/missing"), None)
            .is_err()
    );
    view.revalidate_pin(&pin).unwrap();

    let fixture = Fixture::new();
    let view = fixture.view();
    let pin = view
        .pinned_file(&view.logical_root().join("servers/example/index.js"), None)
        .unwrap();
    assert!(crate::admission::require_executable(&pin.2).is_err());
    view.revalidate_pin(&pin).unwrap();
}

#[test]
fn mapped_pin_closes_payload_drift_named_replacement_and_competing_final_root() {
    for mutation in 0..3 {
        let fixture = Fixture::new();
        let view = fixture.view();
        let path = view.logical_root().join("servers/example/index.js");
        let pin = view.pinned_file(&path, None).unwrap();
        view.revalidate_pin(&pin).unwrap();
        let physical = fixture.stage.join("servers/example/index.js");
        match mutation {
            0 => std::fs::write(&physical, b"other payload").unwrap(),
            1 => {
                std::fs::rename(&physical, fixture.stage.join("servers/example/old.js")).unwrap();
                std::fs::write(&physical, b"stage payload").unwrap();
                std::fs::set_permissions(&physical, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            _ => {
                std::fs::create_dir(view.logical_root()).unwrap();
                std::fs::set_permissions(
                    view.logical_root(),
                    std::fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
        }
        assert!(view.revalidate_pin(&pin).is_err());
    }
}

fn mapped_server(view: &MappedStagingViewV2<'_>, id: &str) -> crate::registration::Server {
    use crate::registration::*;
    Server {
        server_id: id.into(),
        enabled_projects: Default::default(),
        transport: Destination::Stdio {
            command: view.logical_root().join("servers/example/index.js"),
            args: vec![],
            cwd: view.logical_root().join("servers/example"),
            sandbox_profile: "read-only".into(),
            secret_env: Default::default(),
            readable_roots: vec![],
            executable_sha256: None,
            script: None,
            script_sha256: None,
        },
        protocol_profile: Default::default(),
        capabilities: Default::default(),
        timeouts: Default::default(),
    }
}
fn admission_limits() -> Limits {
    Limits {
        max_entries: 16,
        max_files: 8,
        max_file_bytes: 4096,
        max_total_bytes: 8192,
        max_depth: 8,
    }
}

#[test]
fn mapped_registry_admits_all_profiles_and_retains_whole_inventory() {
    use crate::{
        admission::validate_mapped_servers_no_exec, registration::Registry,
        store::documents_v2::CredentialDocumentV2,
    };
    let fixture = Fixture::with_executable(true);
    let view = fixture.view();
    let (home, _, _, _) = fixture.source.binding.coordinates().unwrap();
    let credential = CredentialDocumentV2::baseline(&"a".repeat(64), None).unwrap();
    let mut servers = Vec::new();
    for (index, profile) in ["read-only", "loopback", "workspace-write", "full-access"]
        .into_iter()
        .enumerate()
    {
        let mut server = mapped_server(&view, &format!("server{index}"));
        if let crate::registration::Destination::Stdio {
            sandbox_profile, ..
        } = &mut server.transport
        {
            *sandbox_profile = profile.into();
        }
        servers.push(server);
    }
    let registry = Registry {
        schema_version: 1,
        revision: 1,
        servers,
    };
    let witness = validate_mapped_servers_no_exec(
        &view,
        &registry,
        &credential,
        crate::admission::ProjectAdmissionScope::new("project", &[]),
        home,
        &[],
        admission_limits(),
    )
    .unwrap();
    assert_eq!(witness.count(), 4);
    witness.revalidate().unwrap();
    assert!(!view.logical_root().exists());
    std::fs::write(
        fixture.stage.join("servers/example/index.js"),
        b"other payload",
    )
    .unwrap();
    assert!(witness.revalidate().is_err());
}

#[test]
fn mapped_registry_closes_schema_reference_every_server_and_project_exclusions() {
    use crate::{
        admission::validate_mapped_servers_no_exec,
        registration::{Destination, Registry},
        store::documents_v2::CredentialDocumentV2,
    };
    let fixture = Fixture::with_executable(true);
    let view = fixture.view();
    let (home, _, _, _) = fixture.source.binding.coordinates().unwrap();
    let credential = CredentialDocumentV2::baseline(&"a".repeat(64), None).unwrap();
    let mut registry = Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![mapped_server(&view, "example")],
    };
    let validate = |r: &Registry, excluded: &[PathBuf], limits| {
        validate_mapped_servers_no_exec(
            &view,
            r,
            &credential,
            crate::admission::ProjectAdmissionScope::new("selected", excluded),
            home,
            &[],
            limits,
        )
    };
    validate(&registry, &[], admission_limits()).unwrap();
    registry.revision = 0;
    assert!(validate(&registry, &[], admission_limits()).is_err());
    registry.revision = 1;
    if let Destination::Stdio { secret_env, .. } = &mut registry.servers[0].transport {
        secret_env.insert("TOKEN".into(), "missing".into());
    }
    assert!(matches!(
        validate(&registry, &[], admission_limits()),
        Err(crate::Error::Auth)
    ));
    if let Destination::Stdio { secret_env, .. } = &mut registry.servers[0].transport {
        secret_env.clear();
    }
    let cwd = view.logical_root().join("servers/example");
    // Disabled servers are still admitted, but selected-project exclusions only
    // apply when that project is enabled, matching ordinary no-exec admission.
    validate(&registry, &[cwd.clone()], admission_limits()).unwrap();
    registry.servers[0]
        .enabled_projects
        .insert("selected".into());
    assert!(validate(&registry, &[cwd], admission_limits()).is_err());
    registry.servers[0].enabled_projects.clear();
    assert!(
        validate(
            &registry,
            &[],
            Limits {
                max_entries: 0,
                ..admission_limits()
            }
        )
        .is_err()
    );
    let mut bad = mapped_server(&view, "later");
    if let Destination::Stdio { command, .. } = &mut bad.transport {
        *command = view.logical_root().join("servers/example/missing");
    }
    registry.servers.push(bad);
    assert!(validate(&registry, &[], admission_limits()).is_err());
    assert!(!view.logical_root().exists());
}

#[test]
fn mapped_registry_uses_explicit_live_external_resources_and_same_fd_script_hash() {
    use crate::{
        admission::validate_mapped_servers_no_exec,
        registration::{Destination, Registry},
        store::documents_v2::CredentialDocumentV2,
    };
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new();
    let (home, _, _, _) = fixture.source.binding.coordinates().unwrap();
    let install = home.join("external/installer");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::set_permissions(&install, std::fs::Permissions::from_mode(0o700)).unwrap();
    let command = install.join("runtime");
    std::fs::write(&command, b"external executable").unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let view = fixture.view();
    let script = view.logical_root().join("servers/example/index.js");
    let mut server = mapped_server(&view, "example");
    if let Destination::Stdio {
        command: cmd,
        args,
        script: s,
        script_sha256,
        ..
    } = &mut server.transport
    {
        *cmd = command.clone();
        args.push(script.to_str().unwrap().into());
        *s = Some(script);
        *script_sha256 = Some(format!("{:x}", Sha256::digest(b"stage payload")));
    }
    let mut registry = Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![server],
    };
    let credential = CredentialDocumentV2::baseline(&"a".repeat(64), None).unwrap();
    let witness = validate_mapped_servers_no_exec(
        &view,
        &registry,
        &credential,
        crate::admission::ProjectAdmissionScope::new("project", &[]),
        home,
        &[install.clone()],
        admission_limits(),
    )
    .unwrap();
    witness.revalidate().unwrap();
    if let Destination::Stdio { script_sha256, .. } = &mut registry.servers[0].transport {
        *script_sha256 = Some("0".repeat(64));
    }
    assert!(
        validate_mapped_servers_no_exec(
            &view,
            &registry,
            &credential,
            crate::admission::ProjectAdmissionScope::new("project", &[]),
            home,
            &[install],
            admission_limits(),
        )
        .is_err()
    );
    std::fs::write(&command, b"different executable").unwrap();
    assert!(witness.revalidate().is_err());
}

#[test]
fn mapped_registry_admits_inventoried_install_root_with_live_cwd() {
    use crate::{
        admission::validate_mapped_servers_no_exec,
        registration::{Destination, Registry},
        store::documents_v2::CredentialDocumentV2,
    };
    use sha2::{Digest, Sha256};
    let fixture = Fixture::with_executable(true);
    let (home, _, _, _) = fixture.source.binding.coordinates().unwrap();
    let cwd = home.join("private-workspace");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o700)).unwrap();
    let view = fixture.view();
    let root = view.logical_root().join("servers/example");
    let mut server = mapped_server(&view, "example");
    server.enabled_projects.insert("selected".into());
    if let Destination::Stdio {
        cwd: registered_cwd,
        readable_roots,
        executable_sha256,
        ..
    } = &mut server.transport
    {
        *registered_cwd = cwd;
        readable_roots.push(root.clone());
        *executable_sha256 = Some(format!("{:x}", Sha256::digest(b"stage payload")));
    }
    let mut registry = Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![server],
    };
    let credential = CredentialDocumentV2::baseline(&"a".repeat(64), None).unwrap();
    let witness = validate_mapped_servers_no_exec(
        &view,
        &registry,
        &credential,
        crate::admission::ProjectAdmissionScope::new("selected", &[]),
        home,
        &[],
        admission_limits(),
    )
    .unwrap();
    witness.revalidate().unwrap();
    assert!(
        validate_mapped_servers_no_exec(
            &view,
            &registry,
            &credential,
            crate::admission::ProjectAdmissionScope::new("selected", &[root.clone()]),
            home,
            &[],
            admission_limits(),
        )
        .is_err()
    );
    if let Destination::Stdio { cwd, .. } = &mut registry.servers[0].transport {
        *cwd = root;
    }
    assert!(
        validate_mapped_servers_no_exec(
            &view,
            &registry,
            &credential,
            crate::admission::ProjectAdmissionScope::new("selected", &[]),
            home,
            &[],
            admission_limits(),
        )
        .is_err()
    );
    std::fs::write(
        fixture.stage.join("servers/example/index.js"),
        b"other payload",
    )
    .unwrap();
    assert!(witness.revalidate().is_err());
}

#[test]
fn mapped_registry_final_close_rejects_first_pin_changed_after_later_admission() {
    use crate::{
        admission::{before_mapped_registry_close, validate_mapped_servers_no_exec},
        registration::{Destination, Registry},
        store::documents_v2::CredentialDocumentV2,
    };
    for mutate_stage in [false, true] {
        let fixture = Fixture::with_executable(true);
        let (home, _, _, _) = fixture.source.binding.coordinates().unwrap();
        let install = home.join("external-runtime");
        std::fs::create_dir(&install).unwrap();
        std::fs::set_permissions(&install, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = install.join("runtime");
        std::fs::write(&command, b"external executable").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let view = fixture.view();
        let mut first = mapped_server(&view, "first");
        if let Destination::Stdio { command: cmd, .. } = &mut first.transport {
            *cmd = command.clone();
        }
        let registry = Registry {
            schema_version: 1,
            revision: 1,
            servers: vec![first, mapped_server(&view, "later")],
        };
        let credential = CredentialDocumentV2::baseline(&"a".repeat(64), None).unwrap();
        validate_mapped_servers_no_exec(
            &view,
            &registry,
            &credential,
            crate::admission::ProjectAdmissionScope::new("project", &[]),
            home,
            &[install.clone()],
            admission_limits(),
        )
        .unwrap();
        let changed = if mutate_stage {
            fixture.stage.join("servers/example/index.js")
        } else {
            command
        };
        before_mapped_registry_close(move || {
            std::fs::write(changed, b"changed after all admissions").unwrap()
        });
        assert!(
            validate_mapped_servers_no_exec(
                &view,
                &registry,
                &credential,
                crate::admission::ProjectAdmissionScope::new("project", &[]),
                home,
                &[install],
                admission_limits(),
            )
            .is_err()
        );
        assert!(!view.logical_root().exists());
    }
}

#[test]
fn mapped_registry_reference_validation_accepts_present_zeroizing_document() {
    use crate::{
        admission::validate_mapped_servers_no_exec,
        registration::{Destination, Registry},
        store::{Secrets, documents_v2::CredentialDocumentV2},
    };
    let fixture = Fixture::with_executable(true);
    let view = fixture.view();
    let (home, _, _, _) = fixture.source.binding.coordinates().unwrap();
    let mut server = mapped_server(&view, "example");
    if let Destination::Stdio { secret_env, .. } = &mut server.transport {
        secret_env.insert("TOKEN".into(), "owned_ref".into());
    }
    let registry = Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![server],
    };
    let secrets = Secrets::new(
        7,
        std::collections::BTreeMap::from([("owned_ref".into(), "fixture-secret".into())]),
    )
    .unwrap();
    let credential = CredentialDocumentV2::baseline(&"a".repeat(64), Some(secrets)).unwrap();
    let restored = CredentialDocumentV2::decode(&credential.encode().unwrap()).unwrap();
    let witness = validate_mapped_servers_no_exec(
        &view,
        &registry,
        &restored,
        crate::admission::ProjectAdmissionScope::new("project", &[]),
        home,
        &[],
        admission_limits(),
    )
    .unwrap();
    witness.revalidate().unwrap();
    assert_eq!(restored.metadata().unwrap().revision, 7);
}
