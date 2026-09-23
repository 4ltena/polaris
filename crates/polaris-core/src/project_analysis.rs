//! Minimal, state-driven bootstrap for a first-turn project analysis.
//!
//! This is deliberately transient session state. It selects one source at
//! the user boundary and projects only that state into the request; it does
//! not grow the fixed system prompt or copy an entire handoff/file map into
//! every request.

use std::path::{Path, PathBuf};

pub const CONTEXT_HEADING: &str = "## Initial project analysis state";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ProjectAnalysisState {
    #[default]
    Inactive,
    Current {
        source: PathBuf,
    },
    Filemap {
        source: PathBuf,
    },
    Repository,
}

/// One bounded file read selected by the state machine before the root model
/// sees the first project-analysis request. The task text deliberately names
/// one absolute path: `file-reader` has no parent conversation and cannot
/// infer which checkout the root meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderRequest {
    pub path: PathBuf,
    pub task: String,
}

impl ProjectAnalysisState {
    /// Resolve exactly one stage for a first user turn.
    pub fn resolve(root: Option<&Path>, input: &str, first_user_turn: bool) -> Self {
        if !first_user_turn || !is_system_analysis(input) {
            return Self::Inactive;
        }
        let Some(root) = root else {
            return Self::Repository;
        };
        for candidate in [
            "docs/superpowers/CURRENT.md",
            "CURRENT.md",
            ".polaris/notes/CURRENT.md",
        ] {
            if is_regular_file(root, candidate) {
                return Self::Current {
                    source: candidate.into(),
                };
            }
        }
        for candidate in ["docs/filemap.md", "filemap.md"] {
            if is_regular_file(root, candidate) {
                return Self::Filemap {
                    source: candidate.into(),
                };
            }
        }
        Self::Repository
    }

    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Inactive)
    }

    pub fn stage(&self) -> Option<&'static str> {
        match self {
            Self::Inactive => None,
            Self::Current { .. } => Some("current"),
            Self::Filemap { .. } => Some("filemap"),
            Self::Repository => Some("repository"),
        }
    }

    /// Build the first delegated read for a source-bearing stage.
    ///
    /// `resolve` already rejects symlinks, but the source can change before
    /// the async child starts. Recheck it here and require the canonical file
    /// to remain inside the canonical project root. A changed or missing
    /// source leaves the root on the explicit fallback path instead of
    /// handing a child an untrusted replacement.
    pub fn reader_request(&self, root: Option<&Path>) -> Option<ReaderRequest> {
        let root = root?;
        let (source, question) = match self {
            Self::Current { source } => (
                source,
                "Summarize the current milestone, completed work, blockers, and exact next tasks. Preserve identifiers, versions, measured values, negation, and completion status. Report any additional file needed as a gap.",
            ),
            Self::Filemap { source } => (
                source,
                "Identify the smallest set of files or areas the parent must inspect to analyze this project. Preserve paths and responsibilities, and report any missing coverage as a gap.",
            ),
            Self::Inactive | Self::Repository => return None,
        };
        let metadata = std::fs::symlink_metadata(root.join(source)).ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }
        let canonical_root = root.canonicalize().ok()?;
        let path = root.join(source).canonicalize().ok()?;
        if !path.starts_with(&canonical_root) {
            return None;
        }
        Some(ReaderRequest {
            task: format!("Read the complete file at {}. {question}", path.display()),
            path,
        })
    }

    /// A small request-only state slice. For a source-bearing stage the host
    /// delegates the first read before the root request; this base context is
    /// also the safe fallback when the bundled `file-reader` is unavailable.
    pub fn context(&self) -> Option<String> {
        match self {
            Self::Inactive => None,
            Self::Current { source } => Some(format!(
                "{CONTEXT_HEADING}\nstage=current\nsource={}\nnext=filemap_if_current_is_insufficient,repository_if_filemap_is_missing_or_insufficient\nfallback_action=read_source_first_and_expand_only_as_needed",
                source.display()
            )),
            Self::Filemap { source } => Some(format!(
                "{CONTEXT_HEADING}\nstage=filemap\nsource={}\nnext=repository_if_filemap_is_insufficient\nfallback_action=read_source_first_and_expand_only_as_needed",
                source.display()
            )),
            Self::Repository => Some(format!(
                "{CONTEXT_HEADING}\nstage=repository\nsource=project_root\nnext=none\naction=inspect_top_level_and_manifests_first_then_expand_only_as_needed"
            )),
        }
    }
}

