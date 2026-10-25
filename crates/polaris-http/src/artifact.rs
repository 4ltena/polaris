//! Explicit, credential-free artifact transfers with pinned redirect origins.
//! Size is enforced while receiving; callers must independently verify SHA-256
//! and preserve their install/consent boundary before publishing any bytes.
use std::{collections::BTreeSet, future::Future, time::Duration};

const TRANSFER_LIMIT: Duration = Duration::from_secs(3600);
const IDLE_LIMIT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 5;

/// A fixed source described by package-approved download metadata. Construction
/// is read-only; the fetch caller obtains consent before making a request.
/// This value grants no filesystem destination, authentication or installation.
pub struct ArtifactSource {
    url: reqwest::Url,
    origins: BTreeSet<String>,
    bytes: u64,
}

impl ArtifactSource {
    pub fn new(url: &str, redirect_origins: &[String], bytes: u64) -> Result<Self, Error> {
        if url.len() > 2048
            || redirect_origins.len() > 16
            || bytes == 0
            || bytes > 64 * 1024 * 1024 * 1024
        {
            return Err(Error::Policy);
        }
        let url = parse_url(url)?;
        if url.query().is_some() {
            return Err(Error::Policy);
        }
        let mut origins = BTreeSet::from([url.origin().ascii_serialization()]);
        for raw in redirect_origins {
            if raw.len() > 512 {
                return Err(Error::Policy);
            }
            let parsed = parse_url(raw)?;
            if parsed.query().is_some()
                || parsed.path() != "/"
                || parsed.origin().ascii_serialization() != *raw
            {
                return Err(Error::Policy);
            }
            origins.insert(raw.clone());
        }
        Ok(Self {
            url,
            origins,
            bytes,
        })
    }
}

/// Errors deliberately carry neither request URLs nor response bodies: a
/// provider's redirect query may contain an ephemeral signed credential.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("artifact download policy refused the source or redirect")]
    Policy,
    #[error("artifact download transport is unavailable")]
    Transport,
    #[error("artifact download returned HTTP {0}")]
    Status(u16),
    #[error("artifact download size or encoding does not match the fixed artifact")]
    Size,
    #[error("artifact download destination rejected the bytes")]
    Sink,
    #[error("artifact download was cancelled")]
    Cancelled,
    #[error("artifact download timed out")]
    Timeout,
}

fn parse_url(raw: &str) -> Result<reqwest::Url, Error> {
    if raw.len() > 8192
        || raw.contains('\\')
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::Policy);
    }
    let url = reqwest::Url::parse(raw).map_err(|_| Error::Policy)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Policy);
    }
    Ok(url)
}

fn client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client, Error> {
    builder
        .no_proxy()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .connect_timeout(Duration::from_secs(15))
        .timeout(TRANSFER_LIMIT)
        .build()
        .map_err(|_| Error::Transport)
}

/// No credentials, cookies, proxy, automatic redirects, retries or resume.
/// Only a caller-supplied sink receives bytes; this function never writes files.
pub async fn fetch(
    source: &ArtifactSource,
    sink: impl FnMut(&[u8]) -> Result<(), ()> + Send,
    cancelled: impl FnMut() -> bool + Send,
) -> Result<(), Error> {
    let client = client(crate::client_builder().map_err(|_| Error::Transport)?)?;
    fetch_with_client(&client, source, sink, cancelled).await
}

/// Credential-free transfers with mandatory rejection of compromised CA keys
/// on the actual verified path. Unlike the compatibility client, this does not
/// load process-provided CA bundles. The caller still verifies artifact digests.
pub async fn fetch_verified(
    source: &ArtifactSource,
    sink: impl FnMut(&[u8]) -> Result<(), ()> + Send,
    cancelled: impl FnMut() -> bool + Send,
) -> Result<(), Error> {
    let client = client(crate::distrust::client_builder().map_err(|_| Error::Transport)?)?;
    fetch_with_client(&client, source, sink, cancelled).await
}

async fn wait<T>(
    future: impl Future<Output = Result<T, reqwest::Error>>,
    cancelled: &mut (impl FnMut() -> bool + Send),
) -> Result<T, Error> {
    tokio::pin!(future);
    let operation = async {
        loop {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            tokio::select! {
                result = &mut future => return result.map_err(|_| Error::Transport),
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
        }
    };
    tokio::time::timeout(IDLE_LIMIT, operation)
        .await
        .map_err(|_| Error::Timeout)?
}

async fn fetch_with_client(
    client: &reqwest::Client,
    source: &ArtifactSource,
    mut sink: impl FnMut(&[u8]) -> Result<(), ()> + Send,
    mut cancelled: impl FnMut() -> bool + Send,
) -> Result<(), Error> {
    let operation = async {
        let mut url = source.url.clone();
        for redirects in 0..=MAX_REDIRECTS {
            let request = client
                .get(url.clone())
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .header(reqwest::header::CACHE_CONTROL, "no-store");
            let mut response = wait(request.send(), &mut cancelled).await?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                if redirects == MAX_REDIRECTS {
                    return Err(Error::Policy);
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| v.len() <= 8192)
                    .ok_or(Error::Policy)?;
                if location.contains('\\')
                    || location
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err(Error::Policy);
                }
                // Signed redirect queries stay transient and are never logged.
                let next = url.join(location).map_err(|_| Error::Policy)?;
                let next = parse_url(next.as_str())?;
                if !source
                    .origins
                    .contains(&next.origin().ascii_serialization())
                {
                    return Err(Error::Policy);
                }
                url = next;
                continue;
            }
            if status != reqwest::StatusCode::OK {
                return Err(Error::Status(status.as_u16()));
            }
            if response.content_length().is_some_and(|n| n != source.bytes)
                || response
                    .headers()
                    .get(reqwest::header::CONTENT_ENCODING)
                    .is_some_and(|v| v.as_bytes() != b"identity")
            {
                return Err(Error::Size);
            }
            let mut received = 0u64;
            while let Some(chunk) = wait(response.chunk(), &mut cancelled).await? {
                received = received
                    .checked_add(chunk.len() as u64)
                    .ok_or(Error::Size)?;
                if received > source.bytes {
                    return Err(Error::Size);
                }
                for part in chunk.chunks(128 * 1024) {
                    if cancelled() {
                        return Err(Error::Cancelled);
                    }
                    sink(part).map_err(|_| Error::Sink)?;
                }
            }
            if cancelled() {
                return Err(Error::Cancelled);
            }
            return if received == source.bytes {
                Ok(())
            } else {
                Err(Error::Size)
            };
        }
        Err(Error::Policy)
    };
    tokio::time::timeout(TRANSFER_LIMIT, operation)
        .await
        .map_err(|_| Error::Timeout)?
}

#[cfg(test)]
mod tests;
