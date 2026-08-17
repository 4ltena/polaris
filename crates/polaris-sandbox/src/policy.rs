//! 方針と書込可能ルート。ルートは構築時に正規化する。

use std::path::{Path, PathBuf};

use crate::SandboxError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxMode {
    /// 方針名。監査ログと拒否メッセージが同じ綴りを使う。
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::FullAccess => "full-access",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    mode: SandboxMode,
    writable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// ルートは必ず正規化して保持する。経路の途中にシンボリックリンクが
    /// あると、強制側は与えられたパスと実際のパスを別物と判断して全てを
    /// 拒否する。macOS の `/tmp` と `/var` が該当するため端の事例ではない。
    pub fn new(mode: SandboxMode, roots: &[PathBuf]) -> Result<Self, SandboxError> {
        match mode {
            SandboxMode::ReadOnly if !roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "read-only に書込可能ルートは指定できない".into(),
                ));
            }
            SandboxMode::WorkspaceWrite if roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "workspace-write には書込可能ルートが 1 件以上要る".into(),
                ));
            }
            SandboxMode::FullAccess if !roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "full-access に書込可能ルートは指定できない".into(),
                ));
            }
            _ => {}
        }

        // full-access ではルートを保持しない。保持すると、実際には効いて
        // いない値が方針の説明に現れ、読む側を誤らせる。
        let writable_roots = if mode == SandboxMode::FullAccess {
            Vec::new()
        } else {
            let mut canonical = Vec::with_capacity(roots.len());
            for r in roots {
                canonical.push(r.canonicalize()?);
            }
            canonical
        };

        Ok(Self {
            mode,
            writable_roots,
        })
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// 拒否メッセージ用。仕様は拒否されたパスと現在の方針と書込可能ルートを
    /// 含めることを要求している。ここは後半 2 つを担う。
    pub fn describe(&self) -> String {
        if self.writable_roots.is_empty() {
            return self.mode.as_str().to_string();
        }
        let roots: Vec<String> = self
            .writable_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        format!("{}（書込可能: {}）", self.mode.as_str(), roots.join(", "))
    }

    /// 正規化済みの対象パスが、いずれかのルートの内側にあるか。
    /// `full-access` は常に真、`read-only` は常に偽。
    pub fn contains(&self, canonical_target: &Path) -> bool {
        match self.mode {
            SandboxMode::FullAccess => true,
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => self
                .writable_roots
                .iter()
                .any(|r| canonical_target.starts_with(r)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_reached_through_a_symlink_is_stored_canonicalised() {
        // macOS の /tmp は /private/tmp へのシンボリックリンクであり、
        // tempfile::tempdir() もその下に作られる。正規化しないと、強制側は
        // 与えられたルートと実際のパスが一致しないと判断して「全部拒否」に
        // 倒れる。しかもその拒否は方針違反の拒否と区別がつかないため、
        // 受け入れ基準のテストが誤った理由で通ってしまう。
        let real = tempfile::tempdir().expect("一時ディレクトリ");
        let link_parent = tempfile::tempdir().expect("一時ディレクトリ");
        let link = link_parent.path().join("link-to-root");
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&link))
            .expect("方針を作れない");

        let stored = &policy.writable_roots()[0];
        assert_eq!(
            stored,
            &real.path().canonicalize().expect("canonicalize"),
            "ルートが正規化されていない。格納値 {} はリンクのまま",
            stored.display()
        );
        assert_ne!(stored, &link, "リンクのパスがそのまま入っている");
    }

    #[test]
    fn workspace_write_requires_at_least_one_root() {
        // ルート 0 件の workspace-write は「どこへも書けない」を意味するが、
        // 呼び出し側の組み立て漏れと区別がつかない。区別できない状態を
        // 黙って受け取らない。
        let err = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[])
            .expect_err("ルート 0 件が通ってしまった");
        assert!(
            matches!(err, SandboxError::NotEnforced(_)),
            "想定と違うエラー: {err}"
        );
    }

    #[test]
    fn read_only_rejects_writable_roots() {
        // read-only にルートを渡せてしまうと、方針の名前と実際の権限が
        // 食い違う。宣言と強制を同一のオブジェクトにするという設計目標に反する。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let err = SandboxPolicy::new(SandboxMode::ReadOnly, &[dir.path().to_path_buf()])
            .expect_err("read-only にルートが通ってしまった");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn a_nonexistent_root_is_rejected_rather_than_silently_dropped() {
        // 存在しないルートは canonicalize できない。黙って捨てると、
        // 書けるつもりの場所が減っていることに誰も気づかない。
        let missing = std::path::PathBuf::from("/definitely/not/here/polaris-test");
        let err = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[missing])
            .expect_err("存在しないルートが通ってしまった");
        assert!(matches!(err, SandboxError::Io(_)), "{err}");
    }

    #[test]
    fn full_access_needs_no_roots_and_keeps_none() {
        let policy =
            SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("full-access を作れない");
        assert_eq!(policy.mode(), SandboxMode::FullAccess);
        assert!(policy.writable_roots().is_empty());
    }

    #[test]
    fn describe_names_the_mode_and_every_root() {
        // 拒否メッセージはこの文字列を含む。仕様が「拒否されたパス、現在の
        // 方針、書込可能ルート」を要求しているので、ルートを 1 件でも
        // 落とす整形はモデルに誤った地図を渡すことになる。
        let a = tempfile::tempdir().expect("一時ディレクトリ");
        let b = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            &[a.path().to_path_buf(), b.path().to_path_buf()],
        )
        .expect("方針を作れない");

        let s = policy.describe();
        assert!(s.contains("workspace-write"), "方針名が無い: {s}");
        for root in policy.writable_roots() {
            assert!(
                s.contains(&root.display().to_string()),
                "ルート {} が説明に無い: {s}",
                root.display()
            );
        }
    }

    #[test]
    fn full_access_rejects_writable_roots() {
        // full-access にルートを渡せてしまうと、方針の名前と実際の権限が
        // 食い違う。read-only と同様に拒否する。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let err = SandboxPolicy::new(SandboxMode::FullAccess, &[dir.path().to_path_buf()])
            .expect_err("full-access にルートが通ってしまった");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn contains_includes_root_and_child_paths() {
        // ルート自身と、その下の子ディレクトリ・ファイルはcontainsする。
        // 兄弟ディレクトリはしない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let root_path = root.path().canonicalize().expect("canonicalize");
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&root_path.clone()),
        )
        .expect("方針を作れない");

        // ルート自身が含まれる
        assert!(
            policy.contains(&root_path),
            "root itself should be contained"
        );

        // 子ディレクトリが含まれる
        let child = root_path.join("child");
        assert!(
            policy.contains(&child),
            "child directory should be contained"
        );

        // 兄弟ディレクトリは含まれない
        let sibling = root_path.parent().unwrap().join("sibling");
        assert!(
            !policy.contains(&sibling),
            "sibling directory should not be contained"
        );
    }

    #[test]
    fn contains_full_access_always_true() {
        // full-access は任意のパスに対して true を返す。
        let policy =
            SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("full-access を作れない");

        let arbitrary = Path::new("/arbitrary/path");
        assert!(
            policy.contains(arbitrary),
            "full-access should contain any path"
        );
    }

    #[test]
    fn contains_read_only_always_false() {
        // read-only は任意のパスに対して false を返す。
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("read-only を作れない");

        let arbitrary = Path::new("/arbitrary/path");
        assert!(
            !policy.contains(arbitrary),
            "read-only should contain no paths"
        );
    }

    #[test]
    fn contains_resists_string_prefix_confusion() {
        // Path::starts_with はパス成分で比較する。<root>/work と <root>/work-evil は異なる。
        // 単純な文字列比較に「簡略化」されるのを防ぐためのテスト。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let root_path = root.path().canonicalize().expect("canonicalize");

        // work ディレクトリを実際に作る
        let work = root_path.join("work");
        std::fs::create_dir(&work).expect("create work dir");
        let work = work.canonicalize().expect("canonicalize work");

        // work-evil はルート下に存在させるが、ルートのパスにはしない
        let work_evil = root_path.join("work-evil");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&work))
            .expect("方針を作れない");

        // work 以下は含まれる
        assert!(policy.contains(&work), "work should be contained");
        let work_child = work.join("file.txt");
        assert!(
            policy.contains(&work_child),
            "work/child should be contained"
        );

        // work-evil は含まれない
        assert!(
            !policy.contains(&work_evil),
            "work-evil should not be contained (string prefix confusion)"
        );
    }
}
