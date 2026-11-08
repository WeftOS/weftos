//! The relay path end to end over real sockets: a fake tool server registers,
//! a fake harness binds a session, one tool call goes through and comes back.

mod common;

use common::*;
use serde_json::json;
use weft_hub::passkey::Role;

#[tokio::test]
async fn register_bind_and_relay_one_tool_call() {
    let env = start().await;
    let skey = env.key("weftos-xpc", "mathew", &[Role::Server], Some("xpc"));
    let hkey = env.key("weftos-xpc", "mathew", &[Role::Harness], None);

    let (mut server, ack) = env.peer("tool_server", &skey, Some("xpc")).await;
    assert_eq!(ack["user_id"], "mathew");
    assert_eq!(ack["supported_protocol_versions"], json!(["1.0.0"]));

    let (mut harness, ack) = env.peer("harness", &hkey, None).await;
    assert_eq!(ack["user_id"], "mathew");

    send(
        &mut harness,
        req(
            json!("o1"),
            Some("sess-1"),
            "session_open",
            json!({"resume": false}),
        ),
    )
    .await;
    assert_eq!(recv(&mut harness).await["result"], json!({}));

    // servers.list: host_kind comes from the minter, not from hello metadata ("sandbox").
    send(
        &mut harness,
        req(json!("l1"), Some("sess-1"), "servers.list", json!({})),
    )
    .await;
    let list = recv(&mut harness).await;
    let s = &list["result"]["servers"][0];
    assert_eq!(s["server_id"], "xpc");
    assert_eq!(s["host_kind"], "desktop");
    assert_eq!(s["status"], "ready");
    assert_eq!(s["description"], "aepod-xpc");
    assert_eq!(list["result"]["servers"].as_array().unwrap().len(), 1);

    // session_bind_server -> hub sends session.bind to the server.
    send(
        &mut harness,
        req(
            json!(5),
            Some("sess-1"),
            "session_bind_server",
            json!({"server_id":"xpc","cwd":"/work","metadata":{"k":"v"}}),
        ),
    )
    .await;
    let bind = recv(&mut server).await;
    assert_eq!(bind["method"], "session.bind");
    assert!(
        bind.get("session_id").is_none(),
        "connection-level frame: no envelope session_id"
    );
    assert_eq!(bind["params"]["session_id"], "sess-1");
    assert_eq!(bind["params"]["cwd"], "/work");
    assert_eq!(bind["params"]["metadata"], json!({"k":"v"}));
    send(
        &mut server,
        json!({"jsonrpc":"2.0","id":bind["id"],"result":
        {"tools":[],"binary_version":"t-1"}}),
    )
    .await;
    let bound = recv(&mut harness).await;
    assert_eq!(bound["id"], 5, "harness's own id is restored");
    assert_eq!(bound["result"]["binary_version"], "t-1");

    // tool.call relays as tool_call_request and the answer comes back.
    let params = json!({"tool_call_id":"tc-1","tool_id":"fs:read","arguments":{"path":"/x"}});
    send(
        &mut harness,
        req(json!("c1"), Some("sess-1"), "tool.call", params.clone()),
    )
    .await;
    let call = recv(&mut server).await;
    assert_eq!(call["method"], "tool_call_request");
    assert_eq!(call["session_id"], "sess-1");
    assert_eq!(call["params"], params);
    assert_ne!(
        call["id"], "c1",
        "hub allocates its own id toward the server"
    );
    send(
        &mut server,
        json!({"jsonrpc":"2.0","id":call["id"],"session_id":"sess-1","result":
        {"tool_call_id":"tc-1","output":{"json":{"ok":true}}}}),
    )
    .await;
    let done = recv(&mut harness).await;
    assert_eq!(done["id"], "c1");
    assert_eq!(done["result"]["tool_call_id"], "tc-1");

    // Progress from the server reaches the owning harness.
    send(
        &mut server,
        json!({"jsonrpc":"2.0","session_id":"sess-1","method":"tool_call_progress",
        "params":{"tool_call_id":"tc-1","kind":"text","body":{"text":"hi"}}}),
    )
    .await;
    let prog = recv(&mut harness).await;
    assert_eq!(prog["method"], "tool_call_progress");
}

#[tokio::test]
async fn server_dropping_mid_call_fails_the_call_retryably_and_notifies() {
    let env = start().await;
    let skey = env.key("weftos-xpc", "u", &[Role::Server], None);
    let hkey = env.key("weftos-xpc", "u", &[Role::Harness], None);
    let (mut server, _) = env.peer("tool_server", &skey, Some("xpc")).await;
    let (mut harness, _) = env.peer("harness", &hkey, None).await;
    send(
        &mut harness,
        req(json!("o"), Some("s"), "session_open", json!({})),
    )
    .await;
    recv(&mut harness).await;
    send(
        &mut harness,
        req(
            json!("b"),
            Some("s"),
            "session_bind_server",
            json!({"server_id":"xpc"}),
        ),
    )
    .await;
    let bind = recv(&mut server).await;
    send(
        &mut server,
        json!({"jsonrpc":"2.0","id":bind["id"],"result":{"tools":[]}}),
    )
    .await;
    recv(&mut harness).await;

    send(
        &mut harness,
        req(
            json!("c"),
            Some("s"),
            "tool.call",
            json!({"tool_call_id":"t","tool_id":"fs:read","arguments":{}}),
        ),
    )
    .await;
    recv(&mut server).await;
    drop(server);

    let first = recv(&mut harness).await;
    let second = recv(&mut harness).await;
    let (err, status) = if first.get("error").is_some() {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(err["id"], "c");
    assert_eq!(err["error"]["code"], -32005);
    assert_eq!(err["error"]["data"]["subcode"], "workspace_unavailable");
    assert_eq!(status["method"], "tool_server.status");
    assert_eq!(status["params"]["status"], "disconnected");

    // The server is gone from the list and a new call is refused, not hung.
    send(
        &mut harness,
        req(json!("l"), Some("s"), "servers.list", json!({})),
    )
    .await;
    assert_eq!(recv(&mut harness).await["result"]["servers"], json!([]));
    send(
        &mut harness,
        req(
            json!("c2"),
            Some("s"),
            "tool.call",
            json!({"tool_call_id":"t2","tool_id":"fs:read","arguments":{}}),
        ),
    )
    .await;
    assert_eq!(recv(&mut harness).await["error"]["code"], -32005);
}

#[tokio::test]
async fn harness_disconnect_tells_the_server_to_unbind_the_session() {
    let env = start().await;
    let skey = env.key("weftos-xpc", "u", &[Role::Server], None);
    let hkey = env.key("weftos-xpc", "u", &[Role::Harness], None);
    let (mut server, _) = env.peer("tool_server", &skey, Some("xpc")).await;
    let (mut harness, _) = env.peer("harness", &hkey, None).await;
    send(
        &mut harness,
        req(json!("o"), Some("s9"), "session_open", json!({})),
    )
    .await;
    recv(&mut harness).await;
    send(
        &mut harness,
        req(
            json!("b"),
            Some("s9"),
            "session_bind_server",
            json!({"server_id":"xpc"}),
        ),
    )
    .await;
    let bind = recv(&mut server).await;
    send(
        &mut server,
        json!({"jsonrpc":"2.0","id":bind["id"],"result":{"tools":[]}}),
    )
    .await;
    recv(&mut harness).await;
    drop(harness);
    let unbind = recv(&mut server).await;
    assert_eq!(unbind["method"], "session.unbind");
    assert!(unbind.get("session_id").is_none());
    assert_eq!(unbind["params"]["session_id"], "s9");
}
