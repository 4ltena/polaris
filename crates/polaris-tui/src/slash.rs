//! Slash commands: local, client-side commands recognized when the input
//! buffer starts with `/`. Almost all are handled entirely inside
//! `polaris-tui` — never sent to the model, never added to
//! `session.messages`, never persisted to the session file. `Review` is
//! the one exception: it's a prompt expansion, not a local command — the
//! caller in `lib.rs` recognizes it before dispatch and lets it fall
//! through to a normal model turn instead.
//!
//! Mirrors the *shape* of `codex`'s own in-session slash commands (an
//! autocomplete popup while typing, local execution on submit). codex's
//! real command set, read from its `slash_command.rs`, is ~65 entries —
//! most depend on a subsystem polaris doesn't have (MCP client, IDE
//! integration, multi-agent orchestration, background terminals,
//! terminal pets, ...) and stay out for that reason: `/apps`,
//! `/plugins`, `/vim`, `/keymap`, `/hooks`, `/import`,
//! `/memories`, `/theme`, `/pets`, `/ide`, `/plan`, `/goal`, `/agents`,
//! `/subagents`, `/side`, `/btw`, `/ps`, `/stop`, `/title`,
//! `/statusline`, `/feedback`, `/personality`, `/experimental`,
//! `/approve`, `/mention`, `/usage` (duplicates `/status`'s
//! token display), `/rename` and `/archive`/`/delete` (need a title/
//! lifecycle concept `/resume`'s picker doesn't have yet). `/model` now
//! does switch the model mid-session — `polaris_provider::Provider`
//! grew a `set_model(&self, ...)` method using interior mutability, so
//! it can be called through the same shared provider reference. The
//! interactive picker deliberately leaves reasoning effort to automatic
//! routing; fixed noninteractive measurements can still use `--effort`.

pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "setting",
        description: "inspect Next.js plan or confirm Git ignore recipe",
    },
    SlashCommand {
        name: "handoff",
        description: "render a bounded handoff from actual project evidence",
    },
    SlashCommand {
        name: "session",
        description: "inspect or configure the saved conversation",
    },
    SlashCommand {
        name: "goal",
        description: "inspect or update the saved goal",
    },
    SlashCommand {
        name: "history",
        description: "inspect a bounded original transcript page",
    },
    SlashCommand {
        name: "input",
        description: "inspect an exact admitted input",
    },
    SlashCommand {
        name: "instruction",
        description: "submit through input admission",
    },
    SlashCommand {
        name: "tool-memory",
        description: "configure tool-result retention and loopback embedding",
    },
    SlashCommand {
        name: "remember",
        description: "configure saved conversation memory",
    },
    SlashCommand {
        name: "browse",
        description: "inspect project files locally",
    },
    SlashCommand {
        name: "attachment",
        description: "preview bounded text without sending it",
    },
    SlashCommand {
        name: "recipe",
        description: "inspect built-in recipe definitions",
    },
    SlashCommand {
        name: "models",
        description: "inspect available models",
    },
    SlashCommand {
        name: "subagents",
        description: "inspect or wait for an exact child attempt",
    },
    SlashCommand {
        name: "roles",
        description: "inspect role and tier bindings",
    },
    SlashCommand {
        name: "run",
        description: "inspect a run or change its next turn limit",
    },
    SlashCommand {
        name: "sandbox",
        description: "inspect the fixed startup authority",
    },
    SlashCommand {
        name: "help",
        description: "list available commands",
    },
    SlashCommand {
        name: "status",
        description: "show provider, model, and token usage",
    },
    SlashCommand {
        name: "web-search",
        description: "inspect hosted search availability",
    },
    SlashCommand {
        name: "doctor",
        description: "inspect local configuration and runtime availability",
    },
    SlashCommand {
        name: "account",
        description: "list, register, reauthenticate, unlink, label, or bind an account",
    },
    SlashCommand {
        name: "login",
        description: "register an account without changing the current binding",
    },
    SlashCommand {
        name: "memory",
        description: "search or inspect saved project memory",
    },
    SlashCommand {
        name: "decided",
        description: "inspect recorded project decisions",
    },
    SlashCommand {
        name: "compare",
        description: "compare folder names and sizes without opening file contents",
    },
    #[cfg(target_os = "linux")]
    SlashCommand {
        name: "apply",
        description: "run in an isolated copy, inspect changes, and confirm source apply",
    },
    SlashCommand {
        name: "sync",
        description: "join, receive, leave, inspect, apply, push, or reconcile shared changes",
    },
    SlashCommand {
        name: "phase",
        description: "show or set the explicit workflow phase",
    },
    SlashCommand {
        name: "workflow",
        description: "show workflow mode or set it to manual, adaptive, or off",
    },
    SlashCommand {
        name: "skills",
        description: "browse the skills polaris discovered in this project",
    },
    SlashCommand {
        name: "new",
        description: "start a new chat, keeping this one saved for /resume",
    },
    SlashCommand {
        name: "resume",
        description: "pick a saved conversation to resume",
    },
    SlashCommand {
        name: "clear",
        description: "erase this conversation's saved history",
    },
    SlashCommand {
        name: "compact",
        description: "summarize older history now to free up context",
    },
    SlashCommand {
        name: "init",
        description: "create an AGENTS.md file with instructions for polaris",
    },
    SlashCommand {
        name: "mcp",
        description: "show registered MCP servers and why any are unavailable",
    },
    SlashCommand {
        name: "model",
        description: "choose what model to use",
    },
    SlashCommand {
        name: "diff",
        description: "show git diff (including untracked files)",
    },
    SlashCommand {
        name: "review",
        description: "ask polaris to review the current changes and find issues",
    },
    SlashCommand {
        name: "permissions",
        description: "choose what polaris is allowed to do for the rest of this session",
    },
    SlashCommand {
        name: "fork",
        description: "copy this conversation into a new one you can branch from",
    },
    SlashCommand {
        name: "export",
        description: "export this conversation as markdown",
    },
    SlashCommand {
        name: "pwd",
        description: "show the project root",
    },
    SlashCommand {
        name: "copy",
        description: "copy the last reply to the clipboard",
    },
    SlashCommand {
        name: "logout",
        description: "log out and remove stored ChatGPT credentials",
    },
    SlashCommand {
        name: "quit",
        description: "exit polaris",
    },
];

