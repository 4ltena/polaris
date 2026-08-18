//! 資格情報の保管。`~/.polaris/auth.json`、0600、原子的書き込み。
//!
//! `~/.codex/auth.json` は読まない。コピーもしない。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::{AuthError, Credentials};

/// 既定の保管先。`~/.polaris/auth.json`。
pub fn default_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AuthError::Io(std::io::Error::other("HOME が設定されていない")))?;
    Ok(Path::new(&home).join(".polaris").join("auth.json"))
}

/// 保存する。一時ファイルへ書いてから 0600 にして rename する。rename は
/// 同一ディレクトリ内で原子的なので、途中で落ちても本体が半端な内容に
/// 置き換わることがない。
pub fn save_to(path: &Path, c: &Credentials) -> Result<(), AuthError> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(c)
        .map_err(|e| AuthError::Decode(format!("資格情報を直列化できない: {e}")))?;

    // 既存の一時ファイルが残っている場合に備えて truncate する。open 時の
    // mode は新規作成のときにしか効かず、この truncate 経路で既存ファイル
    // を開いた場合には無力なので、パーミッションは open の引数に頼らず
    // 常に set_permissions で明示する。これが 0600 を保証する唯一の経路
    // であり、単一責任にしておくことで「もう一方があるから消してよい」と
    // いう誤読を防ぐ。
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    drop(f);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 読む。存在しないことは失敗ではない。壊れていることは失敗である。
pub fn load_from(path: &Path) -> Result<Option<Credentials>, AuthError> {
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AuthError::Io(e)),
    };
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| AuthError::Decode(format!("{} を解釈できない: {e}", path.display())))
}

/// 削除する。戻り値は「実際にファイルがあったか」。
pub fn delete_at(path: &Path) -> Result<bool, AuthError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AuthError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Credentials {
        Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: "acct".into(),
            expires_at: Some(1_800_000_000),
        }
    }

    #[test]
    fn a_saved_credential_round_trips() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("保存できない");
        let got = load_from(&p).expect("読めない").expect("無い");
        assert_eq!(got, sample());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let got = load_from(&dir.path().join("nope.json")).expect("存在しないことは失敗ではない");
        assert!(got.is_none());
    }

    /// 資格情報のファイルは所有者だけが読める。他のプロセスから読めては
    /// ならない。
    #[test]
    fn the_saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("保存できない");
        let mode = std::fs::metadata(&p)
            .expect("メタデータ")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "パーミッションが 0600 でない: {:o}",
            mode & 0o777
        );
    }

    /// 上書き保存でもパーミッションが緩まない。1 回目で 0600 になっても、
    /// 2 回目が既定の 0644 で作り直せば穴が開く。
    #[test]
    fn overwriting_keeps_the_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("保存できない");
        let mut second = sample();
        second.access_token = "at2".into();
        save_to(&p, &second).expect("保存できない");
        let mode = std::fs::metadata(&p)
            .expect("メタデータ")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "上書きでパーミッションが緩んだ: {:o}",
            mode & 0o777
        );
        assert_eq!(
            load_from(&p).expect("読めない").expect("無い").access_token,
            "at2"
        );
    }

    /// 一時ファイルが既に緩いパーミッションで残っている場合（前回の
    /// 書き込みが rename 前に中断したなど）、open 時の mode は既存ファイル
    /// を開くだけでは効かない。それでも set_permissions が unconditionally
    /// 効くので、本体は 0600 になる。
    #[test]
    fn a_preexisting_loose_temp_file_is_still_corrected_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, b"leftover").expect("書けない");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
            .expect("パーミッションを設定できない");

        save_to(&p, &sample()).expect("保存できない");

        let mode = std::fs::metadata(&p)
            .expect("メタデータ")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "使い回した一時ファイルの緩いパーミッションが残った: {:o}",
            mode & 0o777
        );
    }

    /// 書き込みは一時ファイルへ書いてから rename する。rename の前に
    /// 落ちても旧ファイルは無傷である。ここでは「一時ファイルが残っていても
    /// 本体は旧内容のまま読める」ことで、書き込み先が本体でないことを見る。
    #[test]
    fn a_leftover_temp_file_does_not_disturb_the_stored_credentials() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("保存できない");

        // 中断された書き込みの痕跡を模す。
        std::fs::write(dir.path().join("auth.json.tmp"), b"half-written").expect("書けない");

        let got = load_from(&p).expect("読めない").expect("無い");
        assert_eq!(got, sample(), "本体が一時ファイルに汚染されている");
    }

    #[test]
    fn delete_reports_whether_a_file_was_there() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        assert!(
            !delete_at(&p).expect("削除で失敗しない"),
            "無いのに消したと言った"
        );
        save_to(&p, &sample()).expect("保存できない");
        assert!(
            delete_at(&p).expect("削除できない"),
            "あったのに消していないと言った"
        );
        assert!(!p.exists());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_silent_absence() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        std::fs::write(&p, b"{ not json").expect("書けない");
        let err = load_from(&p).expect_err("壊れたファイルは失敗であるべき");
        assert!(
            matches!(err, AuthError::Decode(_)),
            "壊れたファイルが Decode 以外になっている: {err:?}"
        );
    }
}
