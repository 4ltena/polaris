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

/// 保存する。一時ファイルを 0600 で新規作成し（既存の tmp を開き直す場合は
/// set_permissions で締める）、書いてから rename する。rename は同一
/// ディレクトリ内で原子的なので、途中で落ちても本体が半端な内容に
/// 置き換わることがない。
pub fn save_to(path: &Path, c: &Credentials) -> Result<(), AuthError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(c)
        .map_err(|e| AuthError::Decode(format!("資格情報を直列化できない: {e}")))?;

    // 0600 は open 時の mode と set_permissions の二重で保証する。これは
    // 冗長ではない。両者は同じ後置条件へ別ルートで到達しているのではなく、
    // それぞれ別の経路だけを担っている: mode は「tmp を新規作成する」経路
    // （通常の save_to 呼び出しはほぼ毎回ここを通る）を、set_permissions は
    // 「前回の書き込みが rename 前に中断し、緩いパーミッションの tmp が
    // 残っていてそれを開き直す」経路を担う。
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        // mode(0o600) はここが担う新規作成経路でのみ意味を持つ。open() が
        // ファイルを新規作成する瞬間にモードをファイル生成へ埋め込むため、
        // 生成からこの後の set_permissions が効くまでの間、ファイルが
        // umask 既定（この環境では 0644）で存在する window は一切生じない。
        // write_all/sync_all/drop の後で権限を締める set_permissions では、
        // 生成の瞬間から締めるまでのこの window 自体を閉じることはできない
        // — ここは平文の OAuth access_token/refresh_token を書き込む対象
        // であり、この window を残さないことに意味がある。
        //
        // ただしこの window は他プロセスからの並行アクセスでしか観測でき
        // ない性質であり、単一スレッド・逐次実行のこのファイル内のテスト
        // では、この mode(0o600) を消しても検出できない（個別ミューテー
        // ション再検証で確認済み）。これは tmp+rename の原子性がテストで
        // 観測不能なのと同種の限界で、polaris-sandbox の述語が「これは
        // 緩和であって保証ではない」と明記する書き方に倣い、ここでも
        // 「テストが無い＝忘れられた」ではなく「性質上テストできない」こと
        // を明記しておく。この行を消してよい根拠には決してならない。
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    drop(f);
    // set_permissions はこちらの経路（既存 tmp の開き直し）を担う。open 時
    // の mode は新規作成のときにしか効かず、この経路で既存ファイルを開いた
    // 場合には無力なので、set_permissions が unconditionally 効くことが
    // この経路で 0600 を保証する唯一の手段になる。
    // a_preexisting_loose_temp_file_is_still_corrected_to_owner_only で
    // 個別にテストされている。
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
