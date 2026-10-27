# ADR-106: The Cognitum Seed is the licence proxy for its WeftOS mesh

- **Status**: Proposed, review complete (3 rounds). Phases 1a to 2 built and integrated; phase 3 in progress (part B: steward relay, run gate, checkout CLI)
- **Successor**: [COG-106](https://github.com/WeftOS/weftos-cogs/blob/v0.1.3/docs/decisions/COG-106-host-licence.md) in WeftOS/weftos-cogs (Cogs extraction, 2026-10-27). Cogs owns Cog Host licence enforcement and its operator behaviour. This ADR still owns the licence protocol, mesh identity, governance facts and the run gate. `weftos-cog-host` now lives in that repository (as `cog-host`); `weft-licence` stays in WeftOS. This record stays as written.
- **Review history**:
  - Round 1: B1–B2, H1–H5, M1–M7, L1–L6.
  - Round 2: N1–N5, decided by the lead.
  - Round 3: final check SOUND-WITH-CHANGES; A1–A4 text edits applied (additive approvals, mesh_id check, origin conditions, per-kind sync cursors).
- **Date**: 2026-10-03
- **Updated**: 2026-10-03. Review round 1 (SOUND-WITH-CHANGES) folded in:
  - B1: the signer is a separate process under its own uid.
  - B2: ADR-100 6.3 is not superseded before phase 4, and members re-check the registry sha256 themselves.
  - H1: `mesh_id` includes the cluster-owner key, and open membership is refused.
  - H2: only mesh-admitted connections are served.
  - H3: the production transport is phase 1 work.
  - H4: catch-up sync.
  - H5: the steward can delay a withdrawal, and any operator-Admin node can unbind.
  - M1 to M7 and L1 to L6 are folded in below.

  Review round 2 (SOUND-WITH-CHANGES), owner decisions written in:
  - N1: `mesh_id` uses an operator `mesh_nonce` instead of the per-machine owner key.
  - N2: an operator hash approval per version is the run gate in phases 1 to 3, and the direct registry check is optional.
  - N3: the mesh service stamps a delivery origin, with an ADR-103 amendment.
  - N4: sync is bounded.
  - N5: every `weft-licence` endpoint except identity needs a steward signature.
  - Nits: the clamp rule (M2 nit) and the request domain (L3).

  This round supersedes the round 1 notes for H1 and B2 above.
- **Deciders**: Owner / platform
- **Depends-On**:
  - ADR-099 (placement; section 6 swarm, section 8 trust)
  - ADR-100 (cog kind; section 5 Seed, section 6 trust)
  - ADR-103 (mesh service, admission A10, K1; machine and user tenants)
  - ADR-105 (cog sources; section 3 licensing)
  - mesh-facts (`RedistributionPolicy` with `Audience`, `GrantOrigin`, `ServePeer`), now in `integrate/cog-repo`
- **Relates-To**: COG-009 (`weft-cog-host` on the appliance) and COG-011 (per-node Ed25519 request signing, clock floor), both in the private cogs repo
- **Implementation**: Phases 1a, 1b, 1c, 1d and 2 built and integrated on branch `integrate/licence` (see the status lines below). Phase 3 part A (the licence path in service mode) and part B (steward relay, run gate, checkout CLI, renewal pull) built and integrated on `0.8-metaharness`; the operator verbs `weaver cog checkout release | renew | list | reset-floor` are built on `wt/lic-verbs`; the `weft-cog-host` start check is built on `wt/seed-start-check` (the real-Seed run is pending); phase 4 is not started.
- **Implementation status, phase 1a**: done (`wt/seed-1a`, integrated). Module `crates/clawft-kernel/src/licence/` (gated like the mesh swarm code): `MeshId`, binding v2 with the member profile, `CheckoutGrant`, `Approval`, `CheckoutGrantStore`, `ApprovalStore`, the persisted clock floor with the clamp, `MeshCheckoutPolicy`, `ArtifactExchange::grant_checkout`, and the `may_run` gate. Not yet: wiring `may_run` into placement (phase 3). Detail choices made in 1a are listed under "Phase 1a implementation notes" below.
- **Implementation status, phase 1b**: done (`wt/seed-1b`, integrated). `licence/exchange*.rs`: `LicenceExchange` floods bindings on `mesh.cog.binding` and grants plus approvals on `mesh.cog.grant`, and runs catch-up sync on `mesh.cog.sync` (all three are runtime control topics in `mesh_runtime.rs`). `ChainLicenceSink` chains the store events as `licence.<name>`, including the new `sync_bad_signature`; it is the node's only licence sink (`licence_boot` installs it). The daemon starts one exchange next to `RevocationExchange` (`wire_licence` in `workload_place_rpc.rs`) over the boot-owned store and policy; it is inert while the local mesh id is unset. Floods and sync go to licensed peers only (`CtxAdmission`: verified and class `node`, the 1c `licensed_peer` rule; outbound from the route's admitted class, `MeshRuntime::peer_licensed`), so an admitted leaf gets none. The exchange is the steward relay's `GrantFlood` (`cog_swarm::grant_flood`), which has no production caller until the daemon gets a steward relay (phase 3). An accepted `workload.node.bind` or `workload.node.unbind` is flooded at once through the exchange (`issue_binding`), not left to the 30-minute sync. Detail choices are under "Phase 1b implementation notes".
- **Implementation status, phase 1c**: done (`wt/seed-1c`, integrated). The `Deliver` origin stamp (ADR-103 A15; `clawft-mesh-local` protocol 2, `clawft-mesh-service` router) and its use in the daemon's `mesh_local_sink`. Kernel: `mesh_artifact_tunnel.rs` (artifact streams over deliveries, with the production `PeerDialer`), `mesh_cog.rs` (the router in front of the daemon's inbox, the requester side), `licence/request.rs` (request signing), `licence/client.rs` (`LicenceClient`, `LicenceTransport`, `SignedLicenceClient`), `licence/relay.rs` (`CheckoutRelay`, `GrantFlood`). Daemon: `cog_swarm.rs`, installed by placement next to the exchange. `CtlConnection::with_peer_verified` replaces the always-unverified peer in `workload_ctl/session.rs`. `scripts/build.sh gate` check 21 fails if the daemon enables `clawft-mesh-local`'s `testing` feature. Not yet: the real HTTP `LicenceTransport` and so a steward relay in the daemon (the daemon answers `no_steward`; a relay built later, in phase 3, takes `cog_swarm::grant_flood()`, which has no production caller until then). Detail choices are listed under "Phase 1c implementation notes".
- **Implementation status, phase 1d**: done (`wt/seed-1d`, integrated). `kernel.mesh.mesh_nonce` config entry next to `genesis_hash`; `weaver mesh nonce generate`; the daemon derives the mesh id and sets `LocalMeshId` at boot (`licence_boot.rs`), builds the checkout policy and grant store there (the node's only ones), owns the 60 s store tick, and chains `licence.binding_orphaned` at boot. `StewardCheck` (the `BindingExtraCheck` for the steward profile) and `SeedBinder::bind_v2` / `unbind_v2` (`workload_runtime/seed_bind/v2.rs`). RPC `workload.node.bind` (Admin, moved out of `NOT_YET`), `workload.node.unbind` (Admin), `workload.node.binding` (Read, status). RPC `workload.node.reset-floor` (Admin; store `reset_floor`, chained as `licence.floor_reset`). CLI `weaver workload node bind|unbind|reset-floor|status`. `weaver doctor` findings `licence.*`. Not in 1d: `weaver cog checkout approve` (the approval verb). Since integration the bind RPCs flood what they accept (phase 1b exchange).
- **Implementation status, phase 3 part B**: done (`wt/lic-p3-relay`). Kernel: `licence/http_transport.rs` (`HttpLicenceTransport`: pinned or lab opt-in only, timeouts, capped bodies, no redirects or proxy), `licence/steward_client.rs` (`StewardLicenceClient`: signs for the binding in effect, refuses `seed_not_bound` / `not_steward` without sending), `licence/run_gate.rs` (`check_run`, `CognitumRunGate`, `StoreRunGate`, stable refusal codes), `licence/store_status.rs` (grant and approval rows), `workload_ctl/host_licence.rs` (the gate at `place`/`load` and at every `start`), `CogMesh::set_relay`. Daemon: `licence_steward.rs` (`licence-link.json`, the relay built at placement start and installed in the cog mesh, the run gate handed to the local `workload-host`), `licence_checkout_rpc.rs` (`workload.cog.checkout` Admin, `workload.cog.checkout.approve` Admin, `workload.cog.checkout.status` Read), CLI `weaver cog checkout <cog>@<version> | approve [--reapprove-orphaned] | status [--explain]`, doctor findings `licence.approval_missing`, `licence.grant_expiring`, `licence.approvals_orphaned`, `licence.no_steward`. A bound steward with the link file no longer answers `no_steward`. Detail choices are under "Phase 3 implementation notes".
- **Implementation status, phase 3 part A (service mode)**: built (`wt/lic-p3-service`). Before it, a user daemon in the recommended ADR-103 service-mode topology had no signing key (`DaemonIdentity::signing_key` returns `KeyHeldByService`), so placement, `licence_boot`, `wire_licence` and `cog_swarm::install` never ran, and the service's runtime consumed `mesh.cog.binding|grant|sync` as control topics with no sink, so those records never reached the daemon. Now: the daemon signs placement and steward requests with a daemon-local control key (decision below), the licence runtime and the licence exchange start at boot in both modes, the service hands licence records from licensed peers to the reserved-topic owner's daemon, and the daemon's exchange runs over `ServiceLicenceLinks` (stamped deliveries in, sends through the service link out, the service's peer view). Detail and the signing decision are under "Phase 3 part A implementation notes".
- **Implementation status, phase 2**: done (`wt/seed-p2`, integrated through `wt/seed-1c`). `crates/weft-licence` (the Seed-side service) and `crates/weft-licence-wire` (the shared grant, binding, envelope and request-signing types the kernel re-exports). Detail choices are under "Phase 2 implementation notes".

## Owner decisions this ADR implements (not reopened here)

1. The Seed works as a proxy for the one WeftOS mesh it is attached to. Licensing works and the Seed device is not overloaded.
2. The Seed fetches and maintains the licence, the Cognitum credential, the cogs and anything else needed. When a node needs to check out a cog, the Seed handles it.
3. One Seed licence covers the whole attached mesh. **Pending C3**: Cognitum must agree to this commercially.
4. Once a cog is checked out, no other node needs permission. A checkout is per mesh, not per node, and any node in that mesh may fetch, run and share the cog with no further Seed round trip. This also keeps load off the Seed: it serves the bytes once per version, and mesh peers share them through the swarm.

## Context

What exists, from local sources:

- **Licence is a per-project declaration.** ADR-105 section 3 gates a `cognitum` install on an unsigned `[[cog_licence]]` record (`crates/weftos-cog-sources/src/licence.rs`). The check is presence, coverage and expiry. Each installing node needs its own record. Cognitum installs record `placement_eligible = false`, because ADR-100 6.3 requires an operator to hash and sign an upstream binary before governed placement.
- **The swarm refuses Cognitum bytes.**
  - The policy point is `RedistributionPolicy::allows(hash, grants, &Audience)` in `mesh_swarm_state.rs`. The audience is `Serve(&ServePeer)` (a node id plus a verified flag), `Advertise` or `Seed`.
  - Each grant carries a `GrantOrigin`: `Cognitum {cog_id, version}`, `OptIn` or `NotFlagged`.
  - The default `ManifestPolicy` allows a hash only when every grant is `OptIn`. That is finding F3.
  - Non-`OptIn` content is never put in broadcast facts. Peers find it only through `who_has`, where the serving side sees who is asking.
  - The policy is the `ExchangeConfig.redistribution` field, set once at construction.
  - `CogPackageBody` is `deny_unknown_fields`.
