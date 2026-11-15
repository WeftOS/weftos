# GBF-003: The `computer` workload, first-class desktops, and computer-use access

- **Status**: Proposed (2026-11-09).
- **Series**: GBF (grok-build fork), after [GBF-001](gbf-001-hard-fork-seams.md) and [GBF-002](gbf-002-mesh-bots-and-cos-top-bot.md). The owner placed this ADR in the GBF series (2026-11-09), not in the main ADR numbers.
- **Deciders**: owner
- **Decisions recorded**: D1, D2 and D5 of the Tapestry Phase 3 questions, answered by the owner on
  2026-11-09 (11:49 and 11:56 ET). D3, D4 and D6 are not decided here.
- **Source**: `docs/tapestry/phase3-prep.md` §4 and the decision memo
  `tapestry/p3-decision-memo:docs/tapestry/phase3-decisions.md` (Tapestry repo). Work items W3, W4, W5.
- **Main ADRs it relies on**: ADR-099 (governed placement), ADR-100 and ADR-101 (workload kinds), ADR-103 (daemon topology),
  ADR-108 (pairing), the `project` kind (`workload_kind/project.rs`) for a kind with no signed package.
- **Prior decisions**: unknown. The decision store consult timed out when this was written (`decisions:
  unavailable`), so earlier decisions on a hub or on desktops may exist that this ADR does not cite.
- **Supersedes**: none.

## Context

Tapestry (the grok-build fork) runs agent sessions whose tools may need to run on another machine, and later to
see and drive that machine's screen. Its workspace server and harness meet at a WebSocket hub. WeftOS has to own
that hub (credentials, which machines exist, who may use them) so one mesh has one answer to "which computers can
a bot reach, and on what terms".

## Decisions

### D1. The hub is a service inside the WeftOS daemon, one per mesh

`clawft-weave` hosts the hub. The hub protocol and routing live in the `weft-computer-hub` library
(`crates/weft-computer-hub`, standalone `[workspace]`, path-depended by `clawft-weave` behind the
`computer-hub` feature). The daemon:

- starts the service only when `<runtime dir>/computer-hub/hub.toml` exists (the daemon's own runtime dir, so
  `WEFTOS_RUNTIME_DIR` isolates tests and side installs; nothing reads `~/.weftos` by itself);
- creates each named minter key on first start (hex, 32 bytes, mode 0600, never overwritten);
- mints pass keys through an RPC. `computer.hub.mint` and `computer.hub.revoke` are **Admin**,
  `computer.hub.status` is **Read**. A pass key is the credential for reaching a desktop, so minting is Admin;
- assigns `host_kind` from the **minter** a machine names, never from the request or the token. A caller can
  neither choose a minter that does not serve the machine nor claim another `host_kind`;
- binds loopback unless the file itself says `allow_non_loopback = true` (the hub node is reached over the
  tailnet; pass keys are required either way).

"One per mesh" is an operating rule: only the hub node carries a `hub.toml`. The daemon does not elect a hub.
The `weft-hub` binary stays as a dev tool (`serve`, `mint`, `keygen` against a `hub.toml`).

### D2. First-class desktops: the MacBook, aepod-xpc and photo-gallery

A desktop is first-class when it appears in `[[machines]]` of `hub.toml` with a minter. The list is data, not
code: adding a machine is a config change. ThreadCone and the ruOS fleet are not in the list.

