//! The agent over HTTP/SSE (contract §3) from a synchronous host loop.
//!
//! Posts run on an outbox thread (a push-to-talk release waits on STT,
//! which must never stall the frame loop); the event stream is read on
//! its own thread and reconnected with backoff; both report back to
//! the loop through one channel of [`Inbound`].

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use super::agent_event::{parse_event, AgentEvent};

/// A request the host wants sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Post {
    Utterance { text: String, source: &'static str },
    /// A guide interaction, already in the `/v1/cos/answer` body shape.
    Answer(Value),
    Ptt { down: bool },
}

impl Post {
    fn path(&self) -> &'static str {
        match self {
            Post::Utterance { .. } => "/v1/cos/utterance",
            Post::Answer(_) => "/v1/cos/answer",
            Post::Ptt { .. } => "/v1/cos/ptt",
        }
    }

    fn body(&self) -> Value {
        match self {
            Post::Utterance { text, source } => json!({"text": text, "source": source}),
            Post::Answer(v) => v.clone(),
            Post::Ptt { down } => json!({"down": down}),
        }
    }
}

/// A completed post: the reply body, or why it failed.
#[derive(Debug, Clone, PartialEq)]
pub struct Posted {
    pub post: Post,
    pub result: Result<Value, String>,
}

/// What the worker threads hand the loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// The event stream is open.
    Connected,
    Event(AgentEvent),
    /// The stream ended or could not be opened; a retry follows.
    Disconnected { reason: String },
    Posted(Posted),
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// `http://127.0.0.1:18300`, no trailing slash.
    pub base: String,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

/// The two threads and their channel ends.
pub struct AgentLink {
    inbox: Receiver<Inbound>,
    outbox: Sender<Post>,
    stop: Arc<AtomicBool>,
}

impl AgentLink {
    pub fn connect(cfg: ClientConfig) -> Self {
        let (in_tx, inbox) = channel();
        let (outbox, out_rx) = channel::<Post>();
        let stop = Arc::new(AtomicBool::new(false));
        let base = cfg.base.trim_end_matches('/').to_string();
        let cfg = ClientConfig { base: base.clone(), ..cfg };

        let tx = in_tx.clone();
        let stop_sse = stop.clone();
        thread::Builder::new()
            .name("cos-host-sse".into())
            .spawn(move || sse_loop(&cfg, &tx, &stop_sse))
            .expect("spawn sse thread");

        thread::Builder::new()
            .name("cos-host-post".into())
            .spawn(move || {
                let agent = ureq::AgentBuilder::new()
                    .timeout_connect(Duration::from_secs(2))
                    .timeout_read(Duration::from_secs(60))
                    .build();
                for post in out_rx {
                    let result = send(&agent, &base, &post);
                    if in_tx.send(Inbound::Posted(Posted { post, result })).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn post thread");

        Self { inbox, outbox, stop }
    }

    /// Queue a request; the reply arrives as [`Inbound::Posted`].
    pub fn post(&self, post: Post) {
        let _ = self.outbox.send(post);
    }

    /// Next message from the threads, if one is waiting.
    pub fn try_recv(&self) -> Option<Inbound> {
        self.inbox.try_recv().ok()
    }
}

impl Drop for AgentLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn send(agent: &ureq::Agent, base: &str, post: &Post) -> Result<Value, String> {
    match agent.post(&format!("{base}{}", post.path())).send_json(post.body()) {
        Ok(resp) => Ok(resp.into_json::<Value>().unwrap_or(Value::Null)),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or(text);
            Err(format!("{code}: {msg}"))
        }
        Err(ureq::Error::Transport(t)) => Err(t.to_string()),
    }
}

/// Open the stream, read it to its end, back off, repeat.
fn sse_loop(cfg: &ClientConfig, tx: &Sender<Inbound>, stop: &AtomicBool) {
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(2)).build();
    let url = format!("{}/v1/cos/events", cfg.base);
    let mut backoff = cfg.reconnect_min;
    while !stop.load(Ordering::SeqCst) {
        let outcome = match agent.get(&url).set("Accept", "text/event-stream").call() {
            Ok(resp) => {
                backoff = cfg.reconnect_min;
                if tx.send(Inbound::Connected).is_err() {
                    return;
                }
                read_stream(resp.into_reader(), tx, stop)
            }
            Err(ureq::Error::Status(code, _)) => format!("HTTP {code}"),
            Err(ureq::Error::Transport(t)) => t.to_string(),
        };
        if stop.load(Ordering::SeqCst) || tx.send(Inbound::Disconnected { reason: outcome }).is_err() {
            return;
        }
        sleep_unless_stopped(backoff, stop);
        backoff = (backoff * 2).min(cfg.reconnect_max);
    }
}

/// Read `data:` events until the stream ends; returns why it ended.
fn read_stream(reader: impl Read, tx: &Sender<Inbound>, stop: &AtomicBool) -> String {
    let mut lines = BufReader::new(reader).lines();
    let mut data = String::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            return "stopped".into();
        }
        match lines.next() {
            Some(Ok(line)) => {
                if line.is_empty() {
                    if !data.is_empty() {
                        if let Some(ev) = parse_event(&data) {
                            if tx.send(Inbound::Event(ev)).is_err() {
                                return "host gone".into();
                            }
                        }
                        data.clear();
                    }
                } else if let Some(d) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(d.strip_prefix(' ').unwrap_or(d));
                }
                // `:` comments (keep-alives) and other fields are ignored.
            }
            Some(Err(e)) => return e.to_string(),
            None => return "stream ended".into(),
        }
    }
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let slice = Duration::from_millis(25);
    let mut left = total;
    while !left.is_zero() && !stop.load(Ordering::SeqCst) {
        let d = left.min(slice);
        thread::sleep(d);
        left -= d;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_lines_join_and_blank_lines_dispatch() {
        // The first event is one JSON object split over two `data:` lines,
        // joined with a newline, which serde reads as whitespace.
        let feed = "retry: 1000\n: keep-alive\ndata: {\"kind\":\"token\",\ndata: \"turn_id\":\"t\",\"text\":\"a\"}\n\ndata: {\"kind\":\"state\",\"state\":\"idle\"}\n\n";
        let (tx, rx) = channel();
        let stop = AtomicBool::new(false);
        let why = read_stream(feed.as_bytes(), &tx, &stop);
        assert_eq!(why, "stream ended");
        let got: Vec<Inbound> = rx.try_iter().collect();
        assert_eq!(
            got,
            vec![
                Inbound::Event(AgentEvent::Token { turn_id: "t".into(), text: "a".into() }),
                Inbound::Event(AgentEvent::State { state: "idle".into(), reason: None }),
            ]
        );
    }

    #[test]
    fn posts_know_their_route_and_body() {
        let p = Post::Ptt { down: true };
        assert_eq!(p.path(), "/v1/cos/ptt");
        assert_eq!(p.body(), json!({"down": true}));
        let p = Post::Utterance { text: "hi".into(), source: "typed" };
        assert_eq!(p.body(), json!({"text": "hi", "source": "typed"}));
    }
}
