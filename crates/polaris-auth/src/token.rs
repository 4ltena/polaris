//! `/oauth/token` への交換と更新。
//!
//! `issuer` を引数で受けるのは、テストが偽のサーバを指せるようにするため
//! である。実 API を叩くテストは 1 本も作らない。

use serde::Deserialize;

use crate::{AuthError, CLIENT_ID, Credentials, REDIRECT_URI};

/// トークン応答。`refresh_token` と `expires_in` は返らないことがある。
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

/// 現在の Unix 秒。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// id_token（JWT）の payload から chatgpt_account_id を取り出す。
///
/// 署名は検証しない。この値は TLS の下でサーバから受け取ったものであり、
/// こちらは発行者を検証する立場に無い。壊れていたら `None` を返す。
/// panic しないことをテストで固定する。
pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = id_token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

/// `access_token`（JWT）の payload から chatgpt_plan_type を取り出す。
///
/// `account_id_from_id_token` と同じ理由で署名は検証しない。ただしこちらは
/// `id_token` ではなく `access_token` を読む — `chatgpt_plan_type` は
/// `access_token` の claim にあり、`id_token` には無い。壊れていたら
/// `None` を返し、panic しないことをテストで固定する。
pub fn plan_type_from_access_token(access_token: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = access_token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

/// フォーム POST を投げて `Credentials` を組み立てる共通部分。
/// `fallback_refresh` は、応答が refresh_token を省いたときに保つ値。
/// `fallback_account` は、応答が id_token を省いたときに保つ account_id。
async fn post_token(
    issuer: &str,
    form: &[(&str, &str)],
    fallback_refresh: Option<&str>,
    fallback_account: Option<&str>,
) -> Result<Credentials, AuthError> {
    let resp = reqwest::Client::new()
        .post(format!("{issuer}/oauth/token"))
        .form(form)
        .send()
        .await
        .map_err(|e| AuthError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AuthError::Http(format!("status {status}: {body}")));
    }

    let t: TokenResponse = resp
        .json()
        .await
        .map_err(|e| AuthError::Decode(e.to_string()))?;

    let access_token = t
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthError::Decode("応答に access_token が無い".into()))?;

    let refresh_token = t
        .refresh_token
        .filter(|s| !s.is_empty())
        .or_else(|| fallback_refresh.map(|s| s.to_string()))
        .ok_or_else(|| AuthError::Decode("応答にも手元にも refresh_token が無い".into()))?;

    // refresh_token と同じ形で、応答が省いたときは手元の値を保つ。
    // refresh_token grant は OIDC の id_token を再発行する義務を負わない
    // ため、更新応答に id_token が無いことは異常ではない。ここに fallback が
    // 無いと、動いていた account_id が更新のたびに "" へ潰れて保存され、
    // 以後 `chatgpt-account-id:` が空のまま送られる。認証の失敗が
    // モデルの失敗に見える経路そのものである。
    //
    // refresh_token と違い、最後まで値が無いことを硬い失敗にはしない。
    // refresh_token が無い資格情報は次の更新ができず回復不能だが、
    // account_id はここで空になったからといって回復不能ではなく、
    // 実バックエンドが空のヘッダを許すかどうかを試験できる場所が無い。
    // 「今日より悪くしない」側に倒し、fallback を尽くしたあとは既存どおり
    // 既定値で埋める。
    let account_id = t
        .id_token
        .as_deref()
        .and_then(account_id_from_id_token)
        .or_else(|| fallback_account.map(|s| s.to_string()))
        .unwrap_or_default();

    Ok(Credentials {
        access_token,
        refresh_token,
        account_id,
        expires_at: t.expires_in.map(|s| now_secs().saturating_add(s)),
    })
}

/// 認可コードを資格情報へ交換する。
pub async fn exchange_code(
    issuer: &str,
    code: &str,
    verifier: &str,
) -> Result<Credentials, AuthError> {
    post_token(
        issuer,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier),
        ],
        None,
        None,
    )
    .await
}

/// refresh token で更新する。
///
/// `account_id` には保管している現在の値を渡す。応答が id_token を省いた
/// ときにこれを保つ。`refresh_token` の fallback と同じ形であり、手元の値を
/// 持たない初回のログイン（`exchange_code`）は、どちらへも `None` を渡す。
pub async fn refresh(
    issuer: &str,
    refresh_token: &str,
    account_id: Option<&str>,
) -> Result<Credentials, AuthError> {
    post_token(
        issuer,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ],
        Some(refresh_token),
        account_id,
    )
    .await
}

/// account_id は id_token の payload に入る。テスト用に
/// `{"chatgpt_account_id":"acct-1"}` を base64url で包んだ JWT 風の
/// 3 分割文字列を作る。署名は検証しない（サーバから TLS で受け取った
/// ものであり、こちらが発行者を検証する立場に無い）。
///
/// `lib.rs` のテストも同じ形の id_token を使うので、モジュール直下に置いて
/// 共有する。書き写すと、片方だけ payload の鍵を直したときに黙って
/// 食い違う。
#[cfg(test)]
pub(crate) fn id_token_with_account(account: &str) -> String {
    use base64::Engine;
    let payload = serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": account }
    });
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());
    format!("header.{b}.signature")
}

