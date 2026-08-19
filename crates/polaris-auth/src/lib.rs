//! ChatGPT のサブスクリプション認証（OAuth）の寿命と資格情報の保管。
//!
//! このクレートは polaris の他のクレートに依存しない。Responses API も
//! `Provider` トレイトも知らない。認証は時計とファイルシステムと
//! ネットワークの競合で壊れ、ワイヤ変換は形式の解釈で壊れる。同じ場所に
//! 置くと、落ちたテストがどちらの欠陥かを指さない。
//!
//! `~/.codex/` には読み書きとも一切触れない。サーバが refresh token を
//! ローテーションさせる場合、こちらが更新した瞬間に codex 側の控えが
//! 失効しうる。独立した store を持てば、この事故は原理的に起きない。

pub mod login;
pub mod pkce;
pub mod store;
pub mod token;

use std::path::Path;

/// 認可の発行者。
pub const ISSUER: &str = "https://auth.openai.com";
/// codex CLI に登録された client_id。redirect_uri もこれに紐づく。
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// 登録済みの redirect_uri。ポートは選び直せない。
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// コールバックを待ち受けるポート。`REDIRECT_URI` と必ず一致させる。
pub const CALLBACK_PORT: u16 = 1455;
/// 要求する scope。`offline_access` が無いと refresh token が返らない。
pub const SCOPE: &str = "openid profile email offline_access";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("入出力エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP エラー: {0}")]
    Http(String),
    #[error("応答を解釈できない: {0}")]
    Decode(String),
    #[error("認可が拒否された: {0}")]
    Denied(String),
    // 書式に `{CALLBACK_PORT}` と書いてはいけない。thiserror は `{NAME}` を
    // variant のフィールド名として解釈するので、定数を書くとコンパイルが
    // 通らない。ポート番号は literal で書き、`CALLBACK_PORT` と一致させる。
    #[error(
        "ポート 1455 を使えない。redirect_uri がこのポートに登録されているため選び直せない。\
         `codex login` が同時に走っていないか確かめること: {0}"
    )]
    PortInUse(String),
    #[error("ログインしていない")]
    NotLoggedIn,
}

/// 保管する資格情報。`expires_at` は Unix 秒。応答が `expires_in` を
/// 持たない場合は `None` にする。0 や現在時刻で埋めると、期限が不明で
/// あることと失効していることの区別が消える。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

/// 期限までこの秒数を切ったら更新する。1 ターンの往復が数十秒に及ぶ
/// ことがあるため、要求の途中で失効しない程度の余裕を取る。
pub const EXPIRY_MARGIN_SECS: u64 = 300;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 更新が要るか。期限が不明なら要る。
pub fn needs_refresh(c: &Credentials, now: u64) -> bool {
    match c.expires_at {
        None => true,
        Some(exp) => exp <= now.saturating_add(EXPIRY_MARGIN_SECS),
    }
}

/// 保管された資格情報を読み、必要なら更新して返す。
pub async fn ensure_fresh(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    let c = store::load_from(store_path)?.ok_or(AuthError::NotLoggedIn)?;
    if !needs_refresh(&c, now_secs()) {
        return Ok(c);
    }
    let fresh = token::refresh(issuer, &c.refresh_token, Some(&c.account_id)).await?;
    store::save_to(store_path, &fresh)?;
    Ok(fresh)
}

/// 期限に関わらず更新する。401 を受けたあとの再試行で使う。
pub async fn force_refresh(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    let c = store::load_from(store_path)?.ok_or(AuthError::NotLoggedIn)?;
    let fresh = token::refresh(issuer, &c.refresh_token, Some(&c.account_id)).await?;
    store::save_to(store_path, &fresh)?;
    Ok(fresh)
}

