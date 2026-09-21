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

type Stream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The proxy this socket must dial through, if the environment names one.
///
/// A confined run is often allowed exactly one outbound destination, so a
/// socket that dials the host directly is refused by the kernel before it can
/// be measured at all. The HTTP path already honours these variables through
/// reqwest; the socket honoured none of them, which is why it could not be
/// measured under confinement.
///
/// Kept as a pure function of the four inputs so the precedence and the
/// `NO_PROXY` exemption can be tested without touching process environment.
fn proxy_for(
    host: &str,
    https: Option<&str>,
    all: Option<&str>,
    no_proxy: Option<&str>,
) -> Option<String> {
    let exempt = no_proxy.is_some_and(|list| {
        list.split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .any(|entry| {
                entry == "*"
                    || host == entry
                    || host.ends_with(&format!(".{}", entry.trim_start_matches('.')))
            })
    });
    if exempt {
        return None;
    }
    let chosen = https
        .filter(|v| !v.trim().is_empty())
        .or(all.filter(|v| !v.trim().is_empty()))?;
    let authority = chosen
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    (!authority.is_empty()).then(|| authority.to_owned())
}

fn proxy_from_environment(host: &str) -> Option<String> {
    let read = |name: &str| std::env::var(name).ok();
    let https = read("HTTPS_PROXY").or_else(|| read("https_proxy"));
    let all = read("ALL_PROXY").or_else(|| read("all_proxy"));
    let no_proxy = read("NO_PROXY").or_else(|| read("no_proxy"));
    proxy_for(host, https.as_deref(), all.as_deref(), no_proxy.as_deref())
}

/// Opens a tunnel to `host:port` through an HTTP proxy.
///
/// Only the tunnel is established here; the TLS handshake and the upgrade
/// happen on top of the returned stream exactly as they would on a direct
/// connection, so the peer certificate is still the origin's.
async fn tunnel(
    proxy: &str,
    host: &str,
    port: u16,
) -> Result<tokio::net::TcpStream, ProviderError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(proxy)
        .await
        .map_err(|error| ProviderError::Decode(format!("proxy connect: {error}")))?;
    let request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| ProviderError::Decode(format!("proxy request: {error}")))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err(ProviderError::Decode("proxy response is too long".into()));
        }
        match stream.read(&mut byte).await {
            Ok(0) => return Err(ProviderError::Decode("proxy closed the tunnel".into())),
            Ok(_) => head.push(byte[0]),
            Err(error) => return Err(ProviderError::Decode(format!("proxy response: {error}"))),
        }
    }
    let status = String::from_utf8_lossy(&head);
    let first = status.lines().next().unwrap_or_default();
    // The status line is the only part that decides; the proxy may refuse a
    // destination that is not on its allowlist, and that refusal must not be
    // mistaken for a transport fault.
    if !first
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code.starts_with('2'))
    {
        return Err(ProviderError::Decode(format!(
            "proxy refused the tunnel: {first}"
        )));
    }
    Ok(stream)
}

/// One open socket, and what the next request on it may continue from.
pub(crate) struct Session {
    stream: Stream,
    /// What this socket can continue from, one entry per request shape.
    ///
    /// **One slot was not enough.** Parent and subagents share a single
    /// provider handle (`agent::run`'s `provider_pool`), and a tier-routed
    /// turn asks a different model, so requests of several shapes arrive on
    /// the same socket. With a single slot each one overwrote the previous,
    /// and the parent's next turn found the child's continuation, failed the
    /// shape check, and re-sent its whole history — the transport was paying
    /// its cost and buying nothing exactly when the run was busiest.
    ///
    /// Bounded: the server holds the state behind each id, and a run that
    /// keeps opening new shapes should not grow this without limit. The
    /// oldest entry goes first.
    continuations: Continuations,
}

/// The continuations one socket holds, one per request shape.
#[derive(Default)]
pub(crate) struct Continuations(std::collections::VecDeque<Continuation>);

impl Continuations {
    pub(crate) fn get(&self, shape: &Shape) -> Option<&Continuation> {
        self.0.iter().find(|held| &held.shape == shape)
    }

