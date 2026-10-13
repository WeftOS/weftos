# Chief of staff: cloud migration plan

- **Status**: Draft for owner review, 2026-10-13. Plan only; nothing here is built.
- **Owner ask (verbatim)**: "Explore how the CoS is being built, and look at how we will migrate it to either a cloudflare worker (probably the best) or on vercel. We will also need to spec out how it will get to the api and weftos so everything works together properly. Right now the grok is making it work with elevenlabs to prep for that."
- **Requirements (owner, 2026-10-13)**: (R1) the always-on CoS and its API keys leave the owner's Mac. (R2) Multitenant: one private chief of staff per person (ADR-121 decision 5). (R3) Phone-first: most users have only the dashboard `/phone` page; Mentra glasses, the CrowPanel pad and the e-ink panel are optional per-user add-ons.
- **Builds on**: ADR-114 (`weftos://` names), ADR-118 and `docs/plans/adr-118-cos-contract.md` (`cos.v1`), ADR-119 (field slate, e-ink), ADR-120 (exposures), ADR-121 (AI org, instances, brains), ADR-108 (node actions), the relay contract `docs/plans/cos-dashboard-bridge.md` (branch `wt/cosbridge`) and dashboard PR #29 (`feat/phone-tabs`, `docs/cos-relay.md`).

Evidence marks: **[V]** verified this session by reading the file or doc named; **[I]** inferred, not tested; **[D]** a design proposal in this plan. Platform numbers were fetched on 2026-10-13 from the URLs in the final section.

---

## 1. Current architecture

### 1.1 Diagram

```
 MAC (owner)                                                         CLOUD (today)
 +-------------------------------------------------------------+   +-----------------------------+
 | local LLM server  GLM-4.7-Flash, OpenAI wire, loopback :8081|   | Vercel: Next.js dashboard   |
 |        ^                                                    |   |  /phone  Phone|Mentra|Weaver |
 |        | (quick tier; plan tier + fallback = keyed clouds)  |   |  /api/nodes/cos/{sync,      |
 | weftos-cos serve  (Rust: agent loop, 17 tools, brain .rvf)  |   |      pending,audio/<id>}    |
 |   cos.v1 HTTP/SSE on loopback :18300 (+ tailnet listener,   |   | Supabase: cos_requests,     |
 |   per-endpoint bearers)                                     |   |  cos_turns, cos_status,     |
 |     ^        ^         ^          ^            ^            |   |  cos-audio bucket, board,   |
 |     |        |         |          |            |            |   |  nodes, node_actions        |
 |  bridge   cos-console  glance   mentra-cos-   mentra-cos-   |   +--------------^--------------+
 |  (relay)  -host (pad)  (e-ink)  listen.py     speak.py      |                  | node bearer
 |     |        |  leaf    serial   adb mic       adb speaker   |                  | (outbound only)
 |     +--------+--channel via weaver daemon socket            |------------------+
 |  weaver daemon RPC (cluster.status, cluster.nodes, ...)     |
 +-------------------------------------------------------------+
```

### 1.2 Component inventory

