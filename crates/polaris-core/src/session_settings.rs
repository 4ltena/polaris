//! Conversation-owned settings, independent draft and revision-bound goals.
//! Management effects serialize publication; a repeated or unknown operation
//! never republishes the conversation marker.

use crate::{
    conversation_state::{ConversationSnapshot, ConversationStateV2, HistoryMode, content_hash},
    desktop_store::SavedGoal,
    session_store::PersistedSession,
};
use polaris_desktop_protocol::ids::DecimalU64;
pub use polaris_desktop_protocol::request::GoalOperation as GoalOperationV1;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

/// Matches the ordinary admitted input bound. Drafts never enter raw history.
pub const MAX_DRAFT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMemoryModeV1 {
    #[default]
    Off,
    History,
    Retrieval,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedToolEmbeddingV1 {
    pub url: String,
    pub model: String,
}

impl SavedToolEmbeddingV1 {
    pub fn new(url: String, model: String) -> io::Result<Self> {
        // Match the existing embedding owner: never delegate localhost to DNS.
        let url = url
            .strip_prefix("http://localhost")
            .filter(|rest| rest.is_empty() || rest.starts_with([':', '/']))
            .map(|rest| format!("http://127.0.0.1{rest}"))
            .or_else(|| {
                url.strip_prefix("https://localhost")
                    .filter(|rest| rest.is_empty() || rest.starts_with([':', '/']))
                    .map(|rest| format!("https://127.0.0.1{rest}"))
            })
            .unwrap_or(url);
        let saved = Self { url, model };
        saved.validate()?;
        Ok(saved)
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.url.len() > 4096
            || self.url.contains(['@', '?', '#', '\\'])
            || self
                .url
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || self.model.trim().is_empty()
            || self.model.len() > 1024
        {
            return Err(invalid("埋め込みURLとモデルの組が不正です"));
        }
        let (scheme, rest) = self
            .url
            .split_once("://")
            .ok_or_else(|| invalid("埋め込みURLが不正です"))?;
        let authority = rest.split('/').next().unwrap_or_default();
        polaris_provider::local::Endpoint::parse(&format!("{scheme}://{authority}"))
            .map_err(|_| invalid("埋め込みには認証情報のないループバックURLが必要です"))?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedSessionSettingsV1 {
    pub revision: u64,
    pub remember: bool,
    pub tool_memory: ToolMemoryModeV1,
    pub tool_memory_embedding: Option<SavedToolEmbeddingV1>,
    pub compact_threshold: Option<usize>,
    pub provider: String,
    pub model: String,
    pub effort: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_bindings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub role_bindings_active: bool,
}

impl SavedSessionSettingsV1 {
    pub fn new(provider: String, model: String, effort: String) -> io::Result<Self> {
        let saved = Self {
            revision: 0,
            remember: false,
            tool_memory: ToolMemoryModeV1::Off,
            tool_memory_embedding: None,
            compact_threshold: None,
            provider,
            model,
            effort,
            role_bindings: BTreeMap::new(),
            role_bindings_active: false,
        };
        saved.validate()?;
        Ok(saved)
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.provider.trim().is_empty()
            || self.provider.len() > 128
            || self.provider.chars().any(char::is_control)
            || self.model.trim().is_empty()
            || self.model.len() > 1024
            || self.model.chars().any(char::is_control)
            || (!matches!(self.effort.as_str(), "" | "auto")
                && !crate::desktop_store::EFFORTS.contains(&self.effort.as_str()))
            || self
                .compact_threshold
                .is_some_and(|threshold| threshold < 1000)
        {
            return Err(invalid("セッション設定が不正です"));
        }
        if let Some(embedding) = &self.tool_memory_embedding {
            embedding.validate()?;
        }
        if (!self.role_bindings_active && !self.role_bindings.is_empty())
            || self.role_bindings.len() > 32
            || self.role_bindings.iter().any(|(role, tier)| {
                role.is_empty()
                    || role.len() > 128
                    || !role
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
                    || !polaris_skills::Tier::ALL
                        .iter()
                        .any(|known| known.name() == tier)
            })
        {
            return Err(invalid(
                "role bindingは32件以内のrole名とdwarf|subgiant|giant|supergiantで指定してください",
            ));
        }
        Ok(())
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedSessionDraftV1 {
    pub revision: u64,
    pub text: String,
}

impl SavedSessionDraftV1 {
    pub fn is_initial(&self) -> bool {
        self.revision == 0 && self.text.is_empty()
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.text.len() > MAX_DRAFT_BYTES || (self.revision == 0 && !self.text.is_empty()) {
            return Err(invalid("下書きの保存範囲が不正です"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum SessionSettingsCommandV1 {
    RoleBindings {
        expected_revision: u64,
        bindings: BTreeMap<String, String>,
    },
    Remember {
        enabled: bool,
    },
    ToolMemory {
        mode: ToolMemoryModeV1,
    },
    EmbeddingEndpoint {
        embedding: Option<SavedToolEmbeddingV1>,
    },
    EmbeddingModel {
        embedding: Option<SavedToolEmbeddingV1>,
    },
    CompactThreshold {
        threshold: Option<usize>,
    },
    Configure {
        expected_revision: u64,
        provider: String,
        model: String,
        effort: String,
    },
    History {
        mode: HistoryMode,
    },
    Draft {
        expected_revision: u64,
        text: String,
    },
    Goal {
        expected_revision: u64,
        operation: GoalOperationV1,
    },
}

impl SessionSettingsCommandV1 {
    pub fn inventory(&self) -> &'static str {
        match self {
            Self::RoleBindings { .. } => "D13",
            Self::Remember { .. } => "F06",
            Self::ToolMemory { .. } => "F07",
            Self::EmbeddingEndpoint { .. } => "F08",
            Self::EmbeddingModel { .. } => "F09",
            Self::CompactThreshold { .. } => "F10",
            Self::Configure { .. } => "D18",
            Self::History { .. } => "F16",
            Self::Draft { .. } => "D17",
            Self::Goal { .. } => "D20",
        }
    }
}

pub(crate) fn validate_empty_history(snapshot: &ConversationSnapshot) -> io::Result<()> {
    let state = &snapshot.state;
    if snapshot.raw_offset != 0
        || !snapshot.events.is_empty()
        || state.epoch != 0
        || state.raw_commit_offset != 0
        || state.raw_commit_hash != content_hash(&[])
        || !state.recent_turn_ids.is_empty()
        || !state.visible_summary_ids.is_empty()
        || !state.visible_structured_publication_ids.is_empty()
        || state.structured_coverage.is_some()
        || !state.ancestors.is_empty()
        || !state.admission_receipts.is_empty()
    {
        return Err(invalid("履歴モードの変更は新しい空の会話で行ってください"));
    }
    Ok(())
}

pub(crate) fn session_only_change(
    before: &ConversationStateV2,
    candidate: &ConversationStateV2,
) -> bool {
    let mut permitted = before.clone();
    permitted.session_settings = candidate.session_settings.clone();
    permitted.draft = candidate.draft.clone();
    permitted.goal = candidate.goal.clone();
    permitted == *candidate
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn advance(revision: u64) -> io::Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| invalid("保存revisionが上限です"))
}

fn cas(expected: u64, actual: u64) -> io::Result<()> {
    if expected != actual {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "保存revisionが変わりました",
        ));
    }
    Ok(())
}

fn change_goal(
    previous: Option<&SavedGoal>,
    expected: u64,
    operation: GoalOperationV1,
) -> io::Result<SavedGoal> {
    cas(expected, previous.map_or(0, |goal| goal.revision.get()))?;
    let goal = match operation {
        GoalOperationV1::Set { text } => {
            if text.trim().is_empty() {
                return Err(invalid("goalは空にできません"));
            }
            SavedGoal {
                text,
                revision: DecimalU64::new(advance(expected)?),
                active: true,
                stopped: previous.is_some_and(|goal| goal.stopped),
                automatic_used: DecimalU64::new(0),
            }
        }
        operation => {
            let previous = previous.ok_or_else(|| invalid("保存されたgoalがありません"))?;
            SavedGoal {
                revision: DecimalU64::new(advance(expected)?),
                active: !matches!(operation, GoalOperationV1::Clear) && previous.active,
                stopped: match operation {
                    GoalOperationV1::Stop => true,
                    GoalOperationV1::Resume => false,
                    _ => previous.stopped,
                },
                ..previous.clone()
            }
        }
    };
    goal.validate().map_err(io::Error::other)?;
    Ok(goal)
}

#[cfg(unix)]
mod owner;
#[cfg(unix)]
pub use owner::{
    PreparedSessionSettingsV1, SessionSettingsError, SessionSettingsOwnerV1,
    SessionSettingsPreviewV1,
};

#[cfg(all(test, unix))]
mod tests;