    /// Replaces this shape's entry. A differently shaped request must not
    /// evict a continuation it never used.
    pub(crate) fn remember(&mut self, held: Continuation) {
        self.0.retain(|other| other.shape != held.shape);
        self.0.push_back(held);
        while self.0.len() > CONTINUATIONS {
            self.0.pop_front();
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// How many distinct request shapes one socket keeps a continuation for.
/// A root, its tier-routed calls and a wave of subagents fit well inside
/// this; beyond it the oldest is dropped and that shape simply re-sends.
const CONTINUATIONS: usize = 8;

/// **A continuation cannot outlive its socket.** Carrying the chain into
/// the next process was tried on 2026-09-21 and the server refused it:
/// `400 invalid_request_error: Invalid previous_response_id`. Three
/// requests of a four-turn task failed that way, fell back to HTTP, and
/// the run cost more than one that never tried — $0.389 against $0.318.
///
/// It matters because the server's prompt cache is reachable only through
/// the chain: of 212 measured websocket requests, all 120 cache hits
/// carried a continuation and every one of the 72 without one missed. So
/// a turn that starts a new process pays for its whole history, and
/// nothing in this transport changes that. Making it cheaper means fewer
/// processes, not a longer-lived id.
pub(crate) struct Continuation {
    pub(crate) response_id: String,
    /// Everything the server already holds: what was sent, followed by what
    /// it produced. **The model's own output is part of this** — diffing
    /// against the sent items alone would resend every answer it gave.
    pub(crate) baseline: Vec<serde_json::Value>,
    /// The request fields the continuation was produced under. A later
    /// request that differs in any of them is a different conversation to
    /// the server, and continuing from this id would splice the two.
    pub(crate) shape: Shape,
}

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

        let host = request
            .uri()
            .host()
            .ok_or_else(|| ProviderError::Decode("websocket URL has no host".into()))?
            .to_owned();
        let port = request.uri().port_u16().unwrap_or(443);
        let (stream, _response) = match proxy_from_environment(&host) {
            Some(proxy) => {
                let tunnelled = tokio::time::timeout(CONNECT, tunnel(&proxy, &host, port))
                    .await
                    .map_err(|_| ProviderError::Decode("proxy connect timed out".into()))??;
                tokio::time::timeout(
                    CONNECT,
                    tokio_tungstenite::client_async_tls_with_config(request, tunnelled, None, None),
                )
                .await
                .map_err(|_| ProviderError::Decode("websocket connect timed out".into()))?
                .map_err(|error| ProviderError::Decode(format!("websocket upgrade: {error}")))?
            }
            None => tokio::time::timeout(
                CONNECT,
                tokio_tungstenite::connect_async_tls_with_config(request, None, false, None),
            )
            .await
            .map_err(|_| ProviderError::Decode("websocket connect timed out".into()))?
            .map_err(|error| ProviderError::Decode(format!("websocket upgrade: {error}")))?,
        };
        Ok(Self {
            stream,
            continuations: Continuations::default(),
        })
    }

    /// The continuation this socket can offer for `shape`, if any.
    pub(crate) fn continuation_for(&self, shape: &Shape) -> Option<&Continuation> {
        self.continuations.get(shape)
    }

    /// Why this socket cannot continue, in the terms the reader needs. For
    /// diagnostics while the transport is behind a flag.
    pub(crate) fn why_no_continuation(&self, shape: &Shape, input: &[serde_json::Value]) -> String {
        why_refused(
            self.continuation_for(shape),
            self.continuations.len(),
            input,
        )
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
        if !response_id.is_empty() {
            let mut baseline = sent_items;
            baseline.extend(produced);
            // Replace this shape's entry rather than the whole slot, so a
            // differently shaped request does not evict the one it never
            // used.
            self.continuations.remember(Continuation {
                response_id,
                baseline,
                shape,
            });
        }
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
    // `same_item` is not symmetric — the server's record comes first — and
    // the zip here reads the other way round.
    let same = prefix
        .iter()
        .zip(baseline)
        .all(|(sent, held)| same_item(held, sent));
    same.then(|| rest.to_vec())
}

/// The words for a refusal, given what the socket held for that shape.
///
/// It has to weigh the items exactly as `delta` does. Scanning the raw items
/// instead names a position `delta` accepts — on the 2026-09-20 todo run that
/// made every refusal look like the empty `content` the server adds, and hid
/// whatever the real cause was.
fn why_refused(held: Option<&Continuation>, kept: usize, input: &[serde_json::Value]) -> String {
    let Some(held) = held else {
        return if kept == 0 {
            "前の応答がない".to_owned()
        } else {
            format!("この形の継続がない（保持 {kept}形）")
        };
    };
    let at = held
        .baseline
        .iter()
        .zip(input)
        .position(|(a, b)| !same_item(a, b));
    match at {
        Some(at) => match first_difference(&held.baseline[at], &input[at], "") {
            Some((path, was, now)) => {
                format!("{at}件目の{path}が違う\n      保持: {was}\n      今回: {now}")
            }
            None => format!("{at}件目が違う（差が見つかりません）"),
        },
        None => format!(
            "短い（保持 {}件 / 今回 {}件）",
            held.baseline.len(),
            input.len()
        ),
    }
}

/// Where two items first differ, as a path into them.
///
/// The message used to print the head of each raw item. Three separate
/// causes have now hidden behind that: a field far inside a long item shows
/// as two identical-looking heads, and the reader blames whatever the head
/// happens to show. Name the field instead.
fn first_difference(
    held: &serde_json::Value,
    sent: &serde_json::Value,
    path: &str,
) -> Option<(String, String, String)> {
    let cut = |value: &serde_json::Value| value.to_string().chars().take(160).collect::<String>();
    match (held, sent) {
        // Only what polaris sends takes part, the same way `same_item`
        // weighs it: a field the server kept beside the item is not a
        // difference, and must not be named as one.
        (serde_json::Value::Object(was), serde_json::Value::Object(now)) => {
            now.iter().find_map(|(key, value)| match was.get(key) {
                Some(other) => first_difference(other, value, &format!("{path}.{key}")),
                None => Some((format!("{path}.{key}"), "（無し）".to_owned(), cut(value))),
            })
        }
        (serde_json::Value::Array(was), serde_json::Value::Array(now)) => {
            for (index, (one, other)) in was.iter().zip(now).enumerate() {
                if let Some(found) = first_difference(one, other, &format!("{path}[{index}]")) {
                    return Some(found);
                }
            }
            (was.len() != now.len()).then(|| {
                (
                    path.to_owned(),
                    format!("{}件", was.len()),
                    format!("{}件", now.len()),
                )
            })
        }
        _ => (held != sent).then(|| (path.to_owned(), cut(held), cut(sent))),
    }
}

/// Whether what the server recorded is still the item polaris would send.
///
/// The server hands back more than it was given: an `id` it issued, a
/// `status` and a `phase` it decided, and the lists it had nothing to put in
/// (`content` on a reasoning item, `annotations` and `logprobs` inside every
/// message part). Polaris renders its own history from its own types and
/// sends none of them. Naming each one as it turned up cost three separate
/// measurements on 2026-09-20 — every one of them a continuation refused on
/// every request, correct and silently expensive — so the rule is the one
/// those exclusions were approximating: only what polaris sends takes part.
/// What the server keeps beside it is the server's business.
fn same_item(held: &serde_json::Value, sent: &serde_json::Value) -> bool {
    match (held, sent) {
        (serde_json::Value::Object(was), serde_json::Value::Object(now)) => now
            .iter()
            .all(|(key, value)| was.get(key).is_some_and(|other| same_item(other, value))),
        (serde_json::Value::Array(was), serde_json::Value::Array(now)) => {
            was.len() == now.len() && was.iter().zip(now).all(|(a, b)| same_item(a, b))
        }
        _ => held == sent,
    }
}

#[cfg(test)]
#[path = "codex_ws/tests.rs"]
mod tests;