/// Commands whose name starts with `prefix` (case-insensitive, without the
/// leading `/`). An empty prefix — a bare `/` just typed — matches every
/// command, which is exactly what should show up as the initial menu.
pub fn matching(prefix: &str) -> Vec<&'static SlashCommand> {
    let prefix = prefix.to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(prefix.as_str()))
        .collect()
}

pub enum Action {
    Help,
    Status,
    WebSearch(String),
    Doctor,
    Account(String),
    Memory(String),
    SessionControl(String),
    WorkspaceControl(String),
    RuntimeControl(String),
    Decided(String),
    Compare(String),
    /// Apply is intercepted by the terminal handler; recovery never resends
    /// the source effect and requires the exact displayed operation key.
    Sync(String),
    #[cfg(target_os = "linux")]
    Apply(String),
    /// Empty means display the current phase. A nonempty value is checked by
    /// the session so unknown names are rejected rather than coerced.
    Phase(String),
    /// Empty displays the durable mode. `manual`, `adaptive`, and `off` are
    /// checked by the session before a new configuration is published.
    Workflow(String),
    /// Same interception story as `New`/`Resume` below — the caller
    /// shows a full-screen, browse-only picker over the discovered
    /// skills (needs the terminal, which `apply_slash_action` doesn't
    /// have).
    Skills,
    /// Not a local command in the usual sense — the caller intercepts
    /// this before dispatch and rotates to a brand-new session (new id,
    /// new file) instead of running it through `apply_slash_action`,
    /// since that needs to mutate state (`session_path`, `meta_path`,
    /// ...) `apply_slash_action` doesn't own. Listed here anyway so it
    /// still shows up in the popup and `/help`.
    New,
    /// Same interception story as `New` — the caller shows a full-screen
    /// picker (needs the terminal, which `apply_slash_action` doesn't
    /// have) and swaps the active session in place on selection.
    Resume,
    Clear,
    /// Not handled by `apply_slash_action` — needs a real `&dyn Provider`
    /// reference and an async LLM call, which `apply_slash_action` (sync,
    /// display-strings-only) doesn't have. The caller in `lib.rs`
    /// intercepts this the same way it does `Model`.
    Compact,
    Init,
    /// Same interception story as `Permissions` — the caller shows a
    /// picker over the fixed model catalog and, on selection, both calls
    /// `provider.set_model(...)` and swaps the display copy
    /// `apply_slash_action` doesn't own.
    Model,
    /// Local and display-only: registration and admission state, read
    /// without connecting to any server and without asking the model.
    Mcp(String),
    Diff,
    /// Not a local command — the caller expands this (plus the carried
    /// extra instructions, if any were typed after the command name) into
    /// a real user message and sends it through the normal model turn
    /// instead of dispatching it via `apply_slash_action`. Listed here
    /// anyway so it still shows up in the popup and `/help`.
    Review(String),
    /// Same interception story as `New`/`Resume` — the caller shows a
    /// picker over `ApprovalPolicy`'s three variants and, on selection,
    /// swaps the `approval_policy` local `apply_slash_action` doesn't own.
    Permissions,
    /// Same interception story as `New` — the caller duplicates the
    /// current session into a new file/id, exactly like `New` except
    /// seeded with the conversation so far instead of starting empty.
    Fork,
    /// The carried text, if any, is the destination path; empty means
    /// "pick a default path". Local — writes a file, never touches the
    /// model.
    Export(String),
    Pwd,
    /// Copies the last reply's text to the system clipboard via an OSC 52
    /// escape sequence — no native clipboard crate, works over SSH/tmux the
    /// same way it does locally. Mirrors codex's own `/copy` (and its
    /// `Ctrl+O` hotkey, wired up alongside this in `run()`).
    Copy,
    Logout,
    Quit,
    Unknown(String),
}

