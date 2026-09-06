# polaris Codex WebSocket トランスポート(フェーズ1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `CodexProvider`が`https://chatgpt.com/backend-api/codex/responses`へWebSocket経由で接続し、失敗時は既存のHTTP/SSE経路へ自動フォールバックする。この段階では送信内容(`store: false`のまま全履歴を送る)は変えない——観測可能な挙動を変えずにトランスポートだけを差し替える。

**Architecture:** 新規モジュール`crates/polaris-provider/src/codex_ws.rs`がWS接続の確立(`connect`)と1リクエスト分の送受信(`send_and_collect`、既存の`Folder`を再利用)を担う。`CodexProvider`はインスタンスごとに`WsState`(未試行/利用不可/接続済み)を保持し、`attempt()`内でWSを優先し、確立・送信いずれかが失敗すれば同一呼び出し内でHTTP/SSE経路へフォールバックする。`spawn`が並行実行する各subagentへは、`Provider`トレイトに追加する`for_new_turn`(デフォルトは`None`を返す)経由で、`CodexProvider`だけが独立したWS接続状態を持つ新規インスタンスを渡す。

**Tech Stack:** `tokio-tungstenite` 0.30(`rustls-tls-webpki-roots`機能、既存の`reqwest`と同じTLSバックエンドに揃える)。

**Spec:** `docs/superpowers/specs/2026-08-27-polaris-codex-ws-transport-design.md`

## Global Constraints

- 対象は`crates/polaris-provider`(新規`codex_ws.rs`、`codex.rs`、`lib.rs`)と`crates/polaris-core/src/spawn.rs`のみ。`openai.rs`・`session.rs`・`compaction.rs`・`agent.rs`(`spawn`呼び出し部分を除く)には触れない
- `store: false`のまま。送信される`input`の内容(全履歴)はこのフェーズで変えない——変わるのはトランスポートと、既知のときだけ乗る`client_metadata.x-codex-turn-state`のみ
- WSのメッセージ封筒は`{"type": "response.create", ...ResponseCreateWsRequest相当のフィールド}`(upstream `codex-api/src/common.rs`の`#[serde(tag = "type")] enum ResponsesWsRequest { #[serde(rename = "response.create")] ResponseCreate(...) }`を実測済み)
- WS接続URLは`wss://chatgpt.com/backend-api/codex/responses`(既存HTTPエンドポイントの`https`→`wss`置換。upstream `codex-api/src/provider.rs:92-103`の`websocket_url_for_path`を実測済み)
- turn-stateは`x-codex-turn-state`というキーで、(a) WSアップグレード応答のHTTPヘッダ、または(b) `"type":"response.metadata"`イベントの`headers`オブジェクト、のいずれかから大文字小文字を無視して読む(upstream `codex-api/src/sse/responses.rs:263-272`を実測済み)。一度取得したら同一インスタンス内では上書きしない(sticky)
- 既存の`Folder`(`crates/polaris-provider/src/codex.rs`、既に`pub`)はSSEデコーダ前提(`data: {json}\n\n`)。WSの生JSONテキストフレームは`format!("data: {text}\n\n").into_bytes()`で包んでから`Folder::push`へ渡し、再利用する。`Folder`自体は変更しない
- `cargo test --workspace`・`cargo clippy --workspace --all-targets -- -D warnings`・`cargo fmt --all -- --check`が全タスクで通ること

---

### Task 1: `codex_ws::connect` — WS接続確立とturn-state取得

**Files:**
- Modify: `Cargo.toml`(ワークスペースルート) — `tokio-tungstenite`を追加
- Modify: `crates/polaris-provider/Cargo.toml` — `tokio-tungstenite`・`url`を追加
- Create: `crates/polaris-provider/src/codex_ws.rs`
- Modify: `crates/polaris-provider/src/lib.rs` — `mod codex_ws;`を追加(`pub(crate) mod codex_ws;`)

**Interfaces:**
- Produces: `pub(crate) struct WsConnection`(内部に`tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>`を持つ)、`pub(crate) struct ConnectOutcome { pub(crate) connection: WsConnection, pub(crate) turn_state: Option<String> }`、`pub(crate) async fn connect(base: &str, access_token: &str, account_id: &str) -> Result<ConnectOutcome, ProviderError>`

- [ ] **Step 1: ワークスペースの依存関係へ`tokio-tungstenite`を追加**

`Cargo.toml`(ワークスペースルート)の`[workspace.dependencies]`セクション、`reqwest`の直後に追加:

```toml
tokio-tungstenite = { version = "0.30", features = ["rustls-tls-webpki-roots"] }
```

`crates/polaris-provider/Cargo.toml`の`[dependencies]`セクションへ追加:

```toml
tokio-tungstenite = { workspace = true }
url = { workspace = true }
```

- [ ] **Step 2: `cargo build -p polaris-provider`でビルドが通ることを確認**

Run: `cargo build -p polaris-provider`
Expected: 依存解決が成功し、警告なくビルドが終わる(まだ`codex_ws`モジュールは無いため、コード自体の変化は無い)

- [ ] **Step 3: ローカルWSテストサーバのヘルパ + 失敗するテストを書く**

`crates/polaris-provider/src/codex_ws.rs`を新規作成:

