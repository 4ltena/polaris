//! Building the authorization URL, and receiving the callback exactly once.
//!
//! Port 1455 is the port of the redirect_uri registered to the client_id,
//! and it cannot be chosen freely. So this cannot run at the same time as
//! `codex login`. When it's occupied, we name that collision instead of a
//! generic bind error. A denial message whose cause can't be grasped
//! invites the same failure to repeat.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{
    AuthError, CALLBACK_PORT, CLIENT_ID, Credentials, REDIRECT_URI, SCOPE, pkce, store, token,
};

/// Builds the authorization URL to open in the browser.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let mut u = url::Url::parse(crate::ISSUER).expect("ISSUER is malformed as a URL");
    u.set_path("/oauth/authorize");
    u.query_pairs_mut()
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    u.to_string()
}

/// Extracts `code` and `state` from the HTTP request line.
///
/// The target is the second whitespace-delimited field of
/// `GET /auth/callback?code=…&state=… HTTP/1.1`. Since `Url::parse` can't
/// take a bare relative path, we join it onto an arbitrary base and read the
/// query from that. The base is used only for parsing and never goes
/// anywhere external.
pub fn parse_callback(request_line: &str) -> Result<(String, String), AuthError> {
    let target = request_line.split_whitespace().nth(1).ok_or_else(|| {
        AuthError::Decode(format!("could not read the request line: {request_line}"))
    })?;

    let parsed = url::Url::parse("http://localhost")
        .expect("base is malformed")
        .join(target)
        .map_err(|e| AuthError::Decode(format!("could not parse the callback URL: {e}")))?;

    let q: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();

    if let Some(e) = q.get("error") {
        let desc = q.get("error_description").map(String::as_str).unwrap_or("");
        return Err(AuthError::Denied(format!("{e} {desc}").trim().to_string()));
    }

    let code = q
        .get("code")
        .cloned()
        .ok_or_else(|| AuthError::Decode("callback has no code".into()))?;
    let state = q.get("state").cloned().unwrap_or_default();
    Ok((code, state))
}

/// Grabs the registered port. If it's occupied, names the collision.
pub async fn bind_callback() -> Result<TcpListener, AuthError> {
    TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| AuthError::PortInUse(e.to_string()))
}

/// Receives the callback exactly once and returns `code`.
///
/// Cut off with `timeout` so we don't wait forever. Returns a short body to
/// the browser. With an empty response, the user would return to the
/// terminal with no idea whether it succeeded.
pub async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, AuthError> {
    let accepted = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| AuthError::Denied("timed out waiting for the callback".into()))?;
    let (mut stream, _) = accepted?;

    // Reading just the request line is enough. No need to wait for the end
    // of headers.
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).await?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let request_line = head.lines().next().unwrap_or_default().to_string();

    let result = parse_callback(&request_line).and_then(|(code, state)| {
        if state != expected_state {
            Err(AuthError::Denied(
                "state doesn't match. May have received the response to a different authorization"
                    .into(),
            ))
        } else {
            Ok(code)
        }
    });

    let (status, body) = match &result {
        Ok(_) => (
            "200 OK",
            "polaris login is complete. Return to the terminal.",
        ),
        Err(_) => (
            "400 Bad Request",
            "polaris login failed. Check the terminal.",
        ),
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;

    result
}

/// Opens the browser. Not opening isn't fatal. Prints the URL and continues.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";

    let _ = std::process::Command::new(program).arg(url).spawn();
}

