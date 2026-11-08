# weft-hub: the WeftOS-side computer hub for Tapestry

Package: `crates/weft-computer-hub/` (binary `weft-hub`, library `weft_hub`).
Branch `feat/weft-computer-hub`, off WeftOS `target-0.8.5` at e3688fedb. Fork docs are
not committed to `target-0.8.5`; this branch is separate.

## What it is

Tapestry's `xai-workspace-server` (a tool server) and `workspace-server-probe` (a
harness) both connect *through* a hub. The fork ships the client SDK
(`xai-computer-hub-sdk`) and a library (`xai-computer-hub-core`) but no listener. `weft-hub`
is that listener: a standalone binary, a plain WebSocket service speaking the fork's
`xai-tool-protocol` 1.0.0 wire format, authenticating every connection with a
WeftOS-minted pass key, and relaying tool calls between a harness session and the tool
server bound to it. It opens no outbound connection (`tokio-tungstenite` is built without
its `connect` feature; the only socket calls are `bind` and `accept`).

Owner decisions applied: D1 standalone binary that weaver supervises (own `[workspace]`
table, not a member of the WeftOS workspace, same pattern as the leaf crates); D2 the
machine list is data (`config/hub.example.toml`), xpc is the only entry.

`xai-computer-hub-core` is **not** used. It is a client-side `Transport`/`ToolRegistry`/
`CompoundResolver` library built on `xai-tool-runtime` and has no server-side routing to
build on. The hub depends only on `xai-tool-protocol` (and its `xai-tool-types`), for the
wire types, so the amount of upstream-derived code in the binary stays small.

## Building against the local fork

The fork is not pushed, so the git dependency cannot be fetched.
`Cargo.toml` names `https://github.com/WeftOS/grok-build` at rev `30302eb1` (the Tapestry
`tapestry/phase0` HEAD, unpushed) and `crates/weft-computer-hub/.cargo/config.toml`
carries a `[patch]` to `/Volumes/ai-models/research/tapestry/crates/common/{xai-tool-protocol,xai-tool-types}`.
Delete that file once the rev is on GitHub. Build with `CARGO_TARGET_DIR` set to your own dir:

```
cd crates/weft-computer-hub
CARGO_TARGET_DIR=/Volumes/ai-models/weftos-worktrees/weft-computer-hub-target cargo build --release -j 4
```

## Run on loopback

```
weft-hub keygen --out ~/.weftos/hub/xpc-mint.key           # 32 random bytes, hex, mode 0600, refuses to overwrite
cp config/hub.example.toml ~/.weftos/hub/hub.toml          # edit machines/minters
weft-hub serve --config ~/.weftos/hub/hub.toml             # binds 127.0.0.1:9490
```

Any non-loopback `--addr`/`listen.addr` is refused unless `--allow-non-loopback` is given;
hostnames are refused outright (no resolution). Pass keys are required on every
connection, loopback included.

## Wire contract (what `p3-passkey` codes against)

Transport: WebSocket, JSON-RPC 2.0 text frames, `ws://127.0.0.1:9490/v1/tools?role=<role>`.
`role` is `harness` or `tool_server` (`server` accepted as an alias; `bot_client` is refused
with 400). Any other path is 404. The SDK appends `role` itself.

**Pass key.** `Authorization: Bearer <passkey>` on the upgrade request. No key or an
invalid key: HTTP 401, no socket. A valid key whose `roles` lack the requested role: 403.

**Handshake.** First frame within 10 s: `HelloMsg`
`{"protocol_version":"1.0.0","kind":"harness"|"tool_server","server_id":..,"description":..,"metadata":..}`.
Reply `HelloAckMsg {connection_id, user_id (= key sub), computer_hub_version, supported_protocol_versions:["1.0.0"], capabilities:[]}`.
Refusals at this stage are a WS Close (policy 1008) with a reason: wrong protocol
version, `kind` not matching the role, `tool_server` without `server_id`, `server_id`
not in the machines config, key minter not the one configured for that machine, key's
`srv` claim naming another machine, `server_id` held by a different user. A
same-user re-registration of a live `server_id` evicts the old socket.

