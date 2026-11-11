# Fleet compatibility: ruOS fleet and Cognitum Seed fleet

> **Cogs project:** the operational detail for Cognitum Seeds, the Pi 5, fleets, tooling, testing on hardware and upstream work lives in the private cogs repo (`weave-logic-ai/cognitum-cogs`, `docs/`; locally `<client workspace>/cognitum/cogs-main`). Its decision records are the COG-NNN series. This WeftOS doc keeps only what WeftOS implements.

Date: 2026-09-29. Read-only research for the placement layer (ADR-099/100/101, cards `mesh-placement-01..21`). Nothing was enrolled, registered, provisioned or written to any live service. Only read calls were made: the ruos-fleet MCP `guide`, `cluster_peers`, `fleet_status`; the public `GET https://api.cognitum.one/v1/openapi` and `/v1/mcp/tools`; and local vendor source.

Labels: **[V]** verified (read source or a live response, cited), **[I]** inferred, **[U]** unverified or private.

## Decisions (2026-09-29, user)

- **Direction (b) is approved.** Using the Seed API and the Cognitum cloud MCP from automated adapters is fine with the user, since these are their own accounts.
- **The ruOS fleet is dropped for now.** It isn't needed. Option b3 and card `mesh-placement-24` are dropped. Sections 1a and 2 (b3) stay only as reference.
- **An x86 cloud node isn't needed.** The user already has them.
- **Cog Studio** (`/v1/me/cog-*`) is unknown, and will be raised on the Cognitum call before we build anything that could overlap it.
- **Cards `mesh-placement-22` and `-23` are adopted** and added to `goal-and-cards.md`.

## 0. Names: "ruvfleet" and "cogfleet"

Neither string appears anywhere I could search **[V]**: `grep -rIn -i 'cogfleet|ruvfleet|ruv-fleet|cog-fleet'` over the local clones of the Cognitum vendor repos hits only `cog-fleet-auth` (below); `gh search repos` for `ruos-fleet` and `ruos-desktop` returns nothing relevant; `ruvnet/ruos-desktop` and `cognitum-one/ruos-desktop` do not resolve (private or nonexistent). The ruvnet-brain `search_ruvnet` tool returned no results (router declined twice; coverage of this area is nil) **[V]**.

Closest real things:

| Name people might mean | What it actually is | Evidence |
|---|---|---|
| **ruOS fleet** ("ruvfleet") | Hosted control plane for cloud Linux desktops plus enrolled Macs/PCs, at `https://ruos.cognitum.one/mcp` | plugin `.mcp.json`; `guide` response **[V]** |
| **Seed fleet** ("cogfleet") | Cognitum Seed devices registered with the Cognitum cloud (`/v1/seed/*`), plus Seed-to-Seed mesh/swarm | OpenAPI; `sdks/docs/adr/0002`, `0016a` **[V]** |
| `cog-fleet-auth` | A **cog** (binary in `vendor/cogs/src/cogs/fleet-auth`) doing device-certificate management across Seeds. Its own header says its signing is "simplified... not cryptographic", so do not treat it as an identity source | `fleet-auth/src/main.rs:1-60` **[V]** |
| `swarm-*` cogs | Cogs that fan out over the Seed HTTP API to peer Seeds (mesh-manager, deploy, cluster-monitor, witness-federation, delta-sync) | `cogs/src/cogs/swarm-*/src/main.rs` **[V]** |
| `cognitum-one/ruOS` | A **different product**: the "ruvultra" local AI-workstation distro (`ruos-agent`, OTA from GitHub releases, ESP32 sensing, .deb pipeline). Not the fleet. Same brand word, different thing | `vendor/ruOS/README.md`; `gh search repos` **[V]** |
| `ruvnet/ruos` (MIT) | Public repo for the *desktop* product: `ruos-mcp` computer-use server + skills. No fleet/enrollment code | cloned, `README.md`, `mcp/README.md` **[V]** |

