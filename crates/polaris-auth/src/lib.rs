//! Lifecycle of ChatGPT subscription auth (OAuth) and storage of credentials.
//!
//! This crate depends on no other polaris crate. It knows nothing of the
//! Responses API or the `Provider` trait. Auth breaks on races among the
//! clock, the filesystem, and the network, while wire conversion breaks on
//! misinterpreting a format. Putting them in the same place would leave a
//! failing test unable to point at which of the two is broken.
//!
//! It never reads from or writes to `~/.codex/`. If the server rotates the
//! refresh token, the moment we refresh here, codex's own stored copy can
//! become invalid. Keeping an independent store makes this accident
//! impossible in principle.

pub mod api_key;
pub mod login;
pub mod pkce;
pub mod protection;
pub mod store;
pub mod token;

use std::{ffi::OsStr, path::Path};

/// The authorization issuer.
pub const ISSUER: &str = "https://auth.openai.com";
/// The client_id registered to the codex CLI. redirect_uri is tied to this too.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The registered redirect_uri. The port cannot be chosen freely.
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// The port that listens for the callback. Must always match `REDIRECT_URI`.
pub const CALLBACK_PORT: u16 = 1455;
/// The scope we request. Without `offline_access` no refresh token comes back.
pub const SCOPE: &str = "openid profile email offline_access";
/// When set to `1`, only credentials that can be used without refreshing are
/// accepted. This is for isolated runs whose credential store is mounted
/// read-only.
pub const AUTH_READ_ONLY_ENV: &str = "POLARIS_AUTH_READ_ONLY";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("authentication path protection is unavailable")]
    Protection,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("could not parse the response: {0}")]
    Decode(String),
    #[error("authorization was denied: {0}")]
    Denied(String),
    // Do not write `{CALLBACK_PORT}` in the format string. thiserror
    // interprets `{NAME}` as the variant's field name, so writing the
    // constant there fails to compile. Write the port number as a literal,
    // and keep it matching `CALLBACK_PORT`.
    #[error(
        "cannot use port 1455. redirect_uri is registered to this port, so it \
         cannot be chosen freely. Check whether `codex login` is running at \
         the same time: {0}"
    )]
    PortInUse(String),
    #[error("not logged in")]
    NotLoggedIn,
    #[error("authentication refresh is disabled by {AUTH_READ_ONLY_ENV}=1")]
    ReadOnly,
    #[error("{AUTH_READ_ONLY_ENV} must be unset, 0, or 1")]
    InvalidReadOnlyMode,
}

/// The credentials we store. `expires_at` is Unix seconds. When the response
/// has no `expires_in`, this is `None`. Filling it with 0 or the current
/// time would erase the distinction between an unknown expiry and an
/// expired one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

