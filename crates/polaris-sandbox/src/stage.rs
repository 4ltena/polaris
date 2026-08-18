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
            // 0700 は他ユーザーからの保護であり、同一 UID で動く拘束下の
            // subagent からの保護ではない。それを止めるのは直後の検証
            // （退避先が書込可能ルートの外にあることの確定）そのものであり、
            // このモードは多層防御であって、主たる制御ではない。将来
            // 「強化」しても、ここが守っている性質は増えない。
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    let dest_canonical = dest.canonicalize()?;

    // 退避先そのものが書込可能ルートの内側（シンボリックリンク越しの解決を
    // 含む）なら、この関数の存在理由が反転する。呼び出し側が良い state_dir を
    // 渡すことに頼らず、ここで自分の出力を検証する。次の呼び出し元は
    // Task 12 の CLI 配線であり、その次は M4 の subagent スケジューラであって、
    // ここを誤ると拘束下の任意コード実行を作ることになる。
    if let Some(root) = policy
        .writable_roots()
        .iter()
        .find(|r| dest_canonical.starts_with(r))
    {
        return Err(SandboxError::NotEnforced(format!(
            "退避先 {} が書込可能ルート {} の内側にある",
            dest_canonical.display(),
            root.display()
        )));
    }

    Ok(dest_canonical)
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
    fn a_state_dir_inside_a_writable_root_is_rejected() {
        // state_dir がルートの内側なら、複製した先も内側になる。この関数の
        // 目的がそのまま反転してしまうので、呼び出し側の選択に頼らず自分で
        // 拒否する。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let state = root.path().join("state");
        let fake_exe = root.path().join("polaris");
        std::fs::write(&fake_exe, b"x").expect("書けない");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let err = staged_helper_from(&policy, &state, &fake_exe)
            .expect_err("書込可能ルート内の state_dir が通ってしまった");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn a_state_dir_reached_through_a_symlink_into_a_writable_root_is_rejected() {
        // state_dir 自身は書込可能ルートの外にあるパスでも、シンボリック
        // リンクを辿った先がルートの内側なら同じ問題になる。文字列としての
        // starts_with ではなく、canonicalize してから比較する必要がある
        // ことを固定する。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let real_state = root.path().join("state");
        std::fs::create_dir_all(&real_state).expect("作れない");

        let link_parent = tempfile::tempdir().expect("一時ディレクトリ");
        let state_link = link_parent.path().join("state-link");
        std::os::unix::fs::symlink(&real_state, &state_link).expect("symlink");

        let fake_exe = root.path().join("polaris");
        std::fs::write(&fake_exe, b"x").expect("書けない");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let err = staged_helper_from(&policy, &state_link, &fake_exe)
            .expect_err("シンボリックリンク越しの書込可能ルート内が通ってしまった");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
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
