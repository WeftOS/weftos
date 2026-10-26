# Fleet manager: snapshot, locations and the gateway path

The daemon can describe the whole fleet it knows in one read-only document, and
an operator can say where each node physically is. The console reads the
document through the gateway (ADR-102), never straight from a device and never
through the cog host. Design brief: `docs/research/fleet-manager/network-tab-brief.md`.

## From the command line

```
weaver fleet status
weaver fleet status --json
weaver fleet location set c6-01 --site "Plant 2" --room "Room 14"
```

`status` prints one row per node: trust tier and its source, address, last
heartbeat, mesh connection class, placed cogs and location. `--json` prints the
raw snapshot with the provenance of every field. `location set` is an Admin
verb (the local socket owner); it saves the label in the daemon's runtime
directory (`fleet-locations.json`) and appends a `fleet.location.set` event
(source `fleet`) to the chain. A node id need not be a mesh peer: an edge node
that only checks in to a cog host can be labelled by the id it checks in with.

## `fleet.snapshot` (Read)

The snapshot is composed from what the daemon already holds. It contacts no
peer and no device (which is why it does not call `workload.status`, a Write
verb because it does). Every node section is `{"value": ..., "provenance": ...}`:

| Provenance | Meaning |
|---|---|
| `signed_fact` | Signed by the node it describes and verified by this daemon (`cluster.facts`: capabilities, trust tier, signed envelope). The licence binding is operator-signed. |
| `daemon_observed` | Held or observed by this daemon: cluster state, last seen, mesh connection class and heartbeat, revocations, the placement controller's instances and lifecycle, `infer.status`. |
| `operator_claimed` | Set by an operator: location labels, and the tier of each placement target. |
| `peer_claimed` | Announced by the peer itself over the mesh and not authenticated: the node `name`, and `announced.platform` / `announced.address`. An unverified peer chooses these. |
| `self_reported` | Said by an unauthenticated edge node about itself (the cog-host roster). Display only. |

Top level: `schema`, `source` (`daemon`), `fetched_at`, `ttl_secs`,
`local_node_id`, `degraded[]` (a source that could not be read, with the
reason), `nodes[]`, and daemon-wide `placement`, `infer`, `licence` and
`revocations`. A node carries `node_id`, `name`, `local`, and any of `cluster`,
`facts`, `mesh`, `revoked`, `instances`, `location`.

`cluster` holds the fields this daemon observes (state, first and last seen). A
peer's last-seen time moves only on verified paths, so an unverified announce
cannot make a node look fresh. `licence` is an observed section; only its
`binding` (state, mesh id, sequence, grant fingerprint) comes from an
operator-signed record, and the record itself is not returned. Placement
instances carry the controller's lifecycle (`state`, `restarts`, `reschedules`,
`has_error`) but no error text.

Read is machine-wide: a caller with a machine-level token sees every placed
instance, while a token scoped to a project sees only that project's. A label
for an id nothing else knows (an edge node that only checks in to a cog host)
is flagged `unknown_node: true`.

Labels are free text typed by an operator. The daemon refuses control
characters and invisible Unicode formatting characters (bidi overrides,
zero-width characters), but a console must still escape labels, names and every
other string it shows from a snapshot.

`mesh.rtt_ms` is `null` with a note: nothing in this build measures round-trip
time, and a zero would read as a perfect link.

`cluster.last_announce` (also `last_announce` in `cluster.nodes`, formerly
`last_seen`) is when a verified join, recovery or announce from the peer last
arrived. It is not liveness: nothing in this build pings peers, so a node that
went quiet keeps its old time and one that is up can look old. A periodic
ping/pong between verified peers (which would also give a real RTT) is the fix.

A read-only token can also read `/api/chain/events`, which shows operator
activity: location labels, node ids, and token issue and revoke ids. It holds
no secrets, but treat it as operator-visible.

## How the console gets a token

The console talks to the gateway, which validates bearer tokens through the
daemon (ADR-102 D3). There is no new credential type.

1. Start the daemon and the gateway (`weft gateway`, or `weft ui`).
2. Issue a read-only token on the daemon's machine:
   `weft token issue --label console --read-only` (15 minutes by default, 24
   hours at most). The secret is printed once. A console should always use a
   read-only token.
3. The console sends `Authorization: Bearer <token>` to
   `GET http://<gateway>:18789/api/fleet/snapshot`.