fn is_regular_file(root: &Path, relative: &str) -> bool {
    std::fs::symlink_metadata(root.join(relative))
        .is_ok_and(|metadata| metadata.file_type().is_file())
}

fn is_system_analysis(input: &str) -> bool {
    let normalized = input.to_lowercase();
    let analysis = [
        "分析",
        "解析",
        "調査",
        "把握",
        "概要",
        "説明",
        "レビュー",
        "analy",
        "inspect",
        "review",
        "understand",
        "overview",
        "explain",
    ]
    .iter()
    .any(|term| normalized.contains(term));
    let broad_scope = [
        "このプロジェクト",
        "プロジェクト全体",
        "リポジトリ全体",
        "コードベース",
        "システム全体",
        "システム分析",
        "アーキテクチャ",
        "project",
        "repository",
        "codebase",
        "system",
        "architecture",
    ]
    .iter()
    .any(|term| normalized.contains(term));
    analysis && broad_scope
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_project_analysis_uses_current_before_filemap() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("docs/superpowers")).unwrap();
        std::fs::write(root.path().join("docs/superpowers/CURRENT.md"), "current").unwrap();
        std::fs::write(root.path().join("docs/filemap.md"), "filemap").unwrap();
        assert_eq!(
            ProjectAnalysisState::resolve(Some(root.path()), "このプロジェクトを分析する", true),
            ProjectAnalysisState::Current {
                source: "docs/superpowers/CURRENT.md".into()
            }
        );
    }

    #[test]
    fn filemap_then_repository_are_explicit_fallback_stages() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("docs")).unwrap();
        std::fs::write(root.path().join("docs/filemap.md"), "filemap").unwrap();
        assert!(matches!(
            ProjectAnalysisState::resolve(Some(root.path()), "system architecture analysis", true),
            ProjectAnalysisState::Filemap { .. }
        ));
        std::fs::remove_file(root.path().join("docs/filemap.md")).unwrap();
        assert_eq!(
            ProjectAnalysisState::resolve(Some(root.path()), "system architecture analysis", true),
            ProjectAnalysisState::Repository
        );
    }

    #[test]
    fn a_symlink_is_not_accepted_as_project_context() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.path().join("CURRENT.md")).unwrap();
        assert_eq!(
            ProjectAnalysisState::resolve(Some(root.path()), "このプロジェクトを分析する", true),
            ProjectAnalysisState::Repository
        );
    }

    #[test]
    fn ordinary_and_later_turns_add_no_analysis_context() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("CURRENT.md"), "current").unwrap();
        assert_eq!(
            ProjectAnalysisState::resolve(Some(root.path()), "parserのbugを直す", true),
            ProjectAnalysisState::Inactive
        );
        assert_eq!(
            ProjectAnalysisState::resolve(Some(root.path()), "このプロジェクトを分析する", false),
            ProjectAnalysisState::Inactive
        );
    }

    #[test]
    fn context_exposes_only_the_selected_stage_and_source() {
        let state = ProjectAnalysisState::Current {
            source: ".polaris/notes/CURRENT.md".into(),
        };
        let context = state.context().unwrap();
        assert!(context.contains("stage=current"));
        assert!(context.contains("source=.polaris/notes/CURRENT.md"));
        assert!(!context.contains("current contents"));
        assert!(crate::budget::count_tokens(&context) < 128);
    }

    #[test]
    fn reader_request_rechecks_the_selected_file_and_uses_an_absolute_path() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("CURRENT.md"), "current").unwrap();
        let state = ProjectAnalysisState::Current {
            source: "CURRENT.md".into(),
        };
        let request = state.reader_request(Some(root.path())).unwrap();
        assert!(request.path.is_absolute());
        assert!(request.task.contains(request.path.to_str().unwrap()));
        assert!(request.task.contains("current milestone"));

        std::fs::remove_file(root.path().join("CURRENT.md")).unwrap();
        assert!(state.reader_request(Some(root.path())).is_none());
    }
}
