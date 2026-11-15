//! Frame builders and error shapes. Everything on the wire is JSON-RPC 2.0
//! text; ids are carried as raw JSON so a harness's string or number id
//! round-trips unchanged.

use serde_json::{Value, json};
use xai_tool_protocol::{
    WorkspaceGonePhase, WorkspaceGoneReason, error_codes, workspace_unavailable_wire,
};

pub const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Request-id prefix for ids the hub allocates toward tool servers.
pub const HUB_ID_PREFIX: &str = "hub-";

/// Methods the hub relays harness -> tool server for the envelope session.
pub const HARNESS_TO_SERVER_NOTIFS: &[&str] = &["hook", "tool.notify", "system.notify"];
/// Methods the hub relays tool server -> owning harness for the envelope session.
pub const SERVER_TO_HARNESS_NOTIFS: &[&str] = &[
    "tool_call_progress",
    "tool.notification",
    "hook_reply",
    "hook",
];
/// Donation notifications the hub accepts and discards (it has no sink).
pub const DONATE_NOTIFS: &[&str] = &["traces.donate", "logs.donate", "metrics.donate"];

pub fn ok(id: &Value, session: Option<&str>, result: Value) -> String {
    let mut f = json!({"jsonrpc": "2.0", "id": id, "result": result});
    if let Some(s) = session {
        f["session_id"] = json!(s);
    }
    f.to_string()
}

pub fn err(
    id: &Value,
    session: Option<&str>,
    code: i32,
    message: &str,
    data: Option<Value>,
) -> String {
    let mut e = json!({"code": code, "message": message});
    if let Some(d) = data {
        e["data"] = d;
    }
    let mut f = json!({"jsonrpc": "2.0", "id": id, "error": e});
    if let Some(s) = session {
        f["session_id"] = json!(s);
    }
    f.to_string()
}

/// Hub to server `session.bind` request. The envelope deliberately has **no**
/// `session_id`: the SDK demux sends any frame carrying one to a per-session
/// inbox, and `session.bind` is a connection-level frame that the server reads
/// from `params.session_id`. (Found by running the real SDK ToolServer.)
pub fn session_bind(hub_id: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": hub_id, "method": "session.bind", "params": params}).to_string()
}

/// Hub to server `session.unbind` notification; same envelope rule as
/// [`session_bind`].
pub fn session_unbind(session: &str) -> String {
    json!({"jsonrpc": "2.0", "method": "session.unbind", "params": {"session_id": session}})
        .to_string()
}

/// Error for a method the hub does not implement. The message prefix is the
/// one `xai_tool_protocol::UNKNOWN_METHOD_MSG_PREFIX` pins.
pub fn method_not_found(id: &Value, session: Option<&str>, method: &str) -> String {
    err(
        id,
        session,
        -32601,
        &format!(
            "{}{method}` not supported by weft-hub",
            xai_tool_protocol::UNKNOWN_METHOD_MSG_PREFIX
        ),
        None,
    )
}

/// The retryable "workspace gone" error, with its decodable `data`.
pub fn workspace_unavailable(
    id: &Value,
    session: Option<&str>,
    reason: WorkspaceGoneReason,
    phase: WorkspaceGonePhase,
) -> String {
    let wire = workspace_unavailable_wire(reason, phase);
    let data = serde_json::to_value(&wire).ok();
    err(
        id,
        session,
        error_codes::WORKSPACE_UNAVAILABLE_JSONRPC_CODE,
        xai_tool_protocol::WORKSPACE_UNAVAILABLE_MESSAGE,
        data,
    )
}

pub fn invalid_params(id: &Value, session: Option<&str>, why: &str) -> String {
    err(id, session, -32602, &format!("invalid params: {why}"), None)
}

pub fn forbidden(id: &Value, session: Option<&str>, why: &str) -> String {
    err(id, session, -32003, why, None)
}

/// RFC 3339 UTC timestamp for `secs` since the epoch.
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_794_182_400), "2026-11-09T00:00:00Z");
    }

    #[test]
    fn error_frames_carry_exactly_one_of_result_or_error() {
        let id = json!(7);
        let e: Value = serde_json::from_str(&err(&id, Some("s"), -32003, "no", None)).unwrap();
        assert!(
            e.get("result").is_none() && e["error"]["code"] == -32003 && e["session_id"] == "s"
        );
        let o: Value = serde_json::from_str(&ok(&id, None, json!({}))).unwrap();
        assert!(o.get("error").is_none() && o["id"] == 7);
    }

    #[test]
    fn workspace_unavailable_is_recognizable_by_the_protocol_crate() {
        let f: Value = serde_json::from_str(&workspace_unavailable(
            &json!("a"),
            None,
            WorkspaceGoneReason::NotBound,
            WorkspaceGonePhase::RouteMissing,
        ))
        .unwrap();
        assert_eq!(f["error"]["code"], -32005);
        assert_eq!(f["error"]["data"]["subcode"], "workspace_unavailable");
        assert_eq!(f["error"]["data"]["details"]["reason"], "not_bound");
    }

    #[test]
    fn session_bind_and_unbind_frames_have_no_envelope_session_id() {
        let b: Value =
            serde_json::from_str(&session_bind("hub-1", json!({"session_id": "s"}))).unwrap();
        let u: Value = serde_json::from_str(&session_unbind("s")).unwrap();
        for f in [&b, &u] {
            assert!(f.get("session_id").is_none(), "{f}");
            assert_eq!(f["params"]["session_id"], "s");
        }
        assert_eq!((b["id"].as_str(), u.get("id")), (Some("hub-1"), None));
    }

    #[test]
    fn method_not_found_uses_the_pinned_prefix() {
        let f: Value = serde_json::from_str(&method_not_found(&json!(1), None, "nope")).unwrap();
        assert!(
            f["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("unknown method `")
        );
    }
}