| Piece | What it is | Where (all **[V]**) | Mac-bound? | Portable? |
|---|---|---|---|---|
| Agent loop | One utterance in; `state`, `status`, `token`, `tool`, `message` events out. Turns serialized by a mutex; max 6 tool rounds then a forced no-tools answer; 40-message rolling history; a `Confirm`-gated call is held and the next utterance (yes/no) answers it | `crates/weftos-cos/src/agent.rs:25,29,159,285` | No | Yes, as logic. The crate links `clawft_core`, `clawft_rpc` and `clawft_platform`, so it does not compile to a Worker as is; port the ~630-line loop |
| Conversation trait | `utter`, `answer`, `events`, `recent_turns`: the seam every consumer talks through | `conversation.rs:132` | No | Yes; a second implementation can be remote |
| `cos.v1` server | `POST /v1/cos/utterance`, `/answer`, `/ptt`; `GET /v1/cos/events` (SSE), `/turns?limit`, `/status`, `/healthz` | `server.rs:1-20`; `/status` is in the bridge branch (`bridge.rs:311`) | Loopback bind; tailnet listener with per-endpoint bearers (`endpoints.rs:152`) | The wire shapes are portable |
| Tools (17) | `projects.activity`, `notes.record` (harness HTTP); `board.read/claim/note/search/current` (harness HTTP); `board.propose` and a stubbed `work.kickoff` (write staged YAML to a local dir for the steward); `nodes.health`, `mesh.routes` (weaver daemon socket); `ui.layout/page/checklist/check/steps/ask` (event emitters) | `tools/*.rs` | `board.propose`, `work.kickoff`, `nodes.*`, `mesh.*` | Harness tools and `ui.*` are portable |
| Gates | Each tool is `Open` or `Confirm`; `work.kickoff` and board claim/note are `Confirm` | `tools/mod.rs:21`, `kickoff.rs:44`, `cards.rs:110,143` | No | Yes |
| Models | Quick tier: local `GLM-4.7-Flash-4bit` on a loopback OpenAI-compatible server. Plan tier and fallback: whichever of Anthropic (haiku-4-5 quick, sonnet-5-5 plan) or OpenRouter has a key. Tier picked by plan-cue words or complexity score | `model.rs:28-47,92-103`, `routing.rs:147` | Local server is Mac-bound (needs Apple silicon) | Cloud tiers are portable as is |
| System prompt | `agents/chief-of-staff/AGENT.md` (131 lines), compiled in with `include_str!`, plus tool gating notes | `prompt.rs:8` | No | Yes (ship as a string) |
| Brain | RVF file per installation, entries carry `weftos.brain-metadata/1` metadata; scoped to the installation | `brain.rs:1-40`, `config.rs:204` | File on disk | Data model portable; the RVF/embedder is Rust |
| Endpoints | Named endpoints (`host`, `mentra`, ...) with a `speak` flag; a reply to a remote voice turn is flagged `speak:true` so that endpoint speaks it | `config.rs:55-100`, `agent.rs:262` | No | Yes |
| STT | Parakeet ONNX through `weftos-cos transcribe`; push-to-talk captures the host mic (cpal, `mic` feature) | `voice.rs`, `mentra-cos-listen.py:418` | Mic and ONNX build are Mac-bound | Replaceable (see section 4) |
| TTS | `say` or a `command` engine; replies cut to 700 characters; host endpoint only | `speak.rs:1-30` | `say` is Mac-bound | Replaceable |
| Daemon access | `nodes.health` and `mesh.routes` dial the weaver daemon over a Unix socket and call read-only methods only | `tools/nodes.rs:28-60,176-190` | Yes, by definition | Needs a bridge (section 3) |
| Bridge | `weftos-cos bridge`: syncs turns and status to the dashboard every 2 s, holds a 25 s long-poll for page requests, posts them to loopback `cos.v1`; relays phone push-to-talk audio | branch `wt/cosbridge`: `bridge.rs` (656 lines), `docs/plans/cos-dashboard-bridge.md` | Runs on the Mac | Becomes the template for the device bridge |
| Dashboard relay | Tables `cos_requests`, `cos_turns`, `cos_status`; RPCs `request_cos_turn`, `request_cos_control`, `request_cos_audio`; routes under `app/api/nodes/cos/`; `/phone` tabs; private `cos-audio` bucket | dashboard PR #29: `supabase/migrations/202610130004_cos_relay.sql`, `docs/cos-relay.md` | No | Already cloud |
| Pad console | `cos-console-host` joins the agent's SSE to a scene on the 800x480 pad through the daemon's `display.*` RPC and the certified-leaf channel | `crates/weftos-cos-console/src/bin/cos_console_host.rs:6-12`, `docs/design/display-session-transport.md` | Needs a node with the leaf channel | Stays on the user's node |
| E-ink glance | Derives five fields from the event stream, writes a JSON line to a serial-attached panel, debounced 2 s, heartbeat 60 s | `glance.rs:1-40,251` | Serial port on the host | Stays on a node |
| Glasses | `mentra-cos-listen.py` records on the glasses over adb, gates on RMS and voiced fraction, transcribes locally, posts with the `mentra` bearer; `mentra-cos-speak.py` watches SSE and renders replies with ElevenLabs, then pushes the wav over adb | `~/dev/mentra/scripts/` (not in the WeftOS repo) | adb, the local transcriber, the tailnet bearer | Audio capture and playback stay local; STT and TTS can move |

### 1.3 What is honestly Mac-bound

1. **The quick-tier model** (MLX on Apple silicon). Moving it is a model-quality decision, not a port. Memory note: a 4B model failed tool calls about half the time; GLM-4.7-Flash is the first one that is reliable. Any cloud quick tier must pass the same tool-call eval before it replaces it. **[V]** `model.rs:1-20`.
2. **The daemon socket.** `nodes.health` and `mesh.routes` need a process that can dial `kernel.sock`.
3. **Audio hardware**: the host mic, `say`, adb to the glasses, the leaf channel to the pad, the serial e-ink.
4. **Local state**: the brain `.rvf`, the staged board proposals directory, the tool log.
5. **A single-tenant design.** One endpoint-token directory, one harness token, one brain, one `host` endpoint. Nothing in `CosAgent` carries a user id **[V]**.

Everything else (loop, gating, prompt, event shapes, harness tools, `ui.*`) is portable logic.

---

## 2. Target architecture options

### 2.1 Shared design rules (both options)

- The `cos.v1` event vocabulary is the contract; clients never learn where the agent runs.
- One chief of staff per person: a single-writer actor per user (turns are already serialized by a mutex today, `agent.rs:159`).
- The dashboard (Next.js on Vercel, Supabase) stays the system of record for board, projects, goals, nodes and membership. The CoS reads and writes it through APIs, not by sharing a database role.
- Secrets never enter prompts, events or logs; `redact` (`events.rs:186`) is ported with its tests.

### 2.2 Option A: Cloudflare

```
phone/browser ──WSS──┐                       ┌── Workers AI / AI Gateway ──> providers (user-keyed)
                     v                       │
 Worker "cos-edge" (auth, routing) ──> Durable Object  CosAgent(user)  ──> ElevenLabs (TTS/STT WS)
 node/device hub  ──WSS (outbound from node)─>  SQLite: turns, history, brain, settings, sealed keys
                                              │  alarms: reminders, retries, cleanup
                                              ├──> Queues: kickoff dispatch, retries, TTS render
                                              ├──> R2: audio clips, brain exports (short TTL)
                                              └──> dashboard API (signed, per-user): board, projects, nodes
```

Design **[D]**:

| Concern | Design |
|---|---|
| Per-user actor | One SQLite-backed Durable Object per user, addressed by the user id. It owns the turn mutex, the rolling history, pending confirmation, endpoint table, brain rows and the tool log. Serialization is natural: a DO is single-threaded per request. |
| Tool loop | Runs inside the DO as the TypeScript port of `agent.rs`. Each model call and each tool call is a subrequest. The 6-round cap and the forced final answer carry over. |
| Fan-out | The DO accepts WebSockets using the Hibernation API; phones, pads, and the node hub are clients. `GET /v1/cos/events` stays available as SSE through the Worker for `cos.v1` compatibility. |
| Scheduling | DO alarms (reminders, "check the build in 20 minutes", retry of a queued kickoff). |
| Storage | DO SQLite for turns, history and brain rows; R2 for audio and brain exports; D1 only for cross-user admin tables (user registry, usage rollups); Supabase stays authoritative for org data. |
| Async work | Queues for kickoff dispatch, device retries and non-realtime TTS (a pattern the estate already uses on Workers). |
| Models | Providers called with the user's own key (or the platform key under a budget) from the DO; Workers AI as an optional cheap tier after an eval. |
| Secrets | Account secrets in Workers Secrets or Secrets Store (account-level, open beta); per-user keys sealed in DO SQLite (section 3.3). |

Platform facts (all **[V]**, URLs in section 8):

| Fact | Value |
|---|---|
| Worker CPU per invocation (paid) | default 30 s, configurable to 5 min; wall clock is unlimited while the client is connected |
| Memory per isolate | 128 MB |
| Subrequests per invocation (paid) | 10,000 |
| DO CPU per request (SQLite-backed) | 30 s default, up to 5 min; alarms 15 min wall |
| DO storage | 10 GB per object (paid); unlimited object count |
| DO soft throughput | about 1,000 requests/s per object |
| DO WebSocket message size | 32 MiB inbound |
| Hibernation | Idle objects with hibernatable server-side sockets are evicted from memory while clients stay connected, and are not billed duration. Outbound WebSockets do not hibernate; an open outbound socket can pin the object for up to 15 minutes. Per-connection attachment limit 16,384 bytes |
| DO pricing | Requests $0.15/M (1M included); duration $12.50/M GB-s (400k included); incoming WebSocket messages billed at 20:1; SQLite rows read $0.001/M (25B included), written $1.00/M (50M included), storage $0.20/GB-month |
| Workers Paid | $5/month; 10M requests included then $0.30/M; 30M CPU-ms included then $0.02/M |
| Queues | $0.40/M operations (1M included); retention 4 days default, up to 14 |
| Workers AI | $0.011 per 1,000 neurons; 10,000 neurons/day free; GLM-5.x and DeepSeek V4 models listed (paid billing) |
| Agents SDK | Agent class on Durable Objects with SQL storage, WebSockets, scheduled tasks, MCP, human-in-the-loop approval, voice pipeline |

Consequences for the CoS:

- The voice leg to ElevenLabs is an **outbound WebSocket** from the DO, which keeps the object awake for the length of a voice turn. That is correct and cheap: a 20 s turn at 128 MB is 2.5 GB-s, about $0.00003 **[I]**.
- The phone's own WebSocket to the DO hibernates between turns.
- 128 MB per isolate is enough for a tool loop and JSON; it is not enough for local ONNX STT. STT must be an API (section 4).

### 2.3 Option B: Vercel

```
phone ──HTTPS/SSE or WS(beta)──> Next.js route handlers on Fluid compute
                                   AI SDK agent loop (tool loop) ── AI Gateway ──> providers (BYOK)
                                   Workflow (durable steps, sleep, hooks) ── Queues
                                   Supabase: turns, history, brain rows, sealed keys (RLS by user)
                                   Supabase Realtime: fan-out of events to phone/pad/glasses
```

Design **[D]**: each turn is a Workflow run (or a function with a Supabase advisory lock) keyed by user id; the loop is the AI SDK tool loop; state lives in Supabase rows under RLS; events are written to a table and fan out through Realtime; sleeps and reminders use Workflow sleep or Cron.

Platform facts (all **[V]**):

| Fact | Value |
|---|---|
| Max duration (Fluid) | Hobby 300 s; Pro 300 s default, 800 s max, 1,800 s extended (beta) |
| Memory | 2 GB default, 4 GB max (Pro) |
| Payload | 4.5 MB request or response body |
| Concurrency | Auto-scales to 30,000 (Hobby, Pro) |
| Billing | Active CPU (paused while awaiting I/O) plus provisioned memory for the instance lifetime; 19 regions priced, for example $0.128 per CPU-hour and $0.0106 per GB-hour in the cheapest US regions |
| WebSockets | Public beta since 2026-06-22; connection pins to one instance; capped at the function duration, so clients must reconnect |
| Regions | Single region by default; Pro can pick up to 3 |
| Workflows | Durable `use workflow` / `use step`, sleep for minutes to months, hooks, streams, run pinned to one region; usage-based pricing |
| AI Gateway | Callable from any infrastructure; BYOK; budgets per team, project, key or member; provider failover; zero markup; modalities include speech, transcription and realtime |

### 2.4 Comparison