- **No production peer-to-peer artifact transport.**
  - `PeerDialer` is test-only.
  - The only production serve is `workload_ctl` `session.rs` (`serve_frame`), and it serves an *unverified* peer.
- **Admission.**
  - A peer counts as verified only when it is admitted: its hello verified, the mesh is in `enforce` mode, and the verdict is a permit (ADR-103 A10, K1).
  - The hello signs a `genesis_hash`. That is an operator-chosen label, not a credential.
  - `kernel.mesh.admission_open_membership` admits any valid hello with that label (`boot.rs`, the `bind_open` branch).
  - `TenantRouter::deliver_local` builds `PeerCtx {node_verified: true}` for every local tenant (`router.rs`), including another OS user's tenant on the same machine.
  - Daemons never see admission:
    - the mesh service's `Deliver` message carries no admitted flag (`clawft-mesh-service/src/router.rs`);
    - the daemon side builds `PeerCtx::unauthenticated` (`mesh_local_sink.rs`).
  - The cluster owner is a per-machine uid, and `user.key` is per machine and per user (`clawft-mesh-service/src/state.rs`, `verdicts.rs`). Neither is a mesh-wide identity.
- **Seed binding is a library.**
  - `workload_runtime/seed_bind.rs` verifies an operator-signed `BindRecord` against the Seed's `GET /api/v1/identity`. It accepts a record only within a 600 s age window and persists replay memory.
  - `workload.node.bind` is a reserved method in the daemon's `NOT_YET` list (`clawft-weave/src/workload_rpc.rs`).
  - Seed links must be pinned (https with an SPKI pin), unless the runtime dir sets a per-Seed lab opt-in.
- **Revocation floods without catch-up.** `mesh_swarm_revoke.rs` signs notices with `Operator`/`Weftos` keys, and any operator node can `issue` one. Each node forwards a notice once. There is no anti-entropy, so a late joiner never receives an earlier notice.
- **`weft-cog-host` on the Seed** (`crates/weftos-cog-host`):
  - It spawns every cog as its own uid (`supervise.rs`).
  - `POST /install` accepts an unsigned binary whose sha the requester supplies.
  - It binds `0.0.0.0` over plain HTTP. Its bearer token is a shared secret on the LAN.
- **The real Seed** (memory notes, firmware 0.24.2, not re-checked here):
  - The agent serves plain HTTP on :80 and a self-signed :8443.
  - `apps/install` also starts the cog. There is no fetch-only call.
  - Active cogs are capped at 3, and the cap is compiled in.
  - The userland is armhf, and there is no RTC.
  - The account sign-in is held by the agent.
- **COG-011 clock handling.** While the clock is below a floor date, the bridge refuses to verify signed requests (`clock_not_set`) and the relay does not send.

**Gap.** Licensing is per project and declarative, and the swarm refuses Cognitum bytes, so every node downloads from Cognitum under its own declared licence. That is the opposite of decisions 3 and 4.

## Decision

**What this ADR is.** It scopes WeftOS redistribution of Cognitum cogs to one admitted mesh. It is not copy protection, and it is not payment enforcement.

### 1. Roles

| Role | Holds | Never holds |
|---|---|---|
| **`weft-licence`** (on the Seed) | the licence, the grant key, the cache of checked-out binaries, the mesh binding, the persisted `seq` and clock floor | mesh membership. It does not seed the swarm and runs no cogs. |
| **Steward node** (one admitted WeftOS node) | the link to `weft-licence`, the steward key, the first copy of each artifact | the grant key, the licence, Cognitum tokens |
| **Member nodes** | the binding and grants (received by flood and sync), and copies fetched from peers | Cognitum tokens, any link to the Seed |

Mesh nodes never see a Cognitum token. `weft-licence` does not need one either, as long as the registry stays public (C2).

### 2. Where the proxy logic runs: `weft-licence`, a separate process on the Seed

The licence check, the Cognitum fetch, the hash checks and the grant signing run on the Seed in **`weft-licence`**. It is a WeftOS-built binary installed signed-only (COG-008) beside `weft-cog-host`, but it is **not part of `weft-cog-host`**:

- It runs as its own system user (`weft-licence`) under its own service unit.
- Its key file is mode 0600 in a 0700 directory that this user owns.
- It shares nothing with `weft-cog-host`: not the uid, not the bearer token, not the API.

`weft-cog-host` runs every cog as its own uid and accepts unsigned installs from anyone holding the LAN-wide token. A grant key inside that process would be readable by any cog it runs and by any token holder. The `weft-licence` unit has no endpoint that runs code. It does not import the cog-host request policy. Its listener is separate from cog-host's `0.0.0.0` (section 7).

Why on the Seed: the Seed is the licence holder (decision 2). Signing belongs on the box that holds the licence and the account. Why not as an agent cog: it would take one of the 3 slots, a config `PUT` restarts it, and the agent's sandbox is unknown.

What the firmware already offers:
- identity (`GET /api/v1/identity`),
- the store listing with sha256,
- the account sign-in.

What it does not offer:
- fetch without install,
- a device-key signing call usable by a local process (the `custody` tools are a hint only; **assumption**),
- an entitlement API.

So `weft-licence` fetches from the registry the way the `cognitum` source kind does (`weftos-cog-sources`: https, registry sha256, https-to-http redirect refused). It signs with its own grant key.

Rejected options:
- A WeftOS node drives the store API. The key and the licence would sit on the node, and the bytes would only come out through install, which also starts the cog, or through SSH.
- A node acts for the Seed. Same objection.
- A module inside `weft-cog-host`. Its uid is shared with the cogs it runs (B1).

### 3. Mesh binding: one Seed, one mesh

**Mesh id (W1, decided).**

```
mesh_id = sha256("weft-licence-v1/mesh-id\n" || genesis_pin || mesh_nonce)
```

- `genesis_pin` is the 32-byte genesis pin that admission compares.
- `mesh_nonce` is a random 32-byte value. The operator generates it once per mesh and writes it into every node's mesh config next to the genesis pin. It is not a secret, but it must be identical on every node.
- Two meshes that copied a genesis label still get different ids unless they also copied the nonce.
- The cluster-owner key cannot be the input. The cluster owner is a per-machine uid and `user.key` is per machine and per user, so members would compute different ids, and a key rotation would silently change the id.
- The owner property comes from the operator signature on the binding record, not from the id.
- **Id changes are surfaced.** If a node's computed `mesh_id` no longer matches its stored binding (the nonce or the pin changed), it chains `licence.binding_orphaned`, turns the checkout policy off, and `weaver doctor` reports it. A new binding for the new id fixes it.

**Admission requirement.** A node accepts a binding, and turns on the checkout part of its policy, only when all of these hold:
- admission is `enforce`,
- a governance verdict source is bound,
- `admission_open_membership` is **off**.

Otherwise it chains `binding_refused: open_membership` and stays on the `ManifestPolicy` behaviour.

**Binding record.** It extends `BindRecord` to v2:

```
{ v: 2, device_id, device_pubkey, node_id, mesh_id, grant_pubkey,
  steward_node_id, steward_pubkey, state: bound|unbound, seq, bound_at }
```

- It is signed by a pinned operator key under the domain tag `weft-licence-v1/binding`.
- Operator approval is the signature. There is no automatic binding.

**Where `grant_pubkey` comes from.**
1. The operator runs `weft-licence init --operator-key <hex>` over the USB link (physical presence).
2. That generates the grant key and prints the grant key's fingerprint.
3. The operator compares the fingerprint with what `weaver` shows before signing the binding.
4. The key is never learned over the network, so it cannot be swapped in transit.

**Two verification profiles (M1).**
- *Steward, full bind.* The full `SeedBinder` checks, including the 600 s age window and the live `GET /api/v1/identity` match. In addition, the grant key's fingerprint must match the one the operator confirmed, `mesh_id` must equal the local mesh id, and `seq` must exceed the last accepted `seq`. It is reached through the reserved method `workload.node.bind`, which this ADR moves out of `NOT_YET` (Admin; state in `dir.join(BIND_STATE_FILE)`).
- *Member, record only.* Members check the operator signature, that `mesh_id` equals the local id, that `seq` exceeds the last stored `seq`, and the `state`. There is no age check and no Seed contact, because members have no Seed link. A different record at an equal `seq` is refused and chained (`binding_conflict`).

**Distribution.** On the control topic `mesh.cog.binding`, using the revocation flood rules:
- cheap checks first (size, pinned key, already applied),
- a per-connection budget before signature verification,
- each record forwarded once,
- plus the sync step in section 5.

**Rebind and unbind.**
- *New steward.* Same Seed, higher `seq`. Grants continue, because they do not name the steward.
- *Unbind.* An operator-signed `state: unbound` with a higher `seq`. **Any operator-Admin node may issue it** and flood it, the same way `RevocationExchange::issue` works, so a steward cannot block it.
- *Move the Seed to another mesh.* Unbind first. `weft-licence` then deletes its grant key, and `init` runs again over USB.
- *No operator available.* A USB local reset wipes the binding and the key. Old grants lapse at expiry, and A's operator can revoke the old grant key with a `SignerKey` notice.

**One mesh only.** `weft-licence` stores one binding and refuses a bind for another `mesh_id` (`seed_bound_elsewhere`). Phases 1 to 3 allow one Seed per mesh.

**Licence scope inside a member machine (ADR-103).** A Seed licence covers the **admitted machines** of the mesh. The project kernels and user daemons on an admitted machine are covered as part of that machine. They obtain Cognitum cogs through that machine's own node, under the owning user daemon's governance gate. They are never served as a swarm peer. Only a delivery stamped `AdmittedPeer` by the mesh service counts as a peer (section 5.3). Another OS user's tenant on the steward's machine is not a swarm peer, and its checkout request is not accepted (section 4). Whether per-machine scope fits Cognitum's terms is part of C3.

### 4. Checkout protocol

1. **Local first (decision 4).** A node that wants `cognitum:<cog>` version V for its arch first looks for a valid grant for this mesh that covers (cog, V, arch). If it finds one, it fetches from peers via `who_has`, with no request to anyone.
2. **Request.** Otherwise it sends `CheckoutRequest {request_id, cog_id, version | "latest", arch}` to the steward on the `mesh.cog.checkout` request topic.
   - The steward accepts it only when the mesh service stamped the delivery `origin: AdmittedPeer` (section 5.3) and the peer's class is `node`. `LocalTenant` and `Unadmitted` deliveries are refused.
   - It also accepts requests from its own kernel.
   - It asks its governance gate for `cog.checkout` (Write).
   - Concurrent requests for the same (cog, version, arch) are merged.
3. **Relay.** The steward calls `POST /licence/v1/checkout` on `weft-licence`. Each request is signed with the steward key, using the COG-011 field layout under its own domain. The signed string starts with `weft-licence-v1/request`, then method, path, node, timestamp, nonce and body hash. `weft-licence` accepts only the bound `steward_pubkey`.
4. **Seed decision.**
   - If no licence covers the cog, or the licence has expired: `cog_unlicensed` or `licence_expired`.
   - If a valid grant already exists, the existing grant is returned. Bytes are sent only on request.
   - Otherwise `weft-licence` fetches that arch from the registry and checks its size and registry sha256. It then computes BLAKE3 (the swarm content hash), caches the binary, **writes the new `seq` durably**, and only then releases the grant.
5. **Steward.** It verifies:
   - the signature, the binding and `mesh_id`,
   - the sha256 and BLAKE3 of the bytes it received,
   - that `issued_at` is within 5 min of its own clock (otherwise `seed_clock_skew`).

   It then stores the bytes, registers the grant and chains `cog.checkout.granted`, and floods the grant on `mesh.cog.grant`.
