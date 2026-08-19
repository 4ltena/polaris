//! Exchange and refresh against `/oauth/token`.
//!
//! `issuer` is taken as an argument so tests can point at a fake server.
//! We never write a single test that hits the real API.

use serde::Deserialize;

use crate::{AuthError, CLIENT_ID, Credentials, REDIRECT_URI};

/// The token response. `refresh_token` and `expires_in` may not come back.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

/// The current Unix seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Extracts chatgpt_account_id from the id_token (JWT) payload.
///
/// Doesn't verify the signature. This value was received from the server
/// under TLS, and we're not in a position to verify the issuer. Returns
/// `None` if malformed. We pin down not panicking with a test.
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

/// Extracts chatgpt_plan_type from the `access_token` (JWT) payload.
///
/// Doesn't verify the signature, for the same reason as
/// `account_id_from_id_token`. This one reads `access_token`, not
/// `id_token`, though — `chatgpt_plan_type` is a claim on `access_token`,
/// not on `id_token`. Returns `None` if malformed, and we pin down not
/// panicking with a test.
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

/// The shared part that sends a form POST and assembles `Credentials`.
/// `fallback_refresh` is the value kept when the response omits
/// refresh_token. `fallback_account` is the account_id kept when the
/// response omits id_token.
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
        .ok_or_else(|| AuthError::Decode("the response has no access_token".into()))?;

    let refresh_token = t
        .refresh_token
        .filter(|s| !s.is_empty())
        .or_else(|| fallback_refresh.map(|s| s.to_string()))
        .ok_or_else(|| {
            AuthError::Decode(
                "there is no refresh_token in either the response or what we hold".into(),
            )
        })?;

    // In the same shape as refresh_token: when the response omits it, keep
    // the value we hold. A refresh_token grant carries no obligation to
    // reissue an OIDC id_token, so a refresh response lacking id_token is
    // not abnormal. Without a fallback here, a working account_id would
    // collapse to "" and get saved on every refresh, and
    // `chatgpt-account-id:` would go out empty from then on. This is
    // exactly the path where an auth failure looks like a model failure.
    //
    // Unlike refresh_token, we don't turn its being missing all the way
    // through into a hard failure. Credentials without a refresh_token
    // can't do the next refresh and are unrecoverable, but account_id going
    // empty here isn't unrecoverable in the same way, and we have no place
    // to test whether the real backend tolerates an empty header. We lean
    // toward "don't make it worse than today," and after exhausting the
    // fallback, fill it with the existing default as before.
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

/// Exchanges an authorization code for credentials.
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

/// Refreshes with the refresh token.
///
/// Pass the current value we hold as `account_id`. This is what gets kept
/// when the response omits id_token. It's the same shape as the
/// `refresh_token` fallback, and the initial login (`exchange_code`), which
/// holds no value of its own yet, passes `None` to both.
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

/// account_id lives in the id_token payload. For tests, builds a
/// JWT-shaped, 3-part string wrapping `{"chatgpt_account_id":"acct-1"}` in
/// base64url. Doesn't verify the signature (it was received from the server
/// under TLS, and we're not in a position to verify the issuer).
///
/// `lib.rs`'s tests use an id_token of the same shape, so this is placed at
/// the module root and shared. Copying it would let the two silently
/// diverge if only one side's payload key ever got fixed.
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
            .expect("should be able to exchange");
        assert_eq!(c.access_token, "at");
        assert_eq!(c.refresh_token, "rt");
        assert_eq!(c.account_id, "acct-1");
        assert!(
            c.expires_at.is_some(),
            "expires_in is present but there's no expiry"
        );
    }

    /// A response without `expires_in` has an "unknown expiry" and becomes
    /// `None`. Filling this with the current time or 0 would erase the
    /// distinction between unknown and expired.
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
            .expect("should be able to exchange");
        assert_eq!(c.expires_at, None, "an unknown expiry didn't become None");
    }

    /// A refresh response that doesn't return a refresh token keeps the
    /// existing value we passed in. Emptying this out would make the next
    /// refresh impossible.
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
            .expect("should be able to refresh");
        assert_eq!(c.access_token, "new-at");
        assert_eq!(
            c.refresh_token, "old-rt",
            "the existing refresh token was discarded"
        );
    }

    /// If the response returns a new refresh token, use that. The
    /// counterpart to the case above. With only one of the two, an
    /// implementation that "always returns the old one" would pass.
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
            .expect("should be able to refresh");
        assert_eq!(
            c.refresh_token, "rotated-rt",
            "didn't adopt the rotated refresh token"
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

        let err = refresh(&s.uri(), "rt", None)
            .await
            .expect_err("should fail");
        assert!(
            matches!(err, AuthError::Http(_)),
            "got something other than Http: {err:?}"
        );
    }

    /// A response without access_token is not a success. Returning empty
    /// credentials would make the next API call a 401, with the cause no
    /// longer traceable back to auth.
    #[tokio::test]
    async fn a_response_without_an_access_token_is_a_decode_error() {
        let s = server_returning(serde_json::json!({ "expires_in": 3600 })).await;
        let err = refresh(&s.uri(), "rt", None)
            .await
            .expect_err("should fail");
        assert!(
            matches!(err, AuthError::Decode(_)),
            "got something other than Decode: {err:?}"
        );
    }

    #[test]
    fn the_account_id_comes_out_of_the_id_token_payload() {
        let got = account_id_from_id_token(&id_token_with_account("acct-xyz"))
            .expect("should be extractable");
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
        let got = plan_type_from_access_token(&access_token_with_plan("plus"))
            .expect("should be extractable");
        assert_eq!(got, "plus");
    }

    #[test]
    fn a_malformed_access_token_does_not_panic() {
        assert!(plan_type_from_access_token("not-a-jwt").is_none());
        assert!(plan_type_from_access_token("a.!!!.c").is_none());
        assert!(plan_type_from_access_token("").is_none());
    }
}