/// The full login sequence. Binds before opening the browser. In the
/// reverse order, by the time the user finishes authorizing, we might not
/// yet be listening, and miss the callback.
pub async fn run(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    let p = pkce::generate()?;
    let state = pkce::random_urlsafe(16)?;

    let listener = bind_callback().await?;
    let url = authorize_url(&p.challenge, &state);
    eprintln!("Authorize in your browser: {url}");
    open_browser(&url);

    let code = wait_for_callback(listener, &state, Duration::from_secs(300)).await?;
    let creds = token::exchange_code(issuer, &code, &p.verifier).await?;
    store::save_to(store_path, &creds)?;
    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_carries_every_parameter_the_flow_needs() {
        let u = authorize_url("the-challenge", "the-state");
        let parsed = url::Url::parse(&u).expect("malformed as a URL");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.host_str(), Some("auth.openai.com"));
        assert_eq!(parsed.path(), "/oauth/authorize");
        assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URI)
        );
        assert_eq!(q.get("scope").map(String::as_str), Some(SCOPE));
        assert_eq!(
            q.get("code_challenge").map(String::as_str),
            Some("the-challenge")
        );
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(q.get("state").map(String::as_str), Some("the-state"));
    }

    #[test]
    fn the_callback_query_is_parsed_into_code_and_state() {
        let (code, state) =
            parse_callback("GET /auth/callback?code=abc&state=xyz HTTP/1.1").expect("should parse");
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    /// A percent-encoded value gets decoded. Passing it raw into the
    /// exchange would produce an invalid_grant on the server side, with no
    /// way to tell encoding was the cause.
    #[test]
    fn percent_encoded_values_are_decoded() {
        let (code, _) = parse_callback("GET /auth/callback?code=a%2Fb%2Bc&state=s HTTP/1.1")
            .expect("should parse");
        assert_eq!(code, "a/b+c");
    }

    /// When authorization is denied, `error` comes back. Treating this as
    /// "there's no code" leaves the user with no idea what happened.
    #[test]
    fn an_error_response_is_surfaced_with_its_reason() {
        let err = parse_callback("GET /auth/callback?error=access_denied HTTP/1.1")
            .expect_err("should fail");
        let AuthError::Denied(msg) = err else {
            panic!("got something other than Denied: {err:?}");
        };
        assert!(
            msg.contains("access_denied"),
            "reason is missing from the text: {msg}"
        );
    }

    #[test]
    fn a_request_line_without_a_code_is_an_error() {
        assert!(parse_callback("GET /auth/callback HTTP/1.1").is_err());
        assert!(parse_callback("garbage").is_err());
    }

    /// A callback with a mismatched state is not accepted. This is a CSRF
    /// defense; removing it would let an attacker-planted code get accepted.
    #[tokio::test]
    async fn a_state_mismatch_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
            s.write_all(b"GET /auth/callback?code=c&state=WRONG HTTP/1.1\r\n\r\n")
                .await
                .expect("write");
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
        });

        let err = wait_for_callback(listener, "EXPECTED", Duration::from_secs(5))
            .await
            .expect_err("a state mismatch should fail");
        assert!(
            matches!(err, AuthError::Denied(_)),
            "other than Denied: {err:?}"
        );
        client.await.expect("client");
    }

    /// The positive counterpart. If state matches, returns the code.
    /// Without this, an implementation that "always denies" would pass the
    /// test above.
    #[tokio::test]
    async fn a_matching_state_yields_the_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
            s.write_all(b"GET /auth/callback?code=the-code&state=EXPECTED HTTP/1.1\r\n\r\n")
                .await
                .expect("write");
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
            // Something must be shown in the browser. With an empty
            // response the user has no way to know whether it succeeded.
            String::from_utf8_lossy(&buf).to_string()
        });

        let code = wait_for_callback(listener, "EXPECTED", Duration::from_secs(5))
            .await
            .expect("should be receivable");
        assert_eq!(code, "the-code");

        let body = client.await.expect("client");
        assert!(
            body.starts_with("HTTP/1.1 200"),
            "didn't return 200: {body}"
        );
        assert!(!body.trim().is_empty(), "returned nothing to the browser");
    }

    #[tokio::test]
    async fn waiting_times_out_instead_of_hanging_forever() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let err = wait_for_callback(listener, "S", Duration::from_millis(50))
            .await
            .expect_err("should time out");
        assert!(
            matches!(err, AuthError::Denied(_) | AuthError::Io(_)),
            "unexpected variant: {err:?}"
        );
    }

    /// When 1455 is occupied, names the collision instead of a generic bind
    /// error.
    #[tokio::test]
    async fn a_busy_callback_port_names_the_collision() {
        let Ok(_held) = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await else {
            // Some other process may already hold it in this environment,
            // in which case this test's premise doesn't hold. Not being
            // able to grab it isn't itself abnormal, so skip it.
            return;
        };
        let err = bind_callback()
            .await
            .expect_err("should fail since it's occupied");
        let AuthError::PortInUse(_) = err else {
            panic!("got something other than PortInUse: {err:?}");
        };
        // The text must mention codex login. `Display` is built from
        // `AuthError`'s attribute.
        let shown = err.to_string();
        assert!(
            shown.contains("codex login"),
            "doesn't name the other party in the collision: {shown}"
        );
        assert!(
            shown.contains(&CALLBACK_PORT.to_string()),
            "port number is missing from the text: {shown}"
        );
    }
}
