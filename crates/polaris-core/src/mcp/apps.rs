//! Built-in creative-app definitions (M2b). Every entry is unverified until a
//! real app is measured, so a match only tightens handling: it never makes a
//! tool read-only, never creates an approval scope, and never skips approval.
use polaris_mcp::registration::{Destination, ProtocolProfile, Server};
use serde_json::Value;
use std::{
    future::Future,
    sync::{Arc, OnceLock},
};

#[derive(Debug)]
pub struct App {
    pub label: &'static str,
    /// stdio command or entry-script file names.
    names: &'static [&'static str],
    /// Leading arguments that must also match, e.g. `unity mcp`.
    args: &'static [&'static str],
    urls: &'static [&'static str],
    pub protocol_profile: ProtocolProfile,
    pub sandbox_profile: Option<&'static str>,
    code_execution: &'static [&'static str],
    /// Unreal's tool search exposes real tools through `call_tool`.
    dispatch: bool,
    /// Calls run one at a time in this process (Unreal's game thread).
    serialized: bool,
}

pub const APPS: &[App] = &[
    App {
        label: "Blender",
        names: &["blender-mcp"],
        args: &[],
        urls: &[],
        protocol_profile: ProtocolProfile::Auto2025Compat,
        sandbox_profile: Some("loopback"),
        code_execution: &["execute_blender_code", "execute_blender_code_for_cli"],
        dispatch: false,
        serialized: false,
    },
    App {
        label: "DaVinci Resolve",
        names: &["ResolveMCP"],
        args: &[],
        urls: &[],
        protocol_profile: ProtocolProfile::Auto2024Compat,
        sandbox_profile: Some("loopback"),
        code_execution: &["run_script", "run_script_unsafe"],
        dispatch: false,
        serialized: false,
    },
    App {
        label: "Unity",
        names: &["unity"],
        args: &["mcp"],
        urls: &[],
        protocol_profile: ProtocolProfile::Auto2025Compat,
        sandbox_profile: Some("loopback"),
        code_execution: &[],
        dispatch: false,
        serialized: false,
    },
    App {
        label: "Unreal Engine",
        names: &["unreal_mcp_proxy"],
        args: &[],
        urls: &["http://127.0.0.1:8000/mcp"],
        protocol_profile: ProtocolProfile::Auto2025Compat,
        sandbox_profile: Some("loopback"),
        code_execution: &[
            "ProgrammaticToolset.execute_tool_script",
            "LiveCodingToolset.CompileLiveCoding",
        ],
        dispatch: true,
        serialized: true,
    },
];

pub fn find(server: &Server) -> Option<&'static App> {
    APPS.iter().find(|app| match &server.transport {
        Destination::StreamableHttp { url, .. } => app.urls.contains(&url.as_str()),
        Destination::Stdio {
            command,
            args,
            script,
            ..
        } => {
            let named = |path: &std::path::Path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| app.names.contains(&name))
            };
            // An env shebang is registered as interpreter + script.
            let rest = if script.as_deref().is_some_and(named) {
                &args[1.min(args.len())..]
            } else if named(command) {
                &args[..]
            } else {
                return false;
            };
            rest.len() >= app.args.len() && rest.iter().zip(app.args).all(|(a, b)| a == b)
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolClass {
    Unknown,
    CodeExecution,
}

/// How one call is shown and classified; host data, never a grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallView {
    pub app: &'static str,
    /// `toolset.tool` inside Unreal's `call_tool`, when readable.
    pub inner: Option<String>,
    pub class: ToolClass,
}