| Criterion | A. Cloudflare (DO per user) | B. Vercel (Fluid + Workflow + Supabase) |
|---|---|---|
| **Voice latency** | One persistent socket per phone into a warm per-user actor, with an outbound socket to the speech vendor. No cold path on the hot loop beyond a DO wake (constructor re-run) **[I]**. Edge placement near the user **[I]** | Request/response plus Realtime hop for events. Functions are single-region unless configured (iad1 default). WebSockets are beta and pin to an instance **[V]**. Adds the Supabase write-then-notify hop to every event **[I]** |
| **Long tool loops** | A loop is CPU time, not wall time: waiting on a model costs none of the 30 s default. Alarms and Queues cover work past a request (15 min) | 300 s default, 800 s max; Workflow removes the limit by checkpointing every step, at the price of the deterministic-replay programming model |
| **Per-user isolation** | Compute and storage isolated per user by construction (one object, one SQLite). A bug cannot cross users unless the Worker routes wrongly | Isolation is rows plus RLS plus advisory locks. Function instances are shared across users (Fluid concurrency) **[V]**; a global-state bug can leak across users |
| **Single-writer turns** | Native | Needs an explicit lock or one workflow run per user |
| **Cost at 1 user** | $5 Workers Paid covers it; the DO is pennies | Existing Pro seat; marginal cost small |
| **Cost at 100 users** | Under $10/month of platform fees **[I]** | Under $20/month **[I]** |
| **Cost at 10k users** | Roughly $0.3k to $1k/month of requests plus duration **[I]** | Same order of magnitude; provisioned-memory time scales with open streams **[I]** |
| **Dominant cost at every size** | Model tokens and speech (section 4.5), not the platform | Same |
| **Secrets** | Per-user sealed blobs in the user's own DO; platform keys in Workers secrets. Secrets Store is account-level only | Supabase Vault or sealed columns; env vars for platform keys; AI Gateway BYOK per team |
| **Local/offline fallback** | The DO speaks `cos.v1` over WebSocket and HTTP; a Mac `weftos-cos serve` can keep serving the same API (section 3.6) | Same, but the Realtime transport is Supabase-specific |
| **Fit with today's stack** | New deploy target, new language for server code (TypeScript Worker beside a Rust agent). Precedent in the estate: a Worker with D1 and R2 bindings (the Sensor Explorer, Hono, `wrangler.jsonc`) **[V]** | Same repo, same deploy, same auth and RLS as the dashboard; the relay already lives there |
| **Maturity risk** | DO/Hibernation/Alarms are established; Agents SDK voice and Secrets Store are newer | WebSockets beta; extended duration beta; Workflow SDK still on a 5.x beta line (multi-region needs `5.0.0-beta.33`) **[V]** |

### 2.5 Recommendation

**Cloudflare for the CoS runtime and voice edge; keep Supabase and Vercel as the system of record and the UI.** The owner's lean is right, for four reasons: (1) a Durable Object per user is the shape of requirement R2 (a private, single-writer chief of staff per person) with isolation by construction; (2) hibernating WebSockets give an always-on feel for a phone at near-zero idle cost; (3) the loop is I/O bound, and Workers bill CPU not wall time; (4) there is precedent in the estate. Choose Vercel only for the Phase 1 relay, which already exists, and keep it as the fallback transport.

Model gateway: call providers directly from the DO with per-user keys, and put a gateway in front only for logs and budgets. Vercel's AI Gateway is verified callable from non-Vercel infrastructure with zero markup **[V]**. Cloudflare's own AI Gateway was not re-verified for this plan (open question Q8).

Do not port to Rust-on-Workers. The agent crate depends on native-only crates, and the loop is small. Write the loop in TypeScript and prove equivalence against the existing fixtures in `crates/weftos-cos/tests/` (`contract_v1.rs`, `api_contract.rs`, `tool_gating.rs`, `loop_control.rs`) **[V: files exist]**.

---

## 3. How the cloud CoS reaches the API and WeftOS

### 3.1 Tool placement

| Tool | Today | Cloud placement |
|---|---|---|
| `board.read`, `board.search`, `board.current`, `projects.activity` | Harness HTTP with a per-project `wfb_` credential (`harness.rs`) | **Dashboard API**, called from the DO |
| `board.claim`, `board.note`, `notes.record` | Harness HTTP, `Confirm` gate for claim/note | **Dashboard API**; gate stays in the DO |
| `board.propose` | Writes staged YAML for the steward on the Mac (`board.rs`, `write_staged`) | **Gap.** Needs a dashboard "proposal" row the steward lane reads. Until then, propose into a `board_proposals` table **[D]** |
| `work.kickoff` | Stub or `team.runtime_command` on the Mac (`kickoff.rs:1-12`) | **Node action** (ADR-108 P2) to the node that holds the project; `Confirm` in the CoS, and the node confirms locally (ADR-121 decision 6) |
| `nodes.health` | `cluster.status`/`cluster.nodes` over the daemon socket | **First path**: dashboard node/heartbeat data (the dashboard already receives node reports) **[I]**. **Second path**: a read-only RPC through the device hub for live detail |
| `mesh.routes` | Read-only daemon RPC methods (allowlist `is_read_only_method`, `nodes.rs:178`) | **Device hub RPC**, same allowlist enforced on the node |
| `ui.*` | Event emitters | **In the DO**; delivered to whichever client is connected |

### 3.2 Three identities, no shared bearers

| Identity | Credential | Lifetime | Proves |
|---|---|---|---|
| **User session** | Supabase Auth JWT from the dashboard | Session | Which person is calling; verified at the edge Worker against the project's keys |
| **CoS service identity (per user)** | JWT signed by the Worker's Ed25519 key, claims `sub=<user>`, `aud=dashboard`, `scope=[board:read, board:write, notes:write, ...]`, expiry 60 s, jti | 60 s | The cloud CoS is acting for exactly that user, with these scopes. The dashboard verifies the signature and re-checks workspace membership (RLS) for `sub` |
| **Node credential** | Existing node bearer for heartbeat and sync, plus an Ed25519 challenge from the node identity key (ADR-025) when opening the device hub | Per connection | This is node N, paired into the mesh, and its owner is user U |

Rules:

1. The cloud never holds a long-lived dashboard token. Phase 1 may reuse the per-project harness credential (`wfb_`), but it must be minted per user per project, stored sealed, and rotatable; the end state is the 60 s service JWT above **[D]**.
2. A node credential only opens a hub to the DO of the user who owns the node. The Worker looks the owner up in Supabase; it is never taken from the connection.
3. The per-endpoint bearers of `cos.v1.1` (`endpoints.rs`) stay for the local mode only. In the cloud, an "endpoint" (`phone`, `mentra`, `pad`, `eink`) is a row in the user's DO tied to a device credential, not a shared secret.
4. Confirmations: a `Confirm` tool call is never executed on a service token alone; the held action is released by the user's own utterance on an authenticated session, as today.

### 3.3 Third-party keys

| Key | Where it lives | Rotation |
|---|---|---|
| Model provider key (Anthropic, OpenRouter) | **Default**: platform key under an AI Gateway budget per user. **Option**: user brings their own key | Platform: rotate in the secret store; BYOK: user re-enters |
| ElevenLabs key | Platform key at first; per-user keys later if a plan needs it. Single-use tokens exist for browser sockets, so the key itself never reaches the phone **[V]** | Rotate the secret; tokens expire on their own |
| Dashboard credentials | Minted by the dashboard on demand (service JWT) | 60 s |

Storage for per-user keys **[D]**: envelope encryption. A data key per user is generated in the DO and wrapped by a master key held in Workers Secrets (Secrets Store is account-level and open beta, so it is a home for the master key, not for per-tenant secrets **[V]**). Sealed blobs live in the user's DO SQLite. Reads decrypt in memory for one call. The master key rotates by re-wrapping data keys, which needs no user action. Keys are never logged, echoed in events, or placed in prompts (the existing `no_secrets.rs` test moves with the port).

### 3.4 The device hub (the bridge in reverse)

The relay in PR #29 is a pull design: the Mac long-polls the dashboard, so nothing needs to reach the Mac. Keep that property. The cloud CoS reaches WeftOS by **an outbound connection from the user's node**:

```
 user's node (Mac, Pi, PG, ...)                         cloud
 weftos-cos bridge (extended) ──WSS, node-authenticated──> DO CosAgent(user) device hub
   - receives:  rpc.call(read-only), display.scene, glance.set, kickoff.request, audio.play
   - sends:     events up, device status, audio.in, results
```

- **Transport**: a hibernatable WebSocket server in the user's DO; the node dials out, so no inbound port, no tailnet, no public address on the node.
- **Fallback**: the existing sync and long-poll routes remain (Supabase queue), for nodes that cannot hold a socket and for Phase 1.
- **Messages** are signed requests: `{id, kind, issued_at, exp, nonce, payload}` signed by the DO's key, verified by the bridge against a key pinned at pairing. Replay is rejected by `id` and `exp`.
- **Capability scoping (ADR-120)**: the bridge executes only what the node's exposure registry lists. Proposed exposure ids **[D]**: `cos.rpc.read` (the read-only daemon methods), `cos.display` (a leaf scene), `cos.glance` (e-ink), `cos.audio.glasses`, `harness.kickoff` (ADR-120 section 3). Nothing is exposed by default; the local switch wins; the dashboard may switch off but not on (ADR-120 decisions); every request lands in the activity log without bodies.
- **Names (ADR-114)**: the user's CoS is `weftos://<mesh>/agents/chief-of-staff?rev=sha256:...`; a running cloud instance is `weftos://<mesh>/instances/<ULID>`; a device is `weftos://<mesh>/nodes/<node id>`; a capability is that node's exposure id, not a path. A name grants nothing: the exposure check and the audit record come first.
- **Gap**: ADR-114 makes the mesh the authority. A phone-only user has no mesh. See Q3.

### 3.5 Device paths and events

| Device | Path | Notes |
|---|---|---|
| **Phone** (default) | Browser mic to the DO over WebSocket, STT in the cloud; replies as text plus streamed TTS audio to the browser. No node needed | Replaces today's WAV upload to `cos-audio` and transcription on the Mac. The 4.5 MB / 15 s clip route stays as fallback |
| **Glasses** | The user's Mac or Android bridge runs the existing record/play loops; STT and TTS happen in the cloud; the bridge sends 16 kHz audio up and plays returned audio over adb. `speak:true` on the `mentra` endpoint is unchanged | Audio capture, RMS gating and adb stay local; the transcriber and the speech render move |
| **Pad** | `cos-console-host` is unchanged but points `--agent` at a local loopback **cloud conversation adapter**: a `Conversation` implementation that forwards to the DO and replays its events. The adapter is the same bridge process | The leaf channel and `display.*` RPC stay on the node (`docs/design/display-session-transport.md`) |
| **E-ink** | `glance` consumes the same adapter's event stream and writes serial as now | No change |

**Events.** One ordered stream per user, with a monotonically increasing `seq` so reconnecting clients resume (`?since=seq`). The DO keeps a ring buffer in SQLite. Fan-out: phone (WebSocket), device hub (WebSocket per node), dashboard Weaver tab (the same WebSocket instead of polling every 2 s). Final turns are also written to `cos_turns` in Supabase for history and the existing tab.

### 3.6 Versioned API contract (`cos.v2` sketch)

`cos.v1` is unchanged and remains the local contract. `cos.v2` is `cos.v1` plus tenancy, sequencing and device addressing. Both are served by the cloud Worker so old clients keep working.