impl Action {
    /// Display-only forms that do not require a native mutation owner.
    /// New actions and additional arguments stay closed until classified.
    pub(super) fn allowed_without_mutation_owner(&self) -> bool {
        match self {
            Self::Help
            | Self::Status
            | Self::WebSearch(_)
            | Self::Doctor
            | Self::Skills
            | Self::Pwd
            | Self::Diff
            | Self::Compare(_)
            | Self::Quit
            | Self::Unknown(_) => true,
            Self::Account(value) | Self::Decided(value) | Self::Mcp(value) => {
                matches!(value.as_str(), "" | "list")
            }
            Self::Phase(value) | Self::Workflow(value) => value.is_empty(),
            Self::Memory(value) => matches!(
                value.split_whitespace().next().unwrap_or(""),
                "" | "search" | "get" | "source"
            ),
            Self::SessionControl(value) => matches!(
                value.as_str(),
                "session"
                    | "session current"
                    | "session history"
                    | "goal"
                    | "goal current"
                    | "history"
            ),
            Self::WorkspaceControl(value) => matches!(
                value.as_str(),
                "browse" | "browse list" | "recipe" | "recipe list" | "setting nextjs"
            ),
            Self::RuntimeControl(value) => matches!(
                value.as_str(),
                "subagents"
                    | "subagents list"
                    | "roles"
                    | "roles current"
                    | "roles list"
                    | "run"
                    | "run settings"
                    | "run details"
                    | "sandbox"
                    | "sandbox status"
            ),
            Self::Sync(value) => matches!(value.as_str(), "" | "status"),
            _ => false,
        }
    }

