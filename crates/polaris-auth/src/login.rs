//! 認可 URL の組み立てと、コールバックの一度きりの受け取り。
//!
//! ポート 1455 は client_id に登録された redirect_uri のポートであり、
//! 選び直せない。したがって `codex login` とは同時に走らない。塞がって
//! いるときは、汎用の bind エラーではなくその衝突を名指しする。原因を
//! 掴めない拒否メッセージは、同じ失敗の反復を招く。

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{
    AuthError, CALLBACK_PORT, CLIENT_ID, Credentials, REDIRECT_URI, SCOPE, pkce, store, token,
};

/// ブラウザで開く認可 URL を組み立てる。
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let mut u = url::Url::parse(crate::ISSUER).expect("ISSUER が URL として壊れている");
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

/// HTTP のリクエスト行から `code` と `state` を取り出す。
///
/// `GET /auth/callback?code=…&state=… HTTP/1.1` の 2 つ目の空白区切りが
/// 対象。相対パスのままでは `Url::parse` が使えないので、任意のベースへ
/// 接いでから query を読む。ベースは解釈のためだけに使い、外へは出ない。
pub fn parse_callback(request_line: &str) -> Result<(String, String), AuthError> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| AuthError::Decode(format!("リクエスト行を読めない: {request_line}")))?;

    let parsed = url::Url::parse("http://localhost")
        .expect("ベースが壊れている")
        .join(target)
        .map_err(|e| AuthError::Decode(format!("コールバックの URL を読めない: {e}")))?;

    let q: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();

    if let Some(e) = q.get("error") {
        let desc = q.get("error_description").map(String::as_str).unwrap_or("");
        return Err(AuthError::Denied(format!("{e} {desc}").trim().to_string()));
    }

    let code = q
        .get("code")
        .cloned()
        .ok_or_else(|| AuthError::Decode("コールバックに code が無い".into()))?;
    let state = q.get("state").cloned().unwrap_or_default();
    Ok((code, state))
}

/// 登録済みのポートを掴む。塞がっていたら衝突を名指しする。
pub async fn bind_callback() -> Result<TcpListener, AuthError> {
    TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| AuthError::PortInUse(e.to_string()))
}

/// コールバックを一度だけ受け、`code` を返す。
///
/// 待ち続けないよう `timeout` で切る。ブラウザには短い本文を返す。空の
/// 応答だと、利用者は成功したのか分からないまま端末へ戻ることになる。
pub async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, AuthError> {
    let accepted = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| AuthError::Denied("コールバックを待ち切れなかった".into()))?;
    let (mut stream, _) = accepted?;

    // リクエスト行だけ読めれば足りる。ヘッダの終端まで待つ必要は無い。
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).await?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let request_line = head.lines().next().unwrap_or_default().to_string();

    let result = parse_callback(&request_line).and_then(|(code, state)| {
        if state != expected_state {
            Err(AuthError::Denied(
                "state が一致しない。別の認可の応答を受け取った可能性がある".into(),
            ))
        } else {
            Ok(code)
        }
    });

    let (status, body) = match &result {
        Ok(_) => (
            "200 OK",
            "polaris のログインが完了しました。端末に戻ってください。",
        ),
        Err(_) => (
            "400 Bad Request",
            "polaris のログインに失敗しました。端末を確認してください。",
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

/// ブラウザを開く。開けなくても致命ではない。URL を出して続ける。
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";

    let _ = std::process::Command::new(program).arg(url).spawn();
}

/// login 一式。bind してからブラウザを開く。順序が逆だと、利用者が
/// 認可を終えた時点でこちらがまだ待ち受けておらず、取りこぼす。
pub async fn run(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    let p = pkce::generate()?;
    let state = pkce::random_urlsafe(16)?;

    let listener = bind_callback().await?;
    let url = authorize_url(&p.challenge, &state);
    eprintln!("ブラウザで認可してください: {url}");
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
        let parsed = url::Url::parse(&u).expect("URL として壊れている");
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
        let (code, state) = parse_callback("GET /auth/callback?code=abc&state=xyz HTTP/1.1")
            .expect("解釈できるべき");
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    /// パーセント符号化された値が復号される。生のまま交換へ渡すと、
    /// サーバ側で invalid_grant になり、原因が符号化だと分からない。
    #[test]
    fn percent_encoded_values_are_decoded() {
        let (code, _) = parse_callback("GET /auth/callback?code=a%2Fb%2Bc&state=s HTTP/1.1")
            .expect("解釈できるべき");
        assert_eq!(code, "a/b+c");
    }

    /// 認可が拒否されたときは `error` が返る。これを「code が無い」で
    /// 片付けると、利用者に何が起きたか伝わらない。
    #[test]
    fn an_error_response_is_surfaced_with_its_reason() {
        let err = parse_callback("GET /auth/callback?error=access_denied HTTP/1.1")
            .expect_err("失敗すべき");
        let AuthError::Denied(msg) = err else {
            panic!("Denied 以外になっている: {err:?}");
        };
        assert!(msg.contains("access_denied"), "理由が文面に無い: {msg}");
    }

    #[test]
    fn a_request_line_without_a_code_is_an_error() {
        assert!(parse_callback("GET /auth/callback HTTP/1.1").is_err());
        assert!(parse_callback("garbage").is_err());
    }

    /// state が一致しないコールバックは受け取らない。CSRF の対策であり、
    /// ここを外すと第三者が仕込んだ code を掴まされうる。
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
            .expect_err("state 不一致は失敗すべき");
        assert!(matches!(err, AuthError::Denied(_)), "Denied 以外: {err:?}");
        client.await.expect("client");
    }

    /// 対になる肯定側。state が一致すれば code を返す。これが無いと、
    /// 「常に拒否する」実装が上のテストを通ってしまう。
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
            // ブラウザに何か表示されること。空の応答だと利用者は
            // 成功したのか分からない。
            String::from_utf8_lossy(&buf).to_string()
        });

        let code = wait_for_callback(listener, "EXPECTED", Duration::from_secs(5))
            .await
            .expect("受け取れるべき");
        assert_eq!(code, "the-code");

        let body = client.await.expect("client");
        assert!(
            body.starts_with("HTTP/1.1 200"),
            "200 を返していない: {body}"
        );
        assert!(!body.trim().is_empty(), "ブラウザへ何も返していない");
    }

    #[tokio::test]
    async fn waiting_times_out_instead_of_hanging_forever() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let err = wait_for_callback(listener, "S", Duration::from_millis(50))
            .await
            .expect_err("タイムアウトすべき");
        assert!(
            matches!(err, AuthError::Denied(_) | AuthError::Io(_)),
            "予期しない種類: {err:?}"
        );
    }

    /// 1455 が塞がっているとき、汎用の bind エラーではなく衝突を名指しする。
    #[tokio::test]
    async fn a_busy_callback_port_names_the_collision() {
        let Ok(_held) = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await else {
            // 何か他のプロセスが既に握っている環境では、この試験の前提が
            // 成立しない。握れないこと自体は異常ではないので飛ばす。
            return;
        };
        let err = bind_callback()
            .await
            .expect_err("塞がっているので失敗すべき");
        let AuthError::PortInUse(_) = err else {
            panic!("PortInUse 以外になっている: {err:?}");
        };
        // 文面に codex login への言及があること。`Display` は
        // `AuthError` の属性で組み立てられる。
        let shown = err.to_string();
        assert!(
            shown.contains("codex login"),
            "衝突の相手を名指ししていない: {shown}"
        );
        assert!(
            shown.contains(&CALLBACK_PORT.to_string()),
            "ポート番号が文面に無い: {shown}"
        );
    }
}
