# Ruflo federation as a ticket exchange between boards

Date: 2026-10-11. Status: research, no code changed.

## Question

Could ruflo's federation be how WeftOS boards pull work in and out of each other (the
owner's idea: an interface on each board that exchanges tickets through federation)?

## Recommendation

Do not build board exchange on ruflo's federation. Build it on our own mesh, `weftos://`
names and signed events, and take two ideas from ruflo (pinned-key verification, a
union-merge event log) as patterns. If a ruflo bridge is ever wanted, make it an optional,
off-by-default adapter behind the same interface, never the transport of record.

## Sources read

| Item | Version read |
|---|---|
| ruflo checkout `~/dev/ruflo` | branch `local/main-plus-prs`, commit `6c9cde949` (2026-10-10, "merge upstream/main 3.56.3 into the local Grok host line"). The brief named `feat/grok-host`; the checkout was on `local/main-plus-prs` and was not switched. |
| Pinned version | `weftos.rufloPin = 3.42.4` in `package.json` (documents the grok-host checkout; the checkout has since merged 3.56.3, so what I read is newer than the pin) |
| Dashboard | `weave-logic-ai/weftos-dashboard` `origin/main` at `0ad352e` (2026-10-11), read with `git show` only |
| WeftOS | ADR-108, ADR-114, ADR-063, ADR-112 (TM6), `docs/plans/` subscription proposal, `CLAUDE.md` |

Everything in "What federation provides" was read from source. The ruvnet-brain was not
needed. The README claims (zero-trust, Byzantine consensus, five trust tiers) were
treated as claims and checked against code where noted.

## What ruflo federation actually is

There are three separate things under the word "federation". They share no code path.

### 1. `x_federation_*` tools: the open swarm on a public Nostr relay

Files: `v3/@claude-flow/cli/src/mcp-tools/x-federation-tools.ts`, `x-federation-join.ts`,
`x-federation-channels.ts`; gateway `plugins/ruflo-x-gateway/src/*.mjs`; ADR
`v3/docs/adr/ADR-386-swarm-channels-public-and-private.md`. Licence MIT (repo `LICENSE`,
gateway `package.json`).

- Transport: a hosted gateway (`https://x.ruv.io`, MCP over HTTP) in front of a Nostr relay
  (`wss://relay.ruv.io`). The tools `sync`, `roster`, `claims`, `registry` read through the
  gateway. `publish`, `invite_mint`, `admit` need `RUFLO_X_ADMIN_TOKEN` and sign as the
  gateway, not as the caller.
- Identity: a secp256k1 Nostr key at `~/.ruflo/nostr.key`. `x_federation_join` registers
  with a NIP-98 signed request and proves membership with NIP-42.
- Message format: a Nostr kind-1 event tagged `["t","ruflo-swarm"]`, free-form JSON content
  with a `type` (`PeerHello`, `Status`, `Task`, `Result`, `Claim*`). Not a ticket schema.
- Channels (ADR-386): `pub:<name>` is plaintext, readable by every relay member.
  `prv:<16hex>` is NIP-44 ciphertext under a 32-byte key kept in `~/.ruflo/channels.json`.
  Access is granted by sealing the key to a member's pubkey. Source says plainly: no
  revocation, no key recovery, "rotate to a new channel" to remove someone.
- Claims ledger (`src/claims.mjs`): owner-per-resource with TTL, release, handoff. This is
  the nearest thing to ticket ownership, and it is a lease on a string resourceId, not a
  ticket store.
- Persistence: the relay's event store plus a gateway TTL cache. No retention or deletion
  guarantee is stated in what I read. Anything published should be assumed kept.
- Trust: membership of one global relay. There is no per-peer authority, no per-ticket
  permission, no way to say "this peer may only update tickets in project X".

### 2. `federation_bbs_*`: AgentBBS room logs

Files: `agentbbs-tools.ts` (683 lines), `agentbbs-federation.ts` (477 lines), plugin
`plugins/ruflo-bbs-federation`.

- The protocol part is implemented inside ruflo in TypeScript. It is a per-room
  append-only JSONL log (`room-<roomId>.jsonl`), envelopes
  `{envelopeId, roomId, seq, msgType, payload, timestamp, origin, hops, signature}`.
- Identity: per-node Ed25519 key, 16-hex node id. Peers are added with their public key
  pinned; a receiver verifies against the pinned key, never a key in the envelope.
- Transport: pull over HTTP, `GET /agentbbs/v1/rooms/:roomId/envelopes?since=N`, bound to
  loopback by default; the source says it is designed for a private overlay (Tailscale or
  WireGuard).
- Merge: grow-only set keyed by `envelopeId` (union, idempotent, order-independent).
  Bounds: 5,000 envelopes per sync, 64 KiB per envelope, 16 MiB per response, 256 peers,
  8 hops.