| Purpose | `cos.v1` (today) | `cos.v2` (cloud) |
|---|---|---|
| Send a turn | `POST /v1/cos/utterance {text, source, endpoint?}` -> `202 {turn_id}` | `POST /v2/me/utterances` same body, adds `client_msg_id` (idempotency) -> `202 {turn_id, seq}` |
| Events | `GET /v1/cos/events` (SSE) | `GET /v2/me/events?since=<seq>` (SSE) and `WSS /v2/me/ws` (both directions). Each event gains `seq` and `ts` |
| Guide answer | `POST /v1/cos/answer` | `POST /v2/me/answers` |
| History | `GET /v1/cos/turns?limit` | `GET /v2/me/turns?before=&limit` |
| Status | `GET /v1/cos/status` | `GET /v2/me/status` (adds `devices[]`, `quota`, `models`) |
| Health | `GET /healthz {ok, api}` | `GET /v2/healthz` |
| Push-to-talk | `POST /v1/cos/ptt` (host mic only) | Removed. Audio arrives on the socket: `audio.start{endpoint,format}`, binary frames, `audio.end` |
| Devices | n/a | `GET/PUT /v2/me/devices/{id}`, `WSS /v2/hub` (node-authenticated) |
| Settings | config file | `GET/PATCH /v2/me/settings` (voice, speak flags per endpoint, model tier, key slots) |
| Auth | loopback free; remote per-endpoint bearer | User JWT (phone), node credential (hub). No static bearers |

New or changed event kinds: `device.status`, `audio.out{format, chunk}` (TTS chunks, or a signed R2 URL), `stt.partial`, `stt.final`, `tool.held{action_id, summary}` for the confirmation turn. All existing kinds (`state`, `token`, `message{final, endpoint, speak}`, `tool`, `page`, `status`) keep their shapes.

**Coexistence.** (1) A **cloud conversation adapter** implements the Rust `Conversation` trait and serves local `cos.v1` on loopback by talking `cos.v2` upstream: every existing consumer works unchanged. (2) A **local mode** flag keeps `weftos-cos serve` as the agent: offline, or when the owner prefers local models. (3) A user setting `agent_location = cloud | local` decides which one the dashboard talks to; the relay tables already carry the node, so a local agent is just a node that answers requests. (4) The two implementations share one conformance suite replayed from the Rust fixtures.

---

## 4. Voice

### 4.1 What Grok has done **[V]**

`~/dev/mentra/scripts/mentra-cos-speak.py` watches the loopback SSE stream; for a final `message` with `endpoint: mentra` and `speak: true` it POSTs the text to the ElevenLabs text-to-speech endpoint with `model_id: eleven_v4`, a fixed voice id, `output_format=pcm_24000`, wraps the PCM as a 24 kHz mono WAV and pushes it to the glasses over adb. The key is read from the environment or the shell profile and is never printed. Replies are cut at 700 characters. STT is unchanged (local Parakeet through `weftos-cos transcribe`).

### 4.2 What the model table says **[V]** (elevenlabs.io/docs/overview/models)

| Model | Role | Latency (vendor figure) | Realtime |
|---|---|---|---|
| `eleven_v4` | Highest quality; what Grok uses | "Standard" | No |
| `eleven_v4_turbo` | Same family, conversational | about 100 ms | Yes |
| `eleven_flash_v2_5` | Cheapest/fastest, 32 languages | about 75 ms | Yes |
| `eleven_v3_conversational` | Conversational v3 | about 280 ms | Yes |
| `scribe_v2` | Batch STT | standard | No |
| `scribe_v2_realtime` | Streaming STT | about 150 ms | Yes |

`eleven_v4` is not a realtime model, so it suits pre-rendered replies on glasses; for a phone conversation use a turbo or flash model over the streaming socket.

### 4.3 Plan

| Leg | Choice | Why |
|---|---|---|
| TTS (phone) | `stream-input` WebSocket from the DO, `eleven_v4_turbo` default, flash as the budget tier; stream sentences from the model as they complete; PCM or Opus chunks forwarded to the phone | The WebSocket accepts streamed text chunks and returns audio chunks; client-side single-use tokens exist **[V]** |
| TTS (glasses, pad) | Render `eleven_v4` once per reply, cache the clip in R2 by content hash, bridge pulls and plays | Quality over latency; replay and barge-in are cheap |
| STT (phone, glasses) | `scribe_v2_realtime` over WebSocket with VAD or turn-prediction commit; PCM 16 kHz | Supports `partial_transcript` and `committed_transcript`; single-use tokens **[V]** |
| Barge-in | A new `audio.start` cancels the current `audio.out` and closes the TTS socket | Matches today's push-to-talk barge-in (`speak.rs`) |
| Local fallback | Parakeet (`weftos-cos transcribe`) and `say`/`command` engines stay behind the same `Speak` seam, used in local mode or when the cloud leg errors | Offline continuity and a cost control |
| Spoken length | Keep the 700 character cap and speak the first sentence first (AGENT.md already requires a standalone first sentence) | Cost and latency |

### 4.4 Latency budget for a phone turn **[I]**

| Stage | Target |
|---|---|
| Phone to DO (warm socket) | 40 to 100 ms |
| STT final after end of speech (turn prediction) | 150 to 300 ms |
| Model first sentence (quick tier, tools skipped) | 400 to 900 ms |
| TTS first audio (turbo, streamed) | 100 to 250 ms |
| Audio to ear | 50 to 100 ms |
| **Total to first sound** | **about 0.75 to 1.6 s** with no tool call; add about 0.5 to 1.5 s per tool round (dashboard API call plus a second model call) |

### 4.5 Cost per active user per month **[I]** (platform prices **[V]**, usage assumed)

Assume 30 voice turns a day, 300 spoken characters per reply, 10 minutes of speech input a day.

