//! Stream requests on the broker socket: answered here, with descriptors.

use std::os::fd::OwnedFd;

use serde_json::json;
use tokio::net::unix::OwnedWriteHalf;
use tokio::time::timeout;

use super::{STREAM_SUB_BASE, SessionStream, fdpass};
use crate::broker::{Broker, BusError, InstanceSpec, Session, refuse};
use crate::proto::{ErrorCode, Event, Request, encode};
use crate::server::FRAME_TIMEOUT;

fn error(id: u64, e: BusError) -> Event {
    Event::Error {
        id,
        code: e.code,
        message: e.message,
    }
}

/// Answer `req` if it is a stream request (or the unsubscribe of a stream
/// subscription); otherwise hand it back.
pub(crate) fn handle(
    broker: &Broker,
    s: &mut Session,
    ss: &SessionStream,
    req: Request,
) -> Result<(Event, Vec<OwnedFd>), Request> {
    let id = req.id();
    let alive = |i: &str| broker.alive(i);
    let r = match req {
        Request::StreamOpen {
            topic_kind,
            source_id,
            schema,
            max_record,
            ring_bytes,
            seq_base,
            ..
        } => manifest(broker, s, "stream_open").and_then(|spec| {
            let (src, sch) = (&source_id, &schema);
            let o = &broker.streams;
            o.open(
                &spec,
                s,
                ss,
                &topic_kind,
                src,
                sch,
                max_record,
                ring_bytes,
                seq_base,
                &alive,
            )
        }),
        Request::StreamSubscribe { topic, schema, .. } => manifest(broker, s, "stream_subscribe")
            .and_then(|spec| broker.streams.subscribe(&spec, s, ss, &topic, &schema)),
        Request::StreamDemote { sub, .. } => {
            return Ok(match broker.streams.demote(s, ss, sub) {
                Ok(result) => (Event::Ok { id, result }, Vec::new()),
                Err(e) => (error(id, e), Vec::new()),
            });
        }
        Request::Unsubscribe { sub, .. } if sub > STREAM_SUB_BASE => {
            let ev = match broker.streams.unsubscribe(s, ss, sub, &alive) {
                Ok(()) => Event::Ok {
                    id,
                    result: json!({}),
                },
                Err(e) => error(id, e),
            };
            return Ok((ev, Vec::new()));
        }
        other => return Err(other),
    };
    Ok(match r {
        Ok(a) => (
            Event::Ok {
                id,
                result: a.result,
            },
            a.fds,
        ),
        Err(e) => (error(id, e), Vec::new()),
    })
}

/// The instance's manifest, or `revoked`.
fn manifest(broker: &Broker, s: &Session, action: &str) -> Result<InstanceSpec, BusError> {
    broker.instance(s.instance_id()).ok_or_else(|| {
        let e = BusError::new(ErrorCode::Revoked, "instance revoked");
        refuse(s.instance_id(), action, "-", e)
    })
}

/// The connection is gone: drop its stream state.
pub(crate) fn close(broker: &Broker, ss: &SessionStream) {
    broker.streams.close(ss, &|i| broker.alive(i));
}

/// Write `ev` with `fds` attached, within [`FRAME_TIMEOUT`].
pub(crate) async fn send_with(w: &mut OwnedWriteHalf, ev: &Event, fds: &[OwnedFd]) -> bool {
    let Ok(bytes) = encode(ev) else {
        return false;
    };
    matches!(
        timeout(FRAME_TIMEOUT, fdpass::send_frame(w, &bytes, fds)).await,
        Ok(Ok(()))
    )
}
