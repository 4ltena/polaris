//! The websocket transport for the Responses endpoint.
//!
//! **This exists for one reason: over plain HTTP the provider's prompt cache
//! is unreachable.** Measured on 2026-09-18 across nine approaches and about
//! thirty-five requests, polaris took zero cached tokens — while codex, on the
//! same model and the same task, took 92.7%. The two parameters that would
//! close the gap are refused by the server on the HTTP endpoint:
//! `previous_response_id` comes back `Unsupported parameter`, and `store:
//! true` comes back `Store must be set to false`. The continuation those
//! parameters express is only available on this socket.
//!
//! What the socket buys is not a discount on the same bytes. It is **not
//! sending them at all**: the server keeps the context behind the id it
//! issued, so request N+1 carries only the items that appeared since request
//! N. The repeated part is then billed as a cache read rather than as new
//! input, and the repeated part never leaves this machine.
//!
//! Off by default (`POLARIS_WS=on`). The HTTP path is untouched and remains
//! the fallback: anything that fails here — the upgrade, the frame, the
//! continuation — drops back to it rather than failing the run.

use crate::{ProviderError, Token};

/// The beta the Responses websocket lives behind. Dated, and therefore
/// expected to move: when it does, the upgrade fails and the run falls back
/// to HTTP rather than speaking an older dialect into a newer server.
const BETA: &str = "responses_websockets=2026-02-06";

/// No frame within this long and the socket is considered dead. Matches the
/// HTTP path's idle expectation; a model thinking for minutes is normal and
/// must not be mistaken for a broken connection.
const IDLE: std::time::Duration = std::time::Duration::from_secs(300);
const CONNECT: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether this run uses the socket at all.
pub(crate) fn enabled() -> bool {
    std::env::var_os("POLARIS_WS").as_deref() == Some(std::ffi::OsStr::new("on"))
}

type Stream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// One open socket, and what the next request on it may continue from.
pub(crate) struct Session {
    stream: Stream,
    /// The id of the last response this socket completed, and the items the
    /// request that produced it carried plus the items the server streamed
    /// back. The next request continues from the id and sends only what is
    /// beyond that baseline.
    ///
    /// `None` until a response completes here. A socket that has not yet
    /// answered anything has nothing to continue from.
    continuation: Option<Continuation>,
}

pub(crate) struct Continuation {
    pub(crate) response_id: String,
    /// Everything the server already holds: what was sent, followed by what
    /// it produced. **The model's own output is part of this** — diffing
    /// against the sent items alone would resend every answer it gave.
    pub(crate) baseline: Vec<serde_json::Value>,
    /// The request fields the continuation was produced under. A later
    /// request that differs in any of them is a different conversation to the
    /// server, and continuing from this id would splice the two.
    pub(crate) shape: Shape,
}

/// The parts of a request whose change invalidates a continuation. `input` is
/// deliberately absent: it is the part that grows, and it is compared
/// separately as a prefix. Per-request telemetry is absent for the same
/// reason it is on the other side — it changes every time and comparing it
/// would refuse every continuation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) model: String,
    pub(crate) instructions: String,
    pub(crate) tools: String,
    pub(crate) reasoning: String,
    pub(crate) cache_key: String,
}

impl Shape {
    pub(crate) fn of(body: &serde_json::Value) -> Self {
        let text = |key: &str| body.get(key).map(ToString::to_string).unwrap_or_default();
        Self {
            model: text("model"),
            instructions: text("instructions"),
            tools: text("tools"),
            reasoning: text("reasoning"),
            cache_key: text("prompt_cache_key"),
        }
    }
}

