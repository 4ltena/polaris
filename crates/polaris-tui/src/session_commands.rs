//! Saved session controls use the conversation owner and shared project lease.
//! Displayed preferences do not claim that a provider has been switched.

use crate::{HistoryBackendFactory, account, approver::KeyReader, render};
use polaris_core::{
    conversation_state::{ConversationSnapshot, HistoryMode},
    management_effect::{ManagementEffectStoreV1, ManagementOperationIdV1},
    rewind_workspace::WorkspaceOwner,
    session::Session,
    session_settings::{
        GoalOperationV1, SavedSessionSettingsV1, SavedToolEmbeddingV1, SessionSettingsCommandV1,
        SessionSettingsOwnerV1, ToolMemoryModeV1,
    },
};
use polaris_provider::Provider;
use ratatui::{Terminal, backend::Backend, text::Line};
use std::cell::RefCell;

const HISTORY_PAGE_MESSAGES: usize = 20;
const MAX_VIEW_BYTES: usize = 64 * 1024;

pub(crate) struct CommandResult {
    pub lines: Vec<Line<'static>>,
    pub settings_changed: bool,
}

impl CommandResult {
    fn display(lines: Vec<Line<'static>>) -> Self {
        Self {
            lines,
            settings_changed: false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    session: &mut Session,
    initial: &SavedSessionSettingsV1,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    provider: &dyn Provider,
    history_factory: Option<&HistoryBackendFactory>,
    has_pending: bool,
) -> Result<CommandResult, String> {
    let request = request.trim().trim_start_matches('/');
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("No saved conversation")?
        .clone();
    if std::fs::symlink_metadata(persistence.store.directory().join("raw.jsonl"))
        .map_err(|e| e.to_string())?
        .len()
        > 16 * 1024 * 1024
    {
        return Err("Conversation source text exceeds the 16 MiB read limit".into());
    }
    let snapshot = persistence.snapshot().map_err(|e| e.to_string())?;
    let saved = snapshot.state.session_settings.as_ref().unwrap_or(initial);
    if matches!(request, "session" | "session current") {
        return Ok(CommandResult::display(vec![
            line(format!(
                "Conversation: {} / generation {}",
                snapshot.state.session_id, snapshot.state.generation
            )),
            line(format!(
                "Current provider: {} / model: {}",
                initial.provider,
                provider.model().unwrap_or_else(|| "unverified".into())
            )),
            line(format!(
                "Saved provider: {} / model: {} / effort: {} / revision: {}",
                saved.provider,
                saved.model,
                if saved.effort.is_empty() {
                    "auto"
                } else {
                    &saved.effort
                },
                saved.revision
            )),
            line(format!(
                "History: {} / remember: {} / tool-memory: {} / compact: {}",
                history_name(snapshot.state.history_mode),
                on_off(saved.remember),
                tool_name(saved.tool_memory),
                saved
                    .compact_threshold
                    .map_or_else(|| "off".into(), |n| n.to_string())
            )),
            line(match &saved.tool_memory_embedding {
                Some(pair) => format!("tool-memory endpoint: {} / model: {}", pair.url, pair.model),
                None => "tool-memory endpoint: off".into(),
            }),
        ]));
    }
    if matches!(request, "goal" | "goal current") {
        return Ok(CommandResult::display(goal_lines(&snapshot)));
    }
    if request == "session history" {
        return Ok(CommandResult::display(vec![line(format!(
            "History mode: {}. Change it in a new, empty conversation.",
            history_name(snapshot.state.history_mode)
        ))]));
    }
    if request == "history" || request.starts_with("history ") {
        return history_lines(&snapshot, request.strip_prefix("history").unwrap().trim())
            .map(CommandResult::display);
    }
    if has_pending {
        return Err("Cannot change session settings while input is queued".into());
    }
    session.check_persistence().map_err(|e| e.to_string())?;
    let command = parse(request, &snapshot, saved, initial, provider)?;
    let history_mode = match &command {
        SessionSettingsCommandV1::History { mode } => Some(*mode),
        _ => None,
    };
    let preview = SessionSettingsOwnerV1::preview(&persistence, initial, command)
        .map_err(|e| e.to_string())?;
    let lease = workspace
        .try_mutation()
        .map_err(|_| "Cannot change session settings while an operation is active")?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|_| "An unresolved rewind operation exists")?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let mut staged = session.clone();
    if let Some(mode) = history_mode {
        crate::ensure_history_backend(&mut staged, mode, history_factory)?;
    }
    let owner = SessionSettingsOwnerV1 {
        effects,
        lease: &lease,
    };
    let prepared = owner
        .prepare(
            &persistence,
            ManagementOperationIdV1::random().map_err(|e| e.to_string())?,
            &preview,
        )
        .map_err(|e| e.to_string())?;
    let detail = match preview.inventory() {
        "D20" => {
            "Save the goal at the specified revision. It does not become input content or authority."
        }
        _ => "Save in this conversation; effective from the next send.",
    };
    if !account::confirm(
        terminal,
        reader,
        "Save session settings",
        preview.session_id(),
        &format!("{request}\n{detail}"),
    ) {
        owner.deny(&prepared).map_err(|e| e.to_string())?;
        return Ok(CommandResult::display(vec![line(
            "Session settings change cancelled",
        )]));
    }
    let published = owner
        .execute(&persistence, &prepared)
        .map_err(|e| e.to_string())?;
    if let Some(mode) = history_mode {
        if mode == HistoryMode::Legacy {
            session.strict_history = None;
            session.summary_usage = None;
        } else {
            session.strict_history = staged.strict_history;
            session.summary_usage = staged.summary_usage;
        }
    }
    let mut lines = vec![line(format!(
        "Saved {} (conversation generation {}).",
        preview.inventory(),
        published.state.generation
    ))];
    if preview.inventory() == "D20" {
        lines.extend(goal_lines(&published));
    }
    Ok(CommandResult {
        lines,
        settings_changed: published.state.session_settings != snapshot.state.session_settings,
    })
}

fn parse(
    request: &str,
    snapshot: &ConversationSnapshot,
    saved: &SavedSessionSettingsV1,
    initial: &SavedSessionSettingsV1,
    provider: &dyn Provider,
) -> Result<SessionSettingsCommandV1, String> {
    let words: Vec<_> = request.split_whitespace().collect();
    match words.as_slice() {
        ["session", "configure", requested_provider, model] | ["session", "configure", requested_provider, model, _] => {
            let effort = words.get(4).copied().unwrap_or(&saved.effort);
            if *requested_provider != initial.provider {
                return Err("Changing the provider requires a restart".into());
            }
            let current = provider.model().ok_or("Cannot verify the current provider's model")?;
            if *model != current && !(matches!(initial.provider.as_str(), "codex" | "openai") && render::MODEL_CATALOG.contains(model)) {
                return Err("Specify a model verified for this provider".into());
            }
            if !matches!(effort, "" | "auto") && effort != initial.effort {
                return Err("Cannot switch to a fixed effort during an interactive session".into());
            }
            Ok(SessionSettingsCommandV1::Configure { expected_revision: saved.revision, provider: requested_provider.to_string(), model: model.to_string(), effort: effort.to_string() })
        }
        ["memory", "configure", "on"] => Ok(SessionSettingsCommandV1::Remember { enabled: true }),
        ["memory", "configure", "off"] => Ok(SessionSettingsCommandV1::Remember { enabled: false }),
        ["session", "history", mode] | ["memory", "configure", mode] => Ok(SessionSettingsCommandV1::History { mode: parse_history(mode)? }),
        ["tool-memory", "off"] => Ok(SessionSettingsCommandV1::ToolMemory { mode: ToolMemoryModeV1::Off }),
        ["tool-memory", "history"] => Ok(SessionSettingsCommandV1::ToolMemory { mode: ToolMemoryModeV1::History }),
        ["tool-memory", "retrieval"] => Ok(SessionSettingsCommandV1::ToolMemory { mode: ToolMemoryModeV1::Retrieval }),
        ["tool-memory", "endpoint", "off"] => Ok(SessionSettingsCommandV1::EmbeddingEndpoint { embedding: None }),
        ["tool-memory", "endpoint", url, model] => Ok(SessionSettingsCommandV1::EmbeddingEndpoint { embedding: Some(SavedToolEmbeddingV1::new(url.to_string(), model.to_string()).map_err(|e| e.to_string())?) }),
        ["tool-memory", "model", model] => {
            let pair = saved.tool_memory_embedding.as_ref().ok_or("Configure the pair first with /tool-memory endpoint <URL> <MODEL>")?;
            Ok(SessionSettingsCommandV1::EmbeddingModel { embedding: Some(SavedToolEmbeddingV1::new(pair.url.clone(), model.to_string()).map_err(|e| e.to_string())?) })
        }
        ["compact", "threshold", threshold] => {
            let threshold = if *threshold == "off" { None } else { Some(threshold.parse::<usize>().map_err(|_| "Specify compact threshold as an integer of at least 1000, or off")?) };
            if threshold.is_some_and(|n| n < 1000) { return Err("compact threshold must be at least 1000".into()); }
            Ok(SessionSettingsCommandV1::CompactThreshold { threshold })
        }
        ["goal", "clear"] | ["goal", "stop"] | ["goal", "resume"] => {
            let operation = match words[1] { "clear" => GoalOperationV1::Clear, "stop" => GoalOperationV1::Stop, _ => GoalOperationV1::Resume };
            Ok(SessionSettingsCommandV1::Goal { expected_revision: snapshot.state.goal.as_ref().map_or(0, |goal| goal.revision.get()), operation })
        }
        ["goal", "set", ..] => {
            let text = request.strip_prefix("goal").unwrap().trim_start().strip_prefix("set").unwrap().trim_start();
            Ok(SessionSettingsCommandV1::Goal { expected_revision: snapshot.state.goal.as_ref().map_or(0, |goal| goal.revision.get()), operation: GoalOperationV1::Set { text: text.to_string() } })
        }
        _ => Err("Specify /session configure <provider> <model> [effort], /session history <mode>, /memory configure on|off, /tool-memory, /compact threshold, or /goal".into()),
    }
}

fn parse_history(value: &str) -> Result<HistoryMode, String> {
    match value {
        "legacy" => Ok(HistoryMode::Legacy),
        "strict10" => Ok(HistoryMode::Strict10),
        "structured10" => Ok(HistoryMode::Structured10),
        _ => Err("Specify history mode as legacy|strict10|structured10".into()),
    }
}

fn goal_lines(snapshot: &ConversationSnapshot) -> Vec<Line<'static>> {
    match &snapshot.state.goal {
        Some(goal) => vec![
            line(format!(
                "goal revision {} / enabled {} / stopped {}",
                goal.revision.get(),
                on_off(goal.active),
                on_off(goal.stopped)
            )),
            line(goal.text.clone()),
        ],
        None => vec![line("No saved goal")],
    }
}

