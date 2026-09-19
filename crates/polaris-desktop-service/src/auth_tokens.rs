//! Trusted desktop owner adapter. Paths never come from model requests.
use polaris_provider::{ProviderError, Token, TokenSource};
use std::path::PathBuf;

pub struct DesktopAuthTokens {
    store: PathBuf,
}

impl DesktopAuthTokens {
    /// Register and validate the existing store before workspace preparation.
    /// This does not refresh credentials or start login/network activity.
    pub fn from_trusted_store(store: PathBuf) -> Result<Self, ProviderError> {
        if !store.is_absolute() {
            return Err(auth_error());
        }
        polaris_auth::store::load_from(&store)
            .map_err(|_| auth_error())?
            .ok_or_else(auth_error)?;
        Ok(Self { store })
    }
}

/// A run-bound source. Every send for one run resolves through the single
/// registration fixed at acceptance; a logout or a new generation fails it
/// closed instead of reaching another account.
pub struct AccountTokens {
    bound: polaris_auth::accounts::observation::BoundTokenSource,
    /// The revision last handed out, so a 401 asks for a newer one.
    used: std::sync::Mutex<Option<u64>>,
}

impl AccountTokens {
    pub fn new(bound: polaris_auth::accounts::observation::BoundTokenSource) -> Self {
        Self {
            bound,
            used: std::sync::Mutex::new(None),
        }
    }

    fn keep(&self, token: polaris_auth::accounts::owner::AccessToken) -> Token {
        *self.used.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.token_revision);
        Token {
            access_token: token.secret().to_owned(),
            account_id: token.account_id.clone(),
            effort: None,
        }
    }
}

#[async_trait::async_trait]
impl TokenSource for AccountTokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        self.bound
            .token()
            .await
            .map(|token| self.keep(token))
            .map_err(refresh_error)
    }

    async fn refreshed(&self) -> Result<Token, ProviderError> {
        let used = *self.used.lock().unwrap_or_else(|e| e.into_inner());
        let token = match used {
            Some(revision) => self.bound.refresh_after_unauthorized(revision).await,
            None => self.bound.token().await,
        };
        token.map(|token| self.keep(token)).map_err(refresh_error)
    }
}

/// The accounts registry exists, so a run without a binding must not send.
pub struct UnboundAccountTokens;

#[async_trait::async_trait]
impl TokenSource for UnboundAccountTokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        Err(ProviderError::Auth("authentication_account_unbound".into()))
    }

    async fn refreshed(&self) -> Result<Token, ProviderError> {
        self.token().await
    }
}

fn auth_error() -> ProviderError {
    ProviderError::Auth(
        "Polarisの認証情報を確認できません。設定から接続を確認してください。".into(),
    )
}

fn refresh_error(error: polaris_auth::AuthError) -> ProviderError {
    use polaris_auth::AuthError;
    // Closed diagnostic labels only: auth errors can contain response bodies
    // and credentials. These labels are recognized by desktop_run's audit.
    let category = match error {
        AuthError::Protection => "authentication_protection",
        AuthError::Busy => "authentication_store_busy",
        AuthError::RecoveryRequired => "authentication_recovery_required",
        AuthError::Io(_) | AuthError::NotLoggedIn => "authentication_store",
        AuthError::AlreadyRegistered => "authentication_already_registered",
        AuthError::AccountLimit => "authentication_account_limit",
        AuthError::Http(message) => match message
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["status", "400"] => "authentication_http_400",
            ["status", "401"] => "authentication_http_401",
            ["status", "403"] => "authentication_http_403",
            ["status", "429"] => "authentication_http_429",
            _ => "authentication_transport_or_status",
        },
        AuthError::Decode(_) => "authentication_response_format",
        AuthError::Denied(_) => "authentication_denied",
        AuthError::ReadOnly | AuthError::InvalidReadOnlyMode => "authentication_refresh_policy",
        AuthError::PortInUse(_) => "authentication_login_port",
    };
    ProviderError::Auth(category.into())
}

fn token(credentials: polaris_auth::Credentials) -> Token {
    Token {
        access_token: credentials.access_token,
        account_id: credentials.account_id,
        // The validated run configuration supplies effort; account plan claims
        // cannot override the user's selection.
        effort: None,
    }
}

#[async_trait::async_trait]
impl TokenSource for DesktopAuthTokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        polaris_auth::ensure_fresh(polaris_auth::ISSUER, &self.store)
            .await
            .map(token)
            .map_err(refresh_error)
    }

    async fn refreshed(&self) -> Result<Token, ProviderError> {
        polaris_auth::force_refresh(polaris_auth::ISSUER, &self.store)
            .await
            .map(token)
            .map_err(refresh_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_errors_discard_private_upstream_details() {
        let private = "synthetic credential and upstream response";
        for (error, expected) in [
            (polaris_auth::AuthError::Busy, "authentication_store_busy"),
            (
                polaris_auth::AuthError::RecoveryRequired,
                "authentication_recovery_required",
            ),
            (
                polaris_auth::AuthError::Http(format!("status 400 Bad Request: {private}")),
                "authentication_http_400",
            ),
            (
                polaris_auth::AuthError::Http(private.into()),
                "authentication_transport_or_status",
            ),
            (
                polaris_auth::AuthError::Decode(private.into()),
                "authentication_response_format",
            ),
            (
                polaris_auth::AuthError::Denied(private.into()),
                "authentication_denied",
            ),
            (
                polaris_auth::AuthError::ReadOnly,
                "authentication_refresh_policy",
            ),
        ] {
            let ProviderError::Auth(message) = refresh_error(error) else {
                panic!("auth variant");
            };
            assert_eq!(message, expected);
            assert!(!message.contains(private));
        }
    }

    #[test]
    fn missing_store_is_not_created_and_errors_do_not_expose_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("missing.json");
        let error = DesktopAuthTokens::from_trusted_store(path.clone())
            .err()
            .unwrap();
        assert!(!path.exists());
        assert!(
            !error
                .to_string()
                .contains(&path.to_string_lossy().to_string())
        );
        assert!(DesktopAuthTokens::from_trusted_store("relative.json".into()).is_err());
    }

    #[test]
    fn existing_synthetic_store_is_accepted_without_refresh() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        let credentials = polaris_auth::Credentials {
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            account_id: "synthetic-account".into(),
            expires_at: Some(1),
        };
        polaris_auth::store::save_to(&path, &credentials).unwrap();
        let before = std::fs::read(&path).unwrap();
        let _adapter = DesktopAuthTokens::from_trusted_store(path.clone()).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(token(credentials).effort.is_none());
    }
}