6. **Use.** Other nodes fetch from peers. The steward is the first seeder.
7. **Before running (B2, N2).** A node installs or runs the bytes only when it holds **both** a valid grant and a valid **operator hash approval** for that (cog, version) whose sha256 set covers the artifact. A grant alone never makes bytes runnable.

**Operator hash approval (the run gate in phases 1 to 3).**
- `weaver cog checkout approve <cog>@<version>` (Admin) signs `{v: 1, mesh_id, cog_id, version, sha256: [..], approved_at}` with a pinned operator key under the domain `weft-licence-v1/approval`.
- The operator approves after checking the hashes, for example against the registry or an upstream release.
- **Approvals are additive and content-addressed, with no seq (A1).** Each one is keyed by (mesh_id, cog, version, sha256 set). A duplicate is idempotent, and a new approval never replaces an older one. To withdraw an approval, use the existing revocation notice for the artifact's hash (`ArtifactHash`, BLAKE3), which also evicts the bytes.
- Approvals are flooded on `mesh.cog.grant` and carried by sync (section 5.5).
- **A verifier accepts an approval only if (A2):**
  - its signature verifies under the `weft-licence-v1/approval` domain tag with a pinned operator key;
  - `approval.mesh_id` equals the local `mesh_id`.
- **Orphaned approvals.** A `mesh_nonce` change orphans approvals as well as the binding. `weaver doctor` lists the orphaned approvals, and `weaver cog checkout approve --reapprove-orphaned` re-signs them in one batch for the new id.
- One signature per version per mesh is the cheap form of ADR-100 6.3. It replaces the per-node operator re-sign for running checked-out cogs. It does not replace the governed-placement `cogpkg` signature (section 10).
- A compromised grant key alone therefore cannot make any node run code.
- **Direct registry check.** A node that can reach the Cognitum registry may also compare the sha256 with the registry entry. That is an extra check, never the only gate: requiring every member to reach Cognitum would break decision 2 and offline meshes.
- **Phase 4 (pending C9).** If Cognitum signs its listing or release entries, `weft-licence` attaches the signed entry verbatim to the grant. Members verify it offline against a pinned Cognitum release key, and that replaces the approval.

**Grant format.** Canonical JSON, signed Ed25519 by the grant key, domain tag `weft-licence-v1/grant`:

```
{ v: 1, grant_id,                        # sha256 of the signed payload
  mesh_id, seed_device_id, grant_key_id, # "ed25519:" + 16 hex
  source: "cognitum", registry, cog_id, version,
  artifacts: [{ arch, size, sha256, blake3 }],
  manifest_sha256,                       # registry entry used
  licence: { ref_sha256, expires },      # hashed licence ref; no plaintext account
  seq, issued_at, expires_at }
```

`grant_id` is the sha256 of the canonical payload with `grant_id` set to the empty string, since the payload cannot contain its own hash. `grant_key_id` is `ed25519:` plus the first 16 hex chars of `sha256(grant public key)`. Verifiers refuse a grant whose `grant_id` or `grant_key_id` does not match, whose lifetime exceeds 7 days, or whose artifacts are unsorted or duplicated by arch.

**Replacement rules (M6).**
- For one (mesh, cog, version), the highest `seq` wins and a lower `seq` is ignored.
- A newer grant carries the **union** of every arch checked out so far for that version, so adding an arch never drops another.
- Two different payloads with the same `seq` under one key are a fault: both are refused, `grant_conflict` is chained, and the newest earlier grant is kept. `weft-licence` makes this impossible by persisting `seq` before release.
- A **withdrawal** is a renewal with `expires_at <= issued_at`. Seed keys never sign revocation notices.

**Renewal.**
- Default TTL is 72 h, with a maximum of 7 days.
- The steward pulls `GET /licence/v1/grants?since=<seq>` every 12 h and on demand. The Seed never connects into the mesh.
- Renewal batches hold at most 256 grants.
- A checkout stays active until it is released (`weaver cog checkout release`) or the licence stops covering it.

**Clock (M2).**
- `weft-licence` refuses to sign until its clock passes a build-time floor and its persisted last `issued_at` (`clock_not_set`).
- Each verifier keeps, per grant key, `floor = max(highest accepted issued_at, persisted local-now high-water mark)`. It persists the floor and uses `max(now, floor)` for expiry, so a reset clock cannot revive a grant.
- A grant more than 5 min ahead of `max(now, floor)` is deferred as not yet valid and retried at the next sync.
- The persisted mark never goes down, and its growth is capped: `hw = max(hw, min(now, max_issued + 30 d))`, recorded only once a grant has been accepted. A forward clock jump therefore cannot push the floor more than 30 days past the newest `issued_at`, and a clock set back never revives an expired grant. Undoing a jump that did land is the manual `weaver cog checkout reset-floor` (Admin, chained). (Round 4 replaced the earlier rule, which clamped the floor back down and so let a set-back clock revive expired grants.)

### 5. Swarm, transport and sync

1. **Policy (M5).** The daemon always builds `ExchangeConfig.redistribution` as `MeshCheckoutPolicy`, in a new `mesh_checkout_policy.rs`. It wraps `ManifestPolicy` and holds `Arc<CheckoutGrantStore>` plus the local `mesh_id`.
   - With no accepted binding, it returns exactly what `ManifestPolicy` returns (tested).
   - A binding that arrives at runtime takes effect with no restart.
   - The rule: allow if `ManifestPolicy` allows. Otherwise allow only when all of these hold:
     - every non-`OptIn` grant for the hash is `Cognitum {cog_id, version}`;
     - the store holds a valid grant for this `mesh_id`, under the current binding's key, that covers the hash with the same cog and version;
     - the audience is `Serve(peer)` with `peer.verified`, or `Seed`.
   - `Advertise` is always denied, so peers look the content up via `who_has`.
   - Validity is checked on every call. A sweep drops expired grants and evicts bytes that no running instance pins.
2. **Grant source.** `ArtifactExchange::grant_checkout(&VerifiedCheckoutGrant)` calls `grant_with(blake3, "checkout:<cog>@<version>", [grant_pubkey_hex], GrantOrigin::Cognitum {..})`. Revocation of the key, the hash or the package then applies through `is_revoked_subject`. This makes the bytes *shareable* inside the mesh. It does not make them runnable (section 4, step 7) or placement-eligible (section 10).
3. **Who is a peer (H2, N3).** The daemon cannot see admission today: `Deliver` carries no admitted flag, and `mesh_local_sink` builds `PeerCtx::unauthenticated`. Decision:
   - The machine mesh service stamps every `Deliver` with a service-vouched `origin: AdmittedPeer {node_id, class} | LocalTenant | Unadmitted`.
   - The daemon trusts `origin` only on the authenticated local service socket, the one the mesh-local protocol already guards with peer credentials. A field arriving any other way is ignored.
   - **Conditions for honouring `origin` (A3).**
     - It is honoured only when the negotiated mesh-local protocol version is at least the version that adds the field.
     - It is honoured only after the client's anti-squat check on the service peer has passed (`clawft-mesh-local` `client.rs`, the service peer-credential check).
     - On a single-user install where `service_uid` equals the daemon's uid, `AdmittedPeer` is only as strong as that uid. Anything running as that uid could already act as the daemon, so this is no new escalation.
     - The daemon is never built with `clawft-mesh-local`'s `testing` feature.
   - Only `AdmittedPeer` with class `node` builds a verified `ServePeer` or is accepted for checkout.
   - `TenantRouter::deliver_local` stamps `LocalTenant`, never `AdmittedPeer`. This settles H2.
   - **ADR-103 amendment needed:** the mesh-local `Deliver` message gains the `origin` field, versioned. A daemon talking to an older service, which sends no `origin`, treats every delivery as `Unadmitted`. A service never forwards an `origin` supplied by a tenant.
4. **Transport (H3, phase 1c).** This is new production work:
   - the `Deliver` origin stamp in `clawft-mesh-service` and its consumption in the daemon's `mesh_local_sink`;
   - a production artifact dialer over admitted mesh connections, replacing the test-only `PeerDialer`;
   - an inbound artifact serve that runs **in the daemon**, which holds the `ArtifactExchange`. It is fed by service-stamped deliveries and builds `ServePeer` from `origin` for `serve_as` / `serve_frame_as`;
   - changing the existing `workload_ctl` `session.rs` `serve_frame` to take the stamped origin instead of `ServePeer::unverified`.

   Until this exists, Cognitum bytes move only between the steward and its own kernel.
5. **Catch-up sync (H4, N4).**
   - On each `AdmittedPeer` connect, and every 30 min, a node asks a peer (preferably the steward) for the binding, the highest-seq grant per (cog, version), and the set of approvals.
   - A node answers at most one sync per peer per minute, and only for `AdmittedPeer`.
   - **Response caps:** at most 256 entries and 256 KiB per response. Anything larger is paged **per record kind, with a separate cursor for each**: grants by `seq`, approvals by content key. A large set of one kind cannot hide the other (A4).
   - **Cheap filters first:**
     1. the record kind and size;
     2. the key id equal to the bound grant key (grants) or to a pinned operator key (binding, approvals);
     3. for grants and the binding, `seq` greater than the stored seq; for approvals, a content key not already held.

     Only entries that pass all of these reach signature verification.
   - **On the first bad signature,** the node aborts the response, discards the rest, and penalises the peer: a 10-minute sync ban and a chained `sync_bad_signature`.
   - **Budgets:** each connection has its own sync token bucket, separate from the binding, grant and revocation flood budgets, so sync traffic cannot starve them.
   - Verification is deferred while `clock_not_set` and retried at the next sync.
   - This brings late joiners, partitions and transient refusals to the same state.
6. **No manifest change.** `CogPackageBody` is `deny_unknown_fields`, so an older verifier rejects an unknown field. The grant is a separate object and is never embedded in a package. New fields go only in `provenance.json`.
7. **Fail closed.** Under `off`, `observe` or open membership, no checkout policy is active and no Cognitum bytes move between nodes. `Legacy` and `Leaf` peers are never served.

### 6. Load on the Seed

- Bytes go from the Seed to the steward once per (cog, version, arch). After that, transfer is peer to peer, and the Seed never seeds.
- The steady state is one signature per active checkout per renewal, plus one small exchange every 12 h.

`weft-licence` defaults, all configurable:

| Limit | Default |
|---|---|
| Checkouts in flight | 1 |
| Requests per minute | 10 |
| Largest artifact | 64 MiB |
| Transfer rate to the steward | 4 MiB/s |
| Cache | 256 MiB LRU (entries under an active grant are kept) |
| Renewal batch | 256 grants |
| Byte transfers per (cog, version, arch) | 3 per 24 h, **per steward key** (L1); an operator-signed override can raise it, for example after a steward rebuild |

Steward defaults: merged duplicate requests, and 5 checkout requests per minute per member.

### 7. Link between steward and `weft-licence`

`weft-licence` serves plain HTTP on **its own listener**. That listener binds only the USB link-local interface and the tailnet interface (COG-002), never `0.0.0.0`.

What protects the link:
- Signed requests and signed grants give integrity and authenticity.
- Bytes are checked against the signed hashes.
- `grant_pubkey` comes only from the USB `init` with fingerprint confirmation (section 3), so swapping it in transit gains nothing.
- Confidentiality of the licensed bytes comes only from the link itself: WireGuard on the tailnet, or the physical USB cable.

**Endpoint rules (N5).**
- Every endpoint except `GET /licence/v1/identity` requires a valid steward signature, including `GET /licence/v1/grants`.
- An unsigned or badly signed request is refused before any work is done.
- Rate limiting has two pools:
  - The steward's rate budget (section 6) is charged only **after** its signature verifies, so forged traffic cannot use it up.
  - Unsigned callers (identity, and refused requests) share a small separate pool, 30 requests per minute in total.
