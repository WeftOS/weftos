//! TCP listener, WebSocket upgrade (where the pass key is checked), and the
//! per-connection pump. Accept-only: nothing in this crate opens an outbound
//! socket (tokio-tungstenite is built without its `connect` feature).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use xai_tool_protocol::HelloMsg;

use crate::hub::{Authed, Hub, Refusal, now_secs};
use crate::passkey::Role;

/// The only path the hub upgrades.
pub const WS_PATH: &str = "/v1/tools";
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// A socket silent this long is dropped; clients ping every 30 s.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
pub const MAX_CONNECTIONS: usize = 256;
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Parse a listen address and apply the loopback rule. Hostnames are refused
/// so the loopback check cannot be bypassed by name resolution.
pub fn check_bind(addr: &str, allow_non_loopback: bool) -> Result<SocketAddr, String> {
    let sa: SocketAddr = addr
        .parse()
        .map_err(|e| format!("listen address {addr:?} must be ip:port ({e})"))?;
    if !sa.ip().is_loopback() && !allow_non_loopback {
        return Err(format!(
            "refusing to bind {sa}: not loopback; pass --allow-non-loopback to expose the hub (pass keys are still required)"
        ));
    }
    Ok(sa)
}

fn refuse(r: &Refusal) -> ErrorResponse {
    http::Response::builder()
        .status(r.status)
        .body(Some(r.reason.clone()))
        .unwrap_or_else(|_| http::Response::new(Some("refused".into())))
}

fn bearer(req: &Request) -> Option<&str> {
    let v = req
        .headers()
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

fn role_of(req: &Request) -> Option<Role> {
    req.uri()
        .query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix("role="))
        .and_then(Role::parse)
}

/// Accept connections until `shutdown` resolves.
pub async fn serve(
    hub: Arc<Hub>,
    listener: TcpListener,
    shutdown: impl std::future::Future<Output = ()>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return,
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    tracing::warn!(%peer, "connection limit reached; dropping");
                    continue;
                };
                let hub = hub.clone();
                tokio::spawn(async move {
                    handle(hub, stream).await;
                    drop(permit);
                });
            }
        }
    }
}

// tungstenite fixes the callback's error type (`http::Response`), which is large.
#[allow(clippy::result_large_err)]
async fn handle(hub: Arc<Hub>, stream: TcpStream) {
    let outcome: Arc<Mutex<Option<Authed>>> = Arc::new(Mutex::new(None));
    let slot = outcome.clone();
    let hub_cb = hub.clone();
    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        if req.uri().path() != WS_PATH {
            return Err(refuse(&Refusal {
                status: 404,
                reason: "not found".into(),
            }));
        }
        let role = role_of(req).ok_or_else(|| {
            refuse(&Refusal {
                status: 400,
                reason: "role=harness or role=tool_server required".into(),
            })
        })?;
        let auth = hub_cb
            .authenticate(bearer(req), role, now_secs())
            .map_err(|r| refuse(&r))?;
        *slot.lock().expect("slot") = Some(auth);
        Ok(resp)
    };
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let ws =
        match tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(cfg)).await {
            Ok(ws) => ws,
            Err(e) => return tracing::debug!(error = %e, "upgrade refused or failed"),
        };
    let Some(auth) = outcome.lock().expect("slot").take() else {
        return;
    };
    pump(hub, ws, auth).await;
}

async fn close<S>(sink: &mut S, reason: &str)
where
    S: futures_util::Sink<Message> + Unpin,
{
    let frame = CloseFrame {
        code: CloseCode::Policy,
        reason: reason.chars().take(120).collect::<String>().into(),
    };
    let _ = sink.send(Message::Close(Some(frame))).await;
}

async fn pump<S>(hub: Arc<Hub>, ws: tokio_tungstenite::WebSocketStream<S>, auth: Authed)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let hello = tokio::time::timeout(HELLO_TIMEOUT, async {
        while let Some(Ok(m)) = stream.next().await {
            match m {
                Message::Text(t) => return serde_json::from_str::<HelloMsg>(t.as_ref()).ok(),
                Message::Ping(_) | Message::Pong(_) => {}
                _ => return None,
            }
        }
        None
    })
    .await;
    let Ok(Some(hello)) = hello else {
        return close(&mut sink, "expected hello within 10s").await;
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let (cid, ack) = match hub.register(&auth, &hello, tx.clone()) {
        Ok(v) => v,
        Err(r) => {
            tracing::info!(status = r.status, reason = %r.reason, "hello refused");
            return close(&mut sink, &r.reason).await;
        }
    };
    let _ = tx.send(Message::Text(ack.into()));
    tracing::info!(cid, kind = %hello.kind, sub = %auth.claims.sub, "connected");
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let is_close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || is_close {
                break;
            }
        }
        let _ = sink.close().await;
    });
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => hub.on_text(cid, t.as_ref()),
            Ok(Some(Ok(Message::Ping(p)))) => {
                let _ = tx.send(Message::Pong(p));
            }
            Ok(Some(Ok(Message::Pong(_)))) => {}
            Ok(Some(Ok(Message::Binary(_)))) => {
                let _ = tx.send(Message::Close(None));
                break;
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
            Ok(Some(Ok(Message::Frame(_)))) => {}
        }
    }
    hub.disconnect(cid);
    drop(tx);
    let _ = writer.await;
    tracing::info!(cid, "disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_default_and_other_addresses_need_the_flag() {
        assert!(check_bind("127.0.0.1:9490", false).is_ok());
        assert!(check_bind("[::1]:9490", false).is_ok());
        assert!(
            check_bind("0.0.0.0:9490", false)
                .unwrap_err()
                .contains("--allow-non-loopback")
        );
        assert!(check_bind("192.168.1.5:9490", false).is_err());
        assert!(check_bind("0.0.0.0:9490", true).is_ok());
    }

    #[test]
    fn hostnames_and_garbage_are_refused_not_resolved() {
        assert!(check_bind("localhost:9490", true).is_err());
        assert!(check_bind("", false).is_err());
        assert!(check_bind("127.0.0.1", false).is_err());
    }

    #[test]
    fn role_and_bearer_parsing() {
        let r = |u: &str, auth: Option<&str>| {
            let mut b = http::Request::builder().uri(u);
            if let Some(a) = auth {
                b = b.header("authorization", a);
            }
            b.body(()).unwrap()
        };
        assert_eq!(
            role_of(&r("/v1/tools?role=harness", None)),
            Some(Role::Harness)
        );
        assert_eq!(
            role_of(&r("/v1/tools?x=1&role=tool_server", None)),
            Some(Role::Server)
        );
        assert_eq!(role_of(&r("/v1/tools?role=bot_client", None)), None);
        assert_eq!(role_of(&r("/v1/tools", None)), None);
        assert_eq!(bearer(&r("/", Some("Bearer abc"))), Some("abc"));
        assert_eq!(bearer(&r("/", Some("bearer abc"))), Some("abc"));
        assert_eq!(bearer(&r("/", Some("Basic abc"))), None);
        assert_eq!(bearer(&r("/", None)), None);
    }
}