    /// A pending durable management effect must be reconciled before these
    /// commands can change session, project, clipboard, or remote state.
    pub fn needs_management_clearance(&self) -> bool {
        match self {
            Self::Phase(value) | Self::Workflow(value) => !value.is_empty(),
            Self::SessionControl(value) => {
                !matches!(
                    value.as_str(),
                    "session" | "session current" | "session history" | "goal" | "goal current"
                ) && !value.starts_with("history")
            }
            Self::WorkspaceControl(_) | Self::RuntimeControl(_) => false,
            Self::Sync(value) => {
                // C10 owns composite admission and explicit recovery exceptions.
                if value.split_whitespace().next() == Some("push") {
                    return false;
                }
                value == "apply"
                    || value.starts_with("join ")
                    || matches!(value.as_str(), "pull" | "leave")
            }
            #[cfg(target_os = "linux")]
            Self::Apply(value) => {
                value.starts_with("run ")
                    || value.starts_with("resolve ") && value.ends_with(" allow")
            }
            Self::Account(value) => !value.is_empty() && value != "list",
            Self::Memory(value) => {
                let command = value.split_whitespace().next().unwrap_or("");
                matches!(command, "import" | "index" | "embed" | "forget")
            }
            Self::Decided(value) => !value.is_empty() && value != "list",
            Self::Mcp(value) => {
                let command = value.split_whitespace().next().unwrap_or("");
                matches!(
                    command,
                    "enable"
                        | "disable"
                        | "secret"
                        | "check"
                        | "setup"
                        | "materialize"
                        | "register"
                        | "configure"
                )
            }
            Self::New
            | Self::Resume
            | Self::Clear
            | Self::Compact
            | Self::Init
            | Self::Fork
            | Self::Export(_)
            | Self::Copy
            | Self::Logout
            | Self::Quit => true,
            _ => false,
        }
    }
}

/// The action for a name taken directly from `COMMANDS` (e.g. the
/// currently-highlighted row in the suggestion popup) — every name in
/// `COMMANDS` is by construction one `parse` already recognizes, so this
/// never produces `Unknown`.
pub fn action_for(name: &str) -> Action {
    parse(&format!("/{name}")).expect("COMMANDS names are always valid commands")
}