Do not conflate three ruOS things: the hosted desktop fleet (private control plane), the public `ruvnet/ruos` MCP/skills repo, and the `cognitum-one/ruOS` workstation distro.

## 1. What each fleet is

### 1a. ruOS fleet (hosted agentic desktops)

- **Control plane [V]:** `https://ruos.cognitum.one/mcp`, streamable HTTP MCP. Server source is **not public [U]** (`ruos-desktop` repo does not resolve; it is cited as ADR-009/016/018/022 in `ruvnet/ruos/README.md`).
- **Auth [V]:** OAuth against `https://auth.cognitum.one`, scopes `mcp:read`, `mcp:invoke`; tenant comes only from the token, no tool takes a tenant arg (plugin `README.md`; `guide`). Headless alternative: `POST https://ruos.cognitum.one/api/v1/mcp/tokens` returns a revocable `ruos_mcp_` token sent as `Authorization: Bearer`.
- **Machines [V]:** `fleet_status` returned one machine: `fly_machine_id`, `gcp_instance: null`, `rustdesk_id: null`, `hbbs_db_available: false`. So the current desktop is a **Fly.io machine**, while `ruvnet/ruos/README.md` describes GCP `e2-standard-4` Ubuntu 24.04 GNOME. The two docs disagree or the hosting changed; treat hosting as unstable **[V/I]**.
- **Node agent [V/U]:** on the desktop, `ruos-mcp` (Rust, 16 tools, stdio, "actions deliberately unrestricted, no per-action confirm" per its README) plus a loopback executor at `127.0.0.1:17870` (`ruos-welcome-server`). Reached over stdio-over-SSH in the public repo; the hosted path's agent/enrollment protocol is **private [U]**.
- **Enrollment [V/U]:** `guide` says enrolled Macs/PCs join as RustDesk endpoints and appear in `fleet_status`. `rustdesk_id` is a field on each machine. The enroll handshake, client (`ruOS Connect`, RustDesk-based) and what identity it registers are **undocumented [U]**.
- **Identity [V]:** machines have opaque `id`, `fly_machine_id`, optional `gcp_instance`, `rustdesk_id`. No public key or signature field is exposed in any response I saw. RustDesk itself uses Ed25519 keys at hbbs **[I, RustDesk upstream design, not read here]**.
- **Transport [V]:** cluster peers get Fly 6PN hosts (`<machine-id>.vm.ruos-desktop.internal`, app `ruos-desktop`, profile `desktop`, from `cluster_peers`). 6PN is Fly's private IPv6 network, reachable only from inside that Fly org **[I]**. Remote viewing is RustDesk, noVNC or iPad PWA (`ruvnet/ruos/README.md`). No Tailscale/WireGuard in this path.
- **Workload model [V]:** there is no cog or package concept. The unit is a *desktop* (provision, duplicate, start, stop, delete, keepawake, share, exec, upload, computer-use, schedules, secrets, llm route). Tool list from the connector: `desktop_*`, `fleet_status`, `fleet_health`, `cluster_peers`, `computer_*`, `schedule*`, `secret*`, `llm_route_get/set`, `share_*`, `clip_*`, `activity_*`, `slack_*`. The guide says the `cluster` group offers "swarm dispatch across desktops", but only `cluster_peers` is exposed on this connector, so the dispatch tool and its semantics are **unverified [U]**.
- **Telemetry [V]:** `peer_telemetry: "unavailable"`; the hbbs DB "is not readable from this control plane, so registration and online state are UNKNOWN" (ADR-074 D4). Real liveness is only `desktop_status.last_heartbeat_at`. Weekday 23:00 America/Toronto auto-stop applies to `desktop_start`/`provision`.
- **Commercial terms [U]:** no ToS found for the hosted service. The plugin manifest names the author as Mathew Beane and the service as ruos.cognitum.one; no license text for the service itself.

### 1b. Cognitum Seed fleet