**Tool server side** (role `tool_server`):

| direction | frame |
|---|---|
| hub to server | `session.bind` request: id `hub-N`, **no envelope `session_id`**, `params {session_id, cwd?, metadata?}`. Server answers `SessionBindResult` (`tools`, `binary_version`, ...). |
| hub to server | `session.unbind` notification, **no envelope `session_id`**, `params {session_id}`, sent when the harness closes/detaches/unbinds/disconnects. |
| hub to server | `tool_call_request`: id `hub-N`, envelope `session_id`, `params` = the harness's `ToolCallParams` verbatim. |
| server to hub | response to the above (same id). The hub restores the harness's own id and forwards the frame otherwise unchanged. |
| server to hub, relayed to the session's harness | `tool_call_progress`, `tool.notification`, `hook_reply`, `hook` notifications (by envelope `session_id`). |
| server to hub, handled | `serve` (replies `{accepted: n}`; the tool list is not tracked), `tool_server.status` (recorded for `servers.list`). |
| server to hub, discarded | `traces.donate`, `logs.donate`, `metrics.donate`. |

**Harness side** (role `harness`), all scoped to the key's `sub`:

- `session_open` (reopening a session you own, e.g. on reconnect replay, takes the connection over and keeps the binding; another user's session id is refused with -32003).
- `session_close`, `session_detach` (identical: the bound server is told `session.unbind`).
- `servers.list` returns only servers whose registering key has the same `sub`. `host_kind` is the hub's configured value for the key's minter, never the value in hello `metadata`.
- `session_bind_server {server_id, cwd?, metadata?}`: forwarded as `session.bind`; 10 s timeout. A server that is absent or belongs to another user gives the retryable `workspace_unavailable` error (reason `not_bound`), indistinguishable from absent.
- `session_unbind_server`, `tools.list` (the tools from the last bind result, plus `workspace_bound`).
- `tool.call`: forwarded as `tool_call_request`; the response comes back under the harness's id.
- `hook`, `tool.notify`, `system.notify` notifications are relayed to the bound server.
- App-level `ping` is answered with `pong`.

**Disconnects.** Server drops: every in-flight call or bind to it fails with the retryable
`workspace_unavailable` error (-32005, `data.subcode = "workspace_unavailable"`, reason
`disconnect`, phase `in_flight_cancelled`), and the owning harness gets a
`tool_server.status` notification with `status: "disconnected"` for each affected session.
Later calls on the session get `workspace_unavailable` (reason `not_bound`) until it is
rebound. Harness drops: its sessions are released and the server gets `session.unbind`.
A socket silent for 180 s is dropped.

**Not implemented, answered -32601 with the pinned `unknown method \`` prefix:**
`session_attach_server`, `subscribe_notifications`/`unsubscribe_notifications`, `auth.refresh`,
`tools.search`, `tool_server.get_status`/`evict`, all `bot.*`. `hello_ack.capabilities` is
empty, so a client that gates on it sees none of these.

Limits: 4096 relayed requests in flight (then -32016), 256 connections, 16 MiB messages,
64 KiB of server `metadata`.

## Pass-key scheme

Nothing WeftOS already mints fits a standalone hub. `SessionTokenStore` /
`DaemonTokenValidator` (ADR-075/076, `clawft-services` MCP and gateway auth) validate
against state inside a running daemon, and this hub must not touch the daemon. So this
adds a small scheme, `wfhub1`:

```
wfhub1.<b64url(claims JSON)>.<b64url(HMAC-SHA256(minter_key, "wfhub1." + b64url(claims JSON)))>
claims = {v:1, mnt, sub, roles:["server"|"harness"], iat, exp, jti, srv?}
```

The hub holds one 32-byte-plus secret per trusted minter (`[[auth.minters]]`, key file hex,
refused unless mode 0600). It looks the key up by `mnt`, checks the MAC in constant time,
then version, `exp`, `iat` (60 s skew), lifetime <= `max_ttl_secs` (default 24 h),
`revoked_jti`, and role. `host_kind` (`desktop|container|sandbox`) comes from the minter's
config entry. For servers the machine's configured minter must be the key's minter.

Mint: `weft-hub mint --config hub.toml --minter weftos-xpc --sub <user> --role server --srv xpc --ttl 3600 --out srv.key`
(`--role harness` for the harness; roles can be comma-joined; `--out` creates a 0600 file,
exclusive; without `--out` the key prints to stdout). `sub` becomes the hub `user_id`, so
the server's key and the harness's key must carry the same `sub`.

## Supervision

`crates/weft-computer-hub/deploy/weftapp.toml` (printed by `weft-hub print-app`) is an
`AppManifest` with one `[[services]]` entry (`name`, `command = "weft-hub serve"`, port
9490/tcp, `RUST_LOG`), `network = true` (listener only), `shell = false`, filesystem
`~/.weftos/hub`. It follows `crates/clawft-kernel/src/app.rs` (`AppManifest`, `ServiceSpec`,
`PortMapping`, `AppCapabilities`). Restart behaviour is whatever weaver's managed-process
policy gives (`workload_runtime/infer/config.rs` `RestartPolicy`: 2 s base, doubling, 60 s
cap, 5 restarts); the definition overrides nothing.

What I did and did not establish: the file parses in the manifest's field shape (the unit
test mirrors the kernel's field names with `deny_unknown_fields`), is argv-style, widens no
capability, and its port matches the example config's loopback port. I did **not** run it
through `AppManager`/weaver, and in reading `app.rs` I found validation and persistence of
`services` but no code that spawns a service `command` (its header says hook execution and
install-from-disk are future work). The code that actually launches and restarts an
external binary is the infer workload runtime (`workload_runtime/infer/launch.rs`), which
is specific to model servers. So whether weaver will start `weft-hub` from this file today
is unestablished; a thin adapter on that launch path (argv, cleared env, port-in-use
guard, stop only what it spawned) is the likely follow-up.

The two "no envelope `session_id`" rows matter: the SDK demux sends any frame carrying one to
a per-session inbox, so a bind with it set is never seen by the server. The first version of
the hub had it set; the unit and loopback tests (fake peers) passed anyway, and only a run
with the real SDK `ToolServer` exposed it (the bind timed out). The tests now assert its
absence.

## Test results

All from `crates/weft-computer-hub`, `CARGO_TARGET_DIR=/Volumes/ai-models/weftos-worktrees/weft-computer-hub-target`, `-j 4`,
Rust 1.95.0. Package only; never `--workspace`.

| gate | command | exit |
|---|---|---|
| format | `cargo fmt --check` | 0 |
| lint | `cargo clippy --all-targets -- -D warnings` | 0 |
| tests | `cargo test` | 0: 27 passed, 0 failed (19 unit, 3 in `tests/loopback.rs`, 5 in `tests/refusals.rs`) |
| release build | `cargo build --release` | 0 |

Loopback integration (`tests/loopback.rs`): fake server registers, fake harness opens a
session, lists servers, binds, one tool call relays and the answer returns under the
harness's id; progress relays; server dropping mid-call fails the call with the retryable
error and notifies; harness dropping sends `session.unbind`. Refusals
(`tests/refusals.rs`, each by status or reason): 401 for no/garbage/wrongly-signed key,
403 for wrong role, 400 for `bot_client`, 404 for another path, WS close reasons for
unconfigured machine, wrong minter, key bound to another machine, missing `server_id`,
kind/role mismatch, bad protocol version, other user's registration; cross-user
`servers.list` and bind give nothing; session ownership; the unimplemented methods answer
-32601. Unit tests cover the pass key (round trip, tamper, wrong key, unknown minter,
expiry, ttl, revocation, malformed inputs, mint refusals), config refusals (no machines,
unknown minter, short/non-hex/world-readable key, bad host_kind), loopback-only binding,
and the supervision definition.

**Real SDK check (not part of the package).** A throwaway crate outside the repo ran the
fork's real `xai-computer-hub-sdk` against the hub on loopback: a real `ToolServer`
(`server_id` xpc, Bearer pass key) registered, a real `ToolHarness` connected, `list_servers`
returned `[xpc, host_kind=Desktop]`, `session_bind` returned 1 tool, and `harness.call` of an
echo tool returned the echoed arguments. Exit 0. This is not a committed test because it
pulls the SDK's full dependency tree (about six minutes cold) into the package.

Binary smoke (release build): `keygen` creates a 0600 file and refuses to overwrite;
`mint` writes the key, refuses an unknown minter and a ttl above `max_ttl_secs`; `serve
--addr 0.0.0.0:9499` exits 2 with the loopback refusal; `serve` on 127.0.0.1 listens, an
unauthenticated upgrade gets HTTP 401, SIGINT stops it.

## Diff gate

`scripts/diff-gate.sh --worktree` exits 1 (REJECTED), 22 findings, all in
`crates/weft-computer-hub/`, 0 approved:

- 15 `new-dep`: the package's `Cargo.toml` `[dependencies]` and `[dev-dependencies]`
  entries (xai-tool-protocol, tokio, tokio-tungstenite, futures-util, serde, serde_json,
  toml, hmac, sha2, base64, uuid, tracing, tracing-subscriber; dev: tokio-tungstenite,
  tempfile).
- 7 `net`: `tokio::net::{TcpListener, TcpStream}` in `src/main.rs`, `src/server.rs`,
  and `tests/common/mod.rs`. These are the listener and the test peers.

No `unsafe`, `build.rs`, proc-macro, process, FFI, CI-file or approvals findings. I did
not edit `config/diff-gate-approvals.toml`, did not set `DIFF_GATE_OWNER_APPROVED`, and did
not restructure code to avoid the findings. Approval is the owner's call. Full output:
`/Volumes/ai-models/weftos-worktrees/weft-hub-diffgate.log`.

## Not established

- **Weaver actually launching it.** See Supervision: the definition is checked for shape,
  not run through weaver, and I found no code path that spawns a manifest `ServiceSpec`.
- **Key expiry on a live socket.** A pass key is checked at upgrade only. A socket opened
  with a key stays open after the key's `exp` (until it idles out or drops).
  `auth.refresh` is not implemented. The Tapestry server provider does refresh tokens, but
  only calls `auth.refresh` when the ack advertises it, which this hub does not.
- **Revocation** is a static `revoked_jti` list in the config, read at start; there is no
  reload and a revoked key does not close an existing socket.
- **Tool cancellation** semantics beyond relaying `hook` notifications. When a harness
  drops mid-call, the server is told `session.unbind` but is not sent a per-call cancel.
- **Tool list tracking.** `serve` is acknowledged but its tool snapshot is not stored, and
  no `tools_changed` is emitted; `tools.list` returns the tools from the last bind result.
- **`session_attach_server`, `subscribe_notifications`, `auth.refresh`, `bot.*`** are not
  implemented (see above). Whether the real Tapestry workspace server or harness (as
  opposed to the SDK pieces I ran) needs any of them is unchecked: I did not run
  `xai-workspace-server` or `workspace-server-probe` against the hub.
- **Non-loopback exposure** is guarded (flag plus pass key) but unexercised: there is no
  TLS in the hub, so a non-loopback listener carries the pass key in cleartext unless
  something in front terminates TLS. The SDK itself refuses plaintext `ws://` to a
  non-loopback host unless `allow_insecure_ws` is set.
- **Multi-instance/Redis-style hub behaviour** (remote routes, hibernation) is out of scope.
- **The pinned rev** `30302eb1` is an unpushed local commit; the committed `.cargo/config.toml`
  holds absolute local paths and must be removed once the fork is pushed.
- **Full workspace gate** (`scripts/build.sh gate`) was not run, per the targeted-tests rule
  and because this package is outside the workspace; CI is the green-of-record.
