//! Stream topics across nodes (ADR-122 section 14, B6d), on the existing
//! stream API and nothing below it.
//!
//! - Source: each served stream subscription is a *tap*: a broker-internal
//!   instance (`mesh-out-<n>`, the topic's project, one input) that connects
//!   to this broker's socket and reads the ring with `subscribe_stream`
//!   (drop-oldest). Records wait in a bounded buffer and leave as `chunk`
//!   frames, binary, only against the subscriber's byte credit and the cog's
//!   `bytes_per_sec`; the rest are dropped and counted, so the ring `seq`
//!   the subscriber sees has a gap.
//! - Subscriber: each remote stream subscription has a *proxy*: an internal
//!   instance (`mesh-in-<rsub>`, the project the audience names) that writes
//!   `<kind>/<source_id>` here with `StreamWriter::write_at`, keeping the
//!   source seq. Local cogs read it with `stream_subscribe` as any ring.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use clawft_types::cog_topics::{InputPolicy, TopicClass};

use super::MeshTopicHub;
use super::wire::{Credit, TopicReq};
use crate::broker::InstanceSpec;
use crate::proto::{InputSpec, OutputSpec};
use crate::stream::{StreamOptions, StreamRecord, StreamWriter};

/// Records a tap holds for its peer before it drops the oldest.
const TAP_BUFFER: usize = 256;
/// Largest record a proxy ring takes.
pub const MAX_RECORD: u64 = 64 * 1024;

/// The source side of one served stream subscription.
pub(crate) struct Tap {
    instance: String,
    buf: Arc<Mutex<VecDeque<StreamRecord>>>,
    lost: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

struct Proxy {
    instance: String,
    writer: Arc<Mutex<Option<StreamWriter>>>,
    task: tokio::task::JoinHandle<()>,
}

/// Both halves' stream state.
#[derive(Default)]
pub(crate) struct Streams {
    socket: std::sync::OnceLock<PathBuf>,
    proxies: Mutex<HashMap<u64, Proxy>>,
    next: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// 64 hex chars from the system's random source.
fn token() -> Option<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut b).ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

impl MeshTopicHub {
    /// The broker socket taps and proxies connect to; without it no stream
    /// crosses the mesh.
    pub fn set_socket(&self, path: PathBuf) {
        let _ = self.streams.socket.set(path);
    }

    /// Register an internal instance and run `f` with a client of it.
    fn spawn_internal<F, Fut>(&self, spec: InstanceSpec, f: F) -> Result<tokio::task::JoinHandle<()>, String>
    where
        F: FnOnce(crate::client::Client) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let socket = self.streams.socket.get().cloned().ok_or("this node's broker has no socket for streams")?;
        let rt = tokio::runtime::Handle::try_current().map_err(|_| "no async runtime")?;
        let token = token().ok_or("no random source for an internal token")?;
        let id = spec.instance_id.clone();
        self.broker.register(spec, &token).map_err(|e| e.message)?;
        Ok(rt.spawn(async move {
            match crate::client::Client::connect(&socket, &token).await {
                Ok(c) => f(c).await,
                Err(e) => tracing::warn!(target: "cogbus", instance = %id, error = %e, "internal stream client did not connect"),
            }
        }))
    }

    /// Open a tap on `<kind>/<source>` for a remote subscriber.
    pub(crate) fn open_tap(&self, kind: &str, source: &str, schema: &str, project: Option<String>) -> Result<Tap, String> {
        let n = self.streams.next.fetch_add(1, Ordering::Relaxed) + 1;
        let instance = format!("mesh-out-{n}");
        let input = InputSpec { topic: kind.into(), schema: schema.into(), policy: InputPolicy::DropOldest };
        let spec = InstanceSpec { instance_id: instance.clone(), cog_id: "mesh-hub".into(), project_id: project, uid: None, outputs: vec![], inputs: vec![input] };
        let (buf, lost) = (Arc::new(Mutex::new(VecDeque::new())), Arc::new(AtomicU64::new(0)));
        let (b, l, wake) = (buf.clone(), lost.clone(), self.wake.clone());
        let (topic, schema) = (format!("{kind}/{source}"), schema.to_string());
        let task = self.spawn_internal(spec, move |c| async move {
            let mut r = match c.subscribe_stream(&topic, &schema).await {
                Ok(r) => r,
                Err(e) => return tracing::warn!(target: "cogbus", topic, error = %e, "mesh tap could not read the ring"),
            };
            while let Some(rec) = r.next().await {
                let mut q = lock(&b);
                if q.len() >= TAP_BUFFER {
                    q.pop_front();
                    l.fetch_add(1, Ordering::Relaxed);
                }
                q.push_back(rec);
                drop(q);
                wake.notify_one();
            }
        })?;
        Ok(Tap { instance, buf, lost, task })
    }

