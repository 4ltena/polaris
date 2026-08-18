//! ヘルパ用バイナリを書込可能ルートの外へ退避する。
//!
//! `current_exe()` は通常ビルド生成物の中、すなわちワークスペースの内側に
//! ある。ワークスペースへ書ける者がそれを差し替えれば、次の変更操作が
//! 差し替えられたコードを拘束下で実行する。M4 の `read write` 型 subagent に
//! とって、これは型が与えていない権限を得る経路そのものになる。

use std::path::{Path, PathBuf};

use crate::SandboxError;
use crate::policy::SandboxPolicy;

/// 実行中のバイナリを退避したうえでその場所を返す。
pub fn staged_helper(policy: &SandboxPolicy, state_dir: &Path) -> Result<PathBuf, SandboxError> {
    let exe = std::env::current_exe()?;
    staged_helper_from(policy, state_dir, &exe)
}

/// テストから実体を差し替えられるようにした本体。
pub fn staged_helper_from(
    policy: &SandboxPolicy,
    state_dir: &Path,
    exe: &Path,
) -> Result<PathBuf, SandboxError> {
    let canonical = exe.canonicalize()?;

    let inside_writable = policy
        .writable_roots()
        .iter()
        .any(|r| canonical.starts_with(r));
    if !inside_writable {
        return Ok(canonical);
    }

    std::fs::create_dir_all(state_dir)?;
    let dest = state_dir.join("polaris-helper");

    // 内容が変わっていれば必ず複製し直す。古い複製を使い続けると、
    // 直したはずのヘルパが動かないうえ、症状が「直っていない」なので
    // 原因が見えにくい。サイズと更新時刻ではなく中身で比べる。
    let need_copy = match std::fs::read(&dest) {
        Ok(existing) => existing != std::fs::read(&canonical)?,
        Err(_) => true,
    };
    if need_copy {
        std::fs::copy(&canonical, &dest)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    Ok(dest.canonicalize()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    #[test]
    fn a_helper_inside_a_writable_root_is_copied_out_of_it() {
        // ここが M4 の権限昇格を塞ぐ。ワークスペースへ書ける者がヘルパを
        // 差し替えられる状態を残さない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let state = tempfile::tempdir().expect("一時ディレクトリ");

        let fake_exe = root.path().join("polaris");
        std::fs::write(&fake_exe, b"#!/bin/sh\nexit 0\n").expect("書けない");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let staged = staged_helper_from(&policy, state.path(), &fake_exe).expect("退避できない");

        assert!(
            !staged.starts_with(policy.writable_roots()[0].as_path()),
            "退避先が書込可能ルートの内側にある: {}",
            staged.display()
        );
        assert!(staged.exists(), "退避先にファイルが無い");
    }

    #[test]
    fn a_helper_already_outside_every_root_is_used_as_is() {
        // 不要な複製をしない。インストール済みのバイナリを毎回コピーする
        // 必要は無い。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let elsewhere = tempfile::tempdir().expect("一時ディレクトリ");
        let state = tempfile::tempdir().expect("一時ディレクトリ");

        let exe = elsewhere.path().join("polaris");
        std::fs::write(&exe, b"x").expect("書けない");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let staged = staged_helper_from(&policy, state.path(), &exe).expect("解決できない");
        assert_eq!(staged, exe.canonicalize().expect("canonicalize"));
    }

    #[test]
    fn a_stale_staged_copy_is_refreshed_when_the_source_changes() {
        // 内容が変わったのに古い複製を使い続けると、直したはずのヘルパが
        // 動かない。しかも症状は「直っていない」であり、原因が見えにくい。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let state = tempfile::tempdir().expect("一時ディレクトリ");
        let exe = root.path().join("polaris");

        std::fs::write(&exe, b"version-1").expect("書けない");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");
        let first = staged_helper_from(&policy, state.path(), &exe).expect("退避");
        assert_eq!(std::fs::read(&first).expect("読めない"), b"version-1");

        std::fs::write(&exe, b"version-2").expect("書けない");
        let second = staged_helper_from(&policy, state.path(), &exe).expect("退避");
        assert_eq!(
            std::fs::read(&second).expect("読めない"),
            b"version-2",
            "古い複製が使われている"
        );
    }
}