- **Cloud control plane [V]:** `https://api.cognitum.one` (OpenAPI 1.0.0, 102 paths). Seed-facing routes only: `POST /v1/seed/register` (no auth; body `deviceId` uuid, `publicKey` base64 Ed25519, optional `firmware`; 200/409), `POST /v1/seed/heartbeat` (body `uptime_secs`, `free_memory_kb`, `total_vectors`, `epoch`, `wifi_ip`, `version`), `POST /v1/seed/analytics`, `POST /v1/seed/event`, `GET /v1/seed/check`, `GET /v1/seed/firmware/{version}`. The last five require scheme `ed25519`: header `X-Device-Signature` over `METHOD\nPATH\nTIMESTAMP\nsha256(body)` with `X-Device-Id` and `X-Device-Timestamp`.
  - The lead's brief said `X-Device-ID + X-Signature`; the OpenAPI says `X-Device-Signature`/`X-Device-Id`/`X-Device-Timestamp`. The Seed-local API is different again: `X-Signature`/`X-Signed` are "reserved, not enforced" in ADR-0003 (2026-04, seed v0.20.0). Firmware 0.24.2 may have changed this **[U]**.
- **Cloud fleet view [V]:** the OpenAPI has **no `/v1/fleet*` or `/v1/devices` REST paths**. Fleet exists only as MCP tools at `POST /v1/mcp` (public catalog at `GET /v1/mcp/tools`): `fleet_status` (input optional `region`, permission `devices:manage`) and `device_register` (same shape as REST register). Auth: OAuth 2.1 (`mcp:read`/`mcp:invoke` per the OpenAPI; the lead saw more scopes, unverified here). Also present: `/v1/me/cog-instances`, `/v1/me/cog-instances/operations`, `/v1/me/cog-operations`, `/v1/me/cog-proposals` (Firebase `bearerAuth`, tenant-scoped, "Cog Studio" governed change proposals). Whether these manage on-device cogs or cloud-hosted cog instances is **unknown [U]**; request bodies are untyped `object`.
- **Node agent [V]:** `cognitum-agent` (Rust, `seed/src/cognitum-agent/`, **source not public [U]**; described only through `sdks/docs/adr/0002`, `0016a`, `0017` and `vendor/support/docs/api.md`).
- **Enrollment [V]:** two separate things. (1) Cloud registration: device generates an Ed25519 key at first boot and calls `/v1/seed/register`; how a registered device is *claimed by a tenant* is not in the OpenAPI **[U]**. (2) Local pairing: `GET /api/v1/pair/status`, `POST /api/v1/pair {client_name}` within a 30 s window returns a token sent as `X-Pairing-Token`; v0.20.0 lets an already-authed admin open the window from any path (ADR-0003, `api.rs:4452-4471` as cited).
- **Auth tiers [V]:** unpaired reads (10 burst/2 rps), paired writes (100/20), localhost (1000/200), lockdown adds client-cert mTLS (ADR-0002 table). Self-signed TLS on 8443 (plain HTTP on 80 also serves).
- **Transport [V]:** USB gadget `169.254.42.1`, `cognitum.local` mDNS (`_cognitum._tcp.local`), LAN/WiFi, optionally a Tailscale address (ADR-0011). Seed-to-Seed: HTTPS delta sync of `/api/v1/store/sync`, and a **mesh overlay (ADR-084)**: encrypted UDP tunnels with mTLS, STUN, relay fallback via the GCP control plane, max 20 peers (ADR-0016a lines 20-40). The lead observed overlay IP in 100.64/10 and a NAT type on firmware 0.24.2 (from the lead's brief, not re-checked **[U]**). Overlay wire protocol is private.
- **Workload model [V]:** cogs. `GET /api/v1/apps/installed|available`, `POST /api/v1/apps/install`, `DELETE /api/v1/apps/{id}`, plus per-app `start|stop|console|logs|config|manifest` (ADR-100 s5; `vendor/support/docs/api.md`). `swarm-deploy` fans `POST /api/v1/apps/install` and `DELETE` out to peers (`swarm-deploy/src/main.rs:3-4,91-94`); `swarm-mesh-manager` builds a peer registry from each peer's `/api/v1/status` (`main.rs:1-8,63`). Peer roster endpoints: `GET /api/v1/peers`, `/swarm/peers`, `/swarm/status`, `/cluster/health`, `POST /api/v1/peers/sync` (ADR-0016a).
- **Telemetry [V]:** heartbeat/analytics/event to the cloud; local `GET /api/v1/status`, `/witness/chain`, `/custody/*`, `/identity` (ADR-0002/0003).
- **Licensing [V]:** `vendor/cogs` is MIT (`cogs/LICENSE`); `vendor/sdks` is Apache-2.0; the Claude plugin's `fleet-auth` manifest says `"license": "Proprietary"`. Cognitum's 30% per cog is contractual (settled decision), not a technical gate.

## 2. Compatibility options

Effort scale: S under 2 days, M under 2 weeks, L longer. "Private" means source or protocol not available to us.

### (b) Their machines as WeftOS placement targets (recommended direction)

| Option | Shape | Effort | Risks | Private/undocumented |
|---|---|---|---|---|
| b1. **Seed via its own API** | `remote.api` runtime adapter over `/api/v1/apps/*`. **Already the decided v1 (ADR-100 s5; folded into card 09; onboarding in card 15).** | already planned | Tenant model: local pairing token is per-client and per-Seed, not tenant-scoped. Firmware 0.10.x upgrade hit the witness-chain `writes_gated` state (ADR-100). Docs (ADR-0002, 2026-04) lag firmware 0.24.2 | Agent source; mesh overlay; whether `X-Signature` is now enforced |
| b2. **Seed fleet via cloud MCP as a facts/telemetry source only** | A read-only `remote.api` "inventory" adapter calling `fleet_status` on `/v1/mcp` to discover Seeds and their state; placement still goes through b1 | S-M | Needs `devices:manage` permission and an OAuth token stored as an operator secret; tenant is the token's tenant; fleet output shape unverified (I never called it) | `fleet_status` output schema; scope list |
| b3. **ruOS desktop as a plain WeftOS node** | Use `desktop_exec` (fleet MCP) to install `weaver` on a desktop VM, then it is a normal mesh member (native adapter). Fleet API is only the *provisioning* channel | M | Desktop auto-stops weekday 23:00 Toronto (`guide`), so it is a preemptible node; 6PN is not reachable from outside Fly so the desktop must dial out to a reachable seed peer, and our Mac is behind NAT (needs a public rendezvous); `ruos-mcp` "no per-action confirm" plus `desktop_exec` means the fleet token is effectively root on that VM. x86_64, not ARM, so cogs (armv7/aarch64) run only under emulation, which is operator opt-in | Hosted ToS, VM SLA, whether `desktop_exec` is allowed for long-running daemons, Fly vs GCP hosting |
| b4. **ruOS cluster "swarm dispatch" as a runtime** | Adapter mapping a workload to cluster dispatch | L, blocked | Only `cluster_peers` is visible; dispatch tool and semantics unknown; desktops have no package/cog model, so there is nothing to place except arbitrary shell | Everything about dispatch |
| b5. **Enrolled Macs/PCs (RustDesk endpoints) as targets** | Would need the private enroll protocol | not feasible now | Registration/online state is UNKNOWN by design (ADR-074 D4); no exec channel besides RustDesk remote control | Enroll handshake, hbbs |

### (a) WeftOS nodes joining their fleets

| Option | Shape | Effort | Risks | Private/undocumented |
|---|---|---|---|---|
| a1. **weaver enrolls as a ruOS machine** | Would need the private enroll flow (RustDesk-based) | not feasible | Impersonating a desktop client in a commercial service; unknown ToS | Whole protocol |
| a2. **WeftOS node presents a Seed-compatible API and registers as a Seed** | Technically the public parts are small: generate Ed25519, `POST /v1/seed/register`, signed heartbeats (`METHOD\nPATH\nTIMESTAMP\nsha256(body)`), and a local `/api/v1/*` facade. **Do not do this.** | M for a facade, L to be convincing | It would pose as Cognitum hardware in Cognitum's fleet and analytics; likely a ToS/misrepresentation problem; polluting `devices:manage` inventory; heartbeat fields (`total_vectors`, `epoch`) are meaningless for us. Registration is unauthenticated, so the tenant-claiming step is unknown [U] | Claim/tenant binding; overlay mTLS (ADR-084) so a fake Seed could not join the mesh anyway |
| a3. **Honest sibling: publish WeftOS nodes to *their* orchestration as generic MCP servers** | Expose a WeftOS MCP surface Cognitum tooling could call | M | Adds an inbound surface to weaver; no consumer identified | none |

Verdict: (a) has no upside that (b) lacks, and a2 carries real terms-of-service risk. Keep WeftOS as the placement authority and treat their fleets as *targets*.

### (c) Identity and governance bridging

| Item | Design | Status |
|---|---|---|
| Seed identity | Seed device key is Ed25519, base64 pubkey plus uuid `deviceId` (`/v1/seed/register`; `GET /api/v1/identity` local **[V]**). WeftOS `node_id` is a "hex-encoded public key hash" (`mesh.rs:121-123` **[V]**). Bind them with an **operator-signed record** `{seed_device_id, ed25519_pub, operator_node_id}` chained as `workload.node.bind`; the adapter's node id is the WeftOS-side hash, never the Seed's raw id. This matches ADR-100's "operator-assigned node id per Seed" and makes the open question there concrete | design [I] |
| Never share keys | Seed's private key stays on the device; we only pin its public key so we can verify custody/witness attestations from `GET /api/v1/witness/chain` and `/custody/attestation` | [I] |
| ruOS identity | Opaque ids only; no key to map. Bind by `machine_id` plus operator record, trust tier `paired` at most | [V/I] |
| Capability claims | A Seed/desktop cannot sign `SignedCapabilityAdvertisement` (`capability_claim.rs:109`) itself. Facts are **adapter-attested**: signed by the adapter's WeftOS node key, provenance `claimed` (from `/api/v1/status`, `fleet_status`) or `measured` (conformance harness, card 08). Never `probed` from the device | [I] |
| Gating | Every call through either adapter goes through `workload.*` gate actions first and chains the result (card 05). Governance lives at the adapter, not on the device, exactly as ADR-100 s5 already states. Their tokens (`X-Pairing-Token`, `ruos_mcp_`, OAuth) go in the operator secret store, never in chain events | [I] |
| Tenant mapping | Fleet tenant = whoever holds the token. WeftOS records `{fleet, tenant_hint, token_fingerprint}` in the chain for each adapter registration; one adapter instance per tenant token | [I] |

## 3. Recommendation

**Direction: (b) first, starting with what is already decided (b1), then add b2 (read-only inventory) and b3 (weaver on a ruOS desktop) as the new pieces. Do not build a1/a2.**

Reasons: b1 is validated on real hardware and is already cards 09/15; b2 adds discovery without any write path; b3 is the only way the ruOS fleet becomes useful to us today, and it is honest (we run our own software on a machine we rent). b4/b5 are blocked on private information.

**Minimal vertical slice (extends Goal A, no new goal):**
1. Cards 02, 04, 05, 07, 09 land as planned (Seed adapter included).
2. New card 22 adds the identity bind record and adapter-attested facts (section 2c) to the Seed adapter, so `weaver cluster nodes --facts` shows the Seed with `trust.tier.paired`, provenance `claimed`, and a chained `workload.node.bind`.
3. New card 23 adds a read-only `fleet_status` inventory adapter that lists Seeds from the cloud MCP and proposes (does not create) node bindings.
4. Card 15 acceptance is unchanged; add "Seed appears both via local pairing and via the cloud inventory, with matching public key, and a mismatch is refused and chained".
5. (Dropped 2026-09-29: ruOS fleet not needed.) Card 24 (stretch, after the slice) provisions weaver on a ruOS desktop via `desktop_exec` under a chained Permit, joins the mesh, and receives an x86 `inference` or `cog` workload under emulation flag off (so it should be `Unplaceable` for aarch64 cogs, which is itself a useful negative test).

### New cards (22 and 23 adopted 2026-09-29; 24 dropped)

| Key | Title | Depends on | Acceptance / completion |
|---|---|---|---|
| mesh-placement-22 | Fleet identity binding and adapter-attested facts | 03, 05, 09 | Operator-signed `{device id, pubkey, node id}` record verified against the Seed's `/api/v1/identity`; tampered or mismatched key refused; facts signed by adapter with provenance `claimed`. Completion: chain export shows bind event for the real Seed |
| mesh-placement-23 | Cognitum cloud MCP inventory adapter (read-only) | 22 | Calls `fleet_status` with an operator-held OAuth token; outputs candidate bindings; zero write calls (enforced by test); token never in chain. Completion: candidate list matches `weaver cluster nodes` for the paired Seed. Needs: user grants the scope, and output schema is captured first |
| ~~mesh-placement-24~~ (dropped 2026-09-29) | ruOS desktop as a preemptible weaver node | 12, 13, 22 | `desktop_exec` install of weaver under a chained Permit; node joins by dialing out to a reachable seed peer; auto-stop at 23:00 Toronto is handled as `Dead` then reschedule (card 13). Completion: scripted place, auto-stop, reschedule. Blocked on the ToS and open questions below |

## 4. Open questions for the user

1. **Terms.** Is there a ToS or agreement for ruos.cognitum.one and the Seed cloud (`api.cognitum.one`)? Running our own daemon on a rented ruOS desktop (b3) and calling the cloud MCP from an automated adapter (b2) both need this cleared. Who at Cognitum can confirm?
2. **Are ruos-fleet and the Seed cloud the same tenant/account system?** Both use `auth.cognitum.one` per the plugin README and the OpenAPI, so probably yes, but I could not confirm. It decides whether b2 and b3 can share one token.
3. **Scopes.** The OpenAPI lists only `mcp:read`/`mcp:invoke` for `mcpOAuth`, while your brief lists `account/governance/spaces` too. Which scopes should the inventory adapter be allowed to hold (I recommend `mcp:read` only for b2)?
4. **Desktop hosting.** `fleet_status` shows a Fly machine (`gcp_instance: null`) while `ruvnet/ruos` documents GCP. Is the Fly path the real one now? It changes the 6PN reachability story in b3.
5. **Is a cloud-hosted x86 node useful to you?** Cogs are ARM (armv7/aarch64), and inference workloads want GPU/unified memory. A ruOS desktop is CPU x86. If the answer is no, drop b3 and card 24.
6. **What are `/v1/me/cog-instances` and Cog Studio?** If they manage on-device cogs, there is a second, governed, cloud-side path to Seeds that overlaps our own `workload.*` governance and should be discussed with Cognitum before we go direct.
7. **Firmware drift.** Everything I read about the Seed API is ADR-0002/0003 (2026-04, v0.20.0) plus your 0.24.2 observations. Do you want a re-measured endpoint inventory on the real Seed (read-only `GET`s on `/api/v1/*`, `/openapi.json`) before card 09 hard-codes paths?

## Could not verify

- Private repos: `ruos-desktop` control plane, `seed/src/cognitum-agent` (only ADR citations of file:line, not read), hbbs/RustDesk enrollment.
- `fleet_status` (cloud MCP) output schema; I never called `/v1/mcp` (OAuth). Only the ruos-fleet `fleet_status` was called.
- Any `cluster` dispatch tool (not exposed on this connector).
- Whether Seed request signing is enforced on firmware 0.24.2, and the meaning of Cog Studio endpoints.
- Overlay (100.64/10, NAT type) facts come from your brief, not from a re-read of the device.
- Scratch clone of `ruvnet/ruos` was made in the session scratchpad only.