- Recommended: a tailnet ACL that allows the `weft-licence` port only from the steward host.

A LAN path is plain text and needs the explicit per-Seed lab opt-in (`allow_unpinned_lab_link`, read only from the runtime dir). Without TLS, a normal deployment uses the tailnet or USB. In practice every Seed-bound mesh that wants a LAN-only link needs the opt-in until TLS with a pinned SPKI is added to `weft-licence` (W4, decided in phase 2).

### 8. Lapse, unbinding and revocation

| Event | Effect on sharing | Effect on running instances |
|---|---|---|
| Licence lapses | No renewals. Grants lapse within one TTL, and a withdrawal can stop them sooner. | Keep running. A start or restart is refused (phase 3). |
| Seed offline or steward down | Grants lapse at expiry. The offline grace is one TTL. | Same. |
| Unbind (any operator-Admin node) | Every grant under the key stops when the record arrives. | Same. |
| Revocation of the key, hash or package | Immediate, and the bytes are evicted. | Same (existing semantics). |

- **Latencies (L2).**
  - A withdrawal reaches the mesh within the next steward pull (at most 12 h), then flood or sync (30 min).
  - A TTL of T means a lapse can go unnoticed for up to T. The 7-day maximum therefore means up to a 7-day lapse.
  - The steward can delay a withdrawal for up to one TTL (section 9), which is why the default is 72 h.
- **Unbind and rebind.** Unbind alone does not void grants against a later rebind to the same grant key: the grants resume. The response to a rogue or stolen Seed is unbind plus a `SignerKey` revocation of the grant key.
- **Soft stop recommended.** Sharing and new starts stop, and running instances finish. This matches current revocation semantics. It does not kill safety-relevant cogs over a clock or network fault, and the overrun is bounded by the instance's next restart.
- **Hard stop, if C5 demands it.** A `hard_stop: true` withdrawal would make placement stop those instances through the teardown path that `check_teardown` already allows.

### 9. Trust and threats

- **Grant key isolation (B1).** The key lives only in `weft-licence`'s own uid and directory. Cogs, `weft-cog-host`, its token holders and the agent cannot read it. A compromised `weft-cog-host` or cog does not yield the key; what remains is root on the Seed, which is the stolen-Seed case.
- **A compromised grant key (B2)** lets its holder make member nodes *share* bytes inside the mesh under a forged grant. It does **not** let them run code:
  - every install or run also needs an operator hash approval for that version (section 4, step 7), and the grant key cannot sign one;
  - governed placement still needs an operator signature until phase 4 (section 10).

  Response: unbind from any operator-Admin node, then a `SignerKey` revocation.
- **A malicious member forging grants.** Refused: grants verify only under the bound key (`grant_untrusted`). Replays are stopped by `seq`, expiry and the floor.
- **Honest limit.** An admitted member that holds the bytes can copy them out of the mesh. This is a redistribution scope, not DRM.
- **A malicious steward (H5)** cannot mint grants. However, it can:
  - withhold requests and renewals, which is an availability attack;
  - **suppress a withdrawal for up to one TTL**, and suppress an unbind that it is asked to relay.

  Mitigations:
  - unbind can be issued and flooded by any operator-Admin node;
  - sync from any peer spreads the record;
  - the TTL bounds the damage;
  - the steward can be replaced by a rebind.
- **A stolen Seed** can sign grants for its `mesh_id`, which only admitted members act on, and only for sharing. Response: unbind plus key revocation. There is no hardware key store (**assumption**).
- **Clocks.** `weft-licence` refuses to sign before its floor. The steward refuses grants skewed by more than 5 min. Verifiers keep a persisted floor with a clamp.
- **Local tenants (H2, N3).** They are stamped `LocalTenant` by the mesh service. They are not swarm peers, and their checkout requests are refused (section 3, licence scope).
- **A malicious sync peer (N4)** costs a node at most one capped response per minute, filtered cheaply before any signature verification. Its first bad signature gets it banned from sync.
- **Privacy.** Grants carry only a hash of the licence reference, so the Cognitum account label is not flooded around the mesh.

### 10. Reconciliation with ADR-105, ADR-100 and F3

- **ADR-105 section 3.** In a Seed-bound mesh, the `[[cog_licence]]` record is no longer the install gate on member nodes; a valid grant plus an operator hash approval is, with an optional direct registry check. The licence record moves to `weft-licence`: operator-signed in phase 2, Cognitum-signed in phase 4. Meshes without a Seed are unchanged.
- **ADR-105 section 2 and ADR-100 6.3: not superseded before phase 4 (B2).** Governed placement of a Cognitum cog still needs an operator hash and signature. The grant is shareability evidence, not a placement signer.
- For running a checked-out cog through `weft-cog-host`, the one-per-version operator hash approval is the cheap form of 6.3 and replaces the per-node re-sign.
- Governed placement keeps the `cogpkg` operator signature.
- In phase 4, once a grant carries a Cognitum-signed entry that members can verify offline (C9, C1), that entry replaces the approval, and the grant may become placement-eligible inside its mesh.
- **ADR-105 open question 4.** The grant is a signed statement of an operator-declared licence. It is not a Cognitum proof until phase 4.
- **Provenance.** Adds `trust = "mesh-checkout-grant"`, `grant_id`, `mesh_id`, `grant_key_id`, `approval_id` (the content key) and `registry_sha256_checked`.
- **ADR-103.** Needs an amendment for the mesh-local `Deliver` `origin` field (section 5.3), and the `mesh_nonce` mesh config entry next to the genesis pin (section 3).
- **ADR-099 8.4 and ADR-100 6.4.** Commercial terms stay contractual.
- **ADR-100 section 5.** The reserved method `workload.node.bind` is implemented in phase 1d. The store-pin rule for cogs the agent runs on the Seed is unchanged.
- **F3.** The default stays: Cognitum content is never redistributable. `MeshCheckoutPolicy` is the only exception, and it applies to admitted members of the grant's mesh while the grant is valid.

## Phases

All of phase 1 runs in process, with no hardware and no network, under `scripts/build.sh test` and `clippy`.

**1a. Types, store and policy (pure unit tests).** `mesh_checkout.rs` (grant, binding v2, operator hash approval, `mesh_id` from `mesh_nonce`, canonical JSON, domain tags, both verification profiles, seq rules, arch union, conflicts, the persisted floor and clamp), `CheckoutGrantStore`, `MeshCheckoutPolicy` and `grant_checkout`. Tests:
- a run or install without an approval, or with an approval whose sha256 set does not cover the artifact, is refused, even with a valid grant;
- a duplicate approval is idempotent, an approval for another `mesh_id` is refused, and an `ArtifactHash` revocation withdraws the approval;
- a changed `mesh_nonce` chains `licence.binding_orphaned` and turns the checkout policy off;
- without a binding the policy equals `ManifestPolicy`;
- a binding that arrives at runtime takes effect with no restart;
- a grant signed by a node key, signed by an unbound key, or for another `mesh_id` is refused;
- expiry stops serving and leaves a running instance untouched;
- an unbind stops serving at once;
- a lower-seq grant is ignored, and a withdrawal stops serving;
- a newer grant keeps the union of arches;
- a same-seq conflict is refused;
- a clock set back cannot revive a grant, the floor and the store survive a restart, and the far-future growth cap holds (only `reset-floor` undoes a forward jump);
- a `SignerKey` revocation of the grant key ends every checkout grant, while a hash that another unrevoked package grant still lists keeps only that grant's own policy result (L6);
- open membership, `observe` and `off` refuse the binding.

**1b. Floods and sync.** The `mesh.cog.binding` and `mesh.cog.grant` topics (budgets, forward-once) and the sync step. Tests:
- a late joiner reaches the binding and the highest grants through sync;
- a partition heals;
- a grant deferred under `clock_not_set` is accepted later;
- an unbind issued by a non-steward Admin node reaches every node;
- sync limits:
  - an oversized response is cut at the caps;
  - entries under a non-bound key or with a stale `seq` are dropped before any signature verification;
  - the first bad signature aborts the response and bans the peer;
  - a second sync from the same peer within a minute is not answered;
  - a non-`AdmittedPeer` gets no answer;
  - sync flooding does not consume the revocation budget.

**1c. Transport and steward relay.** Includes:
- the `Deliver` `origin` stamp in `clawft-mesh-service` and its use in the daemon's `mesh_local_sink` (ADR-103 amendment);
- the production dialer;
- the daemon-side inbound serve fed by the stamped origin;
- the origin in `session.rs` `serve_frame`;
- the `mesh.cog.checkout` handler;
- the steward client against a stub `weft-licence` that implements the HTTP contract and the signature rules in sections 4 and 7.

Tests:
- in a 3-node `enforce` mesh, A checks out, and B and C install from peers; the stub records exactly 1 checkout and 1 byte transfer;
- **through the real `clawft-mesh-service` and daemon path**, a remote admitted peer is stamped `AdmittedPeer` and served, while a local tenant (`LocalTenant`) and an unadmitted connection are neither served nor accepted for checkout;
- a daemon facing a service that sends no `origin` treats every delivery as `Unadmitted`;
- an unverified peer, a `Legacy` peer and a `Leaf` peer are refused;
- the stub refuses an unsigned `GET /grants`, and forged requests do not consume the steward's budget;
- `origin` is ignored when the negotiated protocol version is below the version that adds it, and when the service peer check failed;
- **gate check:** `scripts/build.sh gate` fails if the daemon's dependency graph enables `clawft-mesh-local`'s `testing` feature.

**1d. Bind and approval RPCs.** Implement the reserved method `workload.node.bind` (Admin), the full steward profile, the fingerprint confirmation, and `weaver cog checkout approve` (Admin). Test the end-to-end bind and refusal paths with a stub Seed identity.

**Phase 2. `weft-licence` on the Seed (armhf).** Depends on C2 (registry access) and C7 (arches).
- `init` over USB, the own uid and service unit, the key, an operator-signed licence record (the ADR-105 fields plus `mesh_id`);
- the checkout, renew and identity endpoints, registry fetch, durable `seq`, the limits and override, the clock floor;
- a listener on USB and the tailnet only;
- a W4 decision on TLS.

`weft-licence`'s own tests cover `seed_bound_elsewhere` (this replaces the old test 9, which only exercised a stub). Acceptance:
- the armhf build size is recorded;
- `weftos-cog-sources` is confirmed to build for `armv7-unknown-linux-gnueabihf`;
- a cog's uid cannot read the key (tested on Linux);
- **owner-run on the real Seed**: check out `fall-detect` for aarch64, a second node runs it from peers, the `weft-licence` log shows one byte transfer, and switching the Seed off for longer than the TTL stops sharing.

**Phase 3. Lifecycle.**
- the start-time grant check in `weft-cog-host` and the workload host;
- `weaver cog checkout list|release|renew|reset-floor`;
- the chain events `cog.checkout.request|granted|renewed|refused|lapsed`;
- `weaver doctor` checks for binding health (including a `licence.binding_orphaned` binding), missing approvals and the expiry horizon.

Placement still uses operator re-signing. Acceptance: after a lapse, a restart is refused.

**Phase 4. Needs Cognitum.** A Cognitum-signed entitlement and a signed registry entry (C9) attached verbatim to grants and verified offline by members, replacing the operator approval. Then grant-backed placement eligibility (replacing operator re-signing inside the mesh), licensed download, a device-key cross-certificate of the grant key, a Cognitum withdrawal feed, and multi-Seed meshes if wanted.

## Phase 1a implementation notes

Choices made while building 1a that the text above leaves open. None change a decision.