```
curl -H "Authorization: Bearer $WEFT_TOKEN" http://localhost:18789/api/fleet/snapshot
```

What the token can and cannot do:

- The gateway asks the daemon with an explicit `read` scope for this route
  (`DaemonKernelFacade`), so the call can only reach Read-class verbs, and the
  route table maps only `fleet.snapshot`. `fleet.location.set` is Admin on the
  daemon and has no gateway route (a POST to `/api/fleet/location` is 404).
- A read-only token (ADR-102 D4 amendment) is held by the daemon with scope
  `read`: the daemon grants it Read only, and the gateway lets it `GET` only
  `/api/fleet/snapshot`, `/api/health`, `/api/processes`, `/api/services`,
  `/api/chain/status`, `/api/chain/events` and `/api/vectors/status` (plus
  revoking itself). Every other route, `/mcp` and `/ws` included, answers 403.
  An ordinary `weft token issue` token is still owner-equivalent, so do not give
  that to a console.
- Without a token the route answers 401. With no daemon it answers 503
  (`weaver kernel start`).

### Project reads and project-bound tokens

Read tokens can also `GET /api/projects` (the registered projects),
`GET /api/projects/{ulid}` (manifest plus a sanitised supervisor status: state,
restarts, versions; no pid, socket or failure text) and
`GET /api/fleet/snapshot?project=<ulid>` (only that project's instances; nodes
stay machine-level, as in the daemon's own project-scoped snapshot).

`weft token issue --read-only --project <ulid>` mints a token confined to one
project. The gateway then lists only that project, answers 403 for any other
`/api/projects/{id}` or `?project=`, forces the snapshot to that project, gives
`/api/health` only the anonymous view and refuses the machine-wide reads
(`/api/processes`, `/api/services`, `/api/chain/*`, `/api/vectors/*`). Owner
tokens that carry a project claim are still refused by the gateway.

A console on the tailnet can get such a token itself from
`POST /api/console/token` `{"project":"<ulid>"}`; see `gateway.tailnetIdentity`
in [configuration](configuration.md#gateway).

## Edge heartbeat v2

`POST /fleet/heartbeat` on a cog host accepts the v1 body unchanged and these
optional fields: `uptime_s`, `load` (fraction of one core), `free_heap`,
`reset_reason`, `chip`, `mac`, `channel`, `sample_hz`. Out-of-range numbers are
ignored, strings are capped, and unknown fields are skipped. Every roster row in
`/network` carries `"provenance": "self_reported"`: heartbeats stay
unauthenticated and display-only until leaf keys are provisioned, so nothing may
use them for placement or policy, and the console must label them.

## After a daemon restart: placed instances

A node's `workload-host` journals every instance placed on it (`workload-host-journal.json` in the runtime directory) and puts them back when the daemon starts. A cog that was running runs again; one an operator stopped comes back loaded and stopped; one that was unloaded stays gone. The cog's `data/` is kept across restarts and re-places, per project: the same package placed for another project is a separate instance with its own `data/`.

What the node checks before it trusts the journal, and what you see when a check fails:

- **Signature.** The file is signed with the node key. An edited or foreign file is refused whole: the status says `refused (signature)` and no instance is restored.
- **Generation.** Each save records a generation on the chain. A file older than the chain's record (an old copy put back) is refused: `replayed`. A file the chain has never heard of, or no chain at all, restores every instance loaded and starts none: `unverified`.
- **Missing file.** If the chain says a journal existed and the file is gone or unreadable, nothing is restored and the status says `journal missing` (or `unreadable`; the damaged file is kept as `workload-host-journal.json.bad.<time>`, newest three). Do not treat an empty node after that as normal: restore the file from a backup, or place the cogs again.
- **Controller.** The controller that placed an instance must still be one under the node's current policy.
- **Retries.** A restore that fails for a reason that can pass (the artifact store not up yet, an unreadable revocation list, the runtime failing) keeps the entry and tries again at the next start, five times in all.

Read the state with `status` and `include_departed` (the `journal` row) or the daemon log (`journal missing`, `refused`, `replayed` are logged as errors).

**A crash before the first clean shutdown.** The chain is written out at a clean shutdown today. A node that crashes before its first one comes back with an empty chain record, so the journal reads as `unverified`: every placed instance is restored loaded and none is started until an operator starts it. After a crash later in a node's life the chain can also be older than the journal, which the node accepts. The generation is tamper-evidence only up to the chain's durability; a durable chain (owner card a93f7abe) removes both effects.