```rust
//! WebSocket transport for the codex provider. Connects to the same
//! `/responses` path the HTTP transport uses, with `wss` in place of
//! `https`. Falls back to HTTP/SSE at the call site (`codex.rs`) when
//! this fails — this module only ever reports failure, it never retries
//! or falls back on its own.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::ProviderError;

/// The header upstream's server reads turn-state from and echoes back,
/// both on the WS upgrade response and in-band in `response.metadata`
/// events. Matches upstream's `X_CODEX_TURN_STATE_HEADER`
/// (`codex-api/src/endpoint/responses_websocket.rs`).
const TURN_STATE_HEADER: &str = "x-codex-turn-state";

pub(crate) struct WsConnection {
    stream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

pub(crate) struct ConnectOutcome {
    pub(crate) connection: WsConnection,
    pub(crate) turn_state: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Starts a local `ws://` server on an ephemeral port that accepts
    /// one connection, attaches `header_name: header_value` to the
    /// upgrade response, and holds the socket open briefly afterward so
    /// the client's `connect` call has time to finish reading the
    /// upgrade response before the server task (and socket) drops.
    /// Returns the `ws://127.0.0.1:<port>` base URL (without `/responses`
    /// — `connect` appends that itself).
    async fn start_test_server_with_response_header(
        header_name: &'static str,
        header_value: &'static str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("can't bind test listener");
        let port = listener.local_addr().expect("no local addr").port();

        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("no incoming connection");
            let _ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                |_req: &tokio_tungstenite::tungstenite::handshake::server::Request, mut resp| {
                    resp.headers_mut().insert(
                        header_name,
                        tokio_tungstenite::tungstenite::http::HeaderValue::from_static(
                            header_value,
                        ),
                    );
                    Ok(resp)
                },
            )
            .await
            .expect("handshake failed");
            // Hold the connection open briefly so the client's `connect`
            // call can finish reading the upgrade response before this
            // task (and the socket) drops.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        format!("ws://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn connect_captures_turn_state_from_the_upgrade_response() {
        let url =
            start_test_server_with_response_header(TURN_STATE_HEADER, "turn-abc123").await;

        let outcome = connect(&url, "test-token", "test-account")
            .await
            .expect("connect should succeed");

        assert_eq!(outcome.turn_state.as_deref(), Some("turn-abc123"));
    }

    #[tokio::test]
    async fn connect_without_a_turn_state_header_returns_none() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("can't bind test listener");
        let port = listener.local_addr().expect("no local addr").port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("no incoming connection");
            let _ws = tokio_tungstenite::accept_async(tcp)
                .await
                .expect("handshake failed");
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let url = format!("ws://127.0.0.1:{port}");

        let outcome = connect(&url, "test-token", "test-account")
            .await
            .expect("connect should succeed");

        assert_eq!(outcome.turn_state, None);
    }

    #[tokio::test]
    async fn connect_forwards_auth_headers() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("can't bind test listener");
        let port = listener.local_addr().expect("no local addr").port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("no incoming connection");
            let mut sent_headers = None;
            let _ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                    sent_headers = Some((
                        req.headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        req.headers()
                            .get("chatgpt-account-id")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                    ));
                    Ok(resp)
                },
            )
            .await
            .expect("handshake failed");
            let _ = tx.send(sent_headers.expect("callback never ran"));
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let url = format!("ws://127.0.0.1:{port}");

        connect(&url, "secret-token", "acct-42")
            .await
            .expect("connect should succeed");

        let (auth, account) = rx.await.expect("server never reported headers");
        assert_eq!(auth.as_deref(), Some("Bearer secret-token"));
        assert_eq!(account.as_deref(), Some("acct-42"));
    }

    #[tokio::test]
    async fn connect_to_a_closed_port_is_an_error_not_a_panic() {
        // Nothing is listening on this port.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("can't bind probe listener");
        let port = listener.local_addr().expect("no local addr").port();
        drop(listener);
        let url = format!("ws://127.0.0.1:{port}");

        let err = connect(&url, "t", "a").await.expect_err("should fail");
        assert!(matches!(err, ProviderError::Http(_)));
    }
```

- [ ] **Step 4: テストを実行して失敗を確認**

Run: `cargo test -p polaris-provider codex_ws:: 2>&1 | tail -30`
Expected: コンパイルエラー(`connect`関数がまだ存在しない)

- [ ] **Step 5: `connect`を実装する**

`codex_ws.rs`の`tests`モジュールの手前に追加:

```rust
pub(crate) async fn connect(
    base_ws_url: &str,
    access_token: &str,
    account_id: &str,
) -> Result<ConnectOutcome, ProviderError> {
    let url = format!("{base_ws_url}/responses");
    let mut request = url
        .into_client_request()
        .map_err(|e| ProviderError::Http(format!("bad websocket url: {e}")))?;
    let headers = request.headers_mut();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|e| ProviderError::Http(format!("bad access token: {e}")))?,
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_str(account_id)
            .map_err(|e| ProviderError::Http(format!("bad account id: {e}")))?,
    );

    let (stream, response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| ProviderError::Http(format!("websocket connect failed: {e}")))?;

    let turn_state = response
        .headers()
        .iter()
        .find(|(name, _)| name.as_str().eq_ignore_ascii_case(TURN_STATE_HEADER))
        .and_then(|(_, value)| value.to_str().ok())
        .map(str::to_string);

    Ok(ConnectOutcome {
        connection: WsConnection { stream },
        turn_state,
    })
}
```

`base_ws_url`は呼び出し側(`codex.rs`、Task 3)が`wss://chatgpt.com/backend-api/codex`のようなベースを渡す想定——`connect`自身は`/responses`を付け足すところまでで、スキーム変換(`https`→`wss`)は呼び出し側の責務とする(既存の`CodexProvider::base`が`https://...`を持つ場所と対称になるよう、変換はそこに置く。Task 3参照)。

- [ ] **Step 6: テストを実行してパスすることを確認**

Run: `cargo test -p polaris-provider codex_ws:: 2>&1 | tail -30`
Expected: 4件のテストすべてPASS(`connect_captures_turn_state_from_the_upgrade_response`, `connect_without_a_turn_state_header_returns_none`, `connect_forwards_auth_headers`, `connect_to_a_closed_port_is_an_error_not_a_panic`)

- [ ] **Step 7: `lib.rs`へモジュール宣言を追加**

`crates/polaris-provider/src/lib.rs`のモジュール宣言群(`pub mod codex;`等がある箇所)へ追加:

```rust
pub(crate) mod codex_ws;
```

- [ ] **Step 8: ワークスペース全体のビルド・lintを確認**

Run: `cargo build --workspace && cargo clippy -p polaris-provider --all-targets -- -D warnings`
Expected: エラー・警告なし

- [ ] **Step 9: コミット**

```bash
git add Cargo.toml crates/polaris-provider/Cargo.toml crates/polaris-provider/src/codex_ws.rs crates/polaris-provider/src/lib.rs
git commit -m "feat(polaris-provider): add codex_ws::connect for the WS upgrade + turn-state capture"
```

---