- **Envelope.** Every record travels as `{payload, public_key, signature}` (hex). The signed bytes are `domain tag || "\n" || payload`. The payload is the serde struct serialization, and a verifier refuses a payload that does not re-serialize to itself, so each record has one signed spelling.
- **Operator keys.** A binding or approval is accepted only from a `TrustAnchors` key with origin `Operator`. A pinned `Weftos` key is not accepted, unlike revocation notices.
- **Binding fields.** `node_id` from the v1 `BindRecord` is dropped, because `steward_node_id` names the same node. The v2 record is `{v, device_id, device_pubkey, mesh_id, grant_pubkey, steward_node_id, steward_pubkey, state, seq, bound_at}`.
- **Unbind keeps the record.** An `unbound` binding is stored, so its `seq` persists and grants stop at once. A rebind with the same grant key resumes the held grants. A rebind with a new grant key voids them.
- **Grant rules added.** A newer `seq` may not change the hashes of an arch the older grant carried (`ChangesArtifact`). A grant is valid only while `max(now, floor)` is below both `expires_at` and `licence.expires`. Expired and withdrawn grants stay in the store as `seq` tombstones, so an older unexpired grant cannot be replayed to undo a withdrawal.
- **Same-seq conflict.** The store keeps the newest earlier grant per (cog, version). On a conflict it restores that grant and refuses both payloads at that `seq` from then on. If either payload is a withdrawal it fails closed instead: the withdrawal stays as the tombstone and no grant is valid.
- **Saves.** A change is built on a copy, saved, then made the state. An unbind, a withdrawal or a conflict is applied in memory even if the save fails (the error is returned and `tick` retries), so a disk fault cannot keep a lapsed grant alive. Only `tick` writes the high-water mark; reads never write. Events are emitted after the lock is released.
- **Shape bounds.** A grant refuses duplicate sha256 or blake3 across arches, sizes of 0 or over 1 GiB, a non-printable-ASCII `registry`, and times past 2100. A binding may not name a trust-anchor key as `grant_pubkey` or `steward_pubkey`. The floors map in a store file must hold only the bound key. `may_run` takes the BLAKE3 computed from the bytes and requires the grant's (sha256, blake3) pair to equal it.
- **Floor.** The high-water mark never goes down and grows at most 30 days past the newest accepted `issued_at`. Only the Admin `reset_floor` lowers it. The `floor_clamped` event no longer exists.
- **Fail closed.** A store file that is missing is empty. One that cannot be read, parsed or re-verified (every signature is checked again at load) is an error from `open`, or a poisoned store from `open_or_poisoned`, which serves nothing, refuses writes and leaves the file alone.
- **Run gate.** `may_run` checks a valid grant that lists the binary's sha256, then that the grant's BLAKE3 is not revoked as an `ArtifactHash`, then an approval covering the sha256. The revocation is how an approval is withdrawn.
- **Crash window.** An unbind, withdrawal or conflict that could not be saved is applied in memory only. A crash before a later successful save loses it until sync or a pull delivers it again, or until the grant TTL ends.
- **Go-live condition.** The daemon must call `CheckoutGrantStore::tick()` about every 60 s before `mesh_nonce` is ever set, because `tick` is the only writer of the high-water mark and the retry for unsaved restrictive records.
- **`reset_floor` revives by design.** It is Admin-only and chained. Phase 1d shows the floor before and after, and which grants it would bring back, before it runs.
- **Cognitum questions raised by the bounds.** A perpetual licence has no sentinel: `licence.expires` of 0 or `u64::MAX` is refused, so the Seed should clamp to `MAX_UNIX_TIME` (2100-01-01). The 1 GiB artifact cap may be too small for cogs that bundle models.
- **Events.** The stores report `binding_refused`, `binding_conflict`, `binding_orphaned`, `grant_conflict` and `floor_reset` through a `LicenceEventSink`. The daemon chains them as `licence.<name>` (`ChainLicenceSink`, phase 1b).

## Phase 1b implementation notes

Choices made while building 1b that the text above leaves open. None change a decision.

