//! polaris's built-in tools. The always-on tool set never exceeds 6 tools.

pub mod bash;
pub mod edit;
pub mod isolated_read;
pub mod path_policy;
pub mod predicate;
pub mod read;
mod schema_validate;
pub mod skill;
pub mod write;

pub use schema_validate::{McpSchema, McpSchemaError, validate};

use serde::Serialize;

/// Tool definition passed to the model. `parameters` is a JSON Schema.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("path {0} is not permitted to be read")]
    PathDenied(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0} is not a regular file")]
    NotAFile(String),
    #[error("{path} exceeds the {limit} byte cap (actual: {actual} bytes)")]
    TooLarge {
        path: String,
        limit: u64,
        actual: u64,
    },
    #[error("sandbox: {0}")]
    Sandbox(#[from] polaris_sandbox::SandboxError),
    #[error("write to {path} was denied. policy: {policy}. child output: {detail}")]
    WriteDenied {
        path: String,
        policy: String,
        detail: String,
    },
    /// The helper ran, but could not carry out the request as asked (zero
    /// matches for the replacement, multiple matches, the target file is
    /// missing, etc.). This is not a policy matter, so neither the policy
    /// nor the writable roots appear in the text. Mixing this into
    /// `WriteDenied` would make the model start hunting for a permissions
    /// problem in a situation where all it needs to do is pick a different
    /// marker.
    #[error(
        "the change to {path} could not be carried out. This is not a sandbox denial, but a problem with the request itself. reason: {detail}"
    )]
    MutationFailed { path: String, detail: String },
    #[error("the command failed with exit code {status}. policy: {policy}. output: {detail}")]
    CommandFailed {
        status: i32,
        policy: String,
        detail: String,
    },
}

/// The list of tools provided at all times.
pub fn all_specs() -> Vec<ToolSpec> {
    vec![
        read_spec(),
        write_spec(),
        edit_spec(),
        bash_spec(),
        skill_spec(),
        spawn_spec(),
    ]
}

/// Request-local workflow control; never part of the six-tool baseline.
pub fn state_spec() -> ToolSpec {
    ToolSpec {
        name: "state".into(),
        description: "Read workflow focus, read the state window, or propose a phase for the next request. Get revision first; transition requires expected_revision. window returns what changed since the revision you were last given, or the whole window when that base is unknown. Focus never grants permission or proves completion.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "op": {"type":"string", "enum":["get", "window", "transition"]},
                "phase": {"type":"string", "enum":["general", "brainstorm", "specify", "implement", "review", "verify", "deliver"]},
                "expected_revision": {"type":"integer", "minimum":0},
                "since": {"type":"integer", "minimum":0}
            },
            "required":["op"],
            "additionalProperties":false
        }),
    }
}

/// Root-only MCP entry point, separate from the six-tool baseline. Dispatch
/// validates operation-specific fields; this schema cannot create host grants.
pub fn mcp_catalog_spec() -> ToolSpec {
    ToolSpec {
        name: "mcp_catalog".into(),
        description: "Search or select a registered MCP item, read a resource, or read a local result chunk. No registration, grants, or prompt selection.".into(),
        parameters: serde_json::json!({
            "type":"object",
            "properties":{
                "op":{"type":"string","enum":["search","select","read","chunk"]},
                "q":{"type":"string"},
                "server":{"type":"string"},
                "kind":{"type":"string","enum":["tool","resource","resource_template","prompt"]},
                "key":{"type":"string"},
                "generation":{"type":"string"},
                "arguments":{"type":"object"},
                "handle":{"type":"string"},
                "offset":{"type":"integer","minimum":0},
                "limit":{"type":"integer","minimum":1,"maximum":32768}
            },
            "required":["op"],"additionalProperties":false
        }),
    }
}