- What it does not do: the merge has no notion of an update to an existing record. Two
  edits to the same ticket are two envelopes, and nothing says which wins. Signature
  covers `canonicalEnvelopeBytes`; `hops` is excluded by design.
- Licence: this TypeScript module ships inside ruflo (MIT repo; I found no separate
  licence header). The upstream AgentBBS product, its Rust binary and the `late-*`
  substrate are FSL source-available (see `.planning/ruv/packages/agentbbs/overview.md`).
  The `agentbbs` npm launcher builds and shells out to that Rust binary, so using it
  pulls FSL code at runtime. Our standing rule holds: patterns only, no vendoring.

### 3. `@claude-flow/plugin-agent-federation` 1.0.1 (MIT)

Files: `v3/@claude-flow/plugin-agent-federation/src/domain/entities/federation-envelope.ts`,
`trust-level.ts`, `services/wg-mesh-service.ts`, `application/claim-checker.ts`.

- Envelope: HMAC-signed, per-session, with a PII scan result, `messageType` from a closed
  list (`task-assignment`, `memory-query`, `claim-event`, `agent-handoff`, ...).
- Five trust tiers (UNTRUSTED to PRIVILEGED) gated by behavioural score and interaction
  count, with a capability table per tier. This is behavioural trust for agents
  collaborating, not an authorization model for records.
- WireGuard mesh service that only emits commands; it does not run them.
- It is closest in spirit to a node-to-node bridge, but it is agent messaging, not a
  record exchange, and its envelope is HMAC (shared secret), not a public-key signature.
  I did not verify its Byzantine-consensus claim; treat the README as a claim.

## Our side, as built

Read from `origin/main` of the dashboard:

- `board_subscriptions` + `subscribed_items`
  (`supabase/migrations/202609260006_board_subscriptions.sql`, `...0007...`,
  `202609270003_subscribed_assignee.sql`). Key `(subscription_id, item_type, source_id)`.
  One subscription per `(workspace, project)`, authenticated by a `wfs_` bearer token
  stored as a SHA-256 digest.
- `POST /api/subscriptions/snapshot` (`app/api/subscriptions/snapshot/route.ts`): a
  complete replace-all snapshot (up to 1,000 items, 1 MB); rows missing from the snapshot
  are deleted. `SubscriptionsView.tsx` labels the view READ ONLY. Fields carried:
  item type, source id, title, description, status, source URL, optional assignee.
  This is deliberately one-way: the source board is the authority, the dashboard is a
  projection and never writes back. A client-side proposal in `docs/plans/` records the
  same rule and a field allow-list per publication.
- Harness ticket API (`app/api/harness/tickets/route.ts`, `wfb_` token): `GET` list,
  `POST` actions `create` (idempotent on `source_key`; returns `created`), `update`
  (title, description), `goal` link, and `claim` / `status` / `comment`
  (`not_claimable` is a 409). Statuses `todo, in_progress, review, done`. So a native
  write path already exists, scoped by token.
- Mesh: ADR-108 gives pairing (`mesh.pair.request`, Ed25519 key checked against node id),
  per-project fetch grants, chunked transfer inside the Noise XX session between node
  keys, chained on both nodes. ADR-114 gives `weftos://` names where "a name grants
  nothing", cross-mesh grants (phase N5, not built) signed by the owning mesh. ADR-063
  (Proposed) is the signed content-addressed envelope. ADR-112 TM6 is signed bundles.
- Authority rule: `CLAUDE.md` makes the dashboard board the authoritative tracker. A
  client-owned board may be the sole source of truth for its own rows.

## Mapping federation onto boards

| Concern | Ruflo federation gives | What boards need | Gap |
|---|---|---|---|
| Item identity across planes | `envelopeId`, Nostr event id, free-string `resourceId` | A stable global id: owning plane + source key, surviving moves | We would define it. `weftos://` plus the existing `source_key` fits; ruflo has no concept. |
| Ownership / authority | Claims ledger lease; first claim wins | Exactly one authoritative plane per item; others hold projections | Claims are leases, not authority. We need an explicit `authority` field and a rule that only the authority plane accepts status changes. |
| Status sync direction | None (messages, no state) | Authority to mirror one way; mirror proposals back as requests the authority may refuse | Whole design is ours. Today's snapshot is one-way replace-all, which is the right default. |
| Conflicts | BBS union-merge has no update semantics | Per-item version or authority ordering | Need per-item monotonic version from the authority plane; mirror edits become `proposal` events, not writes. |
| Permissions | Whole-relay membership; per-channel key (no revoke) | Per-project, per-peer, revocable, field-level (a client may allow title+status only) | Missing. Our pairing + per-project grant model already has it. |
| Audit | Signed events on a relay; PII scan log in plugin | Chained record on both sides of every exchange | ADR-108 already chains fetches on both nodes; reuse. |
| Transport | Public relay, or loopback/overlay HTTP pull | Works through our mesh and the dashboard heartbeat path | Mesh and dashboard already carry this. |
| Data egress | Public relay retains events | Confidential client rows must not leave their owner | A public relay is disqualifying for client data; private channels still leave ciphertext on third-party infrastructure with no revocation. |