    /// Records a tap read, and how many it lost before they were taken.
    pub(crate) fn drain_tap(tap: &Tap) -> (Vec<StreamRecord>, u64) {
        (lock(&tap.buf).drain(..).collect(), tap.lost.swap(0, Ordering::Relaxed))
    }

    pub(crate) fn close_tap(&self, tap: Tap) {
        tap.task.abort();
        self.broker.revoke(&tap.instance);
    }

    /// One chunk of our remote stream `sub` from `node`.
    pub(crate) fn on_chunk(&self, node: &str, sub: u64, seq: u64, t_ns: u64, bytes: &[u8]) {
        match self.broker.mesh_chunk(node, sub, bytes.len() as u64) {
            None => {
                self.send_req(node, &TopicReq::Unsubscribe { sub });
                return;
            }
            Some(c) if c != Credit::default() => {
                self.send_req(node, &TopicReq::Credit { sub, credit: c });
            }
            Some(_) => {}
        }
        let writer = lock(&self.streams.proxies).get(&sub).map(|p| p.writer.clone());
        let mut slot = writer.as_ref().map(|w| lock(w));
        match slot.as_mut().and_then(|w| w.as_mut()) {
            Some(w) => {
                if let Err(e) = w.write_at(seq, t_ns, bytes) {
                    tracing::debug!(target: "cogbus", node, sub, error = %e, "remote stream record not written");
                }
            }
            None => tracing::debug!(target: "cogbus", node, sub, "remote stream record before its proxy ring was open; dropped"),
        }
    }

    /// Keep one proxy writer per remote stream subscription the broker holds.
    pub(crate) fn sync_proxies(&self) {
        let want = self.broker.mesh_streams();
        let mut have = lock(&self.streams.proxies);
        let gone: Vec<u64> = have.keys().filter(|id| !want.iter().any(|w| w.rsub == **id)).copied().collect();
        for id in gone {
            if let Some(p) = have.remove(&id) {
                p.task.abort();
                self.broker.revoke(&p.instance);
            }
        }
        if self.streams.socket.get().is_none() {
            return;
        }
        let missing: Vec<_> = want.into_iter().filter(|w| !have.contains_key(&w.rsub)).collect();
        for w in missing {
            let instance = format!("mesh-in-{}", w.rsub);
            let out = OutputSpec { topic: w.kind.clone(), schema: w.schema.clone(), class: TopicClass::Stream };
            let spec = InstanceSpec { instance_id: instance.clone(), cog_id: "mesh-hub".into(), project_id: w.project.clone(), uid: None, outputs: vec![out], inputs: vec![] };
            let writer: Arc<Mutex<Option<StreamWriter>>> = Arc::default();
            let slot = writer.clone();
            let task = self.spawn_internal(spec, move |c| async move {
                match c.stream_writer(&w.kind, &w.source, &w.schema, StreamOptions::new(MAX_RECORD)).await {
                    Ok(sw) => {
                        *lock(&slot) = Some(sw);
                        // The connection stays open as long as the proxy does.
                        std::future::pending::<()>().await;
                        drop(c);
                    }
                    Err(e) => tracing::warn!(target: "cogbus", topic = %format!("{}/{}", w.kind, w.source), error = %e, "proxy stream not opened"),
                }
            });
            match task {
                Ok(task) => {
                    have.insert(w.rsub, Proxy { instance, writer, task });
                }
                Err(e) => tracing::warn!(target: "cogbus", rsub = w.rsub, error = %e, "remote stream has no proxy ring"),
            }
        }
    }
}