#[cfg(test)]
pub(crate) fn access_token_with_plan(plan: &str) -> String {
    use base64::Engine;
    let payload = serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_plan_type": plan }
    });
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());
    format!("header.{b}.signature")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server_returning(body: serde_json::Value) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&s)
            .await;
        s
    }

    #[tokio::test]
    async fn exchange_returns_credentials_with_an_expiry() {
        let s = server_returning(serde_json::json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": id_token_with_account("acct-1"),
            "expires_in": 3600
        }))
        .await;

        let c = exchange_code(&s.uri(), "the-code", "the-verifier")
            .await
            .expect("交換できるべき");
        assert_eq!(c.access_token, "at");
        assert_eq!(c.refresh_token, "rt");
        assert_eq!(c.account_id, "acct-1");
        assert!(c.expires_at.is_some(), "expires_in があるのに期限が無い");
    }

    /// `expires_in` が無い応答は「期限不明」であり、`None` になる。
    /// ここを現在時刻や 0 で埋めると、不明と失効の区別が消える。
    #[tokio::test]
    async fn a_response_without_expires_in_has_an_unknown_expiry() {
        let s = server_returning(serde_json::json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": id_token_with_account("acct-1")
        }))
        .await;

        let c = exchange_code(&s.uri(), "c", "v")
            .await
            .expect("交換できるべき");
        assert_eq!(c.expires_at, None, "期限不明が None になっていない");
    }

    /// refresh token を返さない更新応答では、渡した既存の値を保つ。
    /// ここを空にすると、次回の更新ができなくなる。
    #[tokio::test]
    async fn refresh_keeps_the_old_refresh_token_when_the_response_omits_it() {
        let s = server_returning(serde_json::json!({
            "access_token": "new-at",
            "id_token": id_token_with_account("acct-1"),
            "expires_in": 3600
        }))
        .await;

        let c = refresh(&s.uri(), "old-rt", None)
            .await
            .expect("更新できるべき");
        assert_eq!(c.access_token, "new-at");
        assert_eq!(
            c.refresh_token, "old-rt",
            "既存の refresh token が捨てられた"
        );
    }

    /// 応答が新しい refresh token を返したら、そちらを使う。上の対。
    /// 片方だけでは「常に古い方を返す」実装が通ってしまう。
    #[tokio::test]
    async fn refresh_adopts_a_rotated_refresh_token() {
        let s = server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "rotated-rt",
            "id_token": id_token_with_account("acct-1"),
            "expires_in": 3600
        }))
        .await;

        let c = refresh(&s.uri(), "old-rt", None)
            .await
            .expect("更新できるべき");
        assert_eq!(
            c.refresh_token, "rotated-rt",
            "回転した refresh token を採っていない"
        );
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_http_error() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad_grant"))
            .mount(&s)
            .await;

        let err = refresh(&s.uri(), "rt", None).await.expect_err("失敗すべき");
        assert!(
            matches!(err, AuthError::Http(_)),
            "Http 以外になっている: {err:?}"
        );
    }

    /// access_token が無い応答は成功ではない。空の資格情報を返すと、
    /// 次の API 呼び出しが 401 になり、原因が認証まで遡れなくなる。
    #[tokio::test]
    async fn a_response_without_an_access_token_is_a_decode_error() {
        let s = server_returning(serde_json::json!({ "expires_in": 3600 })).await;
        let err = refresh(&s.uri(), "rt", None).await.expect_err("失敗すべき");
        assert!(
            matches!(err, AuthError::Decode(_)),
            "Decode 以外になっている: {err:?}"
        );
    }

    #[test]
    fn the_account_id_comes_out_of_the_id_token_payload() {
        let got =
            account_id_from_id_token(&id_token_with_account("acct-xyz")).expect("取り出せるべき");
        assert_eq!(got, "acct-xyz");
    }

    #[test]
    fn a_malformed_id_token_does_not_panic() {
        assert!(account_id_from_id_token("not-a-jwt").is_none());
        assert!(account_id_from_id_token("a.!!!.c").is_none());
        assert!(account_id_from_id_token("").is_none());
    }

    #[test]
    fn the_plan_type_comes_out_of_the_access_token_payload() {
        let got =
            plan_type_from_access_token(&access_token_with_plan("plus")).expect("取り出せるべき");
        assert_eq!(got, "plus");
    }

    #[test]
    fn a_malformed_access_token_does_not_panic() {
        assert!(plan_type_from_access_token("not-a-jwt").is_none());
        assert!(plan_type_from_access_token("a.!!!.c").is_none());
        assert!(plan_type_from_access_token("").is_none());
    }
}