## What is missing for the owner's idea (any implementation)

1. A ticket exchange schema: `{id, authority, version, title, description, status,
   assignee, source_url}` plus a signed-event envelope. Ruflo defines none.
2. An `authority` field and the rule above. Today `subscribed_items` has no write path and
   `tickets` have no authority marker.
3. A proposal path: a mirror plane asks (comment, claim request, status request) and the
   authority applies or refuses. The harness API's `claim` / `status` / `comment` are the
   natural landing points.
4. Field allow-lists per subscription (the client proposal already enforces this by
   convention in the publisher; it should be a stored policy).
5. A delta feed instead of replace-all, with tombstones, once exchange is bidirectional.
   Replace-all deleting unlisted rows is wrong for a two-way link.
6. Pull-in direction: a plane that has no dashboard (a server mesh with no Supabase) needs
   an adapter that reads the same events.

## Options

A. Ruflo `x_federation_*` over the public relay. Rejected: public membership model, no
   revocation, event retention outside our control, free-form messages, gateway signs
   writes. Wrong for confidential rows.

B. Ruflo BBS room log (`federation_bbs_*`, ruflo's TS module or a port of its
   design). Good patterns (pinned-key verify, pull-based, bounded ingest, idempotent
   union), but no update semantics and a private-overlay assumption we already satisfy
   with the mesh. Using the `agentbbs` binary adds FSL code: no.

C. Ruflo `plugin-agent-federation`. HMAC shared-secret envelope and behavioural trust
   tiers do not match per-project, revocable grants. Would add a second identity system
   next to ADR-025 node keys.

D. Our own: signed board events carried over the mesh we already have. Each plane
   publishes `ticket.upsert` / `ticket.proposal` / `ticket.tombstone` events signed with its
   node or project key (ADR-063 envelope), named `weftos://<mesh>/projects/<ULID>/tickets/<id>`
   (ADR-114), delivered over ADR-108 pairing and grants, with cross-mesh grants from ADR-114
   N5 for different organisations. The dashboard snapshot endpoint becomes one consumer of the
   same events. Costs us the schema and the authority rules; saves a second identity,
   transport and trust model.

Strongest non-ruv alternative to everything above is plain D. The boring baseline
inside D is signed JSON over HTTPS between boards with per-project tokens (what the
snapshot and harness APIs already do), which is enough for the first phases.

## Walk-away conditions for ruflo

Adopt a ruflo piece only if (a) it is MIT and we can pin it, (b) the data crossing it is
non-confidential or encrypted end to end with a revocable key, and (c) it is optional.
Walk away on: public-relay retention of client rows, FSL runtime code, a second identity
system, or the pin diverging from what we reviewed (the checkout is already past the pinned
3.42.4).

Confidence: claims in the federation sections are verified against source at
`6c9cde949`. I did not run any federation tool or contact `x.ruv.io`. Retention and
rate-limit behaviour of the live relay is unverified.

## Phased plan

1. Phase 0 (no new code path): write the exchange schema and authority rules as an ADR
   (extends ADR-114 names and ADR-063 envelope). Add `authority` and a stable global id to
   tickets. Keep replace-all snapshots as-is for existing subscriptions.
2. Phase 1: signed `ticket.upsert` / `tombstone` export from a board, versioned per item,
   consumed by the dashboard snapshot route (accepts either the old snapshot or events).
   Field allow-list stored per subscription. Still read-only for the mirror.
3. Phase 2: proposal path. A mirror sends `ticket.proposal` (comment, claim, status
   request); the authority applies it through the existing harness actions and emits the
   result as an event; both sides chain the exchange. Pull-in between two WeftOS planes
   rides ADR-108 pairing and per-project grants.
4. Phase 3: cross-mesh (ADR-114 N5 grants) for separate organisations, revocable, with
   the same events.
5. Phase 4 (optional, only on demand): a ruflo bridge adapter that republishes
   allow-listed, non-confidential events to a private channel or BBS room for
   ruflo-based agents. Off by default.

Dependencies: ADR-063 (still Proposed) is the real prerequisite for phases 1 to 3.
Open decisions for the owner: whether any exchanged field set beyond title and status is
allowed for client boards by default, and whether the dashboard stays a pure projection
when the source is a WeftOS plane.
