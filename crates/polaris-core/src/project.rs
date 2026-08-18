//! プロジェクトルートの解決。
//!
//! M2 以降、書込可能ルートはここが返す値から導かれる。作業ディレクトリを
//! そのままルートにすると、リポジトリの深い場所から起動しただけで書ける
//! 範囲が変わる。目印が無いときに `/` まで遡らないのは、そこで遡ると
//! 書込可能ルートがファイルシステム全体になるためである。

use std::path::{Path, PathBuf};

/// 目印となるディレクトリ。最も近いものを採る。
const MARKERS: &[&str] = &[".git", ".polaris"];

pub fn resolve_root(start: &Path) -> PathBuf {
    let canonical = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());

    let mut cursor: &Path = &canonical;
    loop {
        if MARKERS.iter().any(|m| cursor.join(m).exists()) {
            return cursor.to_path_buf();
        }
        match cursor.parent() {
            Some(p) => cursor = p,
            None => return canonical,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subdirectory_resolves_to_the_repository_root() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::create_dir(root.path().join(".git")).expect("mkdir");
        let deep = root.path().join("crates/polaris-core/src");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            root.path().canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn a_polaris_directory_also_marks_the_root() {
        // git を使わない利用者もいる。`.polaris/` があればそこをルートとする。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::create_dir(root.path().join(".polaris")).expect("mkdir");
        let deep = root.path().join("a/b");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            root.path().canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn the_nearest_marker_wins() {
        // 入れ子のリポジトリでは内側が勝つ。外側を選ぶと、書込可能ルートが
        // 意図より広がる。
        let outer = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::create_dir(outer.path().join(".git")).expect("mkdir");
        let inner = outer.path().join("vendor/thing");
        std::fs::create_dir_all(inner.join(".git")).expect("mkdir");
        let deep = inner.join("src");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            inner.canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn without_any_marker_the_starting_directory_is_the_root() {
        // 目印が無いときに `/` まで遡ると、書込可能ルートがファイルシステム
        // 全体になる。遡りは必ず止める。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let deep = dir.path().join("x/y");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            deep.canonicalize().expect("canonicalize")
        );
    }
}
