//! Refusals are behaviour: each one is asserted by status and reason.

mod common;

use common::*;
use serde_json::json;
use tokio_tungstenite::tungstenite::{Error, Message};
use weft_hub::passkey::Role;

fn http_status(e: Error) -> u16 {
    match e {
        Error::Http(resp) => resp.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn upgrade_without_or_with_bad_pass_key_is_401() {
    let env = start().await;
    assert_eq!(
        http_status(env.try_connect("harness", None).await.unwrap_err()),
        401
    );
    assert_eq!(
        http_status(
            env.try_connect("harness", Some("wfhub1.e30.AA"))
                .await
                .unwrap_err()
        ),
        401
    );
    // Signed by a key the hub does not hold.
    let evil = weft_hub::config::Minter {
        id: "weftos-xpc".into(),
        host_kind: xai_tool_protocol::HostKind::Desktop,
        key: vec![9; 32],
    };
    let t = weft_hub::passkey::mint(
        &evil,
        "u",
        &[Role::Harness],
        60,
        None,
        weft_hub::hub::now_secs(),
    )
    .unwrap();
    assert_eq!(
        http_status(env.try_connect("harness", Some(&t)).await.unwrap_err()),
        401
    );
    assert_eq!(env.hub.connection_count(), 0);
}

#[tokio::test]
async fn wrong_role_is_403_and_bad_role_or_path_is_refused() {
    let env = start().await;
    let hkey = env.key("weftos-xpc", "u", &[Role::Harness], None);
    assert_eq!(
        http_status(
            env.try_connect("tool_server", Some(&hkey))
                .await
                .unwrap_err()
        ),
        403
    );
    assert_eq!(
        http_status(
            env.try_connect("bot_client", Some(&hkey))
                .await
                .unwrap_err()
        ),
        400
    );
    let mut r = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://{}/other?role=harness", env.addr),
    )
    .unwrap();
    r.headers_mut()
        .insert("authorization", format!("Bearer {hkey}").parse().unwrap());
    assert_eq!(
        http_status(tokio_tungstenite::connect_async(r).await.unwrap_err()),
        404
    );
}

async fn expect_close(ws: &mut Ws, why: &str) {
    loop {
        match ws.next_msg().await {
            Some(Message::Close(Some(f))) => {
                assert!(
                    f.reason.contains(why),
                    "close reason {:?} lacks {why:?}",
                    f.reason
                );
                return;
            }
            Some(Message::Close(None)) | None => panic!("closed without a reason, wanted {why:?}"),
            Some(_) => {}
        }
    }
}

trait NextMsg {
    async fn next_msg(&mut self) -> Option<Message>;
}
impl NextMsg for Ws {
    async fn next_msg(&mut self) -> Option<Message> {
        use futures_util::StreamExt;
        tokio::time::timeout(std::time::Duration::from_secs(5), self.next())
            .await
            .ok()?
            .and_then(Result::ok)
    }
}

#[tokio::test]
async fn unconfigured_machine_wrong_minter_and_bound_key_are_refused_at_hello() {
    let env = start().await;
    let k = env.key("weftos-xpc", "u", &[Role::Server], None);
    let mut ws = env.try_connect("tool_server", Some(&k)).await.unwrap();
    send(
        &mut ws,
        json!({"protocol_version":"1.0.0","kind":"tool_server","server_id":"rogue"}),
    )
    .await;
    expect_close(&mut ws, "not configured").await;

    // `other` is configured, but for the sandbox minter.
    let mut ws = env.try_connect("tool_server", Some(&k)).await.unwrap();
    send(
        &mut ws,
        json!({"protocol_version":"1.0.0","kind":"tool_server","server_id":"other"}),
    )
    .await;
    expect_close(&mut ws, "not allowed to serve").await;

    // Key bound to machine `other` cannot serve `xpc`.
    let bound = env.key("weftos-xpc", "u", &[Role::Server], Some("other"));
    let mut ws = env.try_connect("tool_server", Some(&bound)).await.unwrap();
    send(
        &mut ws,
        json!({"protocol_version":"1.0.0","kind":"tool_server","server_id":"xpc"}),
    )
    .await;
    expect_close(&mut ws, "different machine").await;

    // Missing server_id, kind/role mismatch, wrong protocol version.
    let mut ws = env.try_connect("tool_server", Some(&k)).await.unwrap();
    send(
        &mut ws,
        json!({"protocol_version":"1.0.0","kind":"tool_server"}),
    )
    .await;
    expect_close(&mut ws, "requires server_id").await;
    let mut ws = env.try_connect("tool_server", Some(&k)).await.unwrap();
    send(
        &mut ws,
        json!({"protocol_version":"1.0.0","kind":"harness"}),
    )
    .await;
    expect_close(&mut ws, "does not match role").await;
    let mut ws = env.try_connect("tool_server", Some(&k)).await.unwrap();
    send(
        &mut ws,
        json!({"protocol_version":"9.9.9","kind":"tool_server","server_id":"xpc"}),
    )
    .await;
    expect_close(&mut ws, "unsupported protocol version").await;
    assert_eq!(env.hub.connection_count(), 0);
}

#[tokio::test]
async fn another_users_server_is_invisible_and_unbindable_and_cannot_be_evicted() {
    let env = start().await;
    let skey = env.key("weftos-xpc", "alice", &[Role::Server], None);
    let (_server, _) = env.peer("tool_server", &skey, Some("xpc")).await;
    let bob = env.key("weftos-xpc", "bob", &[Role::Harness], None);
    let (mut h, _) = env.peer("harness", &bob, None).await;
    send(&mut h, req(json!(1), Some("s"), "session_open", json!({}))).await;
    recv(&mut h).await;
    send(&mut h, req(json!(2), Some("s"), "servers.list", json!({}))).await;
    assert_eq!(recv(&mut h).await["result"]["servers"], json!([]));
    send(
        &mut h,
        req(
            json!(3),
            Some("s"),
            "session_bind_server",
            json!({"server_id":"xpc"}),
        ),
    )
    .await;
    let e = recv(&mut h).await;
    assert_eq!(e["error"]["data"]["subcode"], "workspace_unavailable");
    assert_eq!(e["error"]["data"]["details"]["reason"], "not_bound");

    // Bob's server key cannot take over alice's registration.
    let bsrv = env.key("weftos-xpc", "bob", &[Role::Server], None);
    let mut ws = env.try_connect("tool_server", Some(&bsrv)).await.unwrap();
    send(
        &mut ws,
        json!({"protocol_version":"1.0.0","kind":"tool_server","server_id":"xpc"}),
    )
    .await;
    expect_close(&mut ws, "another user").await;
}

#[tokio::test]
async fn session_ownership_unsupported_methods_and_unbound_calls_are_refused() {
    let env = start().await;
    let a = env.key("weftos-xpc", "alice", &[Role::Harness], None);
    let b = env.key("weftos-xpc", "bob", &[Role::Harness], None);
    let (mut ha, _) = env.peer("harness", &a, None).await;
    let (mut hb, _) = env.peer("harness", &b, None).await;
    send(
        &mut ha,
        req(json!(1), Some("shared"), "session_open", json!({})),
    )
    .await;
    recv(&mut ha).await;
    send(
        &mut hb,
        req(json!(1), Some("shared"), "session_open", json!({})),
    )
    .await;
    assert_eq!(recv(&mut hb).await["error"]["code"], -32003);
    // Bob cannot call into alice's session either.
    send(
        &mut hb,
        req(
            json!(2),
            Some("shared"),
            "tool.call",
            json!({"tool_call_id":"t","tool_id":"a:b","arguments":{}}),
        ),
    )
    .await;
    assert_eq!(recv(&mut hb).await["error"]["code"], -32006);
    // Alice, unbound: retryable workspace_unavailable, not a hang.
    send(
        &mut ha,
        req(
            json!(3),
            Some("shared"),
            "tool.call",
            json!({"tool_call_id":"t","tool_id":"a:b","arguments":{}}),
        ),
    )
    .await;
    assert_eq!(
        recv(&mut ha).await["error"]["data"]["details"]["reason"],
        "not_bound"
    );
    // Unimplemented methods say so with the pinned prefix.
    for m in [
        "session_attach_server",
        "subscribe_notifications",
        "bot.roster",
        "auth.refresh",
    ] {
        send(&mut ha, req(json!(m), Some("shared"), m, json!({}))).await;
        let r = recv(&mut ha).await;
        assert_eq!(r["error"]["code"], -32601, "{m}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("unknown method `")
        );
    }
    // Missing session id on a session-scoped method.
    send(&mut ha, req(json!(9), None, "session_open", json!({}))).await;
    assert_eq!(recv(&mut ha).await["error"]["code"], -32602);
}