- **Flood discipline.** As `RevocationExchange`: size, then signer (pinned operator key for bindings and approvals, the bound grant key for grants), then the record naming this mesh, then a `seq` or content key the store does not hold. Only then one token from the connection's bucket for that record kind (2 per second, burst 10), then the store verifies. The seen-set is written after a verify succeeds. The operator's own `issue_*` is exempt. Buckets belong to the exchange, so they are separate from the revocation exchange's by construction.
- **Deferral.** `clock_not_set` is the store's `NotYetValid`: the grant is neither applied, forwarded nor remembered, and the next sync brings it again. A grant that arrives before its binding is dropped the same way and recovered by sync.
- **Wire.** `mesh.cog.binding` carries one signed envelope. `mesh.cog.grant` carries `{kind: grant|approval, record}`. `mesh.cog.sync` carries `{op: request, grant_after, approval_after}` or `{op: response, binding, grants, approvals, more_grants, more_approvals}`.
- **Serve sessions.** A fresh sync opens a session for the peer (`opened`, `pages`, last cursors). A continuation request is answered only inside that session: within 120 s of opening, under 16 pages, and with each cursor not behind the last page served. With no session, or a rewound cursor, there is no answer. Grants and approvals are paged from the stores without cloning the whole set.
- **Paging.** Grants are ordered by `(seq, cog, version)` and approvals by content key, each with its own cursor. The binding rides on the first grant page only. A response holds at most 256 entries and 256 KiB: grants get half of each cap, approvals the rest after the binding and grants. The receiver cuts a response at the same caps. A follow-up request is sent only when a cursor advanced, up to 16 pages per session. A sync page is verified and saved on a blocking task, not on the dispatch path.
- **Rates.** A fresh sync (no cursor) is answered once per peer per minute. A continuation page is bounded by the connection's sync bucket instead (8 tokens per second, burst 600), which also pays one token per request and one per entry verified. A response is accepted only for a request this node has open (120 s), and anything else is dropped unread.
- **Bad signature.** Only a signature that fails to verify aborts the response and bans the peer (10 minutes, no requests to it and no answers for it). A malformed or refused entry is skipped.
- **Forwarding.** A flood is sent only to peers the `PeerAdmission` check accepts: a verified route whose admitted class is `node` (the route records the class from admission, also when a later frame re-registers it), so a verified leaf gets no flood and is refused when it asks to sync. A record applied from a sync response is forwarded only if it is a binding or a withdrawal; grants and approvals are not, because neighbours sync for themselves. A restrictive record (unbind, withdrawal) that is in force but could not be saved is `Outcome::AppliedUnsaved`: it is forwarded and flooded like `Applied`, and `tick` retries the save. A conflicting record is remembered so it is not verified or chained again.
- **Unbind.** `sign_unbind` builds the next-`seq` unbound record from the held one, copying `bound_at`, so concurrent unbinds are byte-identical; `issue_binding` accepts and floods it. The Admin check belongs to the RPC (phase 1d), which floods every bind and unbind it accepts through `issue_binding` (`workload_place_rpc::licence_exchange()`), so members learn an unbind at once.
- **Sync trigger.** The exchange syncs with a peer on `MeshPeerEvent::Joined` and every 30 minutes. It cannot see a peer's admission at that point, so it asks every peer and the answering side applies the `PeerAdmission` check (`CtxAdmission`: verified, class `Node`).
- **Store accessors.** Added read-only `held_signed_binding` (named so at integration; `held_binding` is 1d's record accessor), `bound_grant_key`, `held_grant`, `sync_grants` on `CheckoutGrantStore` and `holds`, `sync_approvals` on `ApprovalStore` (`licence/store_sync.rs`), and the `SyncBadSignature` event.
- **Store tick.** `licence_boot` ticks the grant store every 60 s (each tick runs on a blocking task and a panic costs one tick). `tick` records the clock, persists the floor's high-water mark and retries a restrictive save that failed. `ApprovalStore` has no tick: it keeps no clock state and saves before it commits.
- **Daemon admission posture.** `licence_boot::posture`, as for the bind RPC: `enforce` and `open_membership` come from `kernel.mesh`, `verdict_source_bound` from whether the governance gate is present.

## Phase 1c implementation notes

Choices made while building 1c that the text above leaves open. None change a decision.

- **Origin version.** The stamp is mesh-local protocol 2 (`PROTO_ORIGIN`); the field is optional on the wire. `PeerClass::Legacy` maps to class `other`, which is never verified in the daemon. The daemon records the negotiated version per link, so a service that comes back older stops being believed.
- **Tunnel.** The daemon has no mesh connections, so the piece protocol rides `mesh.artifact.tunnel` messages through the service: `{v, sid, dir: req|rsp, part, last, close, data (hex)}`. A frame is split at 192 KiB because a mesh-local line is capped at 1 MiB. The first `req` frame of an unknown session starts `serve_as` with a verified `ServePeer`. Replies are accepted only from the peer that was dialled. Limits: 8 serve sessions per peer, 64 in all, 64 dialled. Frames from anything but a verified node are dropped and counted, with no answer.
- **Checkout wire.** `mesh.cog.checkout` carries `{request_id, cog_id, version, arch}`; the steward answers on `mesh.cog.checkout.reply` with the grant or a stable code. The requester installs the grant itself (`install_grant`), so it can fetch from peers before any flood. A member that cannot reach the steward gets `licence_unreachable`.
- **Relay order.** Admission (kernel or verified node), request validation, the gate (`cog.checkout`, Permit only), the merge of identical in-flight requests, then: `weft-licence` checkout, grant verified under the bound key and `mesh_id` before any byte is fetched, bytes fetched only when the exchange does not hold the BLAKE3 yet and checked for size, sha256 and BLAKE3, bytes seeded, grant registered (`seed_clock_skew` when the Seed is more than 5 minutes ahead), `grant_checkout`, chain `cog.checkout.granted` (or `cog.checkout.refused`), flood. A request is not answered from the local grant store; `weft-licence` returns the existing grant itself.
- **Request signature.** One definition in `weft-licence-wire::request` (`signing_string`, the domain, the 2100 bound, the 16 to 64 alphanumeric nonce rule, the 120 s window, the 1_780_000_000 s clock floor), called by both `weft-licence` and the kernel client: `weft-licence-v1/request`, method, target (path with query), node, `seed_device_id` (the audience, the binding's `device_id`), timestamp in unix milliseconds, nonce, body sha256, one per line. The kernel tests verify what the client signs under `weft-licence`'s own verifier and the reverse. The client signs nothing while its clock is below the floor (`clock_not_set`).
- **Flood.** `GrantFlood` is the one call 1b fills: the daemon's `LicenceExchange` implements it, and `cog_swarm::grant_flood()` hands it to a relay (`NoFlood` until the exchange has started). No production code calls it until the steward relay lands (phase 3).
- **Reserved topics (1c review).** `AdmittedPeer` vouches for a machine, not a tenant, so the service reserves `mesh.cog.`, `mesh.artifact.` and `mesh.licence.` for the cluster owner's registration, or the service's own uid when no owner is set, and for nobody otherwise ("the only registration" is never trusted; the service logs and `weaver doctor` reports `cluster_owner_uid required for the licence/artifact mesh`): other tenants cannot send them, cannot claim an overlapping prefix, and inbound deliveries on them go only to that registration whatever scope or prefix is named. No daemon registration is needed to receive them.
- **Relay limits.** 5 requests per member per minute and 8 checkouts in flight, both before the gate; the steward's own kernel is exempt from the per-member budget. Over-limit refusals (`rate_limited`, `busy`) are not chained. Every merged caller is chained as a grantee (`merged: true` for those that did not run the call). Bytes already held are checked against the grant's registry sha256 before the grant is installed.
- **Tunnel.** A session whose queue is full is dropped and counted (`dropped_slow`); the delivery worker never waits on a session. A verified leaf is classed `leaf` but `node_verified` is false in the daemon; `licensed_peer` (verified and class node) gates every licensed serve, checkout and flood.

## Phase 2 implementation notes

Choices made while building `weft-licence` that the text above leaves open. None change a decision. Operator doc: `docs/cogs/weft-licence.md`.

- **Shared wire crate.** The kernel is too heavy for armhf, so the pure types moved to `crates/weft-licence-wire` (grant, binding record, signed envelope, `MeshId`, `LicenceError`, sign and verify helpers). The kernel re-exports them unchanged. The Seed service and the member stores use one grant format.
- **Bind is a USB command, not an endpoint.** The steward key is only known once the binding exists, so a bind could not carry a steward signature, and N5 says every endpoint except identity does. `weft-licence bind <file>` applies the operator-signed record (pinned operator key, the Seed's `device_id`, the Seed's own grant key, `seq` above the stored one). The device public key is not checked yet (C4).
- **Bytes are a second signed call.** `POST /licence/v1/checkout` returns the grant and the artifact paths. `GET /licence/v1/artifact/<blake3>` sends the bytes. This is what "bytes are sent only on request" and the 3-per-24-h limit count. The in-flight permit covers both.
- **Renewal.** `POST /licence/v1/renew` re-checks the licence for every active checkout and signs a new grant (`seq + 1`). It also takes `{"release": [...]}`. A released checkout and one the licence no longer covers get a withdrawal (`expires_at <= issued_at`). `GET /licence/v1/grants?since=<ctr>` is read only and lists the latest signed grant per checkout, ordered by a global issue counter, 256 per page.
- **Durable order.** The grant is signed in memory, the slot table (with the new `seq`, the arch union and the grant) is written, fsynced and renamed, and only then is the grant released. A failed write drops the grant and leaves the in-memory table unchanged.
- **Replay memory is persisted.** The nonce list is written on each verified request, so a restart does not reopen the replay window.
- **No expiry on the licence** is written into the grant as `issued_at + 7 days`, so the verifier's `licence.expires` comparison stays finite. A grant never outlives a declared licence expiry.
- **Bind and unbind reach a running service.** The service re-reads `binding.json` when its mtime or length changes. Unbound, missing or corrupt means fail closed: the grant key leaves memory and every checkout is marked released. Each checkout records its `mesh_id` and is live only for the currently bound mesh, so nothing carries over to a binding for another mesh.
- **CLI ownership.** `init`, `bind` and `override` refuse unless the process owns the state directory, so a root-written file never locks the service user out. Use `sudo -u weft-licence weft-licence ...`.
- **Listener limits.** Listen addresses must be loopback, link-local, tailnet CGNAT or tailnet ULA unless `allow_lan_listen` is set. Each request has a 10 s total read deadline, one source address holds at most 2 connections, and a `Host` header must name a listen address (any IP spelling) or a name in `allowed_hosts`. The binding is re-read at the signing lock and by renew and artifact, so an unbind or rebind during a fetch signs nothing.
- **Registry coverage.** The Cognitum registry lists one `arm` binary per cog (C7), and `aarch64` answers `arch_unavailable` until it does.
- **Clock floor and request layout.** `CLOCK_FLOOR` is 1,780,000,000 s (2026-05-28), equal to the COG-011 bridge's `CLOCK_FLOOR_MS`. The signed request uses the bridge's layout (domain, method, path, node, timestamp in unix milliseconds, nonce of 16 to 64 alphanumerics, body sha256, one per line) under the domain `weft-licence-v1/request`. Unlike the bridge, replay memory is persisted, so no process-start rule is needed. The string also carries `seed_device_id` as an audience line after the node, so a request signed for one Seed is refused by another.
## Phase 1d implementation notes

Choices made while building 1d that the text above leaves open. None change a decision.

- **The operator key stays with the operator.** The daemon holds only pinned operator public keys. `weaver workload node bind <seed> --operator-key <file> --grant-pubkey <hex> --grant-fingerprint <ed25519:..>` asks the daemon for the Seed's live identity and the mesh facts (`workload.node.bind` with `prepare`), checks that the typed fingerprint is the fingerprint of the key it was given (the operator's second comparison, after the one at `weft-licence init`), signs the v2 record, and sends it back. The daemon verifies it; it never signs a binding.
- **Verb naming.** `weaver workload node bind|unbind|status`, following the RPC names (`workload.node.*`), beside `weaver workload place`. Not under `weaver cog`.
- **Steward profile order.** Admission posture, pinned link, member checks (operator signature, canonical payload, `mesh_id`), the steward checks that need no Seed (this node is the named steward, fingerprint, 600 s age window, `seq` above the held one), the live `GET /api/v1/identity` match, the replay memory (persisted before anything is accepted), then the store. Refusals are chained as `workload.refuse` with a stable code; a bind as `workload.node.bind`, an unbind as `workload.node.unbind`.
- **`seed_bound_elsewhere` on the steward** is derived from the store, not from a copy: a bind is refused when the held binding has the same `device_id`, state `bound` and a different `mesh_id`. After a `mesh_nonce` change this means: `weaver workload node unbind`, then `weaver workload node bind`. The "a new binding for the new id fixes it" rule in section 3 holds for the member store; the Seed refuses a second mesh id as well (phase 2). A different Seed bound in between does not lock the first one out (a map kept in the binder could drift from the store and did; it was removed).
- **Unbind** needs no Seed (it may be gone), refuses a different device than the one held, a signer that is not a pinned operator key and a `seq` that is not higher, and is accepted under any admission posture (turning the licence off must always work). If the store cannot save it, it is still applied in memory (it only restricts), chained with `save_pending: true`, and the RPC returns success with a warning; the store tick retries the save.
- **The daemon-side fingerprint check guards only against client bugs.** The daemon compares the fingerprint the client sent with the key in the record, but both come from the same caller. The real confirmation is the CLI comparing the value the operator typed (from `weft-licence init`) with the key it was given, before anything is signed.
- **Ownership.** `licence_boot` creates the node's `CheckoutGrantStore` and `MeshCheckoutPolicy` and owns the 60 s `store.tick()` (a `spawn_blocking` with a panic guard). Other code takes them from `licence_boot::store()` / `licence_boot::policy()` (or `LicenceRuntime::store()` / `policy()`); `workload_place_rpc::build` and the licence exchange do.
- **reset-floor.** `workload.node.reset-floor` (Admin) previews the floor now, the floor after, and the grants a reset would revive (`CheckoutGrantStore::floor_preview`); without `confirm` it changes nothing. With `confirm` it chains `licence.floor_reset_requested` (with the preview), then the store chains `licence.floor_reset`. CLI: `weaver workload node reset-floor [--confirm <floor>]` (and the alias `weaver cog checkout reset-floor`): the confirm names the floor the dry run printed and the RPC refuses a confirm without it, or with a floor that has moved (`floor_changed`).
- **Status carries no error text.** `workload.node.binding` (Read) returns `poisoned` as a boolean (the reason, which can hold a path, is in the daemon log). A malformed nonce with a binding held chains `licence.mesh_config_error` at boot.
- **No governance gate on top of Admin.** The operator signature is the approval, and `workload.node.bind` is Admin in the capability table; the node's `WorkloadGate` default-denies `workload.*` mutations without a permit, which would make a bind depend on a second approval that carries no information.
- **Doctor.** `workload.node.binding` (Read) returns the mesh id, whether the pin and nonce are set, the held binding and whether it is orphaned. `weaver doctor` turns that into `licence.mesh_nonce` (Ok while the path is inert), `licence.mesh_id_unset` (Fail: a binding with no mesh id), `licence.binding_orphaned` (Fail) and `licence.binding`.
- **Deferred.** The binder's replay memory is spent when the store then refuses (only a disk error can do that after the `seq` pre-check); the operator signs a newer record.

## Phase 3 implementation notes

Choices made while building phase 3 part B that the text above leaves open. None change a decision.

- **Link file.** The steward's link to `weft-licence` is `licence-link.json` in the runtime dir (`url`, a pin or `allow_unpinned_lab_link`, optional `max_artifact_bytes` and `timeout_secs`), read with the placement policy file checks. Without it a node runs no relay and answers `no_steward`. `weft-licence` serves plain HTTP (W4), so a real link needs the opt-in until it serves TLS; an `https://` link with a pin is supported for then.
- **Transport bounds.** Connect 5 s, call 30 s, artifact 120 s; bodies read chunk by chunk under 256 KiB, or the artifact cap (64 MiB default) on the artifact path; only `/licence/v1/` paths with safe characters, `GET` and `POST`; no redirects, no proxy, no idle pooling. Every failure is a transport error (`licence_unreachable` to the member).
- **Steward client per call.** The client reads the binding in effect on every call and signs for its `device_id` as its `steward_node_id`. It sends nothing when no binding is in effect (`seed_not_bound`) or when the binding names another node or key (`not_steward`), so a rebind or a new steward needs no restart. The relay is therefore installed on any node with a link file; it relays only while the binding names that node.
- **Relay gate.** The relay asks the kernel's governance gate for `cog.checkout`. A kernel without one gets no relay (logged, and `weaver doctor` reports `licence.no_steward` with the reason).
- **Where the run gate applies.** In the `workload-host`, for packages whose signed manifest gives `GrantOrigin::Cognitum`, before load and again before every `start`. The hashes are computed from the bytes of the binary for the placed variant's arch. A node that never held a binding returns `NotSeedBound` and keeps the ADR-105 path (operator-signed package); a held binding in any state, or an unreadable store, means the gate applies and fails closed. A node with no approval store (no mesh) refuses for `no_approval`.
- **Refusal codes.** `binding_inactive`, `no_grant`, `grant_lapsed` (a grant is held but not valid: expired, withdrawn, key revoked), `not_in_grant` (a valid grant lists no binary with these hashes), `hash_revoked`, `no_approval`. The host refuses with the existing `governance` refusal code (no wire change) and a reason `licence run gate: [<code>] ...; remedy: ...`; it is chained as `workload.refuse` and is the attempt reason `weaver workload place --explain` prints. A permit is chained as `cog.run.permit` with the grant id and the approval id (the provenance fields of section 10).
- **Approval flow.** As the bind: `workload.cog.checkout.approve {prepare}` names the mesh id and the held grant's sha256 set (or the operator's `--sha256` list); the CLI prints it and signs only with `--confirm`; `{signed: [..]}` goes through `LicenceExchange::issue_approval` (verify, store, flood) and is chained as `cog.checkout.approved`. `--reapprove-orphaned` re-signs every orphaned approval for the current mesh id in one batch; the old ones stay, inactive.
- **Checkout flow.** `workload.cog.checkout` chains `cog.checkout.request`; on the steward it calls its own relay (which chains `granted` / `refused`), on a member it asks the binding's steward over `mesh.cog.checkout` and chains `cog.checkout.received` or `cog.checkout.refused` (`side: requester`). The default arch is this node's.
- **Doctor.** From `workload.cog.checkout.status` (Read, no side effects): `licence.approval_missing` (a valid grant whose artifact the run gate refuses for `no_approval`), `licence.grant_expiring` (a valid grant with less than 24 h left: at least one 12-hour renewal missed), `licence.approvals_orphaned`, `licence.no_steward` (the steward is not a licensed peer, or this node is the steward and runs no relay once placement started).
- **Review fixes (after `dac65ed1f`).**
  - *Arch.* The arch that runs is the runtime's admission arch (`Admission.arch`), not the controller's variant; a mismatch is `arch_mismatch`. Every binary in a Cognitum package must pass the gate before anything is staged, and `LicensedRun.arch` is the admitted arch.
  - *Local checkouts.* `workload.cog.checkout` is Admin and an extension route, so the handler knows the caller: the relay gets `CheckoutCaller::Local(principal)` (`project:<id>`, `token:<12 hex of its sha256>`, or `operator`), charges it 5 a minute per principal and 20 a minute node-wide, and asks the gate as `local:<principal>`. The kernel's own requests stay exempt.
  - *Sticky bound signal.* The first accepted binding writes `licence-bound.marker` beside the grant store; the gate also applies when the marker exists or approvals are held, so a deleted or replaced store file fails closed (`binding_inactive`) instead of reading as never bound. Not done: holding Cognitum starts after boot until the first binding sync. Residual risk, accepted: deleting the whole `licence/` directory (store, approvals and marker together) makes the node read as never bound, so its Cognitum cogs fall back to the ADR-105 path until a peer re-syncs the binding to it. Whoever can delete that directory can already tamper with the daemon and its runtime dir, so the marker stays beside the store rather than somewhere a daemon-uid attacker could not reach anyway.
  - *Approve.* The dry run prints the approval's content key and `--confirm <content_key>` refuses when the freshly prepared content hashes differently. `--reapprove-orphaned` gets signed envelopes, the CLI verifies each against the operator's own key, keeps those for an earlier mesh id, and needs `--confirm <batch digest>`.
  - *Start.* The binary is rehashed from the staged file (`WorkloadRuntime::staged_payload`, native) at every start; a container image is tagged by the binary's BLAKE3 and keeps the loaded bytes.
  - *Re-packs.* A package is also treated as Cognitum when a binary's sha256 or BLAKE3 matches a held grant's artifact or a revoked hash (`CognitumRunGate::claims`); it is then checked under its own id and version.
  - *Follow-up.* Only the admitted binary needs a grant and an approval (it is the only one staged); the package's other arches refuse it only when their hash is revoked. The catch-up cursor may not move past the Seed's counter from the renew answer plus the grants a page carries (a page that would is refused and the cursor stays), and it restarts at 0 when the binding `seq` or the Seed `device_id` changes. While passes are skipped (no binding naming this node) the renewer polls every 5 minutes, so a binding that arrives later is renewed within that time.
  - *Revocation teardown.* An `ArtifactHash` revocation of a licensed cog's binary is torn down by the existing `enforce_revocations` sweep (the binary's BLAKE3 is a revocation ref); a test covers it.
