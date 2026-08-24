//! `dir_watch` が検出した変更を、実際に `files.md` を書く subagent の
//! 実行(`spawn::run_one`)と `.gitignore` への登録へつなぐ。モデルの
//! `spawn` ツール呼び出しは経由しない——呼び出し元(`agent::run_loop`)
//! がハーネスとして直接呼ぶ。ここでの失敗は audit log にのみ記録され、
//! 呼び出し元のツール結果には一切影響しない。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use polaris_provider::Provider;
use polaris_sandbox::SandboxPolicy;
use polaris_skills::AgentType;

use crate::audit::{AuditLog, Record};
use crate::dir_watch::DirChanges;
use crate::spawn::{SpawnTask, TaskOutcome, run_one};

const FILES_MD_AGENT_TYPE: &str = "files-md-writer";
const FILES_MD_FILENAME: &str = "files.md";

pub(crate) async fn regenerate_for_changes(
    changes: &DirChanges,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) {
    if agent_types.iter().all(|a| a.name != FILES_MD_AGENT_TYPE) {
        return;
    }

    let targets = targets_to_regenerate(changes);

    for dir in targets {
        regenerate_one(
            &dir,
            agent_types,
            provider.clone(),
            audit.clone(),
            base_sandbox,
            helper,
        )
        .await;
    }
}

/// 純粋関数として切り出す——subagent 実行なしにロジックを単体テストする
/// ため。新規ディレクトリは常に対象。新規ディレクトリの親は、既に
/// `files.md` を持つ場合のみ対象(持たない親は観測対象外のまま)。既存
/// ディレクトリへの新規ファイルは、そのファイルが `files.md` という
/// 名前そのものでなく、かつそのディレクトリが既に `files.md` を持つ
/// 場合のみ対象。
fn targets_to_regenerate(changes: &DirChanges) -> Vec<PathBuf> {
    let mut targets: Vec<PathBuf> = Vec::new();

    for dir in &changes.new_dirs {
        targets.push(dir.clone());
        if let Some(parent) = dir.parent()
            && parent.join(FILES_MD_FILENAME).exists()
        {
            targets.push(parent.to_path_buf());
        }
    }

    for file in &changes.new_files_in_existing_dirs {
        if file
            .file_name()
            .map(|n| n == FILES_MD_FILENAME)
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(parent) = file.parent()
            && parent.join(FILES_MD_FILENAME).exists()
        {
            targets.push(parent.to_path_buf());
        }
    }

    targets.sort();
    targets.dedup();
    targets
}

async fn regenerate_one(
    dir: &Path,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) {
    let task = SpawnTask {
        agent_type: FILES_MD_AGENT_TYPE.to_string(),
        task: dir.display().to_string(),
        write_root: Some(dir.display().to_string()),
    };
    let outcome = run_one(
        &task,
        agent_types,
        provider,
        audit.clone(),
        base_sandbox,
        helper,
    )
    .await;
    if let TaskOutcome::Failed(msg) = outcome {
        let _ = audit.lock().await.record(&Record {
            tool: "files-md-writer",
            detail: &dir.display().to_string(),
            sandbox: None,
            target: Some(dir),
            result: &msg,
            caller: "harness",
        });
    }
    let _ = crate::gitignore::ensure_pattern_ignored(dir, "**/files.md");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn changes_with(new_dirs: Vec<PathBuf>, new_files: Vec<PathBuf>) -> DirChanges {
        DirChanges {
            new_dirs,
            new_files_in_existing_dirs: new_files,
        }
    }

    #[test]
    fn a_brand_new_directory_is_always_a_target() {
        let dir = std::env::temp_dir().join("files_md_test_new_dir_never_created");
        let changes = changes_with(vec![dir.clone()], vec![]);
        let targets = targets_to_regenerate(&changes);
        assert_eq!(targets, vec![dir]);
    }

    #[test]
    fn a_new_directorys_parent_is_a_target_only_if_the_parent_already_has_files_md() {
        let root = tempfile::tempdir().unwrap();
        let with_files_md = root.path().join("has_one");
        std::fs::create_dir_all(&with_files_md).unwrap();
        std::fs::write(with_files_md.join("files.md"), "# files.md\n").unwrap();
        let without_files_md = root.path().join("has_none");
        std::fs::create_dir_all(&without_files_md).unwrap();

        let changes = changes_with(
            vec![with_files_md.join("child"), without_files_md.join("child")],
            vec![],
        );
        let targets = targets_to_regenerate(&changes);

        assert!(targets.contains(&with_files_md.join("child")));
        assert!(targets.contains(&with_files_md));
        assert!(targets.contains(&without_files_md.join("child")));
        assert!(!targets.contains(&without_files_md));
    }

    #[test]
    fn a_new_file_named_files_md_itself_is_never_a_trigger() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/files.md"), "# files.md\n").unwrap();

        let changes = changes_with(vec![], vec![root.path().join("dir/files.md")]);
        let targets = targets_to_regenerate(&changes);

        assert!(
            targets.is_empty(),
            "files.md writing itself must not re-trigger regeneration: {targets:?}"
        );
    }

    #[test]
    fn a_new_ordinary_file_is_a_target_only_if_its_directory_already_has_files_md() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("files.md"), "# files.md\n").unwrap();

        let changes = changes_with(vec![], vec![dir.join("new.rs")]);
        let targets = targets_to_regenerate(&changes);

        assert_eq!(targets, vec![dir]);
    }
}
