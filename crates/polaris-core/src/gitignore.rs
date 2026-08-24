//! `files.md`(ハーネスが自動生成する、コミット対象外のファイル)を
//! `.gitignore` へ登録する。冪等——同じパターンを二重に書かない。

use std::io::Write;
use std::path::{Path, PathBuf};

/// Finds where the `.gitignore` should live, searching upward from
/// `starting_dir` but **never past `boundary`**.
///
/// `boundary` is the sandbox's writable root. Walking above it is how this
/// module could otherwise write outside the sandbox entirely: with a
/// writable root of `/repo/subproject` and `.git` up at `/repo` — a
/// monorepo subdirectory, or the very git-worktree layout this feature was
/// developed in — an unbounded upward walk appends to `/repo/.gitignore`,
/// which no policy ever granted and no approval gate ever saw. Unlike
/// every other harness write, this one does not go through the sandbox
/// helper, so the bound has to be enforced here.
///
/// Returns `None` when `starting_dir` is not inside `boundary` at all, in
/// which case nothing is written anywhere. Otherwise the nearest ancestor
/// holding a `.git` wins, and `boundary` itself is the fallback — always a
/// location the sandbox already permits writing to.
fn gitignore_dir(starting_dir: &Path, boundary: &Path) -> Option<PathBuf> {
    if !starting_dir.starts_with(boundary) {
        return None;
    }
    let mut current = starting_dir;
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        if current == boundary {
            return Some(boundary.to_path_buf());
        }
        match current.parent() {
            Some(parent) => current = parent,
            // `starts_with(boundary)` held above, so this is unreachable
            // for a well-formed pair; treated as "nowhere safe to write".
            None => return None,
        }
    }
}

pub(crate) fn ensure_pattern_ignored(
    starting_dir: &Path,
    boundary: &Path,
    pattern: &str,
) -> std::io::Result<()> {
    let Some(root) = gitignore_dir(starting_dir, boundary) else {
        return Ok(());
    };
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

        ensure_pattern_ignored(&nested, root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.lines().any(|l| l == "**/files.md"));
        assert!(!root.path().join("a/.gitignore").exists());
    }

    #[test]
    fn is_idempotent_and_never_duplicates_the_pattern() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();

        ensure_pattern_ignored(root.path(), root.path(), "**/files.md").unwrap();
        ensure_pattern_ignored(root.path(), root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert_eq!(content.matches("**/files.md").count(), 1);
    }

    #[test]
    fn preserves_existing_lines_and_appends_after_them() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "/target\n").unwrap();

        ensure_pattern_ignored(root.path(), root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.contains("/target"));
        assert!(content.contains("**/files.md"));
    }

    #[test]
    fn falls_back_to_the_boundary_when_no_git_root_is_found_within_it() {
        let root = tempfile::tempdir().unwrap();
        // No `.git` anywhere under `root` — falls back to the boundary
        // itself, which the sandbox already permits writing to.
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();

        ensure_pattern_ignored(&nested, root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.contains("**/files.md"));
        assert!(!nested.join(".gitignore").exists());
    }

    #[test]
    fn the_upward_walk_stops_at_the_boundary_and_never_reaches_a_git_above_it() {
        // The escape this bound exists to prevent: the writable root is
        // `<repo>/subproject`, but `.git` lives at `<repo>` — a monorepo
        // subdirectory, or a git worktree. An unbounded walk would append
        // to `<repo>/.gitignore`, outside the declared writable root, with
        // no policy permitting it and no approval gate seeing it.
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let boundary = repo.path().join("subproject");
        let target = boundary.join("newdir");
        std::fs::create_dir_all(&target).unwrap();

        ensure_pattern_ignored(&target, &boundary, "**/files.md").unwrap();

        assert!(
            !repo.path().join(".gitignore").exists(),
            "the walk went above the writable root and wrote outside the sandbox"
        );
        let content = std::fs::read_to_string(boundary.join(".gitignore")).unwrap();
        assert!(content.contains("**/files.md"));
    }

    #[test]
    fn a_git_at_the_boundary_itself_is_still_found() {
        // The bound is inclusive: stopping *at* the boundary must still
        // consider the boundary's own `.git`, or the ordinary
        // one-repo-one-root case would lose the repo root it should use.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();

        ensure_pattern_ignored(&nested, root.path(), "**/files.md").unwrap();

        assert!(root.path().join(".gitignore").exists());
        assert!(!nested.join(".gitignore").exists());
    }

    #[test]
    fn a_starting_directory_outside_the_boundary_writes_nothing_anywhere() {
        // Nothing safe to write, so nothing is written — the same
        // best-effort, never-break-the-real-call posture the rest of this
        // feature takes.
        let boundary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        ensure_pattern_ignored(outside.path(), boundary.path(), "**/files.md").unwrap();

        assert!(!outside.path().join(".gitignore").exists());
        assert!(!boundary.path().join(".gitignore").exists());
    }
}