- **Renewal pull.** `licence/renewal.rs` (`Renewer`), started by the daemon next to the relay with the same client: `POST /licence/v1/renew` every 12 h plus up to 30 min of jitter (first pass 5 min after start), then `GET /licence/v1/grants?since=<ctr>` paged from an in-memory cursor (up to 16 pages; a restart re-reads from 0, and what is already held is neither re-applied nor re-flooded). Each grant goes through `install_grant` (store accept, `grant_checkout`) and the licence exchange's flood (the same as `issue_grant` after the local accept); withdrawals in a response are applied and flooded before anything else. Chained: `cog.checkout.renewed`, `cog.checkout.lapsed`. A pass where the binding does not name this node is skipped without sending; a failed call backs off 1 min doubling to 1 h. Bounded by the transport caps: a renew answer over 256 KiB (about 200 active checkouts) fails the pass. W3 is open, so every active checkout is renewed.
- **Operator verbs (`wt/lic-verbs`).** `workload.cog.checkout.release` (Admin), `.renew` (Admin) and `.list` (Read), CLI `weaver cog checkout release | renew | list | reset-floor` (the last is `workload.node.reset-floor`). Release uses the renewal endpoint's existing `{"release": [{cog_id, version}]}` body (section 4, "Renewal"); no new `weft-licence` endpoint or field. Release and renew go through the steward's own link (the renewer's holder-gated client, `not_holder` / `not_steward` with nothing sent), so they run on the steward; a member is told which node that is. Neither the ADR nor the mesh protocol defines a member-to-steward release or renew message, so none was added. The Seed has no per-checkout renewal either: `renew <cog>@<version>` makes one renewal pass (every active checkout of the mesh is renewed) and reports that checkout's grant before and after. Everything returned is installed and flooded (withdrawals first). Chained: `cog.checkout.release` and `cog.checkout.renew` with their outcome, plus the renewer's `cog.checkout.renewed` / `cog.checkout.lapsed`. Scope gate: release, renew and `workload.node.reset-floor` are user-level (Admin) beside checkout; list is read-only (a reviewed verb under the `workload.` arm). Review follow-up: release and renew are paced to one per 60 s node-wide (`[rate_limited]` with the wait; a refused call is not chained and sends nothing), they are exact-name Admin extension routes so their chain events carry the caller's principal, and the CLI says that a release also renews every other checkout.
- **`weft-cog-host` start check.** Built; see "Phase 3 `weft-cog-host` start check" below. Still to do: the run on the real Seed.

## Phase 3 `weft-cog-host` start check

`weft-cog-host` asks the kernel run gate (`check_run`, through `CognitumRunGate`) before every spawn of a Cognitum-origin cog: the first start, every restart after an exit, and every start after the host process restarts. Code: `crates/weftos-cog-host/src/licence.rs` (state, import, gate) and `supervise.rs` (the spawn path).

- **Which cogs.** A cog is Cognitum-origin when its record says `source: cognitum`, or when its binary is known Cognitum bytes (a held grant lists the hash, or the hash is revoked). Relabelling a checked-out binary as `local` does not escape the gate. Other cogs never reach it.
- **Hashes.** The sha256 and BLAKE3 are computed from the binary file about to run, never taken from the record or the install request.
- **Run what was hashed.** A licensed start executes exactly the hashed bytes, so replacing the binary between the hash and the exec changes nothing. On Linux that is a sealed in-memory `memfd` run through `/proc/self/fd` (no file at all); for a `#!` script, or if the kernel refuses, it is a content-addressed `0700` file `<root>/.run/<blake3>` (written once, reused while it still hashes right, cleared at host start). Other platforms use the file. This closes the swap by another user or by a later write; it does not stop a process with the host's own uid (see "Limits"). Cog binaries over 32 MiB are not read or started, and the hashes of a refused cog are cached by file size and mtime so the file is not re-read at each backoff.
- **State.** The licence directory is `$WEFT_COG_HOST_LICENCE_DIR`, else `<root>/.licence` (a cog id cannot start with a dot, so no collision). It holds `config.json` (`{"mesh_id": "<64 hex>"}`), `trust.json` (the `weftos.workload-trust.v1` operator trust file) and the kernel stores the import writes. Setup is in [weft-licence.md](../cogs/weft-licence.md), "Running cogs on the Seed".
- **Three states.** No directory at all: the gate does not apply (`NotSeedBound`), exactly as a kernel node that never held a binding. Directory configured (`config.json` and `trust.json` readable) but no binding imported yet: fail closed, every Cognitum-origin start is refused `binding_inactive` (a configured directory means the operator intends to bind, and byte-claiming cannot work before any grant exists). Directory present but unusable (no or bad config or trust file, a poisoned store, an unreadable revocation list): fail closed, every Cognitum-origin start is refused `binding_inactive`. Otherwise the kernel verdict decides, with the refusal codes of the run gate (`binding_inactive`, `no_grant`, `grant_lapsed`, `not_in_grant`, `hash_revoked`, `no_approval`).
- **Records in.** Only signed records are accepted, each verified by the kernel code a mesh node uses: `weft-cog-host licence import <records.json>` or `POST /licence/records` (bearer token like every host POST, 2 MB cap, at most 512 records of each kind). `GET /licence` and `weft-cog-host licence status` show the state. The host reloads on a file change within a tick, so an import by the CLI reaches a running host.
- **Stop rules.** A lapse refuses the next start and leaves a running cog alone (soft stop). An `ArtifactHash` revocation stops a running cog on the next supervisor tick and refuses its restart (`hash_revoked`).
- **Status.** `GET /cogs` carries `licence_refusal` (the last refusing code) and `licence_grant` (the grant that covers the running instance) per cog. A refusal is a failed spawn with the usual backoff; a permit is logged with the grant and approval ids.
- **Limits.**
  - *Same uid.* The check is a gate for the supervised start path, not a sandbox. Anyone running as the host's uid can delete `<root>/.licence`, edit its records, edit the cog records, or run a cog binary directly, and so disable the gate. A supported layout that raises the bar: make the licence directory root-owned and read-only to the host user (`chown -R root:root`, `chmod 755` on the directory, `644` on its files), and import records as root (`weft-cog-host licence import` writes the stores, so it must run as root; the host picks the change up on its next tick). The host then cannot save an import of its own (the kernel stores answer `applied_unsaved`: held in memory until the host restarts, not on disk), so records that must last go in as root. This layout is not covered by a test yet.
  - *Unclaimed starts exec by path.* A cog that is not claimed as Cognitum runs from its own path, as before. This is the same capability class as the first limit.
  - *Claiming needs a grant.* `claims()` sees Cognitum bytes only when a held grant lists the hash or the hash is revoked. While the state is `Broken`, or before any grant for those bytes exists, a Cognitum binary labelled `source: local` is not claimed and runs (a configured directory with no binding refuses records that say `cognitum`, but cannot recognise relabelled bytes). Labelling is therefore honest only for cogs installed through the Cognitum source; the fail-closed `Broken` state protects records that already say `cognitum`.
  - *Orphans after a host crash.* The cogs are the host's children but not in their own process group or sandbox; if the host is killed or crashes, running cogs keep running (the unit's `Restart=always` starts a new host, which spawns them again, so an orphan and its replacement can run together until the orphan exits). Not fixed here.
  - *Mutating routes.* `GET /licence` and `POST /licence/records` need the host bearer token; the token is a shared secret on the LAN (see the `weft-cog-host` entry in the context section).
- **Effect on an unbound host.** None until the licence directory exists. From then on, every `source: cognitum` cog on that host is refused until the binding is imported, and after that needs a grant and an approval to start, including ones installed before the bind. Create the directory last, when the binding and records are ready to import.
- **Keys.** The binding's grant key must differ from every pinned operator key: the kernel refuses that pairing (`binding key is a trust anchor`). `trust.json` pins the operator key(s); the grants are signed by a separate grant key. The test generator `crates/weftos-cog-host/examples/mint_test_licence.rs` therefore takes `--key` (operator) and `--grant-key` (grant).

## Phase 3 part A implementation notes (service mode)

### Decision: the service-mode signer is a daemon-local control key

In service mode the box key belongs to the machine mesh service (ADR-103 D7, P3-U) and the user daemon holds only its public half. Placement and the steward still have to sign: `workload.ctl` requests and the in-process workload-host's replies, the steward's signed `weft-licence` requests (section 4 step 3), and, where an operator pinned the key as a revoking key, revocation notices. Operator-signed records (bindings, approvals) are signed by the operator's own key in `weaver`, never by the daemon, so they need nothing here.

**Decided: a daemon-local placement control key**, `<runtime>/control.key` (0600, created atomically on first use by the same code as `node.key`, `load_or_generate_key_file`). `placement_boot::signer` picks it only when the identity is the service's; a collapsed daemon keeps signing with its node key. The control key:

- signs `workload.ctl` and workload-host replies, the steward's `weft-licence` requests, and revocation notices only if an operator listed it as an operator or WeftOS key;
- never signs as the machine: not the admission hello, not the machine's facts, not the chain, not a user certificate;
- is trusted only where an operator names it: `workload-host.json` `controllers` on a target, `workload-peers.json`, and the binding's `steward_pubkey`, which the operator signs after seeing it in `weaver workload node status` (the `steward.pubkey` field);
- is not the mesh identity. The binding's `steward_node_id` and the swarm's node id stay the machine's node id, which is what peers address; `steward_pubkey` is the control key. The binding record already carries the two separately, and `StewardCheck` compares both.

**Controller id.** In service mode the placement controller's id (`workload.status` `controller`, what targets see on signed requests and chain as the controller) is the control key's id, not the machine's node id. The cog ingest bridge's default route names the same id as its controller, on purpose: instances are attributed to the controller that placed them, which is the control key. The swarm and the binding use the machine's node id.