/// Parses a submitted line as a slash command. Returns `None` when the
/// line doesn't start with `/` at all — the caller then treats it as a
/// normal instruction to send to the model, unchanged from today.
///
/// Only the first whitespace-separated word is matched against a command
/// name; everything after it is trailing text. Every current command
/// ignores that trailing text except `/review`, which carries it as
/// custom instructions — but accepting (and ignoring) it uniformly means
/// `/status now`, say, still resolves to `Status` instead of `Unknown`.
pub fn parse(line: &str) -> Option<Action> {
    let rest = line.strip_prefix('/')?;
    let trimmed = rest.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").to_ascii_lowercase();
    let extra = parts.next().unwrap_or("").trim().to_string();
    Some(match name.as_str() {
        "" | "help" => Action::Help,
        "status" => Action::Status,
        "web-search" => Action::WebSearch(extra),
        "doctor" => Action::Doctor,
        "account" => Action::Account(extra),
        "login" => Action::Account(format!("login {extra}").trim_end().to_owned()),
        "memory" if extra.starts_with("configure ") => {
            Action::SessionControl(format!("memory {extra}"))
        }
        "memory" => Action::Memory(extra),
        "session" | "goal" | "history" | "tool-memory" | "remember" => {
            Action::SessionControl(format!("{name} {extra}").trim_end().to_owned())
        }
        "input" | "instruction" | "subagents" | "roles" | "run" | "sandbox" => {
            Action::RuntimeControl(format!("{name} {extra}").trim_end().to_owned())
        }
        #[cfg(not(target_os = "linux"))]
        "models" if extra.is_empty() || extra == "list" => Action::Model,
        "browse" | "attachment" | "recipe" | "models" | "setting" | "handoff" => {
            Action::WorkspaceControl(format!("{name} {extra}").trim_end().to_owned())
        }
        "decided" => Action::Decided(extra),
        "compare" => Action::Compare(extra),
        "sync" => Action::Sync(extra),
        #[cfg(target_os = "linux")]
        "apply" => Action::Apply(extra),
        "phase" => Action::Phase(extra),
        "workflow" => Action::Workflow(extra),
        "skills" => Action::Skills,
        "new" => Action::New,
        "resume" => Action::Resume,
        "clear" => Action::Clear,
        "compact" if extra.starts_with("threshold ") => {
            Action::SessionControl(format!("compact {extra}"))
        }
        "compact" => Action::Compact,
        "init" => Action::Init,
        "mcp" => Action::Mcp(extra),
        "model" => Action::Model,
        "diff" => Action::Diff,
        "review" => Action::Review(extra),
        "permissions" => Action::Permissions,
        "fork" => Action::Fork,
        "export" => Action::Export(extra),
        "pwd" => Action::Pwd,
        "copy" => Action::Copy,
        "logout" => Action::Logout,
        "quit" | "exit" | "q" => Action::Quit,
        _ => Action::Unknown(trimmed.to_ascii_lowercase()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3c_saved_and_read_routes_keep_arguments_and_mutation_clearance() {
        for command in [
            "/session configure codex gpt-5.6-sol",
            "/goal set finish",
            "/memory configure on",
            "/tool-memory endpoint http://127.0.0.1:8080/v1 embed",
            "/compact threshold 1000",
        ] {
            let action = parse(command).unwrap();
            assert!(
                matches!(&action, Action::SessionControl(value) if format!("/{value}") == command)
            );
            assert!(action.needs_management_clearance());
        }
        for command in [
            "/session current",
            "/goal",
            "/history 20",
            "/browse read src/main.rs",
            "/attachment notes.txt",
            "/input status input-id",
            "/subagents wait child attempt",
            "/run details",
        ] {
            assert!(!parse(command).unwrap().needs_management_clearance());
        }
        #[cfg(not(target_os = "linux"))]
        assert!(matches!(parse("/models"), Some(Action::Model)));
        #[cfg(target_os = "linux")]
        assert!(
            matches!(parse("/models"), Some(Action::WorkspaceControl(value)) if value == "models")
        );
        assert!(matches!(
            parse("/models install x"),
            Some(Action::WorkspaceControl(_))
        ));
    }

    #[test]
    fn m2d_sync_routes_read_apply_and_exact_recovery_arguments() {
        for line in ["/sync", "/sync status"] {
            let action = parse(line).unwrap();
            assert!(matches!(&action, Action::Sync(_)));
            assert!(!action.needs_management_clearance());
        }
        assert!(parse("/sync apply").unwrap().needs_management_clearance());
        let action = parse("/sync recover 0123").unwrap();
        assert!(!action.needs_management_clearance());
        assert!(matches!(action, Action::Sync(value) if value == "recover 0123"));
        assert_eq!(matching("syn")[0].name, "sync");
    }

    #[test]
    fn a_bare_slash_matches_every_command() {
        assert_eq!(matching("").len(), COMMANDS.len());
    }

    #[test]
    fn a_prefix_narrows_to_matching_commands_only() {
        let m = matching("cl");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "clear");
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(matching("CL").len(), 1);
    }

    #[test]
    fn a_prefix_matching_nothing_returns_an_empty_list() {
        assert!(matching("zz").is_empty());
    }

    #[test]
    fn text_without_a_leading_slash_is_not_a_command() {
        assert!(parse("clear the table please").is_none());
    }

    #[test]
    fn known_commands_parse_to_their_action() {
        assert!(matches!(parse("/help"), Some(Action::Help)));
        assert!(matches!(parse("/status"), Some(Action::Status)));
        assert!(matches!(parse("/doctor"), Some(Action::Doctor)));
        assert!(matches!(parse("/account list"), Some(Action::Account(value)) if value == "list"));
        assert!(
            matches!(parse("/memory search a"), Some(Action::Memory(value)) if value == "search a")
        );
        assert!(matches!(parse("/decided list"), Some(Action::Decided(value)) if value == "list"));
        assert!(
            matches!(parse("/compare a | b"), Some(Action::Compare(value)) if value == "a | b")
        );
        assert!(matches!(parse("/phase"), Some(Action::Phase(value)) if value.is_empty()));
        assert!(
            matches!(parse("/workflow mode adaptive"), Some(Action::Workflow(value)) if value == "mode adaptive")
        );
        assert!(matches!(parse("/skills"), Some(Action::Skills)));
        assert!(matches!(parse("/new"), Some(Action::New)));
        assert!(matches!(parse("/resume"), Some(Action::Resume)));
        assert!(matches!(parse("/clear"), Some(Action::Clear)));
        assert!(matches!(parse("/init"), Some(Action::Init)));
        assert!(matches!(parse("/model"), Some(Action::Model)));
        assert!(matches!(parse("/diff"), Some(Action::Diff)));
        match parse("/review") {
            Some(Action::Review(extra)) => assert!(extra.is_empty()),
            _ => panic!("expected Review"),
        }
        assert!(matches!(parse("/permissions"), Some(Action::Permissions)));
        assert!(matches!(parse("/fork"), Some(Action::Fork)));
        match parse("/export") {
            Some(Action::Export(extra)) => assert!(extra.is_empty()),
            _ => panic!("expected Export"),
        }
        assert!(matches!(parse("/pwd"), Some(Action::Pwd)));
        assert!(matches!(parse("/copy"), Some(Action::Copy)));
        assert!(matches!(parse("/logout"), Some(Action::Logout)));
        assert!(matches!(parse("/quit"), Some(Action::Quit)));
        assert!(matches!(parse("/exit"), Some(Action::Quit)));
        assert!(matches!(parse("/q"), Some(Action::Quit)));
    }

    #[test]
    fn export_carries_text_typed_after_the_command_name_as_a_destination_path() {
        match parse("/export notes.md") {
            Some(Action::Export(extra)) => assert_eq!(extra, "notes.md"),
            _ => panic!("expected Export"),
        }
    }

    #[test]
    fn every_command_in_the_list_actually_parses_to_something_other_than_unknown() {
        // Catches the class of bug where a name is added to `COMMANDS` (so
        // it shows up in the popup) but its arm is missing from `parse`'s
        // match — the popup would offer it, but selecting it would fall
        // through to "unknown command".
        for c in COMMANDS {
            let action = parse(&format!("/{}", c.name)).expect("starts with /");
            assert!(
                !matches!(action, Action::Unknown(_)),
                "{} parses as Unknown",
                c.name
            );
        }
    }

    #[test]
    fn a_bare_slash_is_help() {
        assert!(matches!(parse("/"), Some(Action::Help)));
    }

    #[test]
    fn action_for_resolves_every_listed_command_without_panicking() {
        for c in COMMANDS {
            let _ = action_for(c.name);
        }
    }

    #[test]
    fn action_for_matches_what_parse_would_give_the_same_name() {
        assert!(matches!(action_for("clear"), Action::Clear));
        assert!(matches!(action_for("quit"), Action::Quit));
    }

    #[test]
    fn parsing_is_case_insensitive() {
        assert!(matches!(parse("/CLEAR"), Some(Action::Clear)));
    }

    #[test]
    fn an_unrecognized_command_reports_its_own_name() {
        match parse("/frobnicate") {
            Some(Action::Unknown(name)) => assert_eq!(name, "frobnicate"),
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn surrounding_whitespace_after_the_slash_is_trimmed() {
        assert!(matches!(parse("/ clear "), Some(Action::Clear)));
    }

    #[test]
    fn compact_is_a_known_command() {
        assert!(matches!(parse("/compact"), Some(Action::Compact)));
        assert!(matches!(action_for("compact"), Action::Compact));
        assert!(COMMANDS.iter().any(|c| c.name == "compact"));
    }

    #[test]
    fn effort_is_not_an_interactive_command() {
        assert!(!COMMANDS.iter().any(|command| command.name == "effort"));
        assert!(matches!(parse("/effort"), Some(Action::Unknown(name)) if name == "effort"));
    }

    #[test]
    fn phase_carries_the_requested_name_for_session_validation() {
        assert!(matches!(
            parse("/phase implement"),
            Some(Action::Phase(value)) if value == "implement"
        ));
        assert!(matches!(
            parse("/phase impossible"),
            Some(Action::Phase(value)) if value == "impossible"
        ));
    }

    #[test]
    fn review_carries_text_typed_after_the_command_name() {
        match parse("/review focus on the auth changes") {
            Some(Action::Review(extra)) => assert_eq!(extra, "focus on the auth changes"),
            _ => panic!("expected Review"),
        }
    }

    #[test]
    fn trailing_text_after_a_command_name_is_ignored_for_non_review_commands() {
        // A command shouldn't turn into Unknown just because the user
        // typed something after it — only `/review` reads what follows.
        assert!(matches!(parse("/status now"), Some(Action::Status)));
    }
}