| Item | Math | Result |
|---|---|---|
| TTS at $0.04 per 1,000 chars (turbo/flash) | 30 x 300 x 30 = 270k chars | about $11 |
| TTS at $0.08 per 1,000 chars (v4/v3) | same | about $22 |
| STT realtime at $0.39/hour | 10 min x 30 days = 5 h | about $2 |
| Cloudflare platform | requests + duration (section 2.4) | well under $1 |
| Model tokens | provider prices not re-verified here | likely the largest line; set a per-user budget |

**Hard constraint**: ElevenLabs lists concurrent-call limits by plan: 4 (Starter), 6, 10, 20, 30 (Business), 40 (Enterprise) **[V]**. A fleet of simultaneous users beyond that needs an enterprise arrangement or a second vendor. Speech vendors must sit behind a `Speak`/`Listen` interface from day one.

---

## 5. Migration plan

Each phase is reversible by a setting, not a redeploy. Before Phase 1 the owner's Mac setup is untouched.

### Phase 0: today (baseline)

Mac-hosted `weftos-cos serve`; bridge relays to the dashboard; Grok's ElevenLabs speak loop on the glasses.
- **Exit criteria to start Phase 1**: PR #29 merged; the bridge branch merged; Grok's speak loop stable; owner answers Q1 to Q4.

### Phase 1: dual-run, owner only (flag `cos_runtime = local | cloud | shadow`)

- Build: Worker + DO with the TypeScript loop; dashboard service-JWT verification; read-only tools (`board.*`, `projects.activity`, `nodes.health` via dashboard data); `ui.*`; text only; one user (the owner).
- `shadow` mode: the cloud CoS receives a copy of each utterance and answers into a hidden log; no tool with side effects; compare answers and tool choices against the Mac.
- **Acceptance**: the Rust conformance fixtures pass against the TypeScript loop (event order, gating, forced final answer, redaction, history cap); a recorded week of owner turns replays with tool-choice agreement of at least 90 percent on the chosen cloud quick model **[D target]**; no secret appears in events, logs or the R2 bucket (scan).
- **Rollback**: set `cos_runtime = local`. No data migrated yet; delete the DO.
- **Owner decisions**: Q1, Q2, Q5, Q8.

### Phase 2: phone-only users in the cloud

- Build: `cos.v2` endpoints; WebSocket on the `/phone` Weaver tab; cloud STT/TTS; per-user DO provisioning on first use; per-user budgets and rate limits; write tools (`board.claim`, `board.note`, `notes.record`) behind the `Confirm` gate; `board.propose` replacement.
- **Acceptance**: a new user with only a phone completes onboarding, asks a question, gets a spoken answer in under 2 s to first audio at the 90th percentile **[D target]**; two users cannot see each other's turns (cross-tenant test across the API, the socket and R2); a killed tab reconnects and resumes with `since=seq`; the daily spend cap stops a runaway user.
- **Rollback**: flip the user to `local` (owner only) or to a read-only "CoS unavailable" state for others; turns are retained.
- **Owner decisions**: Q3, Q4, Q6, Q7.

### Phase 3: data migration (owner's history into the cloud)

| Data | Source | Move | Check |
|---|---|---|---|
| Turns | Mac in-memory ring (200) and the bridge's synced `cos_turns` | Import from Supabase `cos_turns` into the DO | Counts and last-turn ids match |
| Brain | `~/.weftos/brains/chief-of-staff/<installation>.rvf`, entries with `weftos.brain-metadata/1` | Export entries (metadata plus text) as JSONL; re-embed in the cloud; keep ids and `supersedes` | Every required field present (`brain.rs` REQUIRED list); nearest-neighbour spot check; the local `.rvf` stays authoritative until Phase 5 |
| Notes | Project history through the harness | Already in the dashboard | None |
| Settings and endpoints | `cos.json`, token files | Recreate as `settings` and `devices` rows; **do not** copy bearer tokens | Old tokens revoked after cutover |
| Staged proposals | Local staging dir | Drain through Stew first | Directory empty |

Brain sync after the move is an append-only entry log with stable ids, so local and cloud brains can reconcile; the cloud brain is per user and never promoted to another user (ADR-121 decision 9).

### Phase 4: devices

Order by value and risk: glasses (the audio loops already exist, only STT/TTS move), then e-ink (read-only), then pad (needs the leaf channel and the cloud conversation adapter).
- **Acceptance (each)**: with the cloud as the agent, the device works end to end; unplugging the device does not stall the phone; switching the exposure off locally removes the capability in the next request (ADR-120 local-wins); the activity log records each request.
- **Rollback**: point the device loop back at the local agent (`agent_location = local`).
- **Owner decisions**: Q9.

### Phase 5: Mac decommissioned as the always-on host

The Mac runs `weftos-cos serve` only in local mode. Remove the Mac's cloud API keys from its shell profile and rotate the ones that lived there.

### Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| 1 | Cloud quick model fails tool calls (the Qwen3-4B experience) | Med | High | Eval gate in Phase 1; keep Haiku-class fallback; do not ship Workers AI quick tier without the eval |
| 2 | Cross-tenant leak (routing bug, shared cache) | Low | Critical | DO per user by construction; user id only from the verified JWT, never from the body; automated cross-tenant tests |
| 3 | Service-JWT key compromise | Low | High | 60 s tokens, scoped, key rotation, dashboard revalidates membership on each call |
| 4 | Speech vendor concurrency/cost cap | Med | Med | Vendor-neutral interface; per-user caps; local fallback |
| 5 | TypeScript port drifts from the Rust loop | Med | Med | One conformance suite; Rust remains the local mode, so drift is visible |
| 6 | Device hub exposes the node | Low | High | Outbound only; ADR-120 exposures; read-only RPC allowlist; local-wins; hash-chained activity log |
| 7 | Prompt injection through board text reaching write tools | Med | High | `Confirm` gate on every outward write; tool results labeled as data (existing prompt rule); scope tokens per tool |
| 8 | Vendor lock-in to Durable Objects | Med | Low | The contract is `cos.v2`; the loop is plain TypeScript; Vercel is the documented alternative |
| 9 | Platform feature in beta changes (Secrets Store, Vercel WebSockets) | Med | Low | Neither is on the critical path of the recommendation |
| 10 | Rolling out without a mesh identity for phone-only users | High | Med | Q3 |
| 11 | Cost surprise from model tokens | Med | Med | Per-user budgets, gateway logs, spoken-length caps |
| 12 | Audio privacy (voice recordings in the cloud) | Med | High | No storage by default; short-TTL R2 only for fallback clips; user-visible retention setting |

---

## 6. Open questions for the owner

| # | Question | Default recommendation |
|---|---|---|
| Q1 | Cloudflare or Vercel for the runtime? | Cloudflare (DO per user); keep Vercel/Supabase as system of record |
| Q2 | Rewrite the loop in TypeScript or port the Rust crate to WASM? | TypeScript, with the Rust fixtures as the conformance suite |
| Q3 | What is a phone-only user's `weftos://` authority, given ADR-114 makes the mesh the authority? | Give each workspace a dashboard-minted mesh record (the cloud holds the "workspace mesh"); upgrade it to a real mesh when the first node pairs |
| Q4 | Who pays for model and speech usage: the platform, or the user's own keys? | Platform keys with a per-user monthly budget; BYOK as an option |
| Q5 | Which cloud quick model? | Start with the Haiku-class model already in `model.rs`; evaluate a Workers AI model (the GLM family) against the tool-call eval before using it |
| Q6 | Speech vendor policy: ElevenLabs only, or a second vendor behind the interface? | Interface now, ElevenLabs only until concurrency limits bite |
| Q7 | Voice and model for the product voice: keep Grok's current voice and `eleven_v4`? | `eleven_v4` for cached replies, `eleven_v4_turbo` for live phone turns; same voice id |
| Q8 | Add a gateway (Vercel AI Gateway or Cloudflare's) for logs, budgets and failover? | Yes. Vercel's is verified to work from other clouds; confirm Cloudflare's before choosing |
| Q9 | Device order for Phase 4? | Glasses, e-ink, pad |
| Q10 | Should the dashboard be allowed to switch a CoS exposure on, or only off? | Off only, as ADR-120 decided for other exposures |
| Q11 | Data residency and retention for turns and brain entries? | Retain turns 90 days by default, brain until deleted by the user, audio not stored |
| Q12 | Does the cloud CoS appear in the AI org page as an `instances` entry per user? | Yes: one `instances` record per user, reporting a summary only (ADR-121 decision 8) |

---

## 7. First build slice (for the later kickoff)

1. `cos-edge` Worker + `CosAgent` DO skeleton with `cos.v2` health, utterance, events; auth by dashboard JWT.
2. TypeScript loop with `ui.*` and the read-only harness tools; conformance suite from the Rust fixtures.
3. Dashboard routes for the service-JWT (`/api/cos/tools/*` or direct harness calls with per-user tokens) and a `cos_runtime` setting per user.
4. Shadow mode behind the flag; compare to the Mac for a week.

Board tickets are the owner's call; this document creates none.

---

## 8. Sources

Platform facts (fetched 2026-10-13):
- Cloudflare Workers limits: https://developers.cloudflare.com/workers/platform/limits/
- Workers pricing: https://developers.cloudflare.com/workers/platform/pricing/
- Durable Objects pricing: https://developers.cloudflare.com/durable-objects/platform/pricing/
- Durable Objects limits: https://developers.cloudflare.com/durable-objects/platform/limits/
- WebSocket Hibernation: https://developers.cloudflare.com/durable-objects/best-practices/websockets/
- Queues pricing: https://developers.cloudflare.com/queues/platform/pricing/
- Workers AI pricing: https://developers.cloudflare.com/workers-ai/platform/pricing/
- Agents SDK: https://developers.cloudflare.com/agents/
- Secrets Store: https://developers.cloudflare.com/secrets-store/
- Vercel function limits: https://vercel.com/docs/functions/limitations
- Fluid compute: https://vercel.com/docs/fluid-compute
- Fluid pricing: https://vercel.com/docs/functions/usage-and-pricing
- Vercel WebSockets: https://vercel.com/kb/guide/do-vercel-serverless-functions-support-websocket-connections
- Vercel Workflows: https://vercel.com/docs/workflow
- Vercel AI Gateway: https://vercel.com/docs/ai-gateway
- ElevenLabs models: https://elevenlabs.io/docs/overview/models
- ElevenLabs API pricing: https://elevenlabs.io/pricing/api
- ElevenLabs TTS WebSocket: https://elevenlabs.io/docs/api-reference/text-to-speech/v-1-text-to-speech-voice-id-stream-input
- ElevenLabs realtime STT: https://elevenlabs.io/docs/api-reference/speech-to-text/v-1-speech-to-text-realtime

Not verified this session: model-provider token prices, Cloudflare AI Gateway features, Workers AI tool-call reliability, DO placement and wake latency (all marked **[I]** where used).
