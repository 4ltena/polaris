//! `files.md`(ハーネスが自動生成する、コミット対象外のファイル)を
//! `.gitignore` へ登録する。冪等——同じパターンを二重に書かない。

use std::io::Write;
use std::path::{Path, PathBuf};

#[allow(dead_code)]
fn find_repo_root(starting_dir: &Path) -> PathBuf {
    let mut current = starting_dir;
    loop {
        if current.join(".git").exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return starting_dir.to_path_buf(),
        }
    }
}

#[allow(dead_code)]
pub(crate) fn ensure_pattern_ignored(starting_dir: &Path, pattern: &str) -> std::io::Result<()> {
    let root = find_repo_root(starting_dir);
    let gitignore_path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore_path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{pattern}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_the_pattern_to_a_gitignore_at_the_repo_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();

        ensure_pattern_ignored(&nested, "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.lines().any(|l| l == "**/files.md"));
        assert!(!root.path().join("a/.gitignore").exists());
    }

    #[test]
    fn is_idempotent_and_never_duplicates_the_pattern() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();

        ensure_pattern_ignored(root.path(), "**/files.md").unwrap();
        ensure_pattern_ignored(root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert_eq!(content.matches("**/files.md").count(), 1);
    }

    #[test]
    fn preserves_existing_lines_and_appends_after_them() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "/target\n").unwrap();

        ensure_pattern_ignored(root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.contains("/target"));
        assert!(content.contains("**/files.md"));
    }

    #[test]
    fn falls_back_to_the_starting_directory_when_no_git_root_is_found() {
        let root = tempfile::tempdir().unwrap();
        // No `.git` anywhere under `root` — falls back to `root` itself.

        ensure_pattern_ignored(root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.contains("**/files.md"));
    }
}