/// Root-only host setup. Arguments carry candidate IDs, never paths or values.
pub fn mcp_setup_spec() -> ToolSpec {
    ToolSpec {
        name: "mcp_setup".into(),
        description: "Find MCP servers the user already configured for Codex or Claude on this computer, then register one after the user approves it. Results list candidate IDs, names, transports and reasons, never URLs, commands or credentials. A registration is usable from the next connection.".into(),
        parameters: serde_json::json!({
            "type":"object",
            "properties":{
                "op":{"type":"string","enum":["detect","register"]},
                "candidate":{"type":"string"}
            },
            "required":["op"],"additionalProperties":false
        }),
    }
}

/// Root-only bundled setting commands. Arguments carry an operation name
/// and nothing else: no path, no package, no command line.
pub fn setting_spec() -> ToolSpec {
    ToolSpec {
        name: "setting".into(),
        description: "Read what the bundled Next.js and Git setup would do in this project, or add the missing ignore rules. `plan` only reads. `tidy` appends rules the project is missing to .gitignore, keeping existing lines and their order, and asks before writing. Installing packages is the user's to confirm in their terminal and is never done here.".into(),
        parameters: serde_json::json!({
            "type":"object",
            "properties":{
                "op":{"type":"string","enum":["plan","tidy"]}
            },
            "required":["op"],"additionalProperties":false
        }),
    }
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read a file. Returns it with line numbers. Omit limit unless you truly want less than 2000 lines - a guessed smaller value just forces a second read.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer" },
                "limit": { "type": "integer" }
            },
            "required": ["path"]
        }),
    }
}

fn write_spec() -> ToolSpec {
    ToolSpec {
        name: "write".into(),
        description:
            "Create a new file. Overwrites an existing file. Creates intermediate directories."
                .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The path to write to." },
                "content": { "type": "string", "description": "The file's full contents." }
            },
            "required": ["path", "content"]
        }),
    }
}

fn edit_spec() -> ToolSpec {
    ToolSpec {
        name: "edit".into(),
        description: "Replace part of an existing file. `old` must be a string that is uniquely determined within the file. Fails if there are multiple matches.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The path of the file to edit." },
                "old": { "type": "string", "description": "The string before replacement. Must be unique within the file." },
                "new": { "type": "string", "description": "The string after replacement." }
            },
            "required": ["path", "old", "new"]
        }),
    }
}

fn bash_spec() -> ToolSpec {
    ToolSpec {
        name: "bash".into(),
        description: "Run a shell command. Use this for grep and find too. Writes outside the sandbox are denied.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line passed to /bin/sh -c." }
            },
            "required": ["command"]
        }),
    }
}

fn skill_spec() -> ToolSpec {
    ToolSpec {
        name: "skill".into(),
        description: "Look up a skill. Returns the body on an exact name match, otherwise returns candidate names and descriptions.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "q": {
                    "type": "string",
                    "description": "A skill name (exact match returns the body), or a search term (matched against names and descriptions to produce candidates). An empty string lists everything."
                }
            },
            "required": ["q"]
        }),
    }
}

