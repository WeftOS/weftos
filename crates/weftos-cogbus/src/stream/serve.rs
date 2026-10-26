//! Stream requests on the broker socket: answered here, with descriptors.

use std::os::fd::OwnedFd;

use serde_json::{Value, json};
use tokio::net::unix::OwnedWriteHalf;
use tokio::time::timeout;

use super::registry::Answer;
use super::{STREAM_SUB_BASE, SessionStream, fdpass};
use crate::broker::{Broker, BusError, InstanceSpec, Session, refuse};
use crate::policy::{BusAction, BusActivity, BusRequest, Outcome};
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
/// subscription); otherwise hand it back. Opens and subscribes go through
/// the broker's governance gate (B3) as `cogbus.publish` and
/// `cogbus.subscribe`; refusals, allowed actions and demotions go to the
/// activity log the same way readings' do.
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
            let topic = format!("{topic_kind}/{source_id}");
            let gate = |foreign: Option<&str>| {
                let pid = s.project_id.as_deref();
                let mut req =
                    BusRequest::new(BusAction::Publish, s.instance_id(), &spec.cog_id, pid);
                req.topic_project = foreign;
                broker.ext.gate(&req.on(&topic, &schema))
            };
            let (src, sch) = (&source_id, &schema);
            let a = broker.streams.open(
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
                &gate,
            )?;
            broker.ext.noted(s, BusAction::Publish, &topic);
            Ok(a)
        }),
        Request::StreamSubscribe { topic, schema, .. } => manifest(broker, s, "stream_subscribe")
            .and_then(|spec| {
                let held = s.subs.len() + broker.streams.reader_count(ss.conn);
                if held >= broker.config().max_subs_per_conn {
                    return Err(refuse(
                        s.instance_id(),
                        "stream_subscribe",
                        &topic,
                        BusError::new(ErrorCode::Busy, format!("this connection already holds {held} subscriptions")),
                    ));
                }
                let source = topic.split_once('/').map(|(_, src)| src);
                let gov = |foreign: Option<&str>| {
                    let pid = s.project_id.as_deref();
                    let req =
                        BusRequest::new(BusAction::Subscribe, s.instance_id(), &spec.cog_id, pid)
                            .on(&topic, &schema);
                    let also = broker.ext.grants(&req, source);
                    broker.ext.gate(&BusRequest {
                        topic_project: foreign,
                        ..req
                    })?;
                    Ok(also)
                };
                let a = broker
                    .streams
                    .subscribe(&spec, s, ss, &topic, &schema, &gov)?;
                broker.ext.noted(s, BusAction::Subscribe, &topic);
                Ok(a)
            }),
        Request::StreamDemote { sub, .. } => broker.streams.demote(s, ss, sub).map(|result| {
            demoted(broker, &result);
            Answer {
                result,
                fds: Vec::new(),
            }
        }),
        Request::Unsubscribe { sub, .. } if sub > STREAM_SUB_BASE => {
            broker.streams.unsubscribe(s, ss, sub, &alive).map(|topic| {
                broker.ext.noted(s, BusAction::Unsubscribe, &topic);
                Answer {
                    result: json!({}),
                    fds: Vec::new(),
                }
            })
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
        Err(e) => {
            broker.record_refusal(&e, s.instance_id());
            (error(id, e), Vec::new())
        }
    })
}

/// A `block` reader was demoted: an activity record for the reader.
fn demoted(broker: &Broker, r: &Value) {
    let Some(reader) = r["reader"].as_str() else {
        return;
    };
    broker.ext.hooks.sink.record(&BusActivity {
        ts_ms: crate::audit::now_ms(),
        action: "cogbus.stream_demote".into(),
        instance: reader.into(),
        project: r["project"].as_str().map(String::from),
        topic: r["topic"].as_str().unwrap_or("-").into(),
        outcome: Outcome::Demoted {
            policy: "drop-oldest".into(),
            reason: format!(
                "held the writer past the {} ms block deadline",
                broker.streams.deadline_ms()
            ),
        },
    });
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