### Task 2: `codex_ws::send_and_collect` — 送受信ループとFolder再利用

**Files:**
- Modify: `crates/polaris-provider/src/codex_ws.rs`
- Modify: `crates/polaris-provider/src/codex.rs` — `Folder`を`codex_ws`から使えるよう`pub(crate) use`または直接パス参照(既に`pub`のため変更不要、`crate::codex::Folder`で参照できる)

**Interfaces:**
- Consumes: Task 1の`WsConnection`、`crate::codex::Folder`(既存、`pub fn new() -> Self` / `pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProviderError>` / `pub fn finish(self) -> Result<CompletionResponse, ProviderError>`)
- Produces: `pub(crate) async fn send_and_collect(connection: &mut WsConnection, body: &Value, idle: Duration) -> Result<(CompletionResponse, Option<String>), ProviderError>` — 戻り値の`Option<String>`は、この送受信中に`response.metadata`イベントから新たに学習したturn-state(無ければ`None`)

- [ ] **Step 1: 失敗するテストを書く — 正常な1往復**

`codex_ws.rs`の`tests`モジュールへ追加。まずサーバ側が`response.completed`だけを返す最小構成:

```rust
    fn ws_response_frame(kind: &str, extra: Value) -> Message {
        let mut v = serde_json::json!({ "type": kind });
        if let Some(obj) = extra.as_object() {
            for (k, val) in obj {
                v[k] = val.clone();
            }
        }
        Message::Text(v.to_string().into())
    }

    async fn start_scripted_server(frames: Vec<Message>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("can't bind test listener");
        let port = listener.local_addr().expect("no local addr").port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("no incoming connection");
            let mut ws = tokio_tungstenite::accept_async(tcp)
                .await
                .expect("handshake failed");
            // Wait for the client's request frame before replying — a real
            // server does the same, and receive-then-reply exercises the
            // send-then-receive-loop ordering `send_and_collect` relies on.
            let _ = ws.next().await;
            for frame in frames {
                ws.send(frame).await.expect("can't send scripted frame");
            }
            // Keep the socket open briefly so the client finishes reading
            // before the server task (and socket) drops.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        format!("ws://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn send_and_collect_returns_the_completed_response() {
        let url = start_scripted_server(vec![ws_response_frame(
            "response.completed",
            serde_json::json!({
                "response": {
                    "output": [{
                        "type": "message",
                        "content": [{"type": "output_text", "text": "hi there"}],
                    }],
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 2,
                        "total_tokens": 12,
                    },
                }
            }),
        )])
        .await;
        let outcome = connect(&url, "t", "a").await.expect("connect failed");
        let mut connection = outcome.connection;

        let (response, new_turn_state) = send_and_collect(
            &mut connection,
            &serde_json::json!({"type": "response.create", "model": "m"}),
            Duration::from_secs(5),
        )
        .await
        .expect("send_and_collect failed");

        assert_eq!(response.text, "hi there");
        assert_eq!(new_turn_state, None);
    }

    #[tokio::test]
    async fn send_and_collect_captures_turn_state_from_a_metadata_event() {
        let url = start_scripted_server(vec![
            ws_response_frame(
                "response.metadata",
                serde_json::json!({"headers": {"x-codex-turn-state": "turn-xyz"}}),
            ),
            ws_response_frame(
                "response.completed",
                serde_json::json!({
                    "response": {
                        "output": [],
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                    }
                }),
            ),
        ])
        .await;
        let outcome = connect(&url, "t", "a").await.expect("connect failed");
        let mut connection = outcome.connection;

        let (_response, new_turn_state) = send_and_collect(
            &mut connection,
            &serde_json::json!({"type": "response.create", "model": "m"}),
            Duration::from_secs(5),
        )
        .await
        .expect("send_and_collect failed");

        assert_eq!(new_turn_state.as_deref(), Some("turn-xyz"));
    }

    #[tokio::test]
    async fn send_and_collect_replies_to_a_ping_with_a_pong_and_keeps_going() {
        let url = start_scripted_server(vec![
            Message::Ping(vec![1, 2, 3].into()),
            ws_response_frame(
                "response.completed",
                serde_json::json!({
                    "response": {
                        "output": [],
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                    }
                }),
            ),
        ])
        .await;
        let outcome = connect(&url, "t", "a").await.expect("connect failed");
        let mut connection = outcome.connection;

        let result = send_and_collect(
            &mut connection,
            &serde_json::json!({"type": "response.create", "model": "m"}),
            Duration::from_secs(5),
        )
        .await;

        assert!(result.is_ok(), "ping should not abort the response: {result:?}");
    }

    #[tokio::test]
    async fn send_and_collect_errors_when_the_connection_closes_before_completed() {
        let url = start_scripted_server(vec![Message::Close(None)]).await;
        let outcome = connect(&url, "t", "a").await.expect("connect failed");
        let mut connection = outcome.connection;

        let result = send_and_collect(
            &mut connection,
            &serde_json::json!({"type": "response.create", "model": "m"}),
            Duration::from_secs(5),
        )
        .await;

        assert!(result.is_err());
    }
```

- [ ] **Step 2: テストを実行して失敗を確認**

Run: `cargo test -p polaris-provider codex_ws:: 2>&1 | tail -30`
Expected: コンパイルエラー(`send_and_collect`が無い)

- [ ] **Step 3: `send_and_collect`を実装する**

`codex_ws.rs`の`connect`関数の後に追加:

```rust
pub(crate) async fn send_and_collect(
    connection: &mut WsConnection,
    body: &Value,
    idle: Duration,
) -> Result<(crate::CompletionResponse, Option<String>), ProviderError> {
    let text = body.to_string();
    connection
        .stream
        .send(Message::Text(text.into()))
        .await
        .map_err(|e| ProviderError::Http(format!("websocket send failed: {e}")))?;

    let mut folder = crate::codex::Folder::new();
    let mut new_turn_state = None;

    loop {
        let next = tokio::time::timeout(idle, connection.stream.next())
            .await
            .map_err(|_| {
                ProviderError::Http(format!(
                    "no websocket response arrived for {} seconds",
                    idle.as_secs()
                ))
            })?;
        let message = match next {
            None => {
                return Err(ProviderError::Http(
                    "websocket closed before response.completed".into(),
                ));
            }
            Some(Err(e)) => {
                return Err(ProviderError::Http(format!("websocket error: {e}")));
            }
            Some(Ok(message)) => message,
        };

        match message {
            Message::Text(text) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text)
                    && v.get("type").and_then(|t| t.as_str()) == Some("response.metadata")
                    && let Some(headers) = v.get("headers").and_then(|h| h.as_object())
                    && let Some(state) = headers.iter().find_map(|(name, value)| {
                        name.eq_ignore_ascii_case(TURN_STATE_HEADER)
                            .then(|| value.as_str())
                            .flatten()
                    })
                {
                    new_turn_state = Some(state.to_string());
                }

                // `Folder` was built for SSE framing (`data: {json}\n\n`).
                // Wrapping each WS text frame the same way reuses it
                // unchanged rather than forking its event-parsing logic.
                let framed = format!("data: {text}\n\n");
                folder.push(framed.as_bytes())?;
                if folder.is_completed() {
                    break;
                }
            }
            Message::Ping(payload) => {
                connection
                    .stream
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|e| ProviderError::Http(format!("websocket pong failed: {e}")))?;
            }
            Message::Close(_) => {
                return Err(ProviderError::Http(
                    "websocket closed by server before response.completed".into(),
                ));
            }
            Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
        }
    }

    Ok((folder.finish()?, new_turn_state))
}
```

このコードは`Folder`に`is_completed()`という新規の公開メソッドを要求する。`crate::codex::Folder`には現状、完了を外から確認する手段が無い(`finish()`を呼ぶまで分からない)。`codex.rs`側で以下を追加する:

`crates/polaris-provider/src/codex.rs`の`impl Folder`ブロック内、`pub fn push`の直後に追加:

```rust
    /// Whether `response.completed` has already been folded in. WS's
    /// receive loop (`codex_ws.rs`) needs this to know when to stop
    /// reading frames — the HTTP/SSE path just reads until the stream
    /// itself ends, but a WS connection stays open for the next request,
    /// so nothing else signals "this response is done."
    pub fn is_completed(&self) -> bool {
        self.completed
    }
```

`self.completed`は既存のプライベートフィールド(`"response.completed"`のマッチ節で`self.completed = true;`と設定される、Task開始前のコードで確認済み)。

- [ ] **Step 4: テストを実行してパスすることを確認**

Run: `cargo test -p polaris-provider codex_ws:: 2>&1 | tail -40`
Expected: 8件のテストすべてPASS(Task 1の4件+このTaskの4件)

- [ ] **Step 5: ワークスペース全体のビルド・lintを確認**

Run: `cargo test --workspace 2>&1 | grep -E "FAILED|test result" && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全緑、警告・差分なし

- [ ] **Step 6: コミット**

```bash
git add crates/polaris-provider/src/codex_ws.rs crates/polaris-provider/src/codex.rs
git commit -m "feat(polaris-provider): add codex_ws::send_and_collect, reusing Folder for WS frames"
```

---

### Task 3: `CodexProvider`にWS優先+HTTPフォールバックを配線する

**Files:**
- Modify: `crates/polaris-provider/src/codex.rs`

**Interfaces:**
- Consumes: Task 1/2の`codex_ws::{connect, send_and_collect, ConnectOutcome}`
- Produces: `CodexProvider`が内部に`ws: tokio::sync::Mutex<WsState>`を持つ。`WsState`はこのタスク内で定義する非公開型(他クレートから参照されない)

- [ ] **Step 1: 失敗する統合テストを書く**

既存の`crates/polaris-provider/src/codex.rs`の`tests`モジュール(`impl crate::TokenSource for Tokens`が既にある箇所)を確認し、その近くに追加。まず、WSサーバーを使わない既存のwiremockベースのHTTPテストが今後も通ることを前提に、新しいテストは「WSサーバーが動いている場合、そちらが使われる」ことを確認する形にする:

```rust
    #[tokio::test]
    async fn complete_prefers_websocket_when_it_is_reachable() {
        // A minimal scripted WS server standing in for the real backend.
        // If `attempt()` ignores WS and falls straight to HTTP, this
        // server never receives a connection and the test's own request
        // to it (if any) would hang — instead, this test asserts on the
        // *content* of the response, which is only reachable via this
        // server's scripted reply.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("can't bind");
        let port = listener.local_addr().expect("no local addr").port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("no incoming connection");
            let mut ws = tokio_tungstenite::accept_async(tcp)
                .await
                .expect("handshake failed");
            let _ = futures_util::StreamExt::next(&mut ws).await;
            let frame = serde_json::json!({
                "type": "response.completed",
                "response": {
                    "output": [{
                        "type": "message",
                        "content": [{"type": "output_text", "text": "from websocket"}],
                    }],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                }
            });
            futures_util::SinkExt::send(
                &mut ws,
                tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into()),
            )
            .await
            .expect("can't send scripted frame");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let provider = CodexProvider::new(
            format!("http://127.0.0.1:{port}"),
            "m".into(),
            Tokens::new(),
        );
        let res = provider
            .complete(CompletionRequest {
                system: "sys".into(),
                messages: vec![Message::user("hi")],
                tools: vec![],
            })
            .await
            .expect("complete failed");

        assert_eq!(res.text, "from websocket");
    }

    #[tokio::test]
    async fn complete_falls_back_to_http_when_websocket_is_unreachable() {
        // Nothing listens for WS on this port's neighbor — reuse the
        // existing wiremock-based HTTP test server pattern already in
        // this file (see other tests in this module for the exact setup)
        // and confirm a request still succeeds via HTTP when WS can't
        // even connect. The HTTP mock server's address becomes both the
        // HTTP base (used as-is) and the WS base (its `wss` upgrade will
        // fail against a plain HTTP mock server, exercising the fallback
        // naturally without needing a second unreachable port).
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw(
                        sse_body_completed("via http"),
                        "text/event-stream",
                    ),
            )
            .mount(&mock)
            .await;

        let provider = CodexProvider::new(mock.uri(), "m".into(), Tokens::new());
        let res = provider
            .complete(CompletionRequest {
                system: "sys".into(),
                messages: vec![Message::user("hi")],
                tools: vec![],
            })
            .await
            .expect("complete failed");

        assert_eq!(res.text, "via http");
    }