/// The spec's call example `spawn([{type, task}, ...])` is conceptual
/// notation. OpenAI-compatible function calling requires the top level of
/// the schema to be an object (the same reason all five existing tools are
/// `"type": "object"`), so the array is wrapped under a `"tasks"` key.
fn spawn_spec() -> ToolSpec {
    ToolSpec {
        name: "spawn".into(),
        description: "Run subagents in parallel as one wave. Describe the work; the type is matched for you. Each runs a bounded loop and returns a schema-validated result; its steps never enter your context.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "type": {
                                "type": "string",
                                "description": "Optional. Omit it unless one type is the only one that can do the work — it is matched from the task otherwise."
                            },
                            "task": {
                                "type": "string",
                                "description": "What it should do."
                            },
                            "write_root": {
                                "type": "string",
                                "description": "Read-write types only: the one directory it may write."
                            }
                        },
                        "required": ["task"]
                    }
                }
            },
            "required": ["tasks"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_spec_serializes_with_required_path() {
        let specs = all_specs();
        let read = specs.iter().find(|s| s.name == "read").expect("no read");
        let json = serde_json::to_value(read).expect("can't serialize");
        assert_eq!(json["name"], "read");
        assert_eq!(json["parameters"]["required"][0], "path");
        assert_eq!(json["parameters"]["properties"]["path"]["type"], "string");
    }

    #[test]
    fn read_spec_tells_the_model_the_default_limit_instead_of_leaving_it_to_guess() {
        // A live polaris-vs-codex comparison run showed the model choosing
        // small limits (120-300 lines) with the old wording ("Use offset
        // and limit to specify a range"), which said nothing about what
        // happens when limit is omitted. Large files then needed a second
        // read call for the remainder, and every extra round trip resends
        // the whole growing history under this provider's store: false
        // design (see codex.rs:132), so avoidable extra calls are not free
        // the way they'd be under a stateful API. Naming the actual
        // default here gives the model a reason to omit limit instead of
        // guessing.
        let read = all_specs()
            .into_iter()
            .find(|s| s.name == "read")
            .expect("no read");
        assert!(
            read.description.contains("2000"),
            "description should name the actual default so the model omits limit \
             instead of guessing a small one: {:?}",
            read.description
        );
    }

    #[test]
    fn skill_spec_publishes_the_parameter_name_it_requires() {
        // This pins the shape of the public schema, a counterpart to the
        // read side. All this checks is that the published argument name
        // and its type haven't changed. Whether the declared name and the
        // name dispatch actually reads refer to the same thing can't be
        // confirmed from this crate (the caller lives in a different
        // crate), so on the polaris-core side,
        // `agent::tests::the_skill_tool_reads_the_argument_name_its_schema_declares`
        // pulls the argument name out of the published schema and ties the
        // two together.
        let specs = all_specs();
        let skill = specs.iter().find(|s| s.name == "skill").expect("no skill");
        let json = serde_json::to_value(skill).expect("can't serialize");
        assert_eq!(json["name"], "skill");
        assert_eq!(json["parameters"]["required"][0], "q");
        assert_eq!(json["parameters"]["properties"]["q"]["type"], "string");
        // Per-argument description. The tool-level description alone gives
        // the model no way to learn, from the argument side, the
        // one-argument / two-mode convention where passing a name returns
        // the body and anything else becomes a search. Removing the
        // description makes this fail.
        let param_doc = json["parameters"]["properties"]["q"]["description"]
            .as_str()
            .expect("argument q has no description");
        assert!(
            !param_doc.trim().is_empty(),
            "argument q's description is empty"
        );
    }

    #[test]
    fn spawn_spec_wraps_the_task_array_in_a_top_level_object() {
        // The spec's `spawn([...])` notation cannot be published as-is:
        // function calling requires a top-level object. Pin both that the
        // top level is an object and that the array lives under `tasks`,
        // since `agent::dispatch`'s `"spawn"` arm reads exactly that key.
        let specs = all_specs();
        let spawn = specs.iter().find(|s| s.name == "spawn").expect("no spawn");
        let json = serde_json::to_value(spawn).expect("can't serialize");
        assert_eq!(json["parameters"]["type"], "object");
        assert_eq!(json["parameters"]["required"][0], "tasks");
        assert_eq!(json["parameters"]["properties"]["tasks"]["type"], "array");
        let item_required = &json["parameters"]["properties"]["tasks"]["items"]["required"];
        // Only the work is required. **`type` is deliberately optional**:
        // the orchestrator is the most expensive party in the run, and it
        // holds no catalogue, so demanding a type name buys either a guess
        // or a round trip at its rate. The judge matches one from the task
        // for a rate that does not register. A name is still honoured when
        // one is given and is real.
        assert_eq!(item_required[0], "task");
        assert!(
            item_required[1].is_null(),
            "only the task is required: {item_required}"
        );
        assert!(
            json["parameters"]["properties"]["tasks"]["items"]["properties"]["type"].is_object(),
            "type stays offered even though it is not required"
        );
    }

    #[test]
    fn all_specs_has_unique_names() {
        let specs = all_specs();
        let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "tool names are duplicated");
    }
}
