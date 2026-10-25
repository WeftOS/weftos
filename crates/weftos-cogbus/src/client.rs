//! The cog side of cogbus: connect, declare, publish, subscribe, list.
//!
//! ```no_run
//! # async fn f() -> Result<(), weftos_cogbus::ClientError> {
//! let bus = weftos_cogbus::Client::from_env().await?;
//! let seq = bus.publish("telemetry", "venq-board", "telemetry.v1", 17, serde_json::json!({})).await?;
//! let mut sub = bus.subscribe("telemetry/*", "telemetry.v1").await?;
//! while let Some(m) = sub.recv().await { let _ = (m.seq, m.body); }
//! # let _ = seq; Ok(()) }
//! ```
//!
//! One background task reads the socket and routes answers to their
//! request and deliveries to their [`Subscription`]. A subscription buffers
//! up to [`SUB_BUFFER`] messages; past that the newest are dropped here and
//! the reader sees the gap in `seq`.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::proto::{
    Delivery, ErrorCode, Event, InputSpec, OutputSpec, Request, read_frame, write_frame,
};

/// Messages a subscription holds before it drops.
pub const SUB_BUFFER: usize = 1024;

/// A client call failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// `WEFTOS_COGBUS` / `WEFTOS_COGBUS_TOKEN` missing.
    #[error("{0} is not set (no cogbus on this node)")]
    NoEnv(&'static str),
    /// Socket failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The broker refused the request.
    #[error("refused ({code:?}): {message}")]
    Refused {
        /// Machine code.
        code: ErrorCode,
        /// Reason.
        message: String,
    },
    /// The connection is gone.
    #[error("connection closed")]
    Closed,
    /// A frame did not encode or decode.
    #[error("protocol: {0}")]
    Protocol(String),
}

type Answer = Result<Value, ClientError>;

/// Runs in the reader task on the answer (and its descriptors).
type Hook = Box<dyn FnOnce(Result<(Value, Vec<OwnedFd>), ClientError>) + Send>;

enum Pending {
    Call(oneshot::Sender<Answer>),
    Subscribe(oneshot::Sender<Result<Subscription, ClientError>>),
    Hook(Hook),
}

#[derive(Default)]
pub(crate) struct Shared {
    closed: std::sync::atomic::AtomicBool,
    pending: Mutex<HashMap<u64, Pending>>,
    subs: Mutex<HashMap<u64, mpsc::Sender<Delivery>>>,
    pub(crate) streams: Arc<crate::stream::Routes>,
}

/// A connection to the node's broker.
pub struct Client {
    pub(crate) writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    pub(crate) shared: Arc<Shared>,
    next: AtomicU64,
    reader: JoinHandle<()>,
    instance_id: String,
}

/// Messages of one subscription.
#[derive(Debug)]
pub struct Subscription {
    /// Subscription id (for [`Client::unsubscribe`]).
    pub id: u64,
    rx: mpsc::Receiver<Delivery>,
}

impl Subscription {
    /// Next message; `None` once the connection or subscription is gone.
    pub async fn recv(&mut self) -> Option<Delivery> {
        self.rx.recv().await
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("instance_id", &self.instance_id)
            .finish_non_exhaustive()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Client {
    /// Connect with [`crate::ENV_SOCKET`] and [`crate::ENV_TOKEN`].
    pub async fn from_env() -> Result<Self, ClientError> {
        let path =
            std::env::var(crate::ENV_SOCKET).map_err(|_| ClientError::NoEnv(crate::ENV_SOCKET))?;
        let token =
            std::env::var(crate::ENV_TOKEN).map_err(|_| ClientError::NoEnv(crate::ENV_TOKEN))?;
        Self::connect(Path::new(&path), &token).await
    }

    /// Connect to `path` and say hello with `token`.
    pub async fn connect(path: &Path, token: &str) -> Result<Self, ClientError> {
        let (r, w) = UnixStream::connect(path).await?.into_split();
        let shared = Arc::new(Shared::default());
        let sh = shared.clone();
        let reader = tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(crate::stream::fdpass::FdReader::new(r));
            while let Ok(Some(frame)) = read_frame(&mut r).await {
                match serde_json::from_slice::<Event>(&frame) {
                    Ok(ev) => {
                        let fds = r.get_mut().take(fd_count(&ev));
                        route(&sh, ev, fds)
                    }
                    Err(e) => {
                        tracing::debug!(target: "cogbus", error = %e, "unreadable broker frame")
                    }
                }
            }
            // Wake every waiter: the connection is gone.
            sh.closed.store(true, Ordering::SeqCst);
            sh.pending.lock().map(|mut p| p.clear()).ok();
            sh.subs.lock().map(|mut s| s.clear()).ok();
            sh.streams.close_all();
        });
        let mut c = Self {
            writer: Arc::new(tokio::sync::Mutex::new(w)),
            shared,
            next: AtomicU64::new(1),
            reader,
            instance_id: String::new(),
        };
        let hello = c
            .call(|id| Request::Hello {
                id,
                token: token.to_string(),
            })
            .await?;
        c.instance_id = hello["instance_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        Ok(c)
    }

    /// Instance id the broker knows this connection as.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    async fn send(&self, req: &Request) -> Result<(), ClientError> {
        let mut w = self.writer.lock().await;
        write_frame(&mut *w, req)
            .await
            .map_err(|e| ClientError::Protocol(e.to_string()))
    }

