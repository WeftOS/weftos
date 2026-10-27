//! The broker's Unix socket (ADR-122 section 4).
//!
//! The socket is created mode 0660 (owner and group), never wider. When the
//! caller names a group gid ([`ServeOptions::group`]) it is made the socket's
//! group if the process may (`root:cogbus 0660`); otherwise the group stays the
//! daemon's own and [`SocketReport`] says so. Creating the group and putting
//! cog uids in it is the owner-run installer's job. One task per connection:
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
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// How often expired blobs are dropped.
pub const BLOB_SWEEP: Duration = Duration::from_secs(30);
/// How often calls past their deadline are answered with a timeout.
pub const CALL_SWEEP: Duration = Duration::from_millis(50);

/// How the socket is bound.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServeOptions {
    /// Group to give the socket (gid of `cogbus`), when it exists.
    pub group: Option<u32>,
}

/// What the bound socket ended up as, as `stat` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketReport {
    /// Permission bits (always 0o660).
    pub mode: u32,
    /// Owner uid.
    pub uid: u32,
    /// Group gid.
    pub gid: u32,
    /// The requested group is now the socket's group.
    pub group_applied: bool,
    /// Why the group was not applied (none requested, or the chown failed).
    pub note: Option<String>,
}

/// A count held for as long as the guard lives.
struct Held(Arc<AtomicUsize>);

impl Held {
    /// Take one from `counter` unless it already holds `cap`.
    fn take(counter: &Arc<AtomicUsize>, cap: usize) -> Option<Held> {
        if counter.fetch_add(1, Ordering::SeqCst) >= cap {
            counter.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Held(counter.clone()))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Unauthenticated connections, counted per peer uid so one cog cannot use
/// them all up and keep the others from saying hello.
#[derive(Default)]
struct PerUid(std::sync::Mutex<std::collections::HashMap<Option<u32>, usize>>);

struct UidHeld(Arc<PerUid>, Option<u32>);

impl PerUid {
    fn take(this: &Arc<PerUid>, uid: Option<u32>, cap: usize) -> Option<UidHeld> {
        let mut m = this.0.lock().unwrap_or_else(|p| p.into_inner());
        let n = m.entry(uid).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(UidHeld(this.clone(), uid))
    }
}

impl Drop for UidHeld {
    fn drop(&mut self) {
        let mut m = self.0.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = m.get_mut(&self.1) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.1);
            }
        }
    }
}

/// What a connection holds until it has said hello.
struct PreHello {
    _all: Held,
    _uid: UidHeld,
}

/// The listening socket. Dropping it stops accepting and removes the file.
pub struct ServerHandle {
    path: PathBuf,
    report: SocketReport,
    task: JoinHandle<()>,
}

impl ServerHandle {
    /// Socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mode, owner and group of the socket file.
    pub fn report(&self) -> &SocketReport {
        &self.report
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
    serve_with(broker, path, ServeOptions::default()).await
}

/// [`serve`] with the socket's group.
pub async fn serve_with(
    broker: Arc<Broker>,
    path: &Path,
    opts: ServeOptions,
) -> std::io::Result<ServerHandle> {
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
    let note = match opts.group {
        None => Some("no cogbus group requested".to_string()),
        Some(gid) => match std::os::unix::fs::chown(path, None, Some(gid)) {
            Ok(()) => None,
            Err(e) => Some(format!("cannot give the socket to gid {gid}: {e}")),
        },
    };
    // Re-assert the mode after the chown: it never widens.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    let report = {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)?;
        SocketReport {
            mode: m.mode() & 0o7777,
            uid: m.uid(),
            gid: m.gid(),
            group_applied: opts.group.is_some_and(|g| g == m.gid()),
            note,
        }
    };
    if let (Some(n), Some(_)) = (&report.note, opts.group) {
        tracing::warn!(target: "cogbus", reason = %n, "cogbus socket keeps the daemon's group");
    }
    let task = tokio::spawn(async move {
        let mut sweep = tokio::time::interval(CALL_SWEEP);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let (total, prehello) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let per_uid = Arc::new(PerUid::default());
        let mut blob_sweep = tokio::time::interval(BLOB_SWEEP);
        blob_sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = sweep.tick() => {
                    broker.expire_calls();
                }
                _ = blob_sweep.tick() => {
                    broker.sweep_blobs();
                }
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        // Bounded before anything is read: a flood of idle
                        // connections cannot use up the node's descriptors.
                        let cfg = broker.config();
                        let uid = stream.peer_cred().ok().map(|c| c.uid());
                        let (Some(all), Some(pre), Some(mine)) = (
                            Held::take(&total, cfg.max_conns),
                            Held::take(&prehello, cfg.max_prehello_conns),
                            PerUid::take(&per_uid, uid, cfg.max_prehello_per_uid),
                        ) else {
                            tracing::debug!(target: "cogbus", ?uid, "connection refused: connection cap");
                            drop(stream);
                            continue;
                        };
                        let b = broker.clone();
                        tokio::spawn(async move {
                            let _all = all;
                            connection(b, stream, PreHello { _all: pre, _uid: mine }).await
                        });
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
        report,
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

async fn connection(broker: Arc<Broker>, stream: UnixStream, pre: PreHello) {
    let peer_uid = stream.peer_cred().ok().map(|c| c.uid());
    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::channel(16);
    let read_task = tokio::spawn(reader(r, tx));
    let session = hello(&broker, &mut rx, &mut w, peer_uid).await;
    // Said hello (or did not): either way it no longer counts as unauthenticated.
    drop(pre);
    if let Some(mut s) = session {
        let mut ss = crate::stream::SessionStream::for_conn(s.conn());
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
    let first = match timeout(Duration::from_millis(broker.config().hello_timeout_ms), rx.recv()).await {
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
    // Silence ladder: ping after `idle_ping_ms` without a frame, close
    // `pong_timeout_ms` later if still nothing. Only the cog's own frames
    // count; the broker's pushes to it do not.
    let (idle, grace) = (
        Duration::from_millis(broker.config().idle_ping_ms.max(1)),
        Duration::from_millis(broker.config().pong_timeout_ms.max(1)),
    );
    let mut pinged = false;
    let silence = tokio::time::sleep(idle);
    tokio::pin!(silence);
    loop {
        tokio::select! {
            _ = &mut silence => {
                if pinged {
                    let e = BusError::new(ErrorCode::Timeout, "idle: no answer to ping");
                    broker.record_refusal(&e, s.instance_id());
                    send(w, &error(0, e)).await;
                    return;
                }
                if !send(w, &Event::Ping).await {
                    return;
                }
                pinged = true;
                silence.as_mut().reset(tokio::time::Instant::now() + grace);
            }
            item = rx.recv() => {
                if item.is_some() {
                    pinged = false;
                    silence.as_mut().reset(tokio::time::Instant::now() + idle);
                }
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
        // Liveness only: the frame already reset the silence timer.
        Request::Pong { .. } => return None,
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
        Request::Subscribe { topic, schema, raw, .. } => broker
            .subscribe_with(s, &topic, &schema, raw)
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
        Request::BlobBegin { media_type, size, ttl_ms, .. } => broker.blob_begin(s, &media_type, size, ttl_ms),
        Request::BlobChunk { upload, offset, data, last, .. } => broker.blob_chunk(s, upload, offset, &data, last),
        Request::BlobGet { hash, offset, len, .. } => broker.blob_get(s, &hash, offset, len),
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