/// 保管を消す。戻り値は「実際にあったか」。
pub fn logout(store_path: &Path) -> Result<bool, AuthError> {
    store::delete_at(store_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(expires_at: Option<u64>) -> Credentials {
        Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: "acct".into(),
            expires_at,
        }
    }

    /// 期限が不明なら毎回更新する。楽観的に使い回すと、失効した
    /// トークンでの 401 が通常経路になり、認証の問題がモデルの問題に
    /// 見える。
    #[test]
    fn an_unknown_expiry_always_needs_refresh() {
        assert!(needs_refresh(&creds(None), 1_000));
    }

    /// 余裕の外側では更新しない。ここが常に真だと、毎ターン更新が走る。
    #[test]
    fn a_token_well_before_expiry_is_left_alone() {
        let now = 1_000_000;
        let c = creds(Some(now + EXPIRY_MARGIN_SECS + 10));
        assert!(
            !needs_refresh(&c, now),
            "余裕があるのに更新しようとしている"
        );
    }

    /// 余裕の内側では更新する。上の対。
    #[test]
    fn a_token_inside_the_margin_needs_refresh() {
        let now = 1_000_000;
        let c = creds(Some(now + EXPIRY_MARGIN_SECS - 10));
        assert!(needs_refresh(&c, now), "期限が近いのに更新しない");
    }

    #[test]
    fn an_expired_token_needs_refresh() {
        let now = 1_000_000;
        assert!(needs_refresh(&creds(Some(now - 1)), now));
    }

    #[tokio::test]
    async fn ensure_fresh_on_an_empty_store_says_not_logged_in() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let err = ensure_fresh("http://unused.invalid", &dir.path().join("auth.json"))
            .await
            .expect_err("ログインしていないので失敗すべき");
        assert!(
            matches!(err, AuthError::NotLoggedIn),
            "NotLoggedIn 以外になっている: {err:?}"
        );
    }

    /// 余裕のあるトークンは、ネットワークへ出ずにそのまま返る。issuer に
    /// 到達不能な URL を渡しているので、更新しようとすれば失敗する。
    #[tokio::test]
    async fn ensure_fresh_returns_a_valid_token_without_touching_the_network() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        store::save_to(&p, &creds(Some(now + EXPIRY_MARGIN_SECS + 3600))).expect("保存");

        let got = ensure_fresh("http://127.0.0.1:1/unreachable", &p)
            .await
            .expect("更新せずに返るべき");
        assert_eq!(got.access_token, "at");
    }

    /// 余裕のあるトークンでも、`force_refresh` は `needs_refresh` を見ずに
    /// ネットワークへ出る。issuer に到達不能な URL を渡しているので、
    /// 「そのまま返す」のではなく「到達を試みて失敗する」ことを確かめる。
    /// ここへ `ensure_fresh` のガードが紛れ込むと、この失敗が静かに
    /// 「更新せず返す」成功へ変わる。
    #[tokio::test]
    async fn force_refresh_attempts_the_network_even_when_the_token_is_fresh() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        store::save_to(&p, &creds(Some(now + EXPIRY_MARGIN_SECS + 3600))).expect("保存");

        let err = force_refresh("http://127.0.0.1:1/unreachable", &p)
            .await
            .expect_err("新鮮でもネットワークへ出て失敗するべき");
        assert!(
            matches!(err, AuthError::Http(_)),
            "Http 以外になっている: {err:?}"
        );
    }

    /// `/oauth/token` が `body` を返すだけの偽サーバ。実 API は叩かない。
    async fn token_server_returning(body: serde_json::Value) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        let s = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&s)
            .await;
        s
    }

    /// 期限の切れた資格情報を書いて、その保管先を返す。
    fn store_with_expired_credentials(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("auth.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        store::save_to(&p, &creds(Some(now.saturating_sub(1)))).expect("保存");
        p
    }

    /// 更新応答が id_token を省いても、保管していた account_id を保つ。
    ///
    /// refresh_token grant は id_token を再発行する義務を負わないので、
    /// これは例外的な応答ではない。ここで account_id が `""` へ潰れると、
    /// その空値がそのままディスクへ保存され、以後 `chatgpt-account-id:` が
    /// 空のまま送られる。失敗はモデル側の 401 として現れ、原因が認証まで
    /// 遡れなくなる。
    ///
    /// 併せて、`ensure_fresh` の成功経路そのものを固定する —— 偽サーバ相手に
    /// 更新が成功し、新しい access_token がディスクへ書き戻ることを見る。
    /// これまでこの経路は到達不能な issuer に対してしか動かしておらず、
    /// 常に失敗側だけを通っていた。
    #[tokio::test]
    async fn ensure_fresh_keeps_the_old_account_id_when_the_response_omits_the_id_token() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = store_with_expired_credentials(dir.path());
        let s = token_server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "expires_in": 3600
        }))
        .await;

        let got = ensure_fresh(&s.uri(), &p).await.expect("更新できるべき");
        assert_eq!(got.access_token, "new-at", "更新が起きていない");
        assert_eq!(
            got.account_id, "acct",
            "id_token が無い応答で account_id が捨てられた"
        );

        let saved = store::load_from(&p).expect("読める").expect("あるはず");
        assert_eq!(saved.access_token, "new-at", "更新結果が保存されていない");
        assert_eq!(
            saved.account_id, "acct",
            "空の account_id がディスクへ焼き付いた"
        );
    }

    /// 応答が別の account_id を運んできたら、そちらを採る。上の対。
    /// 片方だけでは「常に手元の値を返す」実装が通ってしまい、アカウントの
    /// 切り替わりを取りこぼす。
    #[tokio::test]
    async fn ensure_fresh_adopts_a_rotated_account_id() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = store_with_expired_credentials(dir.path());
        let s = token_server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "id_token": crate::token::id_token_with_account("acct-2"),
            "expires_in": 3600
        }))
        .await;

        let got = ensure_fresh(&s.uri(), &p).await.expect("更新できるべき");
        assert_eq!(got.account_id, "acct-2", "応答の account_id を採っていない");

        let saved = store::load_from(&p).expect("読める").expect("あるはず");
        assert_eq!(
            saved.account_id, "acct-2",
            "新しい account_id が保存されていない"
        );
    }

    #[tokio::test]
    async fn logout_removes_the_store_and_reports_it() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        store::save_to(&p, &creds(None)).expect("保存");
        assert!(logout(&p).expect("削除できる"));
        assert!(!logout(&p).expect("2 回目も失敗しない"));
    }
}
