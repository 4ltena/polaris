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
//! terminal pets, ...) and stay out for that reason: `/mcp`, `/apps`,
//! `/plugins`, `/compact`, `/vim`, `/keymap`, `/hooks`, `/import`,
//! `/memories`, `/theme`, `/pets`, `/ide`, `/plan`, `/goal`, `/agents`,
//! `/subagents`, `/side`, `/btw`, `/ps`, `/stop`, `/title`,
//! `/statusline`, `/feedback`, `/personality`, `/experimental`,
//! `/approve`, `/mention`, `/copy`, `/usage` (duplicates `/status`'s
//! token display), `/rename` and `/archive`/`/delete` (need a title/
//! lifecycle concept `/resume`'s picker doesn't have yet). `/model` now
//! does switch the model mid-session — `polaris_provider::Provider`
//! grew a `set_model(&self, ...)` method using interior mutability, so
//! it can be called through the same shared `&dyn Provider` reference
//! `RunArgs` already holds without `RunArgs` needing to own (and
//! rebuild) its provider. It still has no reasoning-effort dimension,
//! since `CompletionRequest` doesn't carry one at all.

pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        description: "list available commands",
    },
    SlashCommand {
        name: "status",
        description: "show provider, model, and token usage",
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
        name: "init",
        description: "create an AGENTS.md file with instructions for polaris",
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
        description: "show the current working directory",
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
    Init,
    /// Same interception story as `Permissions` — the caller shows a
    /// picker over the fixed model catalog and, on selection, both calls
    /// `provider.set_model(...)` and swaps the display copy
    /// `apply_slash_action` doesn't own.
    Model,
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
    Logout,
    Quit,
    Unknown(String),
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
        "skills" => Action::Skills,
        "new" => Action::New,
        "resume" => Action::Resume,
        "clear" => Action::Clear,
        "init" => Action::Init,
        "model" => Action::Model,
        "diff" => Action::Diff,
        "review" => Action::Review(extra),
        "permissions" => Action::Permissions,
        "fork" => Action::Fork,
        "export" => Action::Export(extra),
        "pwd" => Action::Pwd,
        "logout" => Action::Logout,
        "quit" | "exit" | "q" => Action::Quit,
        _ => Action::Unknown(trimmed.to_ascii_lowercase()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