**Rotation and compromise.** The control key is replaced by stopping the daemon, moving `control.key` aside and starting it again (a new key is generated). Everything that pinned the old key must then be updated by the operator: the `controllers` of each target's `workload-host.json`, any `workload-peers.json` entry, and the Seed binding, by a rebind with the new `steward_pubkey`. Until the rebind the Seed refuses every steward request (it accepts only the bound key). At boot the daemon compares the held binding's `steward_pubkey` with its signer when the binding names this node as steward, and on a difference logs an error and chains `licence.steward_key_mismatch` (held key, signer key, `seq`). A **compromised** control key can place on the targets that list it (subject to each target's own permits and trust) and can sign steward requests to the bound Seed: that is checkouts of covered cogs, which the operator approval gate still stops from running anywhere (section 4 step 7). It cannot sign as the machine, mint grants, or bind. Response: remove it from every `controllers` list, rebind the Seed with a new steward key (or unbind from any operator-Admin node), and revoke the key with a `SignerKey` notice if it was pinned as a revoking key.

Rejected: **a scoped signing RPC on the mesh service.** The service exposes no signing call today (its local protocol offers hello, register, send and deliver, verdicts, `peers.list`, `facts.get` and admin verbs). Adding one would put caller-chosen bytes under the box key, which ADR-103 A7's signing rules (chain-authority follow-up (e)) warn against for any signer of caller-chosen bytes, and every domain the daemon needs would have to be listed and versioned in the service. It would put the service in the path of every placement call and every steward request, and require turning the kernel's `SigningKey`-holding types (`PlacementControlPlane`, `WorkloadHostService`, `SignedLicenceClient`, the ingest bridge) into async signer traits. And it would not narrow anything: whoever can drive the owner's daemon could ask the service to sign. A separate key the operator pins only where it is meant to be used is the smaller grant.

### Licence records through the service

- **Service.** `licence_forward` installs a control sink on the service runtime for each of `mesh.cog.binding`, `mesh.cog.grant` and `mesh.cog.sync`. A record from a licensed peer (admission verified, class `node`) is handed to the reserved-topic holder's registration through `TenantRouter::deliver_reserved`, stamped `AdmittedPeer` like any delivery, in arrival order. Anything else is dropped and counted (`licence_unlicensed`), so an unadmitted connection or a leaf cannot make the daemon spend work. The 1c reserved-topic rules are unchanged: only the owner's registration receives these topics and only it may send them. `peers.list` gains `licensed`, the connected peers whose route is verified with class `node` (ADR-103 A16).
- **Daemon.** `LicenceExchange` now runs over a `LicenceLinks` trait (control sinks, peer list, licensed predicate, send, join events). `MeshRuntime` implements it (collapsed mode, unchanged behaviour). `ServiceLicenceLinks` implements it in service mode: the cog mesh router (`CogMeshDelivery::with_licence`) hands it the three topics, which it gives to the exchange's sinks with the stamped `PeerCtx` (refusing anything but a licensed peer); replies and floods leave through the service link; the peer view is `peers.list`, refreshed every 10 s. A peer that becomes licensed, in the view or by a first stamped delivery, raises `Joined`, so the exchange syncs with it at once. Token buckets key on a hash of the peer id, since the daemon sees no connection and only a verified id reaches a sink.
- **One path.** The exchange is started over the kernel's own runtime when the kernel has one, else over the service links (`workload_place_rpc::licence_links`), never both. The licence topics are never passed to the A2A router; without an exchange over the links they are consumed and counted as `unhandled`.
- **Only the owner's daemon is the licence node.** `AdmittedPeer` vouches for the machine, and the service routes licence records only to the reserved-topic holder. `peers.list` says whether the asking registration is that holder (`reserved_holder`). In service mode the answer is re-read with every peer-view refresh of the service links (at boot, every 10 s, after a reconnect), so it follows a link that comes up late and a `cluster_owner_uid` change. The licence runtime (the grant store and checkout policy, local state) is installed at boot in every mode, so placement, built at its first call, always holds the node's real store even while the role is unknown. Holder: the exchange starts (once; every entry into the role resyncs with every peer), and the binder, relay and renewer act (they check the role on every call). Not the holder: another tenant's daemon places normally, but the licence RPCs, `workload.node.bind` included, refuse with that reason, and `weaver workload node status` shows `reserved  NOT the holder`; a daemon that lost the role idles (its runtime stays, the RPCs refuse, and the service neither routes it licence records nor lets it send them). Unknown (the service could not be asked): the same, with its own reason, "holder status unknown: the mesh service query failed".
- **One licence for the whole mesh, local tenants included.** In a daemon that is not the holder, on a machine of a Seed-licensed mesh (its config gives a mesh id), the run gate refuses every Cognitum-origin or Cognitum-claimed cog (`not_holder`: run it from the cluster owner's daemon) and never falls back to the ADR-105 path. While the role is unknown the same applies to a daemon whose own store was never bound, so a holder's link blip does not stop its restarts. The tenant cannot see the holder's binding, so "Seed-licensed mesh" is read from its own mesh id; a tenant configured without a `mesh_nonce` keeps the ADR-105 path. Consequence: a holder with `mesh_nonce` set that has never received a binding also has its Cognitum cogs refused `not_holder` while the role is unknown (its store was never bound); this is transient and fail-closed, and ends with the next good holder answer.
- **The licence verbs are machine-level (board card e5e20dfc).** `workload.cog.checkout{,.approve,.status,.release,.renew,.list}` and `workload.node.{bind,unbind,binding,reset-floor}` are served only by the machine's licence holder: in service mode the daemon that holds the mesh service's reserved licence topics (the cluster owner's), else the collapsed daemon. `licence_role_gate` (before the scope gate) refuses them on every other daemon with `licence_not_here`, naming the holder (the uid the service reports in `peers.list` as `reserved_holder_uid`, and its user name) and the command to run there (`sudo -u <owner> weaver ...`). `workload.node.binding` still answers there, with the same redirect, for `weaver workload node status` and `weaver doctor`.
  - *Why refuse rather than forward.* Forwarding to the owner's daemon would need a cross-user endpoint (the owner's socket is owner-only by ADR-103 D3, and the TCP relay strips literal scopes) and an authority model in which one user's Admin counts on another user's daemon: tokens issued by the owner's daemon to other users, accepted over a new path. That is a new trust path with no operator-pinned key behind it, which this ADR does not add. Running the verb as the owner (sudo) uses the existing model: the owner's daemon, its own socket, its own Admin.
  - *Scope gate.* The licence verbs left `USER_LEVEL_ALLOW` and `READ_ONLY_ALLOW` (they were never per-user or per-project; they were listed there so a user daemon would serve them outside a project). They are classified as machine-level instead: outside a project, under `read_only`, they pass as the capability table says (the three status verbs to anyone, the rest to Admin), never under `deny_all`, and only after the role gate has admitted them. The population test's `REVIEWED_UNDER_PREFIX` exception (status verbs allow-listed although served under the `workload.` prefix arm) is gone; a test pins the role gate's verb list to the licence modules' own lists, and every ext route must still be classified (read-only, user-level or machine-level).
- **No head-of-line blocking.** `ServiceLicenceLinks::deliver` runs on the link's single delivery worker, so reply sends (sync pages) go out on their own tasks, at most 64 in flight and 4 per peer. A sync request takes its reply slot before the exchange sees it; without one it is dropped and counted, and not recorded as served, so it does not cost the peer the one-per-minute answer.
- **Re-asking.** A sync request unanswered for 120 s (`PENDING_TTL`) is sent again, fresh, at most 3 times in a row (reset by an answer), each paid from the sync bucket; after that the node waits for the next periodic or on-connect sync.
- **Outbound at the service too.** The service sends `mesh.cog.binding|grant|sync` to a remote node only if that node's route is licensed, refusing and counting others (`licence_out_refused`), whatever the daemon asked. The daemon clears its licensed view when a refresh fails (link down), so it floods nobody until the next good view.
- **Started at boot.** `placement_boot::start` installs placement, the licence runtime and the licence exchange at daemon boot in both modes, so a member that never places still takes and passes on bindings and grants. (Before, the exchange started at the first placement build.)
- **A service older than this** sends no `licensed` in `peers.list` and has no forwarding sinks: the daemon floods to nobody and receives no licence records, as before.

Known limits and deferrals:

- `cog_swarm::install` (the artifact tunnel and the checkout router) still runs at the first placement build, in both modes, as before.
- In service mode the in-process workload-host is a placement candidate only once it has signed local facts under the control key; the daemon does not probe them (the service advertises the machine's facts), so service-mode placement targets remote hosts and Seeds. Local candidacy is follow-up work.
- `weaver` shows the control key as the steward key (`weaver workload node status`); there is no dedicated verb that prints it for `workload-host.json` yet.
- The two-service end-to-end test joins the services in process the way the kernel listener does after admission (a verified route each way); the TCP, Noise and admission legs are covered by the existing mesh tests, not repeated here.

Tests (round 3): `tests/licence_holder_follow.rs` (boot before the link is bound, the owner moving away and back, a service restart), `tests/licence_holder_reconnect.rs` (the service drops before boot; the licence path comes up on reconnect), and in `tests_service_links.rs` a dropped request not recorded as served, the per-peer cap and the bounded retry.

Tests (fix round): a non-owner tenant daemon refuses bind and starts no exchange (`tests/placement_service_tenant.rs`); a slow peer does not stall other deliveries and a failed refresh clears the licensed view (`tests_service_links.rs`); a licence send to a leaf is refused at the service (`licence_forward.rs`, and the e2e); `tests/placement_service_mode.rs` now runs `placement_boot::start` over a live service link with the daemon's own `cog_swarm` wiring (holder check, eager exchange, peer view refresh).

Tests: `crates/clawft-kernel/src/licence/tests_service_links.rs`, `crates/clawft-mesh-service/src/licence_forward.rs`, `crates/clawft-weave/src/placement_boot.rs` (unit), `crates/clawft-weave/tests/mesh_licence_service_e2e.rs` (real services and daemon links: a binding, a grant and an approval reach a second service-mode node; a late joiner catches up by sync; a leaf, an unadmitted peer and a non-owner tenant get nothing), `crates/clawft-weave/tests/placement_service_mode.rs` (placement through `dispatch` in a service-mode kernel, signed by the control key; a board that trusts only the machine key is not placed on). Collapsed mode: the existing exchange, sync and daemon wiring tests.

## Open questions

**For Cognitum (C).** Anything not verifiable locally is marked as an assumption above.

- **C1.** Is there an entitlement or release-statement API, a signed artifact and an offline grace? (None today; ADR-105 open question 4.)
- **C2.** Will registry binaries and entries stay public? If they move behind authentication, how does a local process on the Seed fetch without holding the token? (The optional direct registry check in section 4 step 7 would also stop working for members.)
- **C3.** Does one Seed licence per mesh (scoped to admitted machines) fit Cognitum's terms? Are there size limits, and redistribution terms for the binaries?
- **C4.** Is there an HTTP device-key signing call for local processes? What is the key format of `/api/v1/identity`?
- **C5.** Must a lapse hard-stop running instances?
- **C6.** Will Cognitum provide a withdrawn-version feed?
- **C7.** Does the registry carry aarch64 and x86_64 builds for every cog, beside armhf?
- **C8.** Can the agent install without starting the cog?
- **C9.** Does the registry sign its listing or its per-release entries? If so, with what key, and is that key published for pinning? A yes lets phase 4 replace the operator hash approval with an entry members verify offline.

**For us (W).**

- **W1.** Decided in section 3: the genesis pin combined with an operator `mesh_nonce`. The owner property comes from the binding signature.
- **W2.** Should there be a standby steward?
- **W3.** Should unused checkouts be released automatically?
- **W4.** TLS in `weft-licence`, or the tailnet only (decided in phase 2)?
