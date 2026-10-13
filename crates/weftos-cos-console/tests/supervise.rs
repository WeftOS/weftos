//! The supervised host survives transient failures: a publish refused
//! (rate limit), the leaf route missing, the agent's stream cut. Each
//! test ends with the console repainted on a fresh pad and the loop still
//! running. Every (re)open gets a new simulated pad, so "repainted" means
//! the whole scene is there, not that old nodes survived.

mod fake_agent;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_agent::FakeAgent;
use serde_json::json;
use weftos_cos_console::host::supervise::{supervise, Ended, SuperviseConfig};
use weftos_cos_console::host::HostConfig;
use weftos_cos_console::frame::SUBJECT;
use weftos_cos_console::{DisplayError, LeafSim};
use weftos_display_client::{DisplayGrant, DisplaySession, DisplayTarget, GrantGate, SessionOptions};
use weftos_leaf_scene::Primitive;

const WAIT: Duration = Duration::from_secs(8);

/// Refuses every publish while `refuse` is set, as the daemon does
/// when the governance gate rate-limits the caller.
struct Flaky {
    refuse: Arc<AtomicBool>,
}

impl GrantGate for Flaky {
    fn admit(&mut self, _: &DisplayGrant) -> Result<(), DisplayError> {
        Ok(())
    }
    fn still_valid(&mut self, _: &DisplayGrant) -> Result<(), DisplayError> {
        if self.refuse.load(Ordering::SeqCst) {
            Err(DisplayError::Denied("rate limited: evaluation quota exceeded"))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Default)]
struct Pad {
    /// Refuse publishes on sessions that are open.
    refuse_publish: Arc<AtomicBool>,
    /// The leaf is unreachable: `open` fails like a missing route.
    no_route: Arc<AtomicBool>,
    opens: Arc<AtomicUsize>,
    latest: Arc<Mutex<Option<LeafSim>>>,
}

impl Pad {
    fn open(&self) -> Result<DisplaySession, DisplayError> {
        if self.no_route.load(Ordering::SeqCst) {
            return Err(DisplayError::Backend("no route for mesh.leaf.push".into()));
        }
        let (target, sim) = DisplayTarget::sim();
        let opts = SessionOptions { gate: Box::new(Flaky { refuse: self.refuse_publish.clone() }), ..SessionOptions::default() };
        let s = DisplaySession::open_with(target, DisplayGrant::dev(SUBJECT), opts)?;
        self.opens.fetch_add(1, Ordering::SeqCst);
        *self.latest.lock().unwrap() = Some(sim);
        Ok(s)
    }

