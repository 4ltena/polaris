//! M2b synthetic app servers: research-shaped tool names, results and
//! protocol profiles pass through unchanged. Real apps are a separate gate.
use super::*;
#[cfg(target_os = "macos")]
use crate::content::Part;
use crate::{catalog::Kind, content::Reply};
use std::future::pending;

fn names(connection: &Connection) -> Vec<String> {
    let catalog = connection.catalog(1).unwrap();
    catalog
        .items
        .iter()
        .filter(|item| item.id.kind == Kind::Tool)
        .map(|item| item.name.clone())
        .collect()
}

async fn call(
    connection: &mut Connection,
    name: &str,
    arguments: Value,
) -> Result<Reply, OperationFailure> {
    let catalog = connection.catalog(1).unwrap();
    let id = catalog
        .items
        .iter()
        .find(|item| item.name == name)
        .unwrap()
        .id
        .clone();
    let generation = catalog.generation.clone();
    let operation = connection
        .prepare_operation(1, &generation, &id, arguments)
        .unwrap();
    connection
        .perform_once(operation, pending(), |_| true)
        .await
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn blender_resolve_and_unity_shapes_pass_through_unchanged() {
    for (app, profile) in [
        ("blender", ProtocolProfile::Auto2025Compat),
        ("resolve", ProtocolProfile::Auto2024Compat),
        ("unity", ProtocolProfile::Auto2025Compat),
    ] {
        let fixture = python_stdio("tests/fixtures/app_server.py", &[app], profile);
        let mut connection = fixture.connect();
        connection.initialize(pending(), || true).await.unwrap();
        connection
            .refresh_catalog(1, pending(), || true)
            .await
            .unwrap();
        match app {
            "blender" => {
                assert_eq!(
                    names(&connection),
                    [
                        "execute_blender_code",
                        "get_objects_summary",
                        "get_screenshot_of_window_as_image"
                    ]
                );
                let reply = call(
                    &mut connection,
                    "execute_blender_code",
                    json!({"code": "print(1)"}),
                )
                .await
                .unwrap();
                assert!(
                    matches!(&reply.parts[..], [Part::Text { text, .. }] if text.contains("\"stdout\""))
                );
                let shot = call(
                    &mut connection,
                    "get_screenshot_of_window_as_image",
                    json!({}),
                )
                .await
                .unwrap();
                assert!(
                    matches!(&shot.parts[..], [Part::Binary { mime, .. }] if mime == "image/png")
                );
            }
            "resolve" => {
                assert_eq!(
                    names(&connection),
                    ["get_resolve_status", "launch_resolve", "run_script"]
                );
                let reply = call(&mut connection, "run_script", json!({"script": ""}))
                    .await
                    .unwrap();
                assert!(
                    matches!(&reply.parts[..], [Part::Text { text, .. }] if text.contains("no output"))
                );
            }
            _ => {
                assert_eq!(
                    names(&connection),
                    ["Unity.ManageGameObject", "Unity.ManageScene"]
                );
                call(&mut connection, "Unity.ManageScene", json!({}))
                    .await
                    .unwrap();
                // A changed command list invalidates the catalog until it is fetched again.
                assert!(connection.catalog(1).is_err());
                connection
                    .refresh_catalog(1, pending(), || true)
                    .await
                    .unwrap();
                assert!(names(&connection).contains(&"Unity.CustomCommand".to_owned()));
            }
        }
        connection.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn unreal_meta_tools_dispatch_through_call_tool_and_are_never_resent() {
    for disconnect in [false, true] {
        let (url, methods, task) = operation_fixture(OperationFixture {
            reply: json!({"content":[{"type":"text","text":"{\"success\":true}"}]}),
            delay: false,
            disconnect,
            tools: Some(json!({"tools":[
                {"name":"list_toolsets","inputSchema":{"type":"object"}},
                {"name":"describe_toolset","inputSchema":{"type":"object","properties":{"toolset_name":{"type":"string"}}}},
                {"name":"call_tool","inputSchema":{"type":"object","properties":{
                    "toolset_name":{"type":"string"},"tool_name":{"type":"string"},"arguments":{"type":"object"}},
                    "required":["toolset_name","tool_name"]}}
            ]})),
        })
        .await;
        let mut server = server(
            Destination::StreamableHttp {
                url,
                secret_headers: BTreeMap::new(),
            },
            ProtocolProfile::Auto2025Compat,
        );
        server.capabilities.tool_call = true;
        server.timeouts.operation_seconds = 1;
        let mut connection =
            Connection::prepare(&server, "project", &secrets(), None, || true).unwrap();
        connection.initialize(pending(), || true).await.unwrap();
        connection
            .refresh_catalog(1, pending(), || true)
            .await
            .unwrap();
        assert_eq!(
            names(&connection),
            ["call_tool", "describe_toolset", "list_toolsets"]
        );
        let arguments = json!({"toolset_name":"ActorTools","tool_name":"spawn","arguments":{}});
        let result = call(&mut connection, "call_tool", arguments).await;
        assert_eq!(result.is_err(), disconnect);
        assert_eq!(
            methods
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "tools/call")
                .count(),
            1
        );
        connection.shutdown().await.unwrap();
        task.abort();
        let _ = task.await;
    }
}