/// Refresh once fewer than this many seconds remain before expiry. A single
/// turn's round trip can run to several dozen seconds, so we keep enough
/// margin that the token doesn't expire mid-request.
pub const EXPIRY_MARGIN_SECS: u64 = 300;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a refresh is needed. If the expiry is unknown, it is.
pub fn needs_refresh(c: &Credentials, now: u64) -> bool {
    match c.expires_at {
        None => true,
        Some(exp) => exp <= now.saturating_add(EXPIRY_MARGIN_SECS),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshPolicy {
    Allow,
    ReadOnly,
}

fn refresh_policy(value: Option<&OsStr>) -> Result<RefreshPolicy, AuthError> {
    match value {
        None => Ok(RefreshPolicy::Allow),
        Some(value) if value == OsStr::new("0") => Ok(RefreshPolicy::Allow),
        Some(value) if value == OsStr::new("1") => Ok(RefreshPolicy::ReadOnly),
        Some(_) => Err(AuthError::InvalidReadOnlyMode),
    }
}

fn current_refresh_policy() -> Result<RefreshPolicy, AuthError> {
    refresh_policy(std::env::var_os(AUTH_READ_ONLY_ENV).as_deref())
}

/// Determines the `reasoning.effort` to send to the Responses API from
/// `chatgpt_plan_type`.
///
/// Pro's usage tiers (5x/20x, etc.) cannot be distinguished from `plan_type`
/// alone (it carries only categories like `plus`/`pro`/`prolite`/`team`/
/// `enterprise`). Making that distinction needs a separate rate-limit/credits
/// API, and this login scope has no path to fetch it. Since we can't
/// distinguish them, we round every pro-family value up to xhigh.
///
/// Plus gets `high` — a deliberate choice to give Plus users strong
/// reasoning by default, accepting the tradeoff that this reaches Plus's
/// tighter rate limit sooner than a lower effort would.
///
/// For a value that is neither `plus` nor `pro*` (`free`, `team`,
/// `enterprise`, etc.) or when it couldn't be fetched at all, this returns
/// `None` and defers to the server's default. That's so we never guess at
/// the behavior for a category not covered here.
pub fn effort_for_plan_type(plan_type: Option<&str>) -> Option<&'static str> {
    match plan_type {
        Some("plus") => Some("high"),
        Some(s) if s.starts_with("pro") => Some("xhigh"),
        _ => None,
    }
}

/// Determines the default model from `chatgpt_plan_type`, the same way
/// `effort_for_plan_type` determines the default reasoning effort.
///
/// Only Plus gets an override (to `gpt-5.6-terra`, paired with the `high`
/// effort above). Pro-family and every other/unmapped plan return `None`
/// and defer to the caller's own default model
/// (`polaris_provider::codex::DEFAULT_MODEL`) — there's no equivalent
/// "round up" reasoning for model choice the way there is for effort, so
/// this stays narrow rather than guessing.
pub fn model_for_plan_type(plan_type: Option<&str>) -> Option<&'static str> {
    match plan_type {
        Some("plus") => Some("gpt-5.6-terra"),
        _ => None,
    }
}

/// Reads the stored credentials and, if needed, refreshes them before
/// returning.
pub async fn ensure_fresh(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    ensure_fresh_with_policy(issuer, store_path, current_refresh_policy()?).await
}

async fn ensure_fresh_with_policy(
    issuer: &str,
    store_path: &Path,
    policy: RefreshPolicy,
) -> Result<Credentials, AuthError> {
    let c = store::load_from(store_path)?.ok_or(AuthError::NotLoggedIn)?;
    if !needs_refresh(&c, now_secs()) {
        return Ok(c);
    }
    if policy == RefreshPolicy::ReadOnly {
        return Err(AuthError::ReadOnly);
    }
    let fresh = token::refresh(issuer, &c.refresh_token, Some(&c.account_id)).await?;
    store::save_to(store_path, &fresh)?;
    Ok(fresh)
}

/// Refreshes regardless of expiry. Used for a retry after receiving a 401.
pub async fn force_refresh(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    force_refresh_with_policy(issuer, store_path, current_refresh_policy()?).await
}

async fn force_refresh_with_policy(
    issuer: &str,
    store_path: &Path,
    policy: RefreshPolicy,
) -> Result<Credentials, AuthError> {
    let c = store::load_from(store_path)?.ok_or(AuthError::NotLoggedIn)?;
    if policy == RefreshPolicy::ReadOnly {
        return Err(AuthError::ReadOnly);
    }
    let fresh = token::refresh(issuer, &c.refresh_token, Some(&c.account_id)).await?;
    store::save_to(store_path, &fresh)?;
    Ok(fresh)
}