impl Session {
    /// Opens the socket. `base` is the HTTP base the run already resolved, so
    /// the socket cannot reach an endpoint the HTTP path would not have.
    pub(crate) async fn open(base: &str, token: &Token) -> Result<Self, ProviderError> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        // Both `ring` and `aws-lc-rs` are reachable from this dependency
        // graph, so rustls refuses to pick one and panics on first use.
        // Choose once, here, rather than letting the choice depend on which
        // crate happens to touch TLS first. `install_default` returning an
        // error means something already chose; that is fine, and the choice
        // is not overridden.
        static CHOSEN: std::sync::Once = std::sync::Once::new();
        CHOSEN.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });

        let url = format!("{base}/responses")
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        if !url.starts_with("wss://") && !url.starts_with("ws://") {
            return Err(ProviderError::Decode("websocket URL is not ws".into()));
        }
        let mut request = url
            .into_client_request()
            .map_err(|error| ProviderError::Decode(format!("websocket request: {error}")))?;
        let headers = request.headers_mut();
        let set = |headers: &mut tokio_tungstenite::tungstenite::http::HeaderMap,
                   name: &'static str,
                   value: &str|
         -> Result<(), ProviderError> {
            let value = value
                .parse()
                .map_err(|_| ProviderError::Decode(format!("header {name} is not sendable")))?;
            headers.insert(name, value);
            Ok(())
        };
        set(
            headers,
            "authorization",
            &format!("Bearer {}", token.access_token),
        )?;
        set(headers, "chatgpt-account-id", &token.account_id)?;
        set(headers, "openai-beta", BETA)?;
        set(headers, "originator", "polaris")?;

        let (stream, _response) = tokio::time::timeout(
            CONNECT,
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, None),
        )
        .await
        .map_err(|_| ProviderError::Decode("websocket connect timed out".into()))?
        .map_err(|error| ProviderError::Decode(format!("websocket upgrade: {error}")))?;
        Ok(Self {
            stream,
            continuation: None,
        })
    }

    /// The continuation this socket can offer for `shape`, if any.
    pub(crate) fn continuation_for(&self, shape: &Shape) -> Option<&Continuation> {
        self.continuation.as_ref().filter(|held| &held.shape == shape)
    }

    /// Why this socket cannot continue, in the terms the reader needs. For
    /// diagnostics while the transport is behind a flag.
    pub(crate) fn why_no_continuation(
        &self,
        shape: &Shape,
        input: &[serde_json::Value],
    ) -> String {
        match &self.continuation {
            None => "前の応答がない".to_owned(),
            Some(held) if &held.shape != shape => "要求の形が変わった".to_owned(),
            Some(held) => {
                let at = held
                    .baseline
                    .iter()
                    .zip(input)
                    .position(|(a, b)| a != b);
                match at {
                    Some(at) => format!(
                        "{at}件目が違う\n      保持: {}\n      今回: {}",
                        &held.baseline[at].to_string().chars().take(220).collect::<String>(),
                        &input[at].to_string().chars().take(220).collect::<String>(),
                    ),
                    None => format!(
                        "短い（保持 {}件 / 今回 {}件）",
                        held.baseline.len(),
                        input.len()
                    ),
                }
            }
        }
    }

    /// Sends one `response.create` and folds the reply into a completion.
    ///
    /// The frames the server sends are the same JSON objects the HTTP path
    /// receives inside SSE `data:` lines, so they go through the same folder.
    /// Two event types exist only here and are stepped over rather than
    /// guessed at.
    pub(crate) async fn round_trip(
        &mut self,
        mut body: serde_json::Value,
        sent_items: Vec<serde_json::Value>,
        observer: Option<&dyn crate::ResponseObserver>,
    ) -> Result<(crate::CompletionResponse, crate::codex::metrics::TokenUsage), ProviderError> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        body["type"] = serde_json::Value::String("response.create".into());
        let text = serde_json::to_string(&body)
            .map_err(|error| ProviderError::Decode(format!("websocket payload: {error}")))?;
        self.stream
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| ProviderError::Decode(format!("websocket send: {error}")))?;

        let mut folder = crate::codex::Folder::default();
        let mut produced: Vec<serde_json::Value> = Vec::new();
        let mut response_id = String::new();
        loop {
            let frame = tokio::time::timeout(IDLE, self.stream.next())
                .await
                .map_err(|_| ProviderError::Decode("websocket idle timeout".into()))?;
            let Some(frame) = frame else {
                return Err(ProviderError::Decode(
                    "websocket closed before response.completed".into(),
                ));
            };
            let frame =
                frame.map_err(|error| ProviderError::Decode(format!("websocket read: {error}")))?;
            let text = match frame {
                Message::Text(text) => text.to_string(),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(_) => {
                    return Err(ProviderError::Decode(
                        "websocket closed before response.completed".into(),
                    ));
                }
                Message::Binary(_) => {
                    return Err(ProviderError::Decode("unexpected binary frame".into()));
                }
            };
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|error| ProviderError::Decode(format!("frame is not JSON: {error}")))?;
            match value.get("type").and_then(serde_json::Value::as_str) {
                // An error frame without a status is not an error. Treating
                // one as a failure would end a run the server did not end.
                Some("error") if value.get("status").is_some() => {
                    return Err(ProviderError::Http(format!("websocket error: {text}")));
                }
                // Carried only on this transport. Nothing here consumes them,
                // and guessing at their meaning is worse than stepping over
                // them.
                Some("codex.rate_limits")
                | Some("response.metadata")
                | Some("responsesapi.websocket_timing") => continue,
                _ => {}
            }
            if let Some(item) = value.get("item")
                && value.get("type").and_then(serde_json::Value::as_str)
                    == Some("response.output_item.done")
            {
                produced.push(item.clone());
            }
            if value.get("type").and_then(serde_json::Value::as_str) == Some("response.completed") {
                response_id = value
                    .pointer("/response/id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
            }
            folder.take_event(
                crate::sse::SseEvent {
                    event: None,
                    data: text,
                },
                observer,
            )?;
            if folder.is_complete() {
                break;
            }
        }
        let usage = folder.usage();
        let shape = Shape::of(&body);
        let completed = folder.finish()?;
        // Only a response that named itself can be continued from. Everything
        // the server now holds is what was sent plus what it produced.
        self.continuation = (!response_id.is_empty()).then(|| {
            let mut baseline = sent_items;
            baseline.extend(produced);
            Continuation {
                response_id,
                baseline,
                shape,
            }
        });
        Ok((completed, usage))
    }
}

/// The items request N+1 must send, given what the server already holds.
///
/// `None` when the new input is not a strict extension of the baseline — a
/// rewritten history is a different conversation, and continuing from an id
/// that stands for the old one would send the server a story it never heard.
/// The caller then sends everything and omits the continuation.
pub(crate) fn delta(
    baseline: &[serde_json::Value],
    input: &[serde_json::Value],
) -> Option<Vec<serde_json::Value>> {
    let (prefix, rest) = input.split_at_checked(baseline.len())?;
    let same = prefix
        .iter()
        .zip(baseline)
        .all(|(held, sent)| comparable(held) == comparable(sent));
    same.then(|| rest.to_vec())
}

/// The part of an item that both sides agree on.
///
/// The server hands back what it recorded, which carries an `id` it issued
/// and a `status` it decided. Polaris renders its own history from its own
/// types and sends neither — it never sends an id at all. Comparing the raw
/// items therefore finds a difference on the first tool call every time, and
/// the continuation is refused on every request: correct, and silently
/// expensive. Only the fields polaris actually round-trips take part.
fn comparable(item: &serde_json::Value) -> serde_json::Value {
    let mut held = item.clone();
    if let Some(object) = held.as_object_mut() {
        object.remove("id");
        object.remove("status");
    }
    held
}

#[cfg(test)]
#[path = "codex_ws/tests.rs"]
mod tests;