| Machine id | Host | Role | Minter / `host_kind` | Proposed trust tier | Proposed default access (D5) |
|---|---|---|---|---|---|
| `macbook` | the owner's MacBook | daily-use machine, owner's screen | `weftos-macbook` / `desktop` | pinned (the owner's own device) | `granted`: the owner is at this screen |
| `xpc` | aepod-xpc (RTX 4070, Windows + WSL) | mesh CUDA and CI host | `weftos-xpc` / `desktop` | pinned | `own` for the bots placed there |
| `photo-gallery` | photo-gallery (mesh hub node, main store) | mesh hub, RustFS | `weftos-photo-gallery` / `desktop` | pinned | `own` for WeftOS-account bots only; client files stay under `/data1` and client services under the client user (PG client isolation rules) |

The trust tiers and default access are **proposals for the owner to confirm**; D2 named the machines, not their
tiers. Each machine has its own minter so one leaked key serves one machine.

### D5. A bot has full access to its own computers; real desktop control is asked per action, with an allowlist

- **`own`**: a bot has full access to its own computer or computers. No per-action ask. A manifest for an `own`
  computer must not carry an allowlist (it would contradict the access).
- **`granted`**: real desktop control (screenshot, click, type) of a desktop the bot was granted. Every action
  needs a per-action ask through R9 (`escalate_to_human`), **and** the target app and display must be on the
  manifest's allowlist. Anything outside the allowlist is **denied, not asked**. A `granted` manifest must carry
  both lists, non-empty. Matching is exact (no prefix, no case folding). Nothing is remembered between actions.

The rule is code: `ComputerBody::verdict(app, display)` returns `Allow`, `Ask` or `Deny`.

## The `computer` workload kind (W3)

`ComputerKind` (`crates/clawft-kernel/src/workload_kind/computer.rs`) is registered beside `CogKind` and
`ProjectKind`. Manifest body:

```json
{ "desktop": "macbook", "host_kind": "desktop",
  "capabilities": ["screen.capture", "input.click"],
  "access": "granted",
  "allowlist": { "apps": ["Terminal"], "displays": ["main"] } }
```

- `migratable()` is `false`: a desktop's state is bound to its hardware, so losing the node raises an alert.
- Like `project`, a computer has no signed package. `load` refuses a package directory; the workload is built
  from the validated envelope (`computer_workload`), authority being the operator's declaration. The placement
  spec pins to the named desktop through the capability `computer.desktop.<id>`.
- `workload.place` and `workload.explain` accept `{ "computer": <body> }` alone, validate it, and reply with the
  spec. A bad body, an unknown field, or any other parameter next to `computer` is refused.

## Placement, trust and supervision (owner decisions of 2026-11-15)

- **A package-less place path.** `workload.ctl place` on the node's host accepts `{name, variant: "computer",
  computer: <body>}` and no `manifest_hash`. `place` routes it to `WorkloadHostService::computer_order`
  (`workload_ctl/host_computer.rs`): the order must route to the `computer` adapter, its `name` must be the desktop
  id, and the body must pass `ComputerBody::parse` (desktop id `[a-z0-9][a-z0-9_-]*`, at most 64; host kind;
  capabilities; the `own` / `granted` rule). Every other kind keeps requiring a signed package: a package-less
  order without a computer body, or a computer body next to a hash, is refused as an invalid request.
- **The desktop need not be connected yet.** A desktop is minted, then registers when its tool server starts, so
  a place that arrives first is legitimate. Until it connects the adapter reports "no hub connection for this
  desktop" and the supervisor alerts after the detection window. A node with no hub has no `computer` adapter, so
  it refuses the order (`admission`).
- **Governance.** Kind `computer` is admitted by `computer_owner_permit` (`workload_governance/policy.rs`, id
  `COMPUTER-OWNER-PERMIT`): `min_package_trust = operator_attested` (what a declared computer carries), no
  network, no cost, and only for the principals it names. `place` decides `workload.place` for the **requester's**
  principal (the signing controller's node id); the host's own gated `load`, `start`, `stop` and `unload` decide
  as the node. The daemon ships the permit only when the hub runs, naming exactly this node's own id: the
  identity the daemon signs its own placement requests with. A controller listed in `workload-host.json` is not an
  owner for this kind (the narrowest reading of "Mathew's own principals", since nothing in the codebase marks an
  owner more precisely). To admit a remote controller, add a `computer` permit with `principals` to
  `workload-permits.json`, naming this node's id too.
- **Alert, never restart.** `ComputerKind::restart_on_unhealthy()` is `false`. After `miss_limit` (3) polls past
  the 45 s heartbeat window (about 90 s from the last frame) the instance goes `Unhealthy`, a lifecycle event is
  chained with `phase: alert`, `alert: true`, `restart: never` and the named reason, and a warning is logged.
  The supervisor does not stop or start the watch. A later healthy poll returns the instance to `Running`.
- **Wiring.** `workload_host_serve::local_host` registers the `computer` route from
  `computer_hub_svc::computer_runtime()` behind the `computer-hub` feature; the daemon starts the hub before it
  builds the host. `workload.place {computer}` alone (no host call) still only declares.

## Not done here (open)

- **Run-time revocation is in memory.** `computer.hub.revoke` lasts until the daemon restarts; a `jti` in
  `revoked_jti` of `hub.toml` is durable.
- **Diff gate.** The hub library adds a git dependency (`xai-tool-protocol` from the WeftOS grok-build fork) and
  new crates to the lockfile; D1 does not grant the diff-gate approval. The `computer-hub` feature is off by
  default until the owner approves the dependency.

## Consequences

- One place answers which desktops a bot can reach and on what terms; Tapestry's hub client reads it from the
  daemon.
- A leaked pass key reaches one machine for its lifetime; revocation is one RPC.
- Default builds are unchanged until the owner approves the dependency and the feature is switched on.

## Open questions for the owner

1. Confirm the trust tier and default access per desktop in the D2 table.
2. (Answered 2026-11-09: D1 covers the hub's new dependencies and network findings; the lead records the approval rows.)
3. Should the `computer-hub` feature become a default once approved?
