//! Explicit, read-only, single-load credentials for opt-in evaluation executables.
use polaris_provider::{ProviderError, Token, TokenSource};
use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// Credentials are loaded once, from the explicit runner-provided path, only in live mode.
/// No default credential search, refresh request, or token-file write is reachable.
pub(super) struct FixedTokens {
    pub(super) token: Token,
    pub(super) expires_at: u64,
}
#[async_trait::async_trait]
impl TokenSource for FixedTokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProviderError::Auth("clock unavailable".into()))?
            .as_secs();
        if now.saturating_add(polaris_auth::EXPIRY_MARGIN_SECS) >= self.expires_at {
            return Err(ProviderError::Auth(
                "credential expired; refresh disabled".into(),
            ));
        }
        Ok(Token {
            access_token: self.token.access_token.clone(),
            account_id: self.token.account_id.clone(),
            effort: self.token.effort.clone(),
        })
    }
    async fn refreshed(&self) -> Result<Token, ProviderError> {
        Err(ProviderError::Auth("credential refresh prohibited".into()))
    }
}

pub(super) fn tokens(path: &Path, effort: &str) -> Result<Arc<dyn TokenSource>, String> {
    if !path.is_absolute()
        || path.canonicalize().ok().as_deref() != Some(path)
        || !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
    {
        return Err("credential path must be a canonical regular file".into());
    }
    let credentials = polaris_auth::store::load_from(path)
        .map_err(|_| "explicit readonly credential unavailable")?
        .ok_or("explicit readonly credential missing")?;
    let expires_at = credentials.expires_at.ok_or("credential expiry unknown")?;
    if credentials.access_token.is_empty() || credentials.account_id.is_empty() {
        return Err("credential incomplete".into());
    }
    Ok(Arc::new(FixedTokens {
        token: Token {
            access_token: credentials.access_token,
            account_id: credentials.account_id,
            effort: Some(effort.into()),
        },
        expires_at,
    }))
}
