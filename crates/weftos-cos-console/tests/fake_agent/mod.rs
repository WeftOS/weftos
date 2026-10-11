//! A fake chief-of-staff agent: the HTTP/SSE surface of contract §3 on
//! a loopback port, on plain `std::net`, so the host is tested against
//! real sockets without the agent crate. Records every POST; the test
//! pushes events down the open streams and can cut them.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

enum Msg {
    Data(String),
    Close,
}

#[derive(Default)]
struct Shared {
    posts: Vec<(String, Value)>,
    streams: Vec<Sender<Msg>>,
    ptt_up_reply: Option<Value>,
}

pub struct FakeAgent {
    pub addr: SocketAddr,
    shared: Arc<Mutex<Shared>>,
}

impl FakeAgent {
    pub fn start() -> Self {
        Self::serve(TcpListener::bind("127.0.0.1:0").unwrap())
    }

    pub fn start_at(addr: SocketAddr) -> Self {
        Self::serve(TcpListener::bind(addr).unwrap())
    }

    /// A free loopback port that nothing listens on (for offline tests).
    pub fn free_port() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    fn serve(listener: TcpListener) -> Self {
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let s = shared.clone();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let s = s.clone();
                thread::spawn(move || handle(conn, s));
            }
        });
        Self { addr, shared }
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Push one event to every open stream.
    pub fn send(&self, ev: Value) {
        let line = format!("data: {ev}\n\n");
        self.lock().streams.retain(|tx| tx.send(Msg::Data(line.clone())).is_ok());
    }

    /// Cut every open stream mid-flight.
    pub fn drop_streams(&self) {
        for tx in self.lock().streams.drain(..) {
            let _ = tx.send(Msg::Close);
        }
    }

    pub fn stream_count(&self) -> usize {
        self.lock().streams.len()
    }

    pub fn posts(&self) -> Vec<(String, Value)> {
        self.lock().posts.clone()
    }

    pub fn posts_to(&self, path: &str) -> Vec<Value> {
        self.posts().into_iter().filter(|(p, _)| p == path).map(|(_, v)| v).collect()
    }

    /// What `POST /v1/cos/ptt {down:false}` answers.
    pub fn set_ptt_up_reply(&self, v: Value) {
        self.lock().ptt_up_reply = Some(v);
    }
}

fn handle(mut conn: TcpStream, shared: Arc<Mutex<Shared>>) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    match (method.as_str(), path.as_str()) {
        ("GET", "/v1/cos/events") => stream_events(conn, shared),
        ("GET", "/healthz") => respond_json(&mut conn, 200, &json!({"ok": true})),
        ("POST", _) => {
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let reply = {
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                s.posts.push((path.clone(), v.clone()));
                match path.as_str() {
                    "/v1/cos/ptt" if v["down"] == true => json!({"active": true}),
                    "/v1/cos/ptt" => s.ptt_up_reply.clone().unwrap_or(json!({"active": false, "nothing": "no capture"})),
                    _ => json!({"turn_id": format!("t{}", s.posts.len())}),
                }
            };
            respond_json(&mut conn, 202, &reply);
        }
        _ => respond_json(&mut conn, 404, &json!({"error": "no route"})),
    }
}

fn respond_json(conn: &mut TcpStream, code: u16, v: &Value) {
    let body = v.to_string();
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        _ => "Not Found",
    };
    let _ = write!(conn, "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = conn.flush();
}

/// Hold the connection open and write events as they come; the body
/// ends when the connection closes (no content length).
fn stream_events(mut conn: TcpStream, shared: Arc<Mutex<Shared>>) {
    let (tx, rx) = channel();
    shared.lock().unwrap_or_else(|e| e.into_inner()).streams.push(tx);
    if write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").is_err() {
        return;
    }
    let _ = conn.flush();
    for msg in rx {
        match msg {
            Msg::Data(d) => {
                if conn.write_all(d.as_bytes()).and_then(|()| conn.flush()).is_err() {
                    return;
                }
            }
            Msg::Close => {
                let _ = conn.shutdown(Shutdown::Both);
                return;
            }
        }
    }
}