    fn texts(&self) -> Vec<String> {
        let Some(sim) = self.latest.lock().unwrap().clone() else { return Vec::new() };
        sim.snapshot()
            .nodes
            .iter()
            .filter_map(|n| match &n.primitive {
                Primitive::Text { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn shows(&self, needle: &str) -> bool {
        self.texts().iter().any(|t| t.contains(needle))
    }
}

fn wait(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn config(agent: &FakeAgent) -> SuperviseConfig {
    let host = HostConfig { agent: agent.base(), typing: false, reconnect_min: Duration::from_millis(50), reconnect_max: Duration::from_millis(200) };
    let mut cfg = SuperviseConfig::new(host);
    cfg.retry_min = Duration::from_millis(50);
    cfg.retry_max = Duration::from_millis(200);
    cfg.frame = Duration::from_millis(5);
    cfg.reopen_on_close = true;
    cfg
}

/// Run the supervisor in a thread, hand the body the log, then stop it.
fn with_supervisor(agent: &FakeAgent, pad: &Pad, body: impl FnOnce(&Mutex<Vec<String>>, &std::thread::ScopedJoinHandle<'_, Ended>)) {
    let stop = AtomicBool::new(false);
    let log = Mutex::new(Vec::<String>::new());
    let cfg = config(agent);
    std::thread::scope(|sc| {
        let h = sc.spawn(|| supervise(|| pad.open(), || "09:41".to_string(), &cfg, &stop, |l| log.lock().unwrap().push(l.to_string())));
        body(&log, &h);
        assert!(!h.is_finished(), "the host must not exit on a transient error");
        stop.store(true, Ordering::SeqCst);
        assert_eq!(h.join_blocking(), Ended::Stopped);
    });
}

trait JoinBlocking {
    fn join_blocking(&self) -> Ended;
}
impl JoinBlocking for std::thread::ScopedJoinHandle<'_, Ended> {
    fn join_blocking(&self) -> Ended {
        assert!(wait(|| self.is_finished()), "supervisor did not stop");
        Ended::Stopped
    }
}

fn count(log: &Mutex<Vec<String>>, needle: &str) -> usize {
    log.lock().unwrap().iter().filter(|l| l.contains(needle)).count()
}

#[test]
fn a_rate_limited_publish_is_retried_and_the_console_repaints() {
    let agent = FakeAgent::start();
    let pad = Pad::default();
    with_supervisor(&agent, &pad, |log, _| {
        assert!(wait(|| pad.shows("chief of staff")));
        assert_eq!(pad.opens.load(Ordering::SeqCst), 1);

        // The next publishes are refused. An agent event forces one.
        pad.refuse_publish.store(true, Ordering::SeqCst);
        assert!(wait(|| agent.stream_count() > 0));
        agent.send(json!({"kind": "token", "turn_id": "t1", "text": "lost in the outage"}));
        assert!(wait(|| count(log, "disconnected") == 1));
        std::thread::sleep(Duration::from_millis(400)); // several retries
        assert_eq!(count(log, "disconnected"), 1, "one line per state change, not per retry");
        assert!(count(log, "rate limited") == 1 && count(log, "retrying in") == 1);

        pad.refuse_publish.store(false, Ordering::SeqCst);
        assert!(wait(|| count(log, "reconnected") == 1));
        assert!(wait(|| pad.opens.load(Ordering::SeqCst) >= 2 && pad.shows("chief of staff")), "a fresh pad gets the whole scene");
        assert!(wait(|| agent.stream_count() > 0));
        agent.send(json!({"kind": "token", "turn_id": "t2", "text": "back online"}));
        assert!(wait(|| pad.shows("back online")));
    });
}

#[test]
fn a_missing_leaf_route_is_retried_until_it_appears() {
    let agent = FakeAgent::start();
    let pad = Pad::default();
    pad.no_route.store(true, Ordering::SeqCst);
    with_supervisor(&agent, &pad, |log, h| {
        assert!(wait(|| count(log, "disconnected") == 1));
        assert!(log.lock().unwrap()[0].contains("no route for mesh.leaf"));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!h.is_finished());
        assert_eq!(count(log, "disconnected"), 1);
        assert_eq!(pad.opens.load(Ordering::SeqCst), 0);

        pad.no_route.store(false, Ordering::SeqCst);
        assert!(wait(|| count(log, "reconnected") == 1));
        assert!(wait(|| pad.shows("chief of staff")));

        // The route drops again mid-run, as a daemon restart does to the leaf.
        pad.no_route.store(true, Ordering::SeqCst);
        pad.refuse_publish.store(true, Ordering::SeqCst);
        assert!(wait(|| agent.stream_count() > 0));
        agent.send(json!({"kind": "token", "turn_id": "t", "text": "x"}));
        assert!(wait(|| count(log, "disconnected") == 2));
        pad.no_route.store(false, Ordering::SeqCst);
        pad.refuse_publish.store(false, Ordering::SeqCst);
        assert!(wait(|| count(log, "reconnected") == 2 && pad.shows("chief of staff")));
    });
}

#[test]
fn the_agent_stream_closing_resubscribes_and_events_land_again() {
    let agent = FakeAgent::start();
    let pad = Pad::default();
    with_supervisor(&agent, &pad, |log, _| {
        assert!(wait(|| agent.stream_count() == 1 && pad.shows("chief of staff")));
        agent.drop_streams();
        assert!(wait(|| agent.stream_count() == 1), "the host subscribes again");
        agent.send(json!({"kind": "token", "turn_id": "t3", "text": "stream is back"}));
        assert!(wait(|| pad.shows("stream is back")));
        assert_eq!(count(log, "disconnected"), 0, "an agent outage is not a display outage");
    });
}
