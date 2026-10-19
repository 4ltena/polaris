use super::*;
use crate::mcp::operation::ExternalCeiling;
use std::marker::PhantomData;

fn limits() -> Limits {
    Limits {
        max_entries: 32,
        max_files: 16,
        max_file_bytes: 4096,
        max_total_bytes: 8192,
        max_depth: 4,
    }
}

fn registry() -> Registry {
    Registry {
        schema_version: 1,
        revision: 1,
        servers: vec![Server {
            server_id: "fixture".into(),
            enabled_projects: BTreeSet::from(["project".into()]),
            transport: Destination::StreamableHttp {
                url: "http://127.0.0.1:9/mcp".into(),
                secret_headers: BTreeMap::new(),
            },
            protocol_profile: Default::default(),
            capabilities: polaris_mcp::registration::Capabilities {
                catalog_metadata: true,
                ..Default::default()
            },
            timeouts: Default::default(),
        }],
    }
}

struct Unvisited<'a>(PhantomData<&'a Server>);

impl<'a> IntoIterator for Unvisited<'a> {
    type Item = &'a Server;
    type IntoIter = std::iter::Once<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        panic!("unsupported admission must not visit the iterator")
    }
}

#[test]
fn windows_mcp_resources_unavailable_refuses_before_iteration() {
    let absent = std::env::temp_dir().join("polaris-mcp-unavailable-absent");
    let result = RuntimeConfig::validate_servers(
        Unvisited(PhantomData),
        "project",
        &absent,
        &[],
        &[],
        limits(),
    );
    assert!(matches!(result, Err(polaris_mcp::Error::Protection)));
}

#[test]
fn windows_mcp_resources_unavailable_preserves_registered_paths() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let runtime = root.path().join("runtime");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&runtime).unwrap();
    let config = home.join("registered.json");
    let command = runtime.join("server.exe");
    let encoded = serde_json::to_vec(&registry()).unwrap();
    std::fs::write(&config, &encoded).unwrap();
    std::fs::write(&command, b"retained\0\xffserver").unwrap();
    let names = |path: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before_names = [names(root.path()), names(&home), names(&runtime)];
    let before = [
        std::fs::read(&config).unwrap(),
        std::fs::read(&command).unwrap(),
    ];
    let runtime_roots = [runtime.clone()];
    let excluded_roots = [home.clone()];
    for registry in [None, Some(registry())] {
        let result = RuntimeConfig::prepare_registered(
            Snapshot {
                registry,
                secrets: None,
            },
            "project".into(),
            &home,
            &runtime_roots,
            &excluded_roots,
            limits(),
        );
        assert!(matches!(result, Err(polaris_mcp::Error::Protection)));
    }
    assert!(matches!(
        RuntimeConfig::validate_servers(
            &registry().servers,
            "project",
            &home,
            &runtime_roots,
            &excluded_roots,
            limits(),
        ),
        Err(polaris_mcp::Error::Protection)
    ));
    assert!(matches!(
        admit_read_root(&runtime, &home, &command, &runtime_roots),
        Err(polaris_mcp::Error::Protection)
    ));
    assert_eq!(
        [names(root.path()), names(&home), names(&runtime)],
        before_names
    );
    assert_eq!(
        [
            std::fs::read(&config).unwrap(),
            std::fs::read(&command).unwrap(),
        ],
        before
    );
}

fn runtime() -> RunRuntime {
    let registry = registry();
    let config =
        RuntimeConfig::new(registry.clone(), "project".into(), None, BTreeMap::new()).unwrap();
    assert_eq!(config.registry(), &registry);
    assert_eq!(config.project_id(), "project");
    assert_eq!(config.server_ids(), vec!["fixture"]);
    assert!(config.quarantined().is_empty());
    RunRuntime::new(
        config,
        McpPort::local("session", "run", "attempt", ExternalCeiling::Denied),
    )
    .unwrap()
}

#[tokio::test]
async fn windows_mcp_metadata_unavailable_refuses_before_cancellation_or_delay() {
    let mut runtime = runtime();
    let cancelled = || std::future::poll_fn::<(), _>(|_| panic!("cancellation was polled"));
    assert!(matches!(
        runtime.ensure_catalog("fixture", cancelled()).await,
        Err(Error::Transport(polaris_mcp::Error::Protection))
    ));
    assert!(matches!(
        runtime
            .ensure_catalog_with_delay("unregistered", cancelled(), |_| panic!("retry was delayed"))
            .await,
        Err(Error::Transport(polaris_mcp::Error::Protection))
    ));
    assert!(runtime.connections.is_empty());
    assert_eq!(runtime.request_generation, 0);
    assert_eq!(runtime.server_ids(), vec!["fixture"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn windows_mcp_metadata_unavailable_index_preserves_runtime() {
    let mut runtime = runtime();
    assert!(matches!(
        runtime
            .index_entry(
                "fixture",
                std::future::poll_fn::<(), _>(|_| panic!("index cancellation was polled")),
            )
            .await,
        Err(Error::Transport(polaris_mcp::Error::Protection))
    ));
    assert!(runtime.connections.is_empty());
    assert_eq!(runtime.request_generation, 0);
    assert_eq!(runtime.config().project_id(), "project");
    assert_eq!(runtime.server_ids(), vec!["fixture"]);
    runtime.shutdown().await.unwrap();
}