/// Deletes the store. The return value is whether it actually existed.
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

    /// Refresh every time when the expiry is unknown. Reusing it
    /// optimistically would make a 401 from an expired token the normal
    /// path, making an auth problem look like a model problem.
    #[test]
    fn an_unknown_expiry_always_needs_refresh() {
        assert!(needs_refresh(&creds(None), 1_000));
    }

    /// Don't refresh outside the margin. If this were always true, a
    /// refresh would run every turn.
    #[test]
    fn a_token_well_before_expiry_is_left_alone() {
        let now = 1_000_000;
        let c = creds(Some(now + EXPIRY_MARGIN_SECS + 10));
        assert!(
            !needs_refresh(&c, now),
            "trying to refresh even though there's margin left"
        );
    }

    /// Refresh inside the margin. The counterpart to the case above.
    #[test]
    fn a_token_inside_the_margin_needs_refresh() {
        let now = 1_000_000;
        let c = creds(Some(now + EXPIRY_MARGIN_SECS - 10));
        assert!(
            needs_refresh(&c, now),
            "not refreshing even though expiry is close"
        );
    }

    #[test]
    fn an_expired_token_needs_refresh() {
        let now = 1_000_000;
        assert!(needs_refresh(&creds(Some(now - 1)), now));
    }

    #[test]
    fn readonly_refresh_policy_accepts_only_unset_zero_or_one() {
        assert_eq!(
            refresh_policy(None).expect("unset must preserve normal behavior"),
            RefreshPolicy::Allow
        );
        assert_eq!(
            refresh_policy(Some(std::ffi::OsStr::new("0")))
                .expect("zero must preserve normal behavior"),
            RefreshPolicy::Allow
        );
        assert_eq!(
            refresh_policy(Some(std::ffi::OsStr::new("1"))).expect("one must enable readonly"),
            RefreshPolicy::ReadOnly
        );
    }

    #[test]
    fn readonly_refresh_policy_rejects_invalid_present_values() {
        for value in ["", "2", "true", "readonly"] {
            let err = refresh_policy(Some(std::ffi::OsStr::new(value)))
                .expect_err("only zero and one are valid present values");
            assert!(
                matches!(err, AuthError::InvalidReadOnlyMode),
                "{value:?} produced an unexpected error: {err:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn readonly_refresh_policy_rejects_a_nonunicode_value() {
        use std::os::unix::ffi::OsStrExt;

        let err = refresh_policy(Some(std::ffi::OsStr::from_bytes(b"\xff")))
            .expect_err("non-Unicode values must not disable readonly mode");
        assert!(matches!(err, AuthError::InvalidReadOnlyMode));
    }

    #[test]
    fn plus_gets_a_high_effort() {
        assert_eq!(effort_for_plan_type(Some("plus")), Some("high"));
    }

    #[test]
    fn plus_gets_the_terra_model() {
        assert_eq!(model_for_plan_type(Some("plus")), Some("gpt-5.6-terra"));
    }

    #[test]
    fn pro_and_unmapped_plans_get_no_model_override() {
        // Pro-family already gets the strongest effort; overriding its
        // model too isn't part of this — it keeps whatever the caller's
        // own default model is (see DEFAULT_MODEL in polaris-provider).
        assert_eq!(model_for_plan_type(Some("pro")), None);
        assert_eq!(model_for_plan_type(Some("team")), None);
        assert_eq!(model_for_plan_type(None), None);
    }

    /// Pro's usage tiers (5x/20x) can't be distinguished from plan_type
    /// alone, so every value starting with pro rounds up to xhigh. This
    /// checks, as a pair, that not just `pro` itself but a prefix match like
    /// `prolite` is also covered — an implementation weakened to an exact
    /// match `== "pro"` would fail only this one.
    #[test]
    fn pro_and_its_variants_get_a_high_effort() {
        assert_eq!(effort_for_plan_type(Some("pro")), Some("xhigh"));
        assert_eq!(effort_for_plan_type(Some("prolite")), Some("xhigh"));
    }

    /// A value that is neither plus nor pro-family defers to the server's
    /// default instead of guessing.
    #[test]
    fn an_unmapped_plan_defers_to_the_server_default() {
        assert_eq!(effort_for_plan_type(Some("team")), None);
        assert_eq!(effort_for_plan_type(Some("free")), None);
        assert_eq!(effort_for_plan_type(None), None);
    }

    #[tokio::test]
    async fn ensure_fresh_on_an_empty_store_says_not_logged_in() {
        let dir = tempfile::tempdir().expect("temp dir");
        let err = ensure_fresh("http://unused.invalid", &dir.path().join("auth.json"))
            .await
            .expect_err("should fail since not logged in");
        assert!(
            matches!(err, AuthError::NotLoggedIn),
            "got something other than NotLoggedIn: {err:?}"
        );
    }

    /// A token with margin to spare is returned as-is, without touching the
    /// network. We pass an unreachable URL as the issuer, so trying to
    /// refresh would fail.
    #[tokio::test]
    async fn ensure_fresh_returns_a_valid_token_without_touching_the_network() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        store::save_to(&p, &creds(Some(now + EXPIRY_MARGIN_SECS + 3600))).expect("save");

        let got = ensure_fresh("http://127.0.0.1:1/unreachable", &p)
            .await
            .expect("should return without refreshing");
        assert_eq!(got.access_token, "at");
    }

    /// Even with a token that has margin to spare, `force_refresh` goes out
    /// to the network without checking `needs_refresh`. We pass an
    /// unreachable URL as the issuer, so we confirm it "attempts to reach it
    /// and fails" rather than "returns as-is." If `ensure_fresh`'s guard
    /// leaked in here, this failure would silently become a "returns
    /// without refreshing" success.
    #[tokio::test]
    async fn force_refresh_attempts_the_network_even_when_the_token_is_fresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        store::save_to(&p, &creds(Some(now + EXPIRY_MARGIN_SECS + 3600))).expect("save");

        let err = force_refresh("http://127.0.0.1:1/unreachable", &p)
            .await
            .expect_err("should go to the network and fail even when fresh");
        assert!(
            matches!(err, AuthError::Http(_)),
            "got something other than Http: {err:?}"
        );
    }

    async fn assert_no_token_request(server: &wiremock::MockServer) {
        assert!(
            server
                .received_requests()
                .await
                .expect("request log should be available")
                .is_empty(),
            "readonly authentication must stop before posting a refresh token"
        );
    }

    #[tokio::test]
    async fn readonly_ensure_fresh_rejects_an_unknown_expiry_before_refreshing_or_saving() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        let original = creds(None);
        store::save_to(&p, &original).expect("save");
        let s = token_server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "expires_in": 3600
        }))
        .await;

        let err = ensure_fresh_with_policy(&s.uri(), &p, RefreshPolicy::ReadOnly)
            .await
            .expect_err("unknown expiry requires a refresh that readonly mode forbids");
        assert!(
            matches!(err, AuthError::ReadOnly),
            "unexpected error: {err:?}"
        );
        assert_no_token_request(&s).await;
        assert_eq!(store::load_from(&p).expect("load"), Some(original));
    }

    #[tokio::test]
    async fn readonly_ensure_fresh_rejects_the_expiry_margin_before_refreshing_or_saving() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        let now = now_secs();
        let original = creds(Some(now + EXPIRY_MARGIN_SECS));
        store::save_to(&p, &original).expect("save");
        let s = token_server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "expires_in": 3600
        }))
        .await;

        let err = ensure_fresh_with_policy(&s.uri(), &p, RefreshPolicy::ReadOnly)
            .await
            .expect_err("the expiry margin requires a refresh that readonly mode forbids");
        assert!(
            matches!(err, AuthError::ReadOnly),
            "unexpected error: {err:?}"
        );
        assert_no_token_request(&s).await;
        assert_eq!(store::load_from(&p).expect("load"), Some(original));
    }

    #[tokio::test]
    async fn readonly_ensure_fresh_returns_a_fresh_credential_without_a_refresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        let original = creds(Some(now_secs() + EXPIRY_MARGIN_SECS + 3600));
        store::save_to(&p, &original).expect("save");
        let s = token_server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "expires_in": 3600
        }))
        .await;

        let got = ensure_fresh_with_policy(&s.uri(), &p, RefreshPolicy::ReadOnly)
            .await
            .expect("fresh credentials remain usable in readonly mode");
        assert_eq!(got, original);
        assert_no_token_request(&s).await;
    }

    #[tokio::test]
    async fn readonly_force_refresh_stops_before_token_post_or_store_save() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        let original = creds(Some(now_secs() + EXPIRY_MARGIN_SECS + 3600));
        store::save_to(&p, &original).expect("save");
        let s = token_server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "expires_in": 3600
        }))
        .await;

        let err = force_refresh_with_policy(&s.uri(), &p, RefreshPolicy::ReadOnly)
            .await
            .expect_err("readonly mode must block the 401 refresh path");
        assert!(
            matches!(err, AuthError::ReadOnly),
            "unexpected error: {err:?}"
        );
        assert_no_token_request(&s).await;
        assert_eq!(store::load_from(&p).expect("load"), Some(original));
    }

    /// A fake server whose `/oauth/token` just returns `body`. Never hits
    /// the real API.
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

    /// Writes expired credentials and returns their storage path.
    fn store_with_expired_credentials(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("auth.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        store::save_to(&p, &creds(Some(now.saturating_sub(1)))).expect("save");
        p
    }

    /// Even when the refresh response omits id_token, keep the stored
    /// account_id.
    ///
    /// A refresh_token grant carries no obligation to reissue an id_token,
    /// so this is not an exceptional response. If account_id collapsed to
    /// `""` here, that empty value would get saved straight to disk, and
    /// `chatgpt-account-id:` would go out empty from then on. The failure
    /// would show up as a 401 on the model side, and the cause would no
    /// longer trace back to auth.
    ///
    /// This also pins down the success path of `ensure_fresh` itself — that
    /// against a fake server the refresh succeeds and the new access_token
    /// gets written back to disk. Until now this path only ever ran against
    /// an unreachable issuer, and always went through the failure side only.
    #[tokio::test]
    async fn ensure_fresh_keeps_the_old_account_id_when_the_response_omits_the_id_token() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = store_with_expired_credentials(dir.path());
        let s = token_server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "expires_in": 3600
        }))
        .await;

        let got = ensure_fresh(&s.uri(), &p)
            .await
            .expect("should be able to refresh");
        assert_eq!(got.access_token, "new-at", "no refresh happened");
        assert_eq!(
            got.account_id, "acct",
            "account_id was discarded on a response without id_token"
        );

        let saved = store::load_from(&p)
            .expect("should be readable")
            .expect("should exist");
        assert_eq!(
            saved.access_token, "new-at",
            "the refresh result wasn't saved"
        );
        assert_eq!(
            saved.account_id, "acct",
            "an empty account_id got baked into disk"
        );
    }

    /// If the response carries a different account_id, adopt it. The
    /// counterpart to the case above. With only one of the two, an
    /// implementation that "always returns the existing value" would pass,
    /// missing an account switch.
    #[tokio::test]
    async fn ensure_fresh_adopts_a_rotated_account_id() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = store_with_expired_credentials(dir.path());
        let s = token_server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "id_token": crate::token::id_token_with_account("acct-2"),
            "expires_in": 3600
        }))
        .await;

        let got = ensure_fresh(&s.uri(), &p)
            .await
            .expect("should be able to refresh");
        assert_eq!(
            got.account_id, "acct-2",
            "didn't adopt the response's account_id"
        );

        let saved = store::load_from(&p)
            .expect("should be readable")
            .expect("should exist");
        assert_eq!(
            saved.account_id, "acct-2",
            "the new account_id wasn't saved"
        );
    }

    #[tokio::test]
    async fn logout_removes_the_store_and_reports_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        store::save_to(&p, &creds(None)).expect("save");
        assert!(logout(&p).expect("should be able to delete"));
        assert!(!logout(&p).expect("shouldn't fail the second time either"));
    }
}
