//! Home browsing for the read-only helper. Paths are home-relative; the root
//! is fixed by the helper and never named by the client.
use crate::ids::DecimalU64;
use serde::{Deserialize, Deserializer, Serialize, de};

pub const MAX_PAGE_ENTRIES: usize = 256;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_TOKEN_BYTES: usize = 256;

fn valid_path(value: &str, allow_home: bool) -> bool {
    if value.is_empty() {
        return allow_home;
    }
    value.len() <= MAX_PATH_BYTES
        && !value.starts_with('/')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn directory<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    valid_path(&value, true)
        .then_some(value)
        .ok_or_else(|| de::Error::custom("browse path"))
}

fn file<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    valid_path(&value, false)
        .then_some(value)
        .ok_or_else(|| de::Error::custom("browse path"))
}

fn token<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if value.is_empty()
        || value.len() > MAX_TOKEN_BYTES
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(de::Error::custom("browse token"));
    }
    Ok(value)
}

fn optional_token<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    token(d).map(Some)
}

crate::object_wire! {
/// `path` is empty for home itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseList {
    #[serde(deserialize_with = "directory")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "optional_token")]
    pub cursor: Option<String>,
}
}

crate::object_wire! {
/// The identity comes from the listing that showed the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseRead {
    #[serde(deserialize_with = "file")]
    pub path: String,
    pub device: DecimalU64,
    pub inode: DecimalU64,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowseEntryKind {
    Directory,
    File,
    /// Shown by name only; never followed.
    Symlink,
    Unsupported,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseEntry {
    pub name: String,
    #[serde(deserialize_with = "file")]
    pub path: String,
    pub kind: BrowseEntryKind,
    pub device: DecimalU64,
    pub inode: DecimalU64,
}
}

fn entries<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<BrowseEntry>, D::Error> {
    let values = Vec::<BrowseEntry>::deserialize(d)?;
    if values.len() > MAX_PAGE_ENTRIES {
        return Err(de::Error::custom("browse page"));
    }
    Ok(values)
}

crate::object_wire! {
/// `capacity` marks a listing cut at the helper's limit; it is never complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseListPage {
    #[serde(deserialize_with = "token")]
    pub browse_epoch: String,
    #[serde(deserialize_with = "token")]
    pub snapshot_id: String,
    #[serde(deserialize_with = "directory")]
    pub directory: String,
    #[serde(deserialize_with = "entries")]
    pub entries: Vec<BrowseEntry>,
    /// Names that are not UTF-8 or contain control characters.
    pub omitted: DecimalU64,
    pub capacity: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "optional_token")]
    pub next_cursor: Option<String>,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowseUnavailable {
    Deleted,
    Replaced,
    /// Changed while being read.
    RevisionConflict,
    TooLarge,
    Binary,
    PermissionDenied,
    Protected,
    NotRegularFile,
}

fn body<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if value.len() > MAX_BODY_BYTES || value.contains('\0') {
        return Err(de::Error::custom("browse body"));
    }
    Ok(value)
}

/// An unavailable read is never an empty success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowseContent {
    Ready {
        #[serde(deserialize_with = "body")]
        text: String,
    },
    Unavailable {
        reason: BrowseUnavailable,
    },
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseReadResult {
    #[serde(deserialize_with = "token")]
    pub browse_epoch: String,
    #[serde(deserialize_with = "file")]
    pub path: String,
    pub device: DecimalU64,
    pub inode: DecimalU64,
    pub content: BrowseContent,
}
}