    /// Park `p` under `id`, unless the reader is already gone (checked after
    /// the insert, so a reader that ends meanwhile either drops it or is seen).
    fn park(&self, id: u64, p: Pending) -> Result<(), ClientError> {
        self.shared
            .pending
            .lock()
            .map_err(|_| ClientError::Closed)?
            .insert(id, p);
        if self.shared.closed.load(Ordering::SeqCst) {
            self.shared.pending.lock().map(|mut m| m.remove(&id)).ok();
            return Err(ClientError::Closed);
        }
        Ok(())
    }

    /// A call whose answer (with its descriptors) `hook` turns into `T`
    /// inside the reader task, before any later frame is read.
    pub(crate) async fn call_hook<T: Send + 'static>(
        &self,
        make: impl FnOnce(u64) -> Request,
        hook: impl FnOnce(Result<(Value, Vec<OwnedFd>), ClientError>) -> Result<T, ClientError>
        + Send
        + 'static,
    ) -> Result<T, ClientError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let f: Hook = Box::new(move |r| {
            let _ = tx.send(hook(r));
        });
        self.park(id, Pending::Hook(f))?;
        self.send(&make(id)).await?;
        rx.await.map_err(|_| ClientError::Closed)?
    }

    async fn call(&self, make: impl FnOnce(u64) -> Request) -> Answer {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.park(id, Pending::Call(tx))?;
        self.send(&make(id)).await?;
        rx.await.map_err(|_| ClientError::Closed)?
    }

    /// Confirm or narrow the outputs and inputs of this connection.
    pub async fn declare(&self, outputs: Vec<OutputSpec>, inputs: Vec<InputSpec>) -> Answer {
        self.call(|id| Request::Declare {
            id,
            outputs,
            inputs,
        })
        .await
    }

    /// Publish one reading; returns its broker `seq`.
    pub async fn publish(
        &self,
        kind: &str,
        source_id: &str,
        schema: &str,
        t_ns: u64,
        body: Value,
    ) -> Result<u64, ClientError> {
        let v = self
            .call(|id| Request::Publish {
                id,
                topic_kind: kind.into(),
                source_id: source_id.into(),
                schema: schema.into(),
                t_ns,
                body,
            })
            .await?;
        v["seq"]
            .as_u64()
            .ok_or_else(|| ClientError::Protocol("publish answer without seq".into()))
    }

    /// Subscribe to `<kind>/<source_id>` or `<kind>/*`.
    pub async fn subscribe(&self, topic: &str, schema: &str) -> Result<Subscription, ClientError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.park(id, Pending::Subscribe(tx))?;
        self.send(&Request::Subscribe {
            id,
            topic: topic.into(),
            schema: schema.into(),
        })
        .await?;
        rx.await.map_err(|_| ClientError::Closed)?
    }

    /// Drop a subscription (reading or stream).
    pub async fn unsubscribe(&self, sub: u64) -> Result<(), ClientError> {
        self.call(|id| Request::Unsubscribe { id, sub }).await?;
        self.shared.subs.lock().map(|mut s| s.remove(&sub)).ok();
        Ok(())
    }

    /// Topics of this instance's project.
    pub async fn list(&self) -> Answer {
        self.call(|id| Request::List { id }).await
    }
}

/// Descriptors the broker attached to `ev`.
fn fd_count(ev: &Event) -> usize {
    match ev {
        Event::Ok { result, .. } => result["fds"].as_u64().unwrap_or(0) as usize,
        Event::Stream(n) => n.fds(),
        _ => 0,
    }
}

/// Route one broker frame. A subscribe answer opens the subscription's
/// channel here, before any later frame is read, so no delivery for it can
/// arrive ahead of its channel.
fn route(sh: &Shared, ev: Event, fds: Vec<OwnedFd>) {
    match ev {
        Event::Stream(n) => sh.streams.notice(n, fds),
        Event::Msg(d) => {
            if let Ok(subs) = sh.subs.lock()
                && let Some(tx) = subs.get(&d.sub)
            {
                let _ = tx.try_send(d);
            }
        }
        Event::Ok { id, result } => match sh.pending.lock().ok().and_then(|mut p| p.remove(&id)) {
            Some(Pending::Call(tx)) => {
                let _ = tx.send(Ok(result));
            }
            Some(Pending::Hook(f)) => f(Ok((result, fds))),
            Some(Pending::Subscribe(tx)) => {
                let Some(sub) = result["sub"].as_u64() else {
                    let _ = tx.send(Err(ClientError::Protocol(
                        "subscribe answer without sub".into(),
                    )));
                    return;
                };
                let (dtx, rx) = mpsc::channel(SUB_BUFFER);
                sh.subs.lock().map(|mut s| s.insert(sub, dtx)).ok();
                let _ = tx.send(Ok(Subscription { id: sub, rx }));
            }
            None => {}
        },
        Event::Error { id, code, message } => {
            let err = || ClientError::Refused {
                code,
                message: message.clone(),
            };
            match sh.pending.lock().ok().and_then(|mut p| p.remove(&id)) {
                Some(Pending::Call(tx)) => {
                    let _ = tx.send(Err(err()));
                }
                Some(Pending::Subscribe(tx)) => {
                    let _ = tx.send(Err(err()));
                }
                Some(Pending::Hook(f)) => f(Err(err())),
                None => tracing::debug!(target: "cogbus", ?code, %message, "broker error"),
            }
        }
    }
}
