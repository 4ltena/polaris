//! `bash`/`write`/`edit` 呼び出しの前後でファイルシステムを比較し、新規
//! ディレクトリ・既存ディレクトリへの新規ファイルを検出する。中身は一切
//! 読まない——パスと種別(ファイル/ディレクトリ)だけを見る。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const SKIP_DIR_NAMES: &[&str] = &[".git", "target", "node_modules"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirSnapshot {
    pub dirs: BTreeSet<PathBuf>,
    pub files: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirChanges {
    pub new_dirs: Vec<PathBuf>,
    pub new_files_in_existing_dirs: Vec<PathBuf>,
}

pub(crate) fn snapshot_recursive(root: &Path) -> DirSnapshot {
    let mut out = DirSnapshot::default();
    walk(root, &mut out, true);
    out
}

pub(crate) fn snapshot_shallow(dir: &Path) -> DirSnapshot {
    let mut out = DirSnapshot::default();
    walk(dir, &mut out, false);
    out
}

fn walk(dir: &Path, out: &mut DirSnapshot, recurse: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name();
            if SKIP_DIR_NAMES
                .iter()
                .any(|s| name == std::ffi::OsStr::new(s))
            {
                continue;
            }
            out.dirs.insert(path.clone());
            if recurse {
                walk(&path, out, recurse);
            }
        } else if file_type.is_file() {
            out.files.insert(path);
        }
    }
}

/// ツール呼び出しの前後で何をスキャンするかという「対象範囲」と、その
/// 範囲が再帰スキャンか1階層スキャンかを1つの型にまとめる。呼び出し側
/// (`agent::run_loop`)が `bash` か `write`/`edit` かで作り分け、事後の
/// 再スキャンはこの型が自分の種別を覚えているので、呼び出し側は同じ分岐
/// を二度書かなくてよい——前後で違う種別のスキャンを取ってしまえば差分
/// はまるごと嘘になるので、その分岐を1箇所に閉じ込めることそのものが
/// この型の存在理由である。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScanScope {
    Recursive(PathBuf),
    Shallow(PathBuf),
    /// スキャンすべき対象がない場合。`bash` かつ書込許可ルートを持たない
    /// (read-only / full-access)場合や、`write`/`edit` の `path` 引数が
    /// 読めない場合がこれにあたる。
    None,
}

pub(crate) fn snapshot_for_scope(scope: &ScanScope) -> DirSnapshot {
    match scope {
        ScanScope::Recursive(root) => snapshot_recursive(root),
        ScanScope::Shallow(dir) => snapshot_shallow(dir),
        ScanScope::None => DirSnapshot::default(),
    }
}

pub(crate) fn diff(before: &DirSnapshot, after: &DirSnapshot) -> DirChanges {
    let new_dirs: Vec<PathBuf> = after.dirs.difference(&before.dirs).cloned().collect();
    let new_dir_set: BTreeSet<&PathBuf> = new_dirs.iter().collect();

    let new_files_in_existing_dirs: Vec<PathBuf> = after
        .files
        .difference(&before.files)
        .filter(|f| {
            f.parent()
                .map(|p| !new_dir_set.contains(&p.to_path_buf()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    DirChanges {
        new_dirs,
        new_files_in_existing_dirs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_recursive_finds_nested_dirs_and_files_but_skips_known_noise() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/one.txt"), "x").unwrap();
        std::fs::write(root.path().join("a/b/two.txt"), "x").unwrap();
        std::fs::create_dir_all(root.path().join("target/junk")).unwrap();
        std::fs::write(root.path().join("target/junk/ignored.txt"), "x").unwrap();

        let snap = snapshot_recursive(root.path());

        assert!(snap.dirs.contains(&root.path().join("a")));
        assert!(snap.dirs.contains(&root.path().join("a/b")));
        assert!(snap.files.contains(&root.path().join("a/one.txt")));
        assert!(snap.files.contains(&root.path().join("a/b/two.txt")));
        assert!(!snap.dirs.contains(&root.path().join("target")));
        assert!(
            !snap
                .files
                .contains(&root.path().join("target/junk/ignored.txt"))
        );
    }

    #[test]
    fn snapshot_shallow_does_not_recurse() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/deep.txt"), "x").unwrap();
        std::fs::write(root.path().join("top.txt"), "x").unwrap();

        let snap = snapshot_shallow(root.path());

        assert!(snap.files.contains(&root.path().join("top.txt")));
        assert!(snap.dirs.contains(&root.path().join("sub")));
        assert!(!snap.files.contains(&root.path().join("sub/deep.txt")));
    }

    #[test]
    fn a_none_scope_scans_nothing_at_all() {
        // The scope `bash` gets when there is no writable root to watch.
        // It has to come back empty rather than falling back to scanning
        // something — an empty snapshot on both sides yields an empty
        // diff, which is exactly "nothing observed".
        let snap = snapshot_for_scope(&ScanScope::None);
        assert_eq!(snap, DirSnapshot::default());
    }

    #[test]
    fn a_scope_remembers_whether_it_is_recursive_or_shallow() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/deep.txt"), "x").unwrap();

        let recursive = snapshot_for_scope(&ScanScope::Recursive(root.path().to_path_buf()));
        let shallow = snapshot_for_scope(&ScanScope::Shallow(root.path().to_path_buf()));

        assert!(recursive.files.contains(&root.path().join("sub/deep.txt")));
        assert!(!shallow.files.contains(&root.path().join("sub/deep.txt")));
    }

    #[test]
    fn diff_reports_a_new_directory() {
        let root = tempfile::tempdir().unwrap();
        let before = snapshot_recursive(root.path());
        std::fs::create_dir(root.path().join("newdir")).unwrap();
        let after = snapshot_recursive(root.path());

        let changes = diff(&before, &after);

        assert_eq!(changes.new_dirs, vec![root.path().join("newdir")]);
        assert!(changes.new_files_in_existing_dirs.is_empty());
    }

    #[test]
    fn diff_reports_a_new_file_in_an_existing_directory_separately_from_a_new_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("existing")).unwrap();
        let before = snapshot_recursive(root.path());
        std::fs::write(root.path().join("existing/new.txt"), "x").unwrap();
        let after = snapshot_recursive(root.path());

        let changes = diff(&before, &after);

        assert!(changes.new_dirs.is_empty());
        assert_eq!(
            changes.new_files_in_existing_dirs,
            vec![root.path().join("existing/new.txt")]
        );
    }

    #[test]
    fn a_file_inside_a_brand_new_directory_is_not_double_reported_as_a_new_file_in_an_existing_directory()
     {
        let root = tempfile::tempdir().unwrap();
        let before = snapshot_recursive(root.path());
        std::fs::create_dir(root.path().join("newdir")).unwrap();
        std::fs::write(root.path().join("newdir/inside.txt"), "x").unwrap();
        let after = snapshot_recursive(root.path());

        let changes = diff(&before, &after);

        assert_eq!(changes.new_dirs, vec![root.path().join("newdir")]);
        assert!(
            changes.new_files_in_existing_dirs.is_empty(),
            "a file inside a brand-new directory should only be covered by the \
             new directory's own generation, not reported again: {changes:?}"
        );
    }
}