impl App {
    pub fn view(&self, tool: &str, arguments: &Value) -> CallView {
        let inner = (self.dispatch && tool == "call_tool")
            .then(|| {
                let part = |key| {
                    arguments[key].as_str().filter(|v| {
                        !v.is_empty()
                            && v.len() <= 128
                            && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                };
                Some(format!("{}.{}", part("toolset_name")?, part("tool_name")?))
            })
            .flatten();
        let name = inner.as_deref().unwrap_or(tool);
        CallView {
            app: self.label,
            class: if self.code_execution.contains(&name) {
                ToolClass::CodeExecution
            } else {
                ToolClass::Unknown
            },
            inner,
        }
    }

    /// Picked before any arguments exist, so dispatch tools are left out whole.
    pub fn preselectable(&self, tool: &str) -> bool {
        !(self.code_execution.contains(&tool) || self.dispatch && tool == "call_tool")
    }

    /// Waits for this app's turn; returns at once for apps that need none or
    /// when cancelled, leaving the caller's send to observe the cancellation.
    pub(crate) async fn turn(
        app: Option<&'static App>,
        cancelled: impl Future<Output = ()>,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        app.filter(|app| app.serialized)?;
        // ponytail: one process-wide turn shared by serialized apps (only Unreal
        // today); key it per app when a second one appears. CLI and GUI
        // processes are not serialized with each other.
        static TURN: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
        let lock = TURN.get_or_init(Default::default).clone();
        tokio::select! {
            biased;
            _ = cancelled => None,
            guard = lock.lock_owned() => Some(guard),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_mcp::registration::{Capabilities, Timeouts};
    use serde_json::json;
    use std::{collections::BTreeMap, path::PathBuf};

    fn stdio(command: &str, script: Option<&str>, args: &[&str]) -> Server {
        Server {
            server_id: "s".into(),
            enabled_projects: Default::default(),
            transport: Destination::Stdio {
                command: PathBuf::from(command),
                args: args.iter().map(|a| a.to_string()).collect(),
                cwd: PathBuf::from("/tmp/cwd"),
                sandbox_profile: "read-only".into(),
                secret_env: BTreeMap::new(),
                readable_roots: vec![],
                executable_sha256: None,
                script: script.map(PathBuf::from),
                script_sha256: None,
            },
            protocol_profile: ProtocolProfile::Discover2026,
            capabilities: Capabilities::default(),
            timeouts: Timeouts::default(),
        }
    }

    #[test]
    fn apps_match_by_file_name_arguments_or_exact_url() {
        let label = |server: &Server| find(server).map(|app| app.label);
        let script = "/opt/venv/bin/blender-mcp";
        assert_eq!(
            label(&stdio("/opt/venv/bin/python3", Some(script), &[script])),
            Some("Blender")
        );
        assert_eq!(
            label(&stdio("/Applications/R/ResolveMCP", None, &[])),
            Some("DaVinci Resolve")
        );
        assert_eq!(
            label(&stdio("/opt/homebrew/bin/unity", None, &["mcp"])),
            Some("Unity")
        );
        assert_eq!(
            label(&stdio("/opt/homebrew/bin/unity", None, &["build"])),
            None
        );
        assert_eq!(
            label(&stdio("/usr/bin/python3", Some("/x/other"), &["/x/other"])),
            None
        );
        let mut http = stdio("/unused", None, &[]);
        http.transport = Destination::StreamableHttp {
            url: "http://127.0.0.1:8000/mcp".into(),
            secret_headers: BTreeMap::new(),
        };
        assert_eq!(label(&http), Some("Unreal Engine"));
        http.transport = Destination::StreamableHttp {
            url: "http://127.0.0.1:8001/mcp".into(),
            secret_headers: BTreeMap::new(),
        };
        assert_eq!(label(&http), None);
    }

    #[test]
    fn only_listed_names_and_readable_inner_tools_are_code_execution() {
        let [blender, _, _, unreal] = [&APPS[0], &APPS[1], &APPS[2], &APPS[3]];
        assert_eq!(
            blender.view("execute_blender_code", &json!({})).class,
            ToolClass::CodeExecution
        );
        assert_eq!(
            blender
                .view("get_blendfile_summary_datablocks_for_cli", &json!({}))
                .class,
            ToolClass::Unknown
        );
        let call = |toolset: &str, tool: &str| {
            unreal.view(
                "call_tool",
                &json!({"toolset_name": toolset, "tool_name": tool, "arguments": {}}),
            )
        };
        let script = call("ProgrammaticToolset", "execute_tool_script");
        assert_eq!(script.class, ToolClass::CodeExecution);
        assert_eq!(
            script.inner.as_deref(),
            Some("ProgrammaticToolset.execute_tool_script")
        );
        let other = call("ActorTools", "spawn");
        assert_eq!(other.class, ToolClass::Unknown);
        let unreadable = call("Programmatic Toolset", "execute_tool_script");
        assert_eq!(
            (unreadable.inner, unreadable.class),
            (None, ToolClass::Unknown)
        );
        assert!(!unreal.preselectable("call_tool"));
        assert!(!blender.preselectable("execute_blender_code"));
        assert!(blender.preselectable("get_objects_summary"));
    }

    #[tokio::test]
    async fn unreal_calls_take_turns_but_other_apps_and_cancellation_do_not_wait() {
        let unreal = Some(&APPS[3]);
        let first = App::turn(unreal, std::future::pending()).await;
        assert!(first.is_some());
        assert!(App::turn(unreal, std::future::ready(())).await.is_none());
        assert!(
            App::turn(Some(&APPS[0]), std::future::pending())
                .await
                .is_none()
        );
        drop(first);
        assert!(App::turn(unreal, std::future::pending()).await.is_some());
    }
}
