//! The broker's Unix socket (ADR-122 section 4).
//!
//! The socket is created mode 0660 (owner and group; assigning the `cogbus`
//! group is the installer's job, not done here). One task per connection:
//!
//! - the first frame must be `hello` with a registered token, within
//!   [`HELLO_TIMEOUT`]; when the instance was registered with a uid, the
//!   peer's `SO_PEERCRED` / `getpeereid` uid must match;
//! - a frame that stalls mid-way for [`FRAME_TIMEOUT`], an oversize length
//!   or unparseable JSON gets an error frame (when it can) and the
//!   connection is closed;
//! - a write to the cog that does not finish in [`FRAME_TIMEOUT`] closes it;
//! - revoking the instance closes it with a `revoked` error;
//! - every refusal is recorded ([`Broker::record_refusal`]): the activity
//!   log, and too many in a window stop the instance (which closes it).
//!
//! A connection that has said hello may then stay idle: a subscriber can
//! wait as long as its writer is quiet.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::broker::{Broker, BusError, Session};
use crate::proto::{ErrorCode, Event, FrameError, MAX_BODY, Request, read_frame, write_frame};

/// Time a new connection has to say hello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Time a started frame (read or write) has to complete.
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// How often calls past their deadline are answered with a timeout.
pub const CALL_SWEEP: Duration = Duration::from_millis(50);

