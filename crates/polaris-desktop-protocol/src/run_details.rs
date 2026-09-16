//! Saved run content, paged by the owner. Text is display data only; clients
//! never run it as a command, link or markup.
use crate::{
    ids::{AttemptId, DecimalU64, RunId},
    run_state::RunState,
};
use serde::{Deserialize, Deserializer, Serialize, de};

pub const MAX_FRAGMENTS: usize = 4;
pub const MAX_FRAGMENT_BYTES: usize = 32 * 1024;
pub const MAX_PAGE_TEXT_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunDetailKind {
    UserInput,
    /// Untrusted MCP prompt content the host attached to the input.
    McpPrompt,
    Assistant,
    ToolCall,
    ToolResult,
    /// A saved call with no saved result; never an empty success.
    ToolResultUnsaved,
    HostedWebSearch,
}

fn token<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if value.is_empty() || value.len() > MAX_TOKEN_BYTES || value.chars().any(char::is_control) {
        return Err(de::Error::custom("run details token"));
    }
    Ok(value)
}

fn optional_token<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    token(d).map(Some)
}

crate::object_wire! {
/// The first request has neither `snapshot_id` nor `cursor`; later pages
/// carry both from the previous page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDetailsRequest {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "optional_token")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "optional_token")]
    pub cursor: Option<String>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDetailFragment {
    #[serde(deserialize_with = "token")]
    pub entry_id: String,
    pub kind: RunDetailKind,
    /// UTF-8 byte offset of `text` within the entry.
    pub offset: DecimalU64,
    pub total_bytes: DecimalU64,
    pub text: String,
}
}

fn fragments<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<RunDetailFragment>, D::Error> {
    let values = Vec::<RunDetailFragment>::deserialize(d)?;
    if values.len() > MAX_FRAGMENTS
        || values.iter().any(|f| {
            f.text.len() > MAX_FRAGMENT_BYTES
                || f.offset
                    .get()
                    .checked_add(f.text.len() as u64)
                    .is_none_or(|end| end > f.total_bytes.get())
        })
        || values.iter().map(|f| f.text.len()).sum::<usize>() > MAX_PAGE_TEXT_BYTES
    {
        return Err(de::Error::custom("run details page"));
    }
    Ok(values)
}

crate::object_wire! {
/// `history_gap` reports a transcript known to be incomplete; the run state
/// is reported separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDetailsPage {
    #[serde(deserialize_with = "token")]
    pub snapshot_id: String,
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub session_revision: DecimalU64,
    pub accepted_revision: DecimalU64,
    pub run_state: RunState,
    pub history_gap: bool,
    #[serde(deserialize_with = "fragments")]
    pub fragments: Vec<RunDetailFragment>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "optional_token")]
    pub next_cursor: Option<String>,
}
}