fn history_lines(
    snapshot: &ConversationSnapshot,
    offset: &str,
) -> Result<Vec<Line<'static>>, String> {
    let offset = if offset.is_empty() {
        0
    } else {
        offset
            .parse::<usize>()
            .map_err(|_| "Specify /history [message offset]")?
    };
    if offset > snapshot.events.len() {
        return Err("History offset exceeds the saved range".into());
    }
    let mut lines = vec![line(format!(
        "Original history {}..{} / {} messages",
        offset,
        offset
            .saturating_add(HISTORY_PAGE_MESSAGES)
            .min(snapshot.events.len()),
        snapshot.events.len()
    ))];
    let mut remaining = MAX_VIEW_BYTES - lines[0].to_string().len();
    for event in snapshot
        .events
        .iter()
        .skip(offset)
        .take(HISTORY_PAGE_MESSAGES)
    {
        let role = format!("{:?}", event.message.role).to_lowercase();
        let text = render::sanitize(&format!(
            "[{role} / turn {}] {}",
            event.turn_id, event.message.content
        ));
        if text.len() > remaining {
            return Err("History page exceeds 64 KiB. Specify the next message offset".into());
        }
        remaining -= text.len();
        lines.extend(text.lines().map(|part| Line::from(part.to_string())));
    }
    Ok(lines)
}

fn history_name(mode: HistoryMode) -> &'static str {
    match mode {
        HistoryMode::Legacy => "legacy",
        HistoryMode::Strict10 => "strict10",
        HistoryMode::Structured10 => "structured10",
    }
}

fn tool_name(mode: ToolMemoryModeV1) -> &'static str {
    match mode {
        ToolMemoryModeV1::Off => "off",
        ToolMemoryModeV1::History => "history",
        ToolMemoryModeV1::Retrieval => "retrieval",
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn line(text: impl AsRef<str>) -> Line<'static> {
    Line::from(render::sanitize(text.as_ref()))
}

#[cfg(test)]
mod tests;