/// The listening socket. Dropping it stops accepting and removes the file.
pub struct ServerHandle {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl ServerHandle {
    /// Socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind `path` (replacing a stale socket file, never another kind of file)
/// and serve `broker` on it.
pub async fn serve(broker: Arc<Broker>, path: &Path) -> std::io::Result<ServerHandle> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    let task = tokio::spawn(async move {
        let mut sweep = tokio::time::interval(CALL_SWEEP);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = sweep.tick() => {
                    broker.expire_calls();
                }
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let b = broker.clone();
                        tokio::spawn(async move { connection(b, stream).await });
                    }
                    Err(e) => {
                        tracing::warn!(target: "cogbus", error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }
    });
    Ok(ServerHandle {
        path: path.to_path_buf(),
        task,
    })
}

enum Incoming {
    Req(Request),
    Bad(ErrorCode, String),
}

/// Reads frames into `tx` until end of stream or the first bad frame.
async fn reader(mut r: tokio::net::unix::OwnedReadHalf, tx: mpsc::Sender<Incoming>) {
    loop {
        let frame = match read_frame_timed(&mut r).await {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err((code, m)) => {
                let _ = tx.send(Incoming::Bad(code, m)).await;
                return;
            }
        };
        let item = match serde_json::from_slice::<Request>(&frame) {
            Ok(req) => Incoming::Req(req),
            Err(e) => Incoming::Bad(ErrorCode::BadRequest, format!("bad request: {e}")),
        };
        let stop = matches!(item, Incoming::Bad(..));
        if tx.send(item).await.is_err() || stop {
            return;
        }
    }
}

/// One frame, with the mid-frame stall bounded once its length has arrived.
async fn read_frame_timed(
    r: &mut tokio::net::unix::OwnedReadHalf,
) -> Result<Option<Vec<u8>>, (ErrorCode, String)> {
    use tokio::io::AsyncReadExt;
    let mut first = [0u8; 1];
    match r.read(&mut first).await {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err((ErrorCode::BadRequest, e.to_string())),
    }
    let mut chained = tokio::io::AsyncReadExt::chain(&first[..], &mut *r);
    match timeout(FRAME_TIMEOUT, read_frame(&mut chained)).await {
        Ok(Ok(Some(f))) => Ok(Some(f)),
        Ok(Ok(None)) => Err((ErrorCode::BadRequest, "truncated frame".into())),
        Ok(Err(FrameError::Oversize(n))) => {
            Err((ErrorCode::TooLarge, format!("frame of {n} bytes")))
        }
        Ok(Err(e)) => Err((ErrorCode::BadRequest, e.to_string())),
        Err(_) => Err((ErrorCode::BadRequest, "frame stalled".into())),
    }
}

async fn send(w: &mut OwnedWriteHalf, ev: &Event) -> bool {
    matches!(timeout(FRAME_TIMEOUT, write_frame(w, ev)).await, Ok(Ok(())))
}

fn error(id: u64, e: BusError) -> Event {
    Event::Error {
        id,
        code: e.code,
        message: e.message,
    }
}

async fn connection(broker: Arc<Broker>, stream: UnixStream) {
    let peer_uid = stream.peer_cred().ok().map(|c| c.uid());
    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::channel(16);
    let read_task = tokio::spawn(reader(r, tx));
    let session = hello(&broker, &mut rx, &mut w, peer_uid).await;
    if let Some(mut s) = session {
        let mut ss = crate::stream::SessionStream::default();
        serve_session(&broker, &mut s, &mut ss, &mut rx, &mut w).await;
        broker.close(&s);
        crate::stream::serve::close(&broker, &ss);
    }
    read_task.abort();
}

async fn hello(
    broker: &Broker,
    rx: &mut mpsc::Receiver<Incoming>,
    w: &mut OwnedWriteHalf,
    peer_uid: Option<u32>,
) -> Option<Session> {
    let first = match timeout(HELLO_TIMEOUT, rx.recv()).await {
        Ok(Some(i)) => i,
        _ => return None,
    };
    let (id, token) = match first {
        Incoming::Req(Request::Hello { id, token }) => (id, token),
        Incoming::Req(other) => {
            let e = BusError::new(ErrorCode::BadRequest, "the first frame must be hello");
            send(w, &error(other.id(), e)).await;
            return None;
        }
        Incoming::Bad(code, m) => {
            send(w, &error(0, BusError::new(code, m))).await;
            return None;
        }
    };
    match broker.authenticate(&token, peer_uid) {
        Ok(s) => {
            let result = json!({
                "instance_id": s.instance_id(), "node_id": broker.node_id(), "max_body": MAX_BODY,
            });
            send(w, &Event::Ok { id, result }).await.then_some(s)
        }
        Err(e) => {
            broker.record_refusal(&e, "-");
            send(w, &error(id, e)).await;
            None
        }
    }
}

async fn serve_session(
    broker: &Broker,
    s: &mut Session,
    ss: &mut crate::stream::SessionStream,
    rx: &mut mpsc::Receiver<Incoming>,
    w: &mut OwnedWriteHalf,
) {
    let wake = s.wake();
    let mut revoked = s.revoked();
    let mut pushed = ss.take_events();
    let mut frames = s.take_pushed();
    loop {
        tokio::select! {
            item = rx.recv() => {
                let (ev, fds) = match item {
                    None => return,
                    Some(Incoming::Bad(code, m)) => {
                        let e = BusError::new(code, m);
                        broker.record_refusal(&e, s.instance_id());
                        send(w, &error(0, e)).await;
                        return;
                    }
                    Some(Incoming::Req(req)) => match crate::stream::serve::handle(broker, s, ss, req) {
                        Ok(answer) => answer,
                        Err(req) => match handle(broker, s, req) {
                            Some(ev) => (ev, Vec::new()),
                            // A call: answered when the provider replies.
                            None => continue,
                        },
                    },
                };
                if !crate::stream::serve::send_with(w, &ev, &fds).await {
                    return;
                }
            }
            Some(out) = async { match pushed.as_mut() { Some(p) => p.recv().await, None => None } } => {
                if !crate::stream::serve::send_with(w, &out.ev, &out.fds).await {
                    return;
                }
            }
            Some(ev) = async { match frames.as_mut() { Some(f) => f.recv().await, None => None } } => {
                if !send(w, &ev).await {
                    return;
                }
            }
            _ = wake.notified() => {
                for d in s.drain() {
                    if !send(w, &Event::Msg(d)).await {
                        return;
                    }
                }
            }
            _ = revoked.changed() => {
                let e = BusError::new(ErrorCode::Revoked, "instance revoked");
                send(w, &error(0, e)).await;
                return;
            }
        }
    }
}

/// Answer `req`; `None` for a call, which is answered later.
fn handle(broker: &Broker, s: &mut Session, req: Request) -> Option<Event> {
    let id = req.id();
    let r = match req {
        Request::Hello { .. } => Err(BusError::new(ErrorCode::BadRequest, "already said hello")),
        Request::Declare {
            outputs, inputs, ..
        } => broker.declare(s, outputs, inputs),
        Request::Publish {
            topic_kind,
            source_id,
            schema,
            t_ns,
            body,
            ..
        } => broker
            .publish(s, &topic_kind, &source_id, &schema, t_ns, body)
            .map(|seq| json!({ "seq": seq })),
        Request::Subscribe { topic, schema, .. } => broker
            .subscribe(s, &topic, &schema)
            .map(|sub| json!({ "sub": sub })),
        Request::Unsubscribe { sub, .. } => broker.unsubscribe(s, sub).map(|_| json!({})),
        Request::List { .. } => Ok(json!({ "topics": broker.list_for(s) })),
        Request::Serve {
            topic_kind,
            source_id,
            schema,
            ..
        } => broker.serve_topic(s, &topic_kind, &source_id, &schema),
        Request::Call {
            topic,
            schema,
            body,
            timeout_ms,
            ..
        } => match broker.call_service(s, id, &topic, &schema, body, timeout_ms) {
            Ok(()) => return None,
            Err(e) => Err(e),
        },
        Request::Reply {
            call, body, error, ..
        } => broker
            .reply_call(s, call, error.map_or(Ok(body), Err))
            .map(|_| json!({})),
        Request::StreamOpen { .. }
        | Request::StreamSubscribe { .. }
        | Request::StreamDemote { .. } => Err(BusError::new(ErrorCode::BadRequest, "stream request outside the stream handler")),
    };
    Some(match r {
        Ok(result) => Event::Ok { id, result },
        Err(e) => {
            broker.record_refusal(&e, s.instance_id());
            error(id, e)
        }
    })
}
