//! Bounded read-only workspace and explicit attachment projections.
//! These values carry no filesystem errors, source paths, or authority.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewState {
    Ready,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitState {
    Ready,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    pub id: String,
    pub path: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstaged_diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_diff: Option<String>,
    pub modified: bool,
    pub staged: bool,
    pub unsaved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionView {
    pub id: String,
    pub parent: String,
    pub title: String,
    #[serde(rename = "fileID")]
    pub file_id: String,
    pub body: String,
    pub diff: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitView {
    pub state: GitState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_summary: Option<String>,
    pub revisions: Vec<RevisionView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceView {
    pub state: ViewState,
    pub files: Vec<FileView>,
    pub git: GitView,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentState {
    Ready,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentView {
    pub state: AttachmentState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_uses_native_file_identity_key() {
        let revision = RevisionView {
            id: "commit".into(),
            parent: "".into(),
            title: "change".into(),
            file_id: "README.md".into(),
            body: "text".into(),
            diff: "".into(),
        };
        let value = serde_json::to_value(revision).unwrap();
        assert_eq!(value["fileID"], "README.md");
        assert!(value.get("fileId").is_none());
    }
}