```

`sse_body_completed`が既存のヘルパとして無ければ、このモジュール内の他のHTTPテストが使っている`response.completed`のSSEボディ組み立てロジックを探し、同じ形で1つ追加する(このファイル内に既存の類似ヘルパがあるはずなので、命名を揃えて再利用する。無ければ以下を追加):

```rust
    fn sse_body_completed(text: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "output": [{
                        "type": "message",
                        "content": [{"type": "output_text", "text": text}],
                    }],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                }
            })
        )
    }
```

- [ ] **Step 2: テストを実行して失敗を確認**

Run: `cargo test -p polaris-provider codex:: -- complete_prefers_websocket complete_falls_back 2>&1 | tail -40`
Expected: `complete_prefers_websocket_when_it_is_reachable`は失敗(現状`attempt`はHTTPしか試さないため、`from websocket`ではなく接続エラーか別の結果になる)。`complete_falls_back_to_http_when_websocket_is_unreachable`は現状の実装でも恐らく通ってしまう(まだWSを試みていないため)——これは正常。両方揃った状態でStep 3以降に進む

- [ ] **Step 3: `WsState`と配線を実装する**

`crates/polaris-provider/src/codex.rs`の`CodexProvider`構造体定義の直前に追加:

```rust
/// Per-instance WebSocket state. `Untried` and `Unavailable` need no
/// payload — `Untried` means "the next `attempt()` should try WS first",
/// `Unavailable` means "a previous attempt failed, stop trying WS for
/// the rest of this instance's life and use HTTP/SSE directly." Locked
/// behind a `Mutex` because `complete()` takes `&self` and this instance
/// is meant to serve one logical turn's sequential calls (see
/// `for_new_turn`, Task 5) — a `Mutex` here is about interior mutability
/// under a shared reference, not about arbitrating real concurrent
/// access, which `for_new_turn` is what prevents in the first place.
enum WsState {
    Untried,
    Unavailable,
    Connected {
        connection: crate::codex_ws::WsConnection,
        turn_state: Option<String>,
    },
}
```

`CodexProvider`構造体へフィールドを追加:

```rust
pub struct CodexProvider {
    base: String,
    model: std::sync::RwLock<String>,
    effort_override: std::sync::RwLock<Option<String>>,
    tokens: Arc<dyn crate::TokenSource>,
    client: reqwest::Client,
    idle: Duration,
    ws: tokio::sync::Mutex<WsState>,
}
```

両方のコンストラクタ(`new`・`with_idle_timeout`)へ`ws: tokio::sync::Mutex::new(WsState::Untried),`を追加する。

`attempt`関数の本体冒頭(`let model = ...`より前)へ、WSを試すロジックを追加する。既存のHTTP送信ロジック(`self.client.post(...)`以降)はそのまま残し、その手前でWSを試す形にする:

```rust
    async fn attempt(
        &self,
        token: &crate::Token,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let model = self.model.read().expect("model lock poisoned").clone();
        let override_effort = self
            .effort_override
            .read()
            .expect("effort lock poisoned")
            .clone();
        let effort = override_effort.as_deref().or(token.effort.as_deref());
        let body = build_body(&model, req, effort);

        if let Some(response) = self.try_websocket(token, &body).await {
            return response;
        }

        // Falls through to the existing HTTP/SSE path below when
        // `try_websocket` returns `None` — WS is marked `Unavailable`
        // for the rest of this instance's life at that point, so every
        // later call skips straight past this block.
        let resp = self
            .client
            .post(format!("{}/responses", self.base))
            .bearer_auth(&token.access_token)
            .header("chatgpt-account-id", &token.account_id)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        // ... (既存のstatus判定・SSE折り畳みロジックはこのまま変更なし)
```

`try_websocket`を`impl CodexProvider`ブロックへ新規追加(`attempt`の直前に置く):

```rust
    /// Tries the WebSocket transport for this one request. Returns
    /// `None` when WS isn't available (never tried before and connect
    /// just failed, or already known `Unavailable`) — the caller falls
    /// through to HTTP/SSE in that case. Returns `Some(Ok(_))` /
    /// `Some(Err(_))` when WS was actually used, whether it succeeded or
    /// failed *after* connecting: a mid-request WS failure is not
    /// silently retried over HTTP within the same call, since the exact
    /// same content would be re-sent — the model's already-imperfect
    /// signal about whether the tool call happened doesn't get better
    /// by switching transports mid-flight.
    async fn try_websocket(
        &self,
        token: &crate::Token,
        body: &Value,
    ) -> Option<Result<CompletionResponse, ProviderError>> {
        let mut ws = self.ws.lock().await;
        loop {
            match &mut *ws {
                WsState::Unavailable => return None,
                WsState::Untried => {
                    let ws_base = self.base.replacen("https://", "wss://", 1).replacen(
                        "http://",
                        "ws://",
                        1,
                    );
                    match crate::codex_ws::connect(&ws_base, &token.access_token, &token.account_id)
                        .await
                    {
                        Ok(outcome) => {
                            *ws = WsState::Connected {
                                connection: outcome.connection,
                                turn_state: outcome.turn_state,
                            };
                            // Loop back around to the `Connected` arm below
                            // with the same lock held, rather than
                            // duplicating the send logic here.
                        }
                        Err(_) => {
                            *ws = WsState::Unavailable;
                            return None;
                        }
                    }
                }
                WsState::Connected {
                    connection,
                    turn_state,
                } => {
                    let mut wire_body = body.clone();
                    wire_body["type"] = Value::String("response.create".to_string());
                    if let Some(state) = turn_state.as_deref() {
                        wire_body["client_metadata"] = serde_json::json!({
                            "x-codex-turn-state": state,
                        });
                    }
                    let result =
                        crate::codex_ws::send_and_collect(connection, &wire_body, self.idle)
                            .await;
                    return match result {
                        Ok((response, learned_turn_state)) => {
                            if let Some(new_state) = learned_turn_state {
                                *turn_state = Some(new_state);
                            }
                            Some(Ok(response))
                        }
                        Err(e) => Some(Err(e)),
                    };
                }
            }
        }
    }
```

- [ ] **Step 4: テストを実行してパスすることを確認**

Run: `cargo test -p polaris-provider 2>&1 | grep -E "FAILED|test result"`
Expected: 全件PASS(既存のHTTPテスト含む——`complete_falls_back_to_http_when_websocket_is_unreachable`は、WSがwiremockのHTTPモックサーバへの接続に失敗して`Unavailable`になり、既存のHTTP経路が動くことで通る)

- [ ] **Step 5: ワークスペース全体のビルド・lintを確認**

Run: `cargo test --workspace 2>&1 | grep -E "FAILED|test result" && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全緑、警告・差分なし

- [ ] **Step 6: コミット**

```bash
git add crates/polaris-provider/src/codex.rs
git commit -m "feat(polaris-provider): try WebSocket in attempt(), fall back to HTTP/SSE on failure"
```

---

### Task 4: WS切断後の再接続(既存接続の失敗はUnavailableにしない)

**Files:**
- Modify: `crates/polaris-provider/src/codex.rs`

**Interfaces:**
- Consumes: Task 3の`WsState`・`try_websocket`

- [ ] **Step 1: 失敗するテストを書く — 接続済みの状態で送信が失敗したら、次回は再接続を試みる**

`codex.rs`の`tests`モジュールへ追加:

```rust
    #[tokio::test]
    async fn a_dropped_websocket_connection_reconnects_on_the_next_call_instead_of_giving_up() {
        // First connection: accepts, then closes immediately without ever
        // completing a response (simulates the 60-minute connection
        // limit, or any mid-session drop).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("can't bind");
        let port = listener.local_addr().expect("no local addr").port();
        tokio::spawn(async move {
            // First connection: close without responding.
            let (tcp, _) = listener.accept().await.expect("no first connection");
            let mut ws = tokio_tungstenite::accept_async(tcp)
                .await
                .expect("first handshake failed");
            let _ = futures_util::StreamExt::next(&mut ws).await;
            futures_util::SinkExt::send(
                &mut ws,
                tokio_tungstenite::tungstenite::Message::Close(None),
            )
            .await
            .ok();
            drop(ws);

            // Second connection: responds normally.
            let (tcp2, _) = listener.accept().await.expect("no second connection");
            let mut ws2 = tokio_tungstenite::accept_async(tcp2)
                .await
                .expect("second handshake failed");
            let _ = futures_util::StreamExt::next(&mut ws2).await;
            let frame = serde_json::json!({
                "type": "response.completed",
                "response": {
                    "output": [{
                        "type": "message",
                        "content": [{"type": "output_text", "text": "reconnected"}],
                    }],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                }
            });
            futures_util::SinkExt::send(
                &mut ws2,
                tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into()),
            )
            .await
            .expect("can't send scripted frame");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let provider = CodexProvider::new(
            format!("http://127.0.0.1:{port}"),
            "m".into(),
            Tokens::new(),
        );

        let first = provider
            .complete(CompletionRequest {
                system: "sys".into(),
                messages: vec![Message::user("hi")],
                tools: vec![],
            })
            .await;
        assert!(first.is_err(), "the first call should surface the drop, not silently fall back to HTTP with no server listening");

        let second = provider
            .complete(CompletionRequest {
                system: "sys".into(),
                messages: vec![Message::user("hi again")],
                tools: vec![],
            })
            .await
            .expect("second call should succeed via reconnect");
        assert_eq!(second.text, "reconnected");
    }
```

- [ ] **Step 2: テストを実行して失敗を確認**

Run: `cargo test -p polaris-provider a_dropped_websocket_connection -- --nocapture 2>&1 | tail -30`
Expected: 2回目の呼び出しが失敗する(Task 3の実装では`WsState::Connected`での送信失敗は`Unavailable`にせずそのまま`Connected`に留まってしまい、壊れた接続へ送り続けて再度失敗するため)

- [ ] **Step 3: `try_websocket`の`Connected`分岐を、送信失敗時に`Untried`へ戻すよう直す**

`codex.rs`の`try_websocket`内、`WsState::Connected { .. }`分岐の`Err(e) => Some(Err(e)),`を以下に置き換える:

```rust
                        Err(e) => {
                            // The connection is presumed dead — drop it and
                            // let the *next* call reconnect from scratch
                            // (fresh turn-state, matching upstream's own
                            // behavior after a connection is lost). This
                            // call itself still surfaces the error rather
                            // than silently retrying over HTTP: retrying
                            // with different transport mid-call risks a
                            // duplicate tool-affecting request reaching
                            // the model twice.
                            *ws = WsState::Untried;
                            Some(Err(e))
                        }
```

- [ ] **Step 4: テストを実行してパスすることを確認**

Run: `cargo test -p polaris-provider a_dropped_websocket_connection -- --nocapture 2>&1 | tail -30`
Expected: PASS

- [ ] **Step 5: ワークスペース全体のビルド・lintを確認**

Run: `cargo test --workspace 2>&1 | grep -E "FAILED|test result" && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全緑、警告・差分なし

- [ ] **Step 6: コミット**

```bash
git add crates/polaris-provider/src/codex.rs
git commit -m "fix(polaris-provider): reconnect websocket on the next call instead of giving up forever"
```

---

### Task 5: `Provider::for_new_turn` とsubagentへの配線

**Files:**
- Modify: `crates/polaris-provider/src/lib.rs`
- Modify: `crates/polaris-provider/src/codex.rs`
- Modify: `crates/polaris-core/src/spawn.rs`

**Interfaces:**
- Produces: `Provider::for_new_turn(&self) -> Option<Arc<dyn Provider>>`(デフォルト`None`)、`CodexProvider::for_new_turn`が`Some`を返す上書き実装

- [ ] **Step 1: 失敗するテストを書く — デフォルトは`None`**

`crates/polaris-provider/src/lib.rs`の`tests`モジュール(`impl TokenSource for CannedTokens`がある箇所)へ追加:

```rust
    #[test]
    fn for_new_turn_defaults_to_none() {
        struct NoOpinion;
        #[async_trait::async_trait]
        impl Provider for NoOpinion {
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, ProviderError> {
                unimplemented!("not exercised by this test")
            }
        }

        assert!(NoOpinion.for_new_turn().is_none());
    }
```

- [ ] **Step 2: テストを実行して失敗を確認**

Run: `cargo test -p polaris-provider for_new_turn_defaults_to_none 2>&1 | tail -20`
Expected: コンパイルエラー(`for_new_turn`メソッドが無い)

- [ ] **Step 3: トレイトへデフォルト実装を追加**

`crates/polaris-provider/src/lib.rs`の`pub trait Provider`定義、`set_effort`の直後に追加:

```rust
    /// Returns a fresh, independently-scoped provider handle for a new
    /// logical turn (a subagent's own `run_loop`), when this provider
    /// holds per-turn connection state that must not be shared across
    /// concurrently-running turns — see `CodexProvider`'s WebSocket
    /// transport, whose connection and turn-state are tied to one
    /// logical turn (upstream's own constraint: reusing sticky-routing
    /// state across turns causes routing bugs). Returns `None` when
    /// there's no such state to isolate — the default, safe for every
    /// provider without per-turn connection state, real or test double
    /// alike. The caller falls back to sharing the existing handle when
    /// this returns `None` (see `spawn::run_wave`).
    fn for_new_turn(&self) -> Option<Arc<dyn Provider>> {
        None
    }
```

`Arc`が`lib.rs`の先頭で既にimportされていることを確認する(`std::sync::Arc`、`TokenSource`の定義で既に使われているはずなので追加のimportは不要な想定——無ければ`use std::sync::Arc;`を追加)。

- [ ] **Step 4: テストを実行してパスすることを確認**

Run: `cargo test -p polaris-provider for_new_turn_defaults_to_none 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: `CodexProvider`側の失敗するテストを書く**

`crates/polaris-provider/src/codex.rs`の`tests`モジュールへ追加:

```rust
    #[test]
    fn for_new_turn_returns_an_independent_instance() {
        let provider = CodexProvider::new("http://example".into(), "m".into(), Tokens::new());
        let fresh = provider.for_new_turn().expect("CodexProvider should isolate WS state");

        // Not exhaustively checking every field — just that a distinct
        // `Arc` was actually constructed, not the same one handed back.
        assert!(!Arc::ptr_eq(
            &(Arc::new(provider) as Arc<dyn Provider>),
            &fresh
        ));
    }
```

- [ ] **Step 6: テストを実行して失敗を確認**

Run: `cargo test -p polaris-provider for_new_turn_returns_an_independent_instance 2>&1 | tail -20`
Expected: コンパイルエラー(`CodexProvider`に`for_new_turn`の上書きが無い)

- [ ] **Step 7: `CodexProvider::for_new_turn`を実装**

`crates/polaris-provider/src/codex.rs`の`impl Provider for CodexProvider`ブロック、`set_effort`の直後に追加:

```rust
    fn for_new_turn(&self) -> Option<Arc<dyn Provider>> {
        Some(Arc::new(CodexProvider {
            base: self.base.clone(),
            model: std::sync::RwLock::new(
                self.model.read().expect("model lock poisoned").clone(),
            ),
            effort_override: std::sync::RwLock::new(
                self.effort_override
                    .read()
                    .expect("effort lock poisoned")
                    .clone(),
            ),
            tokens: Arc::clone(&self.tokens),
            client: reqwest::Client::new(),
            idle: self.idle,
            ws: tokio::sync::Mutex::new(WsState::Untried),
        }))
    }
```

- [ ] **Step 8: テストを実行してパスすることを確認**

Run: `cargo test -p polaris-provider for_new_turn 2>&1 | tail -30`
Expected: 両方PASS

- [ ] **Step 9: `spawn.rs`の失敗するテストを書く**

`crates/polaris-core/src/spawn.rs`の`tests`モジュールには、既にこのテストが必要とする実部品が揃っている——`readonly_fixture_agent_type()`(`AgentType`を1つ返す)、`shared_test_audit()`(`Arc<Mutex<AuditLog>>`を返す)、`SandboxPolicy::new(SandboxMode::ReadOnly, &[])`、`DEFAULT_CONCURRENCY`/`DEFAULT_WRITE_CONCURRENCY`、`run_wave`の実引数の並びは`tasks_within_the_concurrency_limit_run_genuinely_concurrently`(このファイル内、`tasks, &agent_types, Arc::new(provider), audit, &base_sandbox, Path::new("/bin/true"), DEFAULT_CONCURRENCY, DEFAULT_WRITE_CONCURRENCY, None`の順)で確認済み。これらをそのまま使い、プロバイダだけ「独自の`for_new_turn`を持つプロバイダを渡したら、各タスクがそれ経由で得たハンドルを使う」ことを検証する新しいモックに差し替える:

```rust
    #[tokio::test]
    async fn run_wave_asks_each_task_for_its_own_provider_handle() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountsForNewTurn {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl Provider for CountsForNewTurn {
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                Ok(text("ok"))
            }
            fn for_new_turn(&self) -> Option<Arc<dyn Provider>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Some(Arc::new(CountsForNewTurn {
                    calls: Arc::clone(&self.calls),
                }))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountsForNewTurn {
            calls: Arc::clone(&calls),
        };
        let agent_types = vec![readonly_fixture_agent_type()];
        let tasks: Vec<SpawnTask> = (0..2)
            .map(|i| SpawnTask {
                agent_type: "ro-fixture".to_string(),
                task: format!("task {i}"),
                write_root: None,
            })
            .collect();
        let base_sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("policy");
        let audit = shared_test_audit();

        let _ = run_wave(
            tasks,
            &agent_types,
            Arc::new(provider),
            audit,
            &base_sandbox,
            Path::new("/bin/true"),
            DEFAULT_CONCURRENCY,
            DEFAULT_WRITE_CONCURRENCY,
            None,
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 2, "one for_new_turn call per task");
    }
```

- [ ] **Step 10: テストを実行して失敗を確認**

Run: `cargo test -p polaris-core run_wave_asks_each_task_for_its_own_provider_handle 2>&1 | tail -40`
Expected: コンパイルは通るが、`calls`が0のままアサーション失敗(まだ`run_wave`が`for_new_turn`を呼んでいないため)

- [ ] **Step 11: `run_wave`に配線する**

`crates/polaris-core/src/spawn.rs`の`run_wave`内、各タスクを起動する箇所(`provider_pool.clone()`を各タスクへ渡している行)を、タスクごとに以下へ置き換える:

```rust
let task_provider = provider_pool
    .for_new_turn()
    .unwrap_or_else(|| provider_pool.clone());
```

(既存コードが`provider_pool.clone()`を直接futureへ渡している箇所を、この`task_provider`を使うよう書き換える。`provider_pool`自体は`Arc<dyn Provider>`のまま、ループの外で保持され続ける——`for_new_turn`はループの中、タスクごとに呼ぶ。)

- [ ] **Step 12: テストを実行してパスすることを確認**

Run: `cargo test -p polaris-core run_wave 2>&1 | grep -E "FAILED|test result"`
Expected: 全件PASS(新規テスト含め、既存の`run_wave`テスト群に影響がないこと)

- [ ] **Step 13: ワークスペース全体のビルド・lintを確認**

Run: `cargo test --workspace 2>&1 | grep -E "FAILED|test result" && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全緑、警告・差分なし

- [ ] **Step 14: コミット**

```bash
git add crates/polaris-provider/src/lib.rs crates/polaris-provider/src/codex.rs crates/polaris-core/src/spawn.rs
git commit -m "feat(polaris-core,polaris-provider): give each spawned subagent its own provider turn-scope"
```

---

### Task 6: 実機検証とドキュメント更新

**Files:**
- Modify: `docs/superpowers/CURRENT.md`

**Interfaces:**
- Consumes: Task 1〜5の完成した実装

- [ ] **Step 1: release buildを作る**

Run: `cargo build --release -p polaris-cli`
Expected: ビルド成功

- [ ] **Step 2: 実バックエンドに対して1往復以上のリクエストを送り、WSが実際に使われることを確認する**

`crates/polaris-provider/src/codex.rs`の`try_websocket`の`WsState::Untried`分岐、`connect`呼び出しの直後(接続成功時)に、一時的な`eprintln!`を1行追加する:

```rust
if std::env::var_os("POLARIS_DUMP_USAGE").is_some() {
    eprintln!("[transport] websocket connected");
}
```

Run:

```bash
POLARIS_DUMP_USAGE=1 ./target/release/polaris exec "1+1は？" 2>&1 | grep -E "\[transport\]|\[usage\]"
```

Expected: `[transport] websocket connected`が出力され、続けて`[usage]`行(既存のusageダンプ)が出て応答が返ること。もしWSが使えない環境であれば(接続拒否など)、`[transport]`行が出ないまま`[usage]`行だけが出て、それでも応答自体は成功すること(フォールバックの確認)——どちらの経路でも最終的な応答が壊れていないことを目視で確認する

- [ ] **Step 3: 一時的な`eprintln!`を削除する**

Step 2で追加した`eprintln!`ブロックを削除する(検証専用、恒久的なログではない——この計画のGlobal Constraintsに「観測可能な挙動を変えない」とある通り、恒久的な出力の追加はscope外)。

- [ ] **Step 4: 「このプロジェクトを分析する」プロンプトで、変更前との比較実測を1回行う**

Run:

```bash
POLARIS_DUMP_USAGE=1 ./target/release/polaris exec "このプロジェクトを分析する" \
  > /tmp/ws-after.stdout.log 2> /tmp/ws-after.stderr.log
tail -1 /tmp/ws-after.stderr.log
```

Expected: `tokens: in N / out N / cache N / total N — Mメッセージ`という行が出力される。この値を、`docs/superpowers/CURRENT.md`「polaris vs codex の生トークン量の差」節に記録済みの直近の値(修正2後: raw total 461,611、往復7、キャッシュ比率73.5%)と比較する。**送信内容はこのフェーズで変えていないため、raw total・往復数・キャッシュ比率は実測誤差の範囲でほぼ同じ値になる見込み**——大きく変わっていた場合は、意図しない挙動変化(受け入れ基準5違反)が無いか調査してから先に進む

- [ ] **Step 5: `docs/superpowers/CURRENT.md`を更新する**

「polaris vs codex の生トークン量の差」節の末尾(`store: true`化の設計に触れている段落の直後)に、フェーズ1完了を追記する。実測値はStep 4で得た実際の値に置き換えること(以下は記入例、プレースホルダーではなく実際に測った数値へ差し替える):

```markdown
**フェーズ1(WSトランスポート、2026-08-27〜、実装完了)**: `docs/superpowers/specs/2026-08-27-polaris-codex-ws-transport-design.md`(spec)・`docs/superpowers/plans/2026-08-27-polaris-codex-ws-transport.md`(6タスクplan)をwriting-plans→subagent-driven-developmentで実装した。`CodexProvider`が`wss://chatgpt.com/backend-api/codex/responses`へのWebSocket接続を優先し、確立・送信いずれかに失敗すれば既存のHTTP/SSE経路へ自動フォールバックする。`Provider`トレイトへ`for_new_turn`(デフォルト`None`)を追加し、`spawn`が並行実行する各subagentには`CodexProvider`だけが独立したWS接続状態を持つ新規インスタンスを渡すようにした——`provider_pool`を親・全subagentで共有していた従来の配線のままだと、turn-state sticky routingが並行呼び出し間で競合するため。

このフェーズでは送信内容(`store: false`のまま全履歴を送る)を変えていない。実測(「このプロジェクトを分析する」プロンプト): 往復N・raw total N・キャッシュ比率N%——変更前(往復7・461,611・73.5%)とほぼ同じ値になることを確認した。raw totalの削減自体はフェーズ2(差分送信+`previous_response_id`継続)の対象で、まだ未着手。
```

- [ ] **Step 6: ドキュメントの変更をコミット**

```bash
git add docs/superpowers/CURRENT.md
git commit -m "docs: record phase-1 WS transport completion and its A/B verification"
```
