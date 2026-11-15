//! Shared harness: a real hub on an ephemeral loopback port, pass keys minted
//! with the production code, and raw WebSocket peers speaking the wire.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use weft_hub::config::{HubConfig, Minter};
use weft_hub::hub::{Hub, now_secs};
use weft_hub::passkey::{self, Role};
use weft_hub::server;

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct Env {
    pub addr: SocketAddr,
    pub hub: Arc<Hub>,
    pub cfg: HubConfig,
    _dir: tempfile::TempDir,
}

fn key_file(dir: &std::path::Path, name: &str, byte: u8) {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("{}\n", format!("{byte:02x}").repeat(32))).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
}

pub async fn start() -> Env {
    let dir = tempfile::tempdir().unwrap();
    key_file(dir.path(), "xpc.key", 0x11);
    key_file(dir.path(), "sbx.key", 0x22);
    let toml = r#"
[[auth.minters]]
id = "weftos-xpc"
host_kind = "desktop"
key_file = "xpc.key"
[[auth.minters]]
id = "weftos-sbx"
host_kind = "sandbox"
key_file = "sbx.key"
[[machines]]
id = "xpc"
description = "aepod-xpc"
minter = "weftos-xpc"
[[machines]]
id = "other"
minter = "weftos-sbx"
"#;
    let cfg = HubConfig::from_toml(toml, dir.path()).unwrap();
    let hub = Hub::new(cfg.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(hub.clone(), listener, std::future::pending()));
    Env {
        addr,
        hub,
        cfg,
        _dir: dir,
    }
}

impl Env {
    pub fn minter(&self, id: &str) -> &Minter {
        &self.cfg.minters[id]
    }

    pub fn key(&self, minter: &str, sub: &str, roles: &[Role], srv: Option<&str>) -> String {
        passkey::mint(self.minter(minter), sub, roles, 600, srv, now_secs()).unwrap()
    }

    pub async fn try_connect(
        &self,
        role: &str,
        bearer: Option<&str>,
    ) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
        let mut req = format!("ws://{}/v1/tools?role={role}", self.addr)
            .into_client_request()
            .unwrap();
        if let Some(b) = bearer {
            req.headers_mut()
                .insert("authorization", format!("Bearer {b}").parse().unwrap());
        }
        tokio_tungstenite::connect_async(req)
            .await
            .map(|(ws, _)| ws)
    }

    /// Connect, say hello, and return the socket plus the hello_ack.
    pub async fn peer(&self, role: &str, bearer: &str, server_id: Option<&str>) -> (Ws, Value) {
        let mut ws = self.try_connect(role, Some(bearer)).await.expect("upgrade");
        let kind = if role == "harness" {
            "harness"
        } else {
            "tool_server"
        };
        let mut hello = json!({"protocol_version":"1.0.0","kind":kind});
        if let Some(s) = server_id {
            hello["server_id"] = json!(s);
            hello["metadata"] = json!({"host_kind":"sandbox","hostname":"xpc"});
        }
        send(&mut ws, hello).await;
        let ack = recv(&mut ws).await;
        (ws, ack)
    }
}

pub async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

pub async fn recv(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("socket ended")
            .expect("socket error");
        if let Message::Text(t) = m {
            return serde_json::from_str(t.as_ref()).unwrap();
        }
    }
}

pub fn req(id: Value, session: Option<&str>, method: &str, params: Value) -> Value {
    let mut v = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    if let Some(s) = session {
        v["session_id"] = json!(s);
    }
    v
}
