# ADR-126: Mesh object store: WeftOS-governed buckets for nodes, projects, worktrees and releases

- **Status**: Proposed (2026-10-29; the owner asked for a meshed S3 store and set the bucket model)
- **Updated**: 2026-10-29 (§4's worktree overlay is re-expressed as RVF layers and branch metadata in ADR-128)
- **Updated**: 2026-10-29 (S0 spike applied on PG and run; results in "S0 results"; open question 1 answered: import, not in place; §6 amended: the activity log must take RustFS's own log as well as its audit stream)
- **Updated**: 2026-10-30 (ADR-130 adds a GCS seed bucket as the tier above this store; this store becomes the optional L1 staging tier)
- **Updated**: 2026-11-07 (follower view §4a, Windows-safe fetch, notification/poll answers from ADR-131; S2 implemented on `feat/adr126-s2`, not yet landed: overlay, promotion, agent-git-flow hooks, union mount; §4 amended by the owner extension, board card f40828fb: the worktree tree itself is shared, so unmerged docs stay visible and another agent can merge)
- **Updated**: 2026-11-07 (S1 built on `feat/adr126-s1`: `weftos-store` crate, `weaver store`, daemon wiring, local-directory backend; RustFS S3 + admin transport added and verified against a throwaway RustFS 1.0.4, audit-stream and notification parsers fixed from captured records, `put_if_match`, release-store `append`/`sweep` roles; see "S1 status")
- **Updated**: 2026-11-08 (chain compaction proposal and grant-ledger spike; review fixes and handoffs). Earlier: 2026-11-07 (live run against RustFS 1.0.4 in a loopback container: results in "S3 results"; edge presign fixed)
- **Updated**: 2026-11-07 (card b76c74a7: live STS minter and chain-fed grant ledger added to `weftos-edge-creds`, still tested against a mock store)
- **Updated**: 2026-11-07 (S3 implemented in `crates/weftos-edge-creds` against a mock store; see "S3 results". The ADR stays Proposed: S1, S2, S4, S5 are not landed)
- **Deciders**: owner
- **Research**: `docs/research/rclone-shared-filesystem-2026-10-29.md` (rclone, RustFS,
  OpenDAL, governance gaps). Board card `3605565e`.
- **Prior decisions**: unknown. The decision store (Ruflo) was unavailable when this was
  written. The repo has no earlier ADR on shared storage.
- **Builds on**: ADR-114 (`weftos://` names; a name grants nothing), ADR-120 (exposures and
  the activity log), ADR-122 (cog data plane and blob store), ADR-099 (governed placement),
  ADR-108 (pairing), ADR-103 (per-project daemon topology).

## Context

The owner wants several machines, people and agents to work on the same files, so
swarming can go horizontal. Gitignored files (models, data, fixtures, generated assets)
should already be in place wherever an agent lands, and a local environment should be able
to work against a remote one.

The owner's direction, 2026-10-29:

- **RustFS on PG is the mesh's S3 storage** for every other node.
- **The bucket model:**
  - a **project bucket** for each project. That is the project directory we have on PG today;
  - a **bucket for each worktree** where possible, so one agent or many can work on a
    project at once;
  - **CI/CD buckets** for release builds;
  - a bucket for **each WeftOS node**, even one that is only a Wasm build on an edge device.
    Other WeftOS processes, given access, can then use it.
- **WeftOS controls these spaces and namespaces them properly.** Design starts at the
  higher level, with nodes and projects, and goes deeper into clients and releases later.
- **Governance:** transfers may not always be stoppable, but every connection and
  operation is logged, and a connection that breaks the rules is removed.
- **Credentials:** for people's machines, Terraform writes them as a 0600 `rclone.conf`.

Building blocks (checked 2026-10-29):

- **RustFS**: Apache-2.0, release 1.0.4. It has IAM, policy, audit, bucket notification,
  replication and erasure coding. It is also a library on crates.io (`rustfs`,
  `rustfs-iam`, `rustfs-audit`, ...).
- **OpenDAL** (`opendal`, Apache Software Foundation) is the Rust client for S3 and other
  backends.
- **rclone** is the desktop mount and sync tool. It has no file locking, and each client
  keeps its own cache.

## Decision

### 1. One store service per account, run by the daemon

RustFS runs as a governed workload under the daemon (ADR-099), not linked into the daemon
binary. Linking it in can come later, behind an off-by-default feature.

- **The weftos account on PG holds the mesh's store.** It is bound to the tailnet and
  loopback only.
- **A client account gets its own instance in a later phase (S5)**, running as the client
  user with its data under `/data1`, so the PG client-isolation rules hold. One instance
  never holds two accounts' data.

### 2. Names first, buckets second

Every store has one `weftos://` name. It is a `store` sub-resource of the thing that owns it
(ADR-114: one thing, one name; sub-resources are kind-specific).

| Owner | Name | S3 bucket |
|---|---|---|
| a node | `weftos://<mesh>/nodes/<node id>/store` | `n-<node id>` |
| a project | `weftos://<mesh>/projects/<ULID>/store` | `p-<ulid>` |
| a worktree of a project | `weftos://<mesh>/projects/<ULID>/worktrees/<slug>/store` | `w-<ulid>-<h12(slug)>` |
| a release pipeline of a project | `weftos://<mesh>/projects/<ULID>/releases/<channel>/store` | `r-<ulid>-<h12(channel)>` |

- **New ADR-114 sub-resources:** this adds `worktrees/<slug>` and `releases/<channel>` to
  `projects`, and `store` to `nodes`, `projects`, `worktrees` and `releases`. ADR-114 is
  updated when this ADR is accepted.
- **Bucket names** are derived from the name, never chosen by hand:
  - lowercase;
  - at most 63 characters;
  - `h12` is the first 12 hex digits of BLAKE3 over the slug.

  The readable slug, the owner and the creation time are kept as bucket tags.
- **The mesh is implicit:** one store instance serves one mesh's account.

### 3. The daemon owns the spaces

Only the daemon creates, grants access to, or deletes a store.
   Amended 2026-11-08 (owner): any other node or person may sign a grant; the daemon verifies and applies it. Creating and deleting a store stay with the daemon.

- **Lifecycle follows the owner:**
  - pairing a node creates its bucket;
  - `weaver project add` creates the project bucket;
  - creating a worktree (agent-git-flow `wt.sh`, or `weaver`) creates its bucket;
  - cleaning the worktree up removes its bucket, after its contents are promoted or archived;
  - removing a project or unpairing a node archives that bucket, then deletes it.
- **A name grants nothing.** Access is a grant of (principal, store, read | write | admin),
  recorded on the chain. Principals are nodes, members, agent instances and cog instances.
  The daemon writes each grant into RustFS IAM as a policy on that principal's access key,
  so RustFS enforces what governance decided.
- **Default grants:**

  | Store | Default access |
  |---|---|
  | node | the node writes; other WeftOS processes on that node read and write; nobody else |
  | project | the project's members and agents read; promotion and owners write |
  | worktree | the agents and people working in it write; read falls through to the project (§4) |
  | release | CI writes once; everyone in the project reads |

### 4. Worktree stores overlay the project store

- **Reads fall through.** A worktree bucket starts empty, and a read checks the worktree
  bucket first, then the project bucket. A new worktree therefore has every gitignored
  asset without a copy.
- **Writes stay in the worktree bucket.**
- **Promotion happens when the worktree lands** (agent-git-flow `land`). The worktree's
  objects under promotable paths are copied into the project bucket, and anything else is
  archived or dropped. Promotable prefixes are `assets/` and `shared/` by default; `agents/<id>/`
  scratch is archived. If a project object changed after the worktree was created and differs
  from the worktree's, promotion fails before writing anything (or keeps the project copy, or
  overwrites, by policy). Deleting a project object in a worktree leaves a tombstone, which
  promotion turns into a delete in the project (skipped if the project object changed since the
  worktree was created, unless the policy is overwrite). An object and a tombstone for the same key
  mean the object wins. "Changed" is decided by etag, not by time: the first write or delete of a
  key in a worktree records the project's etag for it (`.wt/base/<key>`, `-` if the project had
  none), and promotion treats the project as changed when its etag now differs. No clock is
  involved, so neither client nor server skew can hide or invent a conflict; an object written
  around the overlay has no recorded base and is treated as a conflict (fail safe). An overlay
  read that falls through to a project object records the base too (best effort), so a stale edit
  of something read earlier conflicts; a read made around the overlay still records none. A
  project-side delete of a file the worktree edited is a conflict as well. Every project version
  that promotion overwrites or deletes is first copied to the archive under `.wt/displaced/`.
  Promotion copies use S1's `put_if_match` (or an if-none-match create) so a project write between
  the plan and the copy is a conflict, not an overwrite; deletes re-check the etag just before
  (no conditional delete exists). Claims carry a fencing epoch that `promote` can require, and the
  daemon decides claim expiry on silence it observed on its own monotonic clock. Registry signing
  mechanism, the key options and the off-by-default daemon-only registry store
  (`WEFTOS_WT_REGISTRY_STORE=1`): see
  `docs/research/adr126-s2-registry-signing-and-claim-clock.md`. The daemon refuses to `create` a slug that has already landed. The run is idempotent and
  safe to retry after a partial failure; once the report is written, later writes to that
  worktree bucket are ignored.
- **Who does the overlay:** the `weftos-store` client does it for daemons and agents. For a
  desktop mount, a small rclone `union` remote (worktree first, project as read-only)
  gives the same view.
- **Git trees and `target/` stay on local disk as the working copy; the store also carries a
  shared snapshot of the tree.** (Amended 2026-11-07, owner, card f40828fb. This replaces the
  earlier rule "the store holds the gitignored layer only", which is why docs and planning
  written in a never-merged worktree were invisible to everyone else.)
  - **What is shared:** the files git can see (tracked, plus untracked and not ignored), under
    the reserved `.wt/tree/` prefix of the worktree bucket; a `git bundle` of the unmerged
    commits (`.wt/git/branch.bundle`); a manifest (`.wt/tree.manifest.json`) listing each file's
    SHA-256, which paths are uncommitted, and an index of docs and planning with titles; and a
    handoff record (`.wt/handoff.json`).
  - **What is not:** `target/`, `node_modules/` and other ignored files (the overlay layer above
    is how those travel), symlinks (or files under a symlinked directory), files over 16 MiB, and
    anything that looks like a secret: names such as `.env*` (not `.env.example`), `*.env`,
    `.envrc`, `*.pem`, `*.key`, `*.jks`, `id_*`, `credentials*`, `.pgpass`, `*.tfstate`,
    `*secret*` (not Markdown), and any file containing `PRIVATE KEY-----`. This is a denylist and
    is incomplete by nature; the writer stays responsible for what is in the tree.
  - **A fetch treats the manifest as untrusted:** it refuses `.git/`, `.gitattributes`,
    `.gitmodules`, `.envrc`, names with `:`, secret-looking names and case-insensitive
    collisions, checks every object's SHA-256, and never overwrites a differing local file
    without `--force`.
  - **Git still lands code.** The store makes the handoff possible across agents and machines: a
    second agent fetches the bundle and the tree, claims the merge, lands it with normal git, and
    marks it merged (which requires holding the claim). Promotion (below) then runs. The claim is
    advisory: there is no cross-client locking, two agents racing can both read back their own
    write, so the merger must still land through git's compare-and-swap. While a claim is live,
    only the holder can republish or re-hand-off; a claim older than 30 minutes can be taken over.
    The daemon evaluating the claim compares the claim's stamp (written by the claimer's clock)
    with its own clock. A claim stamped more than 10 minutes ahead of that clock is treated as
    expired, not refused, so a claimer with a bad clock cannot block a merge forever. The claim
    host is never "unknown": macOS reads `LocalHostName` from the system preferences, and the CLI
    requires `--host` when no name is found.
  - **Reserved state never leaks:** everything under `.wt/` is hidden from overlay reads and
    listings and is never promoted as an asset. On promotion the shared tree, bundle, manifest and handoff
    record are archived with the rest.
  - **Control state is daemon-only:** `.wt/` in a worktree store and `worktrees/` in a project
    store are written only by the daemon. The governed path refuses those keys, and the
    per-principal IAM policy carries a `Deny` for the same prefixes. Promotion still trusts
    `.wt/promoted.json`, `.wt/promoting.json` and `.wt/base/*`, which is safe unless someone holds
    the RustFS root credential, because that credential can forge them.
  - **Ceilings and the bundle hash:** the manifest is untrusted, so `fetch`, `sync-docs` and
    `search` enforce limits on the manifest's size, the number of files and docs it lists, each
    file's size, the total selected and the bundle's size, and refuse over-limit input as an error
    rather than truncating. The manifest records the bundle's SHA-256, and a fetch refuses a bundle
    that does not match it.
  - **Slug and bucket rules:** a registry entry must name this project's bucket and the slug it is
    filed under, or it is unreadable and not followed. A landed or archived worktree's slug cannot
    be created again until its store is archived with `--delete`, and a promotion record from an
    earlier life of a slug is ignored. `publish`, `claim` and `promote` refuse a store this daemon
    knows to be archived, a bucket with no metadata, and a bucket whose metadata names another
    slug or project. A second daemon on the same backend can archive and delete a worktree store
    the first created (with `--delete` only); the creating daemon's own record and grants stay
    until it drops them, which is harmless once the bucket is gone.
  - **Path rules for the `store.worktree` verbs:** the daemon reads and writes the paths itself,
    so every path parameter must be absolute and resolved. They must lie outside the daemon's own
    state (`--dest` and `--cache` may not be inside the runtime directory or contain it),
    `--root` must contain `.git`, and the lists and the bundle must be regular files. No verb
    writes through a symlink, and `fetch_bundle` writes a temporary file and renames it.
  - **Discovery:** the daemon writes a small registry entry per worktree into the project bucket
    (`worktrees/<slug>.json`: branch, head, state, writer, merger, doc count). Worktree principals
    cannot write there, so "writes stay in the worktree bucket" still holds for agents.
- **One writer per path.** There's no cross-client locking. Parallel agents in one
  worktree write under `agents/<instance id>/`, and shared paths are read-mostly.

#### Landing controls, quotas and retention (2026-11-08)

- **Preview and conflict policy.** `weaver store worktree promote --dry-run` (`worktree-store.sh land-preview`)
  reports what a land would do with no writes: keys to promote, keep, archive and delete, and each conflict
  with its kind (`both_edited`, `project_deleted`, `deleted_but_project_edited`). `--on-conflict
  fail|keep_project|overwrite` (`WTS_ON_CONFLICT` for `land`) chooses per land; `fail` stays the default.
- **Size cap on a land.** A land that would copy more than 1 GiB into the project is refused before any
  write (`--max-bytes N`, `0` = no cap, `WTS_MAX_BYTES`). Scratch that is archived does not count.
- **Worktree store quota.** `publish` refuses when the store already holds more than 2 GiB
  (`--quota-bytes N`, `0` = none). This is a check at publish, not a hard limit: an agent writing straight
  to its bucket with its own key is not stopped (RustFS bucket quotas are not used). `store worktree usage`
  lists objects, bytes, landed time and over-quota per worktree store.
- **Retention.** `store worktree gc` is a dry run unless `--confirmed`. It retires worktree stores landed
  at least `--keep-days` ago (default 14; archived then deleted through the store service) and never an
  unlanded one, however old: that is what `leftovers` reports. With `--archive-keep-days N` it also deletes
  archived worktree objects older than N days (default: never expire; this is recovery data).
- Tests: library (`s2_worktree`), daemon (`store_worktree_tests`), script, and a live loopback RustFS run
  (`s2_land_rustfs`, ignored: preview, conflicts, cap, `@`-bearing archive keys, retention listing).

#### Read-modify-write uses `put_if_match` (2026-11-08)

The handoff record (`.wt/handoff.json`) is changed only with a conditional write. `claim_merge`,
`mark_handoff` and `mark_merged_at` read the record's etag, then its body, decide, and write with
`put_cas` (S1 `put_if_match`, or `if-none-match` when creating). If someone else changed it first the
write reports "lost", the caller re-reads and decides again (up to 6 times, then a "being changed by
others" error). Effects: of two claimers that read the same record exactly one write lands and the
other is refused; a writer's late `mark_handoff` cannot erase a claim that landed after it read; a
merge cannot land over a takeover that happened after its epoch check; a first publish cannot
replace a record created between its check and its write. The etag is read before the body, so a
change between the two calls makes the write fail rather than pass on a stale decision. Evidence:
deterministic race tests (`s2_handoff_cas`, they fail on the unconditional code) and a live loopback
RustFS run with 16 simultaneous claimers over 8 rounds (`s2_land_rustfs`; this one passes on the old
code too, because RustFS answered fast enough that the old write-then-read-back rarely interleaved, so
it shows the conditional path is correct on the real backend, not that the old one was wrong).
Not converted: the registry refresh (a last-writer-wins pointer the daemon rewrites from the worktree
bucket), the follower's state file (local), and `publish_tree`'s many-object write (one writer per
worktree; the live-claim check before it is not atomic with it).

### 4a. The L1 follower view: seeing unmerged worktrees before integration

(Added 2026-11-07, owner decision: this is required leftover work, not optional. ADR-130 names
L1 as the tier for "unmerged layers that other nodes should see before integration".)

- **Follower:** any node with project read access. It is read-only; it never writes to a worktree
  or project bucket. It keeps one small JSON state file of what it last reported.
- **Source of truth:** the daemon-written registry in the project bucket (`worktrees/<slug>.json`),
  then each worktree's manifest. The daemon refreshes a registry entry on every publish, handoff,
  claim and merge, and removes it after promotion and bucket deletion.
- **What a follower can do** (`weftos_store::follower`, `worktree-store.sh`):
  - `poll` / `follow`: new, head-moved, state-changed, gone and unreadable worktrees since the last
    look. It fetches registry entries eight at a time and reads `worktrees/_digest.json`, a hint the
    daemon rewrites from a fresh listing on every registry change, so only entries whose etag
    moved are fetched. Two daemons refreshing at once can leave the digest one update behind until
    the next refresh; `follow --full` skips it. A head move and a state change in one poll are both reported; a recreated slug (new
    bucket) is gone-then-new; one corrupt registry entry is reported, not fatal; an empty registry
    when worktrees were known is an error (wrong bucket?), not mass-gone, unless confirmed;
  - `search_docs` / `search`: docs and planning across every unmerged worktree, by path or title;
  - `sync_docs` / `sync-docs`: a read-only local mirror of one worktree's docs, pruned as docs leave;
  - `leftovers`: open worktrees idle 3 days, handoffs unclaimed 1 day, claims past the 30-minute
    TTL, merged entries still in the registry after a day (cleanup stuck), and timestamps in the
    future (writer clock ahead, so idle time is unknown). This is the forgotten-work detector;
  - `overlaps`: files two unmerged worktrees both changed differently, as a merge-conflict hint.
    It names both base commits so a caller can discount a result when they differ, because with
    different bases it over-reports. It also reports a file one
    worktree deleted that the other changed (a rename is a delete plus an add, so editing the old
    name is caught). That needs the manifest's `changes` list, which the script builds from
    `git diff --name-status --no-renames <base>` plus untracked files; a manifest published before
    that field existed gets content comparison only. Paths are compared, never hunks, so absence
    still does not prove a clean merge;
  - **audit:** the S2 verbs a follower's operator runs (create, publish, claim, promote) appear in
    the activity log and on the chain, failures included;
  - **a registry entry is a pointer, not authority:** before reading a bucket a follower checks the
    slug is well-formed and that the bucket's own `.wt/meta.json` names the same slug, so an entry
    cannot aim a follower's read grants at another worktree's bucket.
- **Waking and polling** (owner answers on ADR-131, 2026-11-07): head notifications reach devices
  through the mesh relay from one site subscriber (the PG WeftOS daemon). Followers treat a
  notification only as a wake-up and re-read the registry; a 30-60 s poll is the fallback and the
  source of truth. Polling costs one list plus one get per worktree. Registry entries are not
  signed in S2, so a follower trusts its own read grants and the bucket cross-check below; signed
  worktree heads are an open question if followers ever read from an untrusted relay.
- **Spike evidence and signing wiring (2026-11-08):** `docs/research/adr126-s2-l1-follower-view.md`.
  Against a throwaway loopback RustFS with scoped keys, a read-only follower sees, searches, mirrors
  and overlaps unmerged worktrees, and is refused every write. `promote` now removes the registry
  entry (it used to leave a landed worktree showing as an abandoned claim). Daemon signing is built
  and off unless `WEFTOS_WT_REGISTRY_SIGN_KEY` / `_TRUSTED` / `_REQUIRE_SIGNED` are set; the owner's
  remaining decision is which key.
- **Signature binding and rollback (2026-11-08):** a registry signature covers a domain prefix, the registry
  bucket, the slug, a generation and the entry bytes, so it cannot be replayed into another project or name;
  followers remember the highest generation per worktree and refuse an older signed entry. A trusted-key
  file filters by default (`REQUIRE_SIGNED=0` for an audit mode) and verbs return a `trust` field. See
  `docs/research/adr126-s2-l1-follower-view.md`.
- **Per-client stores:** a client project has its own buckets (ADR-131: one seed bucket and one
  inbox bucket per client), so a follower of a client project reads only that client's registry.
- **Clocks:** ages compare the registry's writer-side timestamp with the follower's clock, so the
  thresholds are days. They are a nudge, not a lock. Claims are the exception and are checked as
  described in §4.
- **Not in scope:** an operator UI (card d18e1ab0), automatic merging, and enforcing that anyone
  acts on a leftover.

#### Gate for moving a project (Sansone) onto shared worktrees

Card 4c4319e0 holds the move until these have passed. S2 supplies the code; the evidence is still
owed and is **not** produced by the in-memory tests.

1. Against real RustFS (not `MemBackend`): publish, poll, search, sync-docs and claim with two
   different principals on two machines; a follower sees a publish within one poll interval.
2. A Windows node loads a worktree bucket natively (never tested; in scope now on aepod-xpc as the
   bucket-load proof, with the ProjFS drive after macOS D1 and Linux D2). Fetch already refuses
   reserved names (`CON`, `NUL`, `COM1`), trailing dots/spaces, `<>:"|?*`, `git~1` aliases and
   case-insensitive collisions on every platform; long paths and CRLF are still to try.
3. Metadata caching is only partial: measure `list` and `head` latency for a worktree of Sansone's
   size (file count, largest file) before assuming one-poll-per-worktree is affordable.
4. Leftover drill: leave a worktree open past the threshold and confirm `leftovers` reports it;
   hand one off and confirm a second agent on another machine lands it with git, then promotion
   and bucket deletion complete and the registry entry disappears.
5. Failure drill: kill the writer mid-publish, kill the merger mid-claim, take the daemon down
   during a registry refresh. Nothing may be lost, and a stale claim must be recoverable.
6. Follower drills on real clocks and a real registry: a writer machine a day ahead; a corrupt and
   an unknown-schema entry; an empty or wrong project bucket; a merged entry whose cleanup never
   ran; an interrupted `sync-docs` followed by a deleted doc; `overlaps` on two real worktrees with
   different bases (measure the false-positive rate); two overlapping `follow` runs sharing a state
   file (`FollowState::save` writes atomically via temp+rename but does not lock, so the last writer
   wins; `load` errors on a corrupt file rather than resetting).
7. ADR-131 D1 and D2 pass first (the card's prerequisite).

### 5. Edge and Wasm nodes hold no long-lived keys

- **The node bucket is reached through the parent.** A node that is only a Wasm build on an
  edge device still has its node bucket, and reaches it through its parent daemon. The parent
  either proxies the request or hands out short-lived credentials: presigned URLs, or STS
  temporary credentials if RustFS supports them (verify in S0).
- **Long-lived access keys live only on full daemons and on people's machines** (via
  Terraform).

### 6. Governance and logging

- **Exposure:** the store is an ADR-120 exposure on its host, with the on/off switch.
  Switching it off withdraws it from the mesh.
- **Logging:** RustFS audit targets and bucket notifications go into the activity log,
  with the principal, the store name, the operation and the size. Every create, read,
  write, rename and delete is logged. S0 proves this; if a type of operation is missing,
  this ADR is amended.
  **Amended after S0:** the audit stream doesn't cover every operation. Refusals (403 with the
  account) and bad signatures go only to RustFS's own JSON log (`s3_authorization_denied`,
  `SignatureDoesNotMatch`), so the activity log takes both sources. Successful listings
  (ListObjects, ListBuckets) and multipart part uploads appear in neither, so listing a store
  isn't logged.
- **Removing a connection:** a rule hit disables that principal's access key, so its next
  request is refused. The daemon also drops its proxied sessions for that principal. A
  transfer already in progress may finish. That's accepted (owner).
- **Notifications drive cache invalidation:** on nodes and mounts, a bucket event tells
  rclone (`vfs/forget`) or the `weftos-store` client that an object changed.

### 7. The `weftos-store` crate

`weftos-store` is a new crate:

- **Name-to-bucket mapping** (§2) and the overlay resolver (§4).
- **An OpenDAL S3 client** used by the daemon, cogs and agents. Agents and cogs go through
  the daemon, so their operations are governed and logged. The ADR-122 blob store can keep
  large items here.
- **The control-plane client:** creates buckets and access keys and applies policies,
  through the RustFS admin API.
- **Weaver commands:** `weaver store ls|grant|revoke|promote|archive`.

## Phases

| Phase | Scope | Done when |
|---|---|---|
| S0 | Spike: RustFS on PG under the weftos account, through Terraform | verified: audit covers every operation type; time from disabling a key to refusal; RAM and disk on PG; the S3 operations we use (multipart, list v2, copy, conditional put); STS support; on-disk format (can the existing project dir be served in place, or must it be imported?) |
| S1 | `weftos-store` crate, node and project stores, grants, exposure, audit and notifications into the activity log | a node and a project store are created by the daemon; a write by a cog shows in the activity log; disabling a grant refuses the next request |
| S2 | Worktree stores: overlay, promotion on land, agent-git-flow hooks, desktop union mount | two agents on two machines in one worktree see each other's outputs; a new worktree sees the project's gitignored assets with no copy; landing promotes |
| S3 | Edge and Wasm nodes through the parent daemon, short-lived credentials | a Wasm node writes to its node bucket without holding a long-lived key |
| S4 | Release stores: CI writes once, retention, signed artifacts referenced by hash | a release build's artifacts land in its release store, and a rebuild can't overwrite them |
| S5 | Clients: one instance per client account under `/data1` as the client user; cross-mesh grants (ADR-114 N5) | a client project's store lives only under the client user, and a WeftOS-account principal is refused unless granted |

## S0 results (2026-10-29)

These come from a spike on PG: RustFS 1.0.4, single drive, run as `weftos`. The Terraform is
branch `tf/rustfs-spike` in `weftos-weavelogic.terraform`, with the runbook in
`docs/rustfs-spike.md`. The client was rclone 1.75.2 on the Mac, using a test user whose
policy covers only `s0-spike`.

| Check | Result |
|---|---|
| Audit coverage | Needs `RUSTFS_AUDIT_ENABLE=true`, which defaults to off. With it on, every event carries the access key, client IP, operation, bucket, object, status code and timing, with auth headers redacted. **Logged:** PutObject (including 412s), GetObject (including presigned), HeadObject, CopyObject, DeleteObject, CompleteMultipartUpload, CreateBucket. A rename shows as copy plus delete. **Not in the audit stream:** ListObjects, ListBuckets and UploadPart, which aren't logged anywhere, plus 403 refusals and bad signatures, which go to `rustfs.log` only. |
| Policy | The test user is refused (403) on any other bucket, for both create and list. |
| Revocation | `admin user disable`: the next GET, about 0.22 s later, gets a 403. A 1 GB multipart upload in progress fails at its next part (403) and is never completed. Re-enabling takes effect at the next request. |
| STS | AssumeRole issues 15-minute temporary credentials, and a GET with them returns 200. §5 can use STS for edge and Wasm nodes. |
| Conditional put | `If-None-Match: *` returns 412 on an existing key and 200 on a new one. |
| On-disk format | RustFS's own (`xl.meta`, as in MinIO). Small objects are inlined, large ones stored as `part.N` files. **Answer to open question 1: the existing project directory can't be served in place; it's imported once.** |
| Footprint | The `rustfs` process: about 310–320 MB RSS, idle and under a 1 GB upload. The service cgroup reached 1.5 GB after the upload, which is mostly the kernel's page cache. The audit sink: 19 MB. An hour-long reading is still to do. |
| Restarts | Objects are intact after `restart rustfs.service` (the md5 of the 1 GB object is unchanged). With the audit sink stopped, events queue on disk and are delivered once it's back. |

**Audit follow-up (2026-10-29, source read of the 1.0.4 tag plus a re-test on PG):**

- **Why the gaps exist.** Audit is emitted per handler, through `OperationHelper`
  (`rustfs/src/storage/helper.rs`). A handler without one emits nothing, and the helper only
  exists for operations that map to a notification event name. About 43 operations map to
  none (`crates/s3-ops/src/lib.rs`). No code comment, doc, issue or PR says this is a
  deliberate scope. It reads as an oversight that comes from tying audit to notification
  events.
- **Not audited, confirmed on PG:**
  - UploadPart and AbortMultipartUpload;
  - PutBucketPolicy and DeleteBucketPolicy;
  - PutBucketVersioning;
  - PutBucketTagging and DeleteBucketTagging;
  - HeadBucket.

  The unaudited policy and config changes are the larger governance gap.
- **CreateMultipartUpload** is emitted but then lost. With a queue directory set, the queued
  event fails to replay ("Invalid event name: s3:ObjectCreated:CreateMultipartUpload") and is
  dropped (`audit_replay_dropped`). That is a RustFS bug.
- **Refusals** are decided in RustFS's `DenialContext::deny` (`rustfs/src/storage/access.rs`),
  which only writes the `s3_authorization_denied` log line. That line has the account,
  bucket, object, action and reason. It's a gap nobody designed, and RustFS owns the hook.
- **Bad signatures** are rejected inside s3s `prepare`, before RustFS sees the request. That
  needs a response-status layer or an s3s change.
- **MinIO**, which RustFS mirrors, audits UploadPart, Abort and middleware rejections, so
  RustFS diverges from it here.

Not tested: bucket notifications (not configured in the spike), and erasure coding and
replication (single drive).

## S1 status (2026-11-07)

Built: name-to-bucket mapping, grants (recorded on the chain, pushed into the backend as access
keys and IAM policies), the exposure per store (`store.<bucket>`, ADR-120), governed
get/put/head/delete/list with every request and refusal in the exposure activity log, parsers for
RustFS audit, `s3_authorization_denied` log lines and bucket notifications, promote, archive, and
`weaver store ls|create|grant|revoke|promote|archive|put|get`. The daemon creates its node store at
start. Tests: `weftos-store` (32) and the `store` tests in `clawft-weave`.

**RustFS transport (2026-11-07, owner approved the HTTP client and the `weftos-store` dependency):**
`RustfsBackend` (feature `rustfs`, `crates/weftos-store/src/rustfs.rs`, `sigv4.rs`) implements both
traits over blocking HTTP with SigV4 signed by the root credential: the S3 data plane (put with
`If-None-Match`, get, head, delete, copy, list v2 with paging, buckets and tags) and the admin API
(`/rustfs/admin/v3`: add-user, set-user-status, add-canned-policy, set-user-or-group-policy,
remove-user, remove-canned-policy). The daemon selects it when `WEFTOS_STORE_RUSTFS_ENV` names an
env file (`MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY`, optional `MESH_S3_REGION`);
each principal's key id is `wk-<16 hex of blake3(principal)>` and its secret is kept 0600 under
`<runtime dir>/store-secrets/`. It is called from blocking contexts only (the weave verbs already
use `spawn_blocking`; start-up runs it on a plain thread).

Verified against a throwaway RustFS 1.0.4 on loopback (not PG), by `cargo test -p weftos-store
--features rustfs -- --ignored` with `WEFTOS_STORE_TEST_RUSTFS_ENV` set:
- object and bucket operations, paging past 1000 keys, tags, a 412 on a conditional put;
- a scoped key is refused outside its bucket, and **a disabled key's next request is a 403**;
  re-creating a key keeps its secret and its disabled state; no grants becomes a deny-all policy
  (RustFS rejects a policy with no statements: HTTP 500);
- the S1 done-when end to end: node and project stores, a cog's write in the activity log, and
  disabling the grant refuses the next request at the service and at RustFS.

Findings that shaped the code: creating a bucket the root already owns returns 200 (so
`create_bucket` checks first); a bucket name needs 3 or more characters; headers such as
`If-None-Match` must be signed; `ObjectMeta.etag` is the backend's etag (MD5 on RustFS, BLAKE3 on
the local backends).

Audit parsers: the `s3_authorization_denied` line was captured from RustFS 1.0.4 and the parser fixed
to match (`action` is `S3Action(GetObjectAction)`, `object` can be `"None"`). **Audit stream and notifications verified (2026-11-08)** against a throwaway loopback RustFS 1.0.4, and the
parsers match the captured records. The earlier "no record" runs failed because RustFS rejects loopback webhook endpoints unless
`RUSTFS_OUTBOUND_ALLOW_ORIGINS=http://127.0.0.1:<port>` is set at start; targets must be named
(`RUSTFS_AUDIT_WEBHOOK_ENABLE_<NAME>`, `..._ENDPOINT_<NAME>`, and the same for `NOTIFY`), and `RUSTFS_AUDIT_ENABLE=true` is
still needed. The admin `set-config-kv` call silently drops `enable` and `endpoint` for audit targets, so it is not a way to
configure them. Notifications additionally need a `PutBucketNotification` pointing at `arn:rustfs:sqs::<name>:webhook`. An
audit post is `{"EventName","Key","Records":[{"event","trigger","api":{"name":"s3:PutObject","bucket","object","status_code",...},"accessKey",...}]}`
with a plain object key and no size. A notification post is a standard S3 event record, also with `EventName` and `Key`, whose object key is
percent-encoded. `parse_audit` was wrong before the capture (it returned `ObjectCreated:Put`, not `PutObject`) and now reads
`Records[0].api`; `parse_notification` decodes the key. Not yet verified: batched posts (`batch_size` above 1), multipart and
versioned operations, and `auth_token`. **Not yet done:** the daemon does not launch or configure RustFS (it is an external
service), so setting those env names and issuing `PutBucketNotification` belongs to whoever deploys RustFS (Phase A):
`docs/guides/rustfs-audit-deploy.md` is the runbook and `scripts/store/rustfs-audit-plan.sh` a dry run that prints it
(`RustfsBackend::put_bucket_notification` is the call). **Conditional write (for S2).** `ObjectBackend::put_if_match(bucket, key, data, etag)` is a compare-and-swap (S3 `If-Match`): a
changed key is `BackendError::Stale`, a missing key is `NoObject`. The service has `StoreService::put_if_match(who, store,
key, data, etag)` (gated and logged like `put`), `svc.store` takes `"if_match": "<etag>"` on `put` (`store.v1`, not with
`if_none_match`), and RustFS gets `If-Match`. Take the etag from the last `head` or `put` result of the same backend
(opaque: MD5 on RustFS, BLAKE3 locally), and on `Stale` read again and retry. The local backend serialises swaps within the
daemon process; across processes only RustFS is atomic. S2 should use this instead of unconditional `put` for any
read-modify-write.

**CI and sweeper roles (release stores).** Two narrow access levels exist besides read, write and admin: `append` (get, head,
list, and a put that must carry `if_none_match`, so nothing is overwritten; no delete or copy) and `sweep` (get, head, list and
delete; no put). A new release store gets the well-known principals `agent:ci` as `append` and `agent:store-sweeper` as
`sweep` by default (`weaver store grant <name> <principal> append|sweep`). Proven live on a throwaway RustFS 1.0.4: the `append` key can read and list, and is refused put, copy and
delete when used straight against RustFS; the `sweep` key reads, lists and deletes and is refused put. RustFS IAM cannot state
write-once (it rejects the `s3:if-none-match` condition key, and a plain PutObject allow lets a key overwrite or copy over an
object), so CI's writes go through the daemon, which makes them `If-None-Match: *` with its own key and refuses an overwrite,
copy or delete for an `append` principal. A CI job therefore writes artifacts through `store.put`/`svc.store` or a daemon-side
client, not with an S3 SDK and its own key. Immutability on the backend itself and retention are S4's. Readers of a release store (the project's
members) are named when the store is made.

**Object Lock (for S4), proven live on a throwaway RustFS 1.0.4 (2026-11-08).** `AdminBackend::create_locked_bucket` makes a bucket with
`x-amz-bucket-object-lock-enabled` (versioned; chosen only at creation; the local backends answer `Unsupported`).
`ObjectBackend::put_locked(bucket, key, data, &PutOpts{if_none_match, retention: Some(Retention{mode: Governance|Compliance, until_unix_s}), legal_hold})`
sends the retention and legal-hold headers and returns the written `version_id`; `delete_version(bucket, key, version_id, bypass_governance)`
and `put_legal_hold(.., on)` act on one version. Seen on RustFS: a bucket made without lock refuses lock headers; a locked
version's delete is refused with `AccessDenied` under COMPLIANCE (bypass does not help), under GOVERNANCE (without bypass), and
under a legal hold until it is lifted; a past retain-until date is refused before sending. At the service: `create_locked_store(name,
actor, parties)`, `is_locked(name)` and `put_locked(who, store, key, data, &opts)` (gated and logged like `put`; an `append` principal must
set `if_none_match`; a store not made locked refuses it). **Listing versions (for S4's replaced-manifest check, live 2026-11-08):**
`ObjectBackend::list_versions(bucket, prefix)` and `StoreService::list_versions(who, store, prefix)` (needs read, logged as a list)
return every version and delete marker as `ObjectVersion{key, version_id, is_latest, is_delete_marker, size, etag, last_modified}`, sorted
by key and newest first. A manifest put once with `if_none_match` shows one entry; a later plain put under the same key shows a
second, newer entry with a different `version_id` and `etag` (the first keeps the etag its put returned), so S4 compares the
newest entry to what it recorded. Local backends answer `Unsupported`. Paging was checked by forcing a page of 7 over 30 keys.
Read keys now carry `s3:ListBucketVersions`, and a Read key listed versions at RustFS. A plain delete (no version id) in a locked bucket,
as the sweeper does, adds a delete marker: the listing shows the marker newest and the retained version beneath it, and a get answers
not found. **Not covered:** default bucket retention (`PutObjectLockConfiguration`) and `PutObjectRetention` on an existing version are not
implemented; `archive(delete)` of a locked store will fail on its retained versions; the key policy for `sweep` still allows only
`s3:DeleteObject`, not `s3:DeleteObjectVersion`/`s3:BypassGovernanceRetention`, which is what you want for retained versions.

**Owner decisions on the release roles (2026-11-08). Each is ready as a switch, tested both ways; the defaults are the safe choice.**

| Decision | Switch | Off (default) | On | Cost of turning it on |
|---|---|---|---|---|
| Sweeper may put | `WEFTOS_STORE_SWEEPER_PUT=1` (`PolicyOpts.sweeper_put`) | sweeper key: read, list, `s3:DeleteObject` | adds `s3:PutObject` and multipart actions | the key can overwrite or copy over any artifact; in a locked store the overwrite adds a version and the retained one stays (live: 2 versions, delete of the original still refused) |
| CI uploads straight to RustFS | `WEFTOS_STORE_CI_DIRECT_PUT=1` (`PolicyOpts.ci_direct_put`) | CI key: read only; uploads go through the daemon (`store.put` / `svc.store`), which makes them write-once | adds `s3:PutObject` to the `append` key | RustFS IAM cannot express write-once (it rejects `s3:if-none-match`), so the key can overwrite and copy as well as create; delete stays refused; again only a locked store keeps the original |
| Receiver diff-gate approvals | none; owner appends `docs/guides/rustfs-store-ingest-approvals.draft.toml` to `config/diff-gate-approvals.toml` | gate fails on `net` (listener, test sockets) and `process` (live test spawns RustFS) | all store_ingest findings become "approved" (checked on a scratch copy) | the entries are narrow (this file, these patterns), no other finding changes |

The switches widen only the direct S3 key; the service gate is unchanged (a sweeper `put` through the service is still refused, and an
`append` put still has to be write-once). Set the env var in the daemon's environment, restart, and the daemon pushes the new policy to
existing keys at start (`reconcile`); `StoreService::set_policy_opts` then `reconcile()` does the same at run time. Tested: policy unit
test (off, each switch alone, back off), service test (backend key policy after reconcile, both ways), and a live test on a throwaway
RustFS 1.0.4 that flips both switches over a locked release store. Recommendation: leave both off. A locked release store with CI
uploading through the daemon gives write-once, retention and no overwrite path; each switch trades that for convenience.

**Transport hardening (2026-11-08).** `BackendError::Unavailable` (with `is_transient()`) now means the backend, not the request, was the
problem: no connection, or HTTP 408/429/500/502/503/504 after retries. 401 maps to `Forbidden`; a 400 stays `Io` and is not transient.
`RustfsBackend` retries 3 tries in all with waits of 100 ms then 400 ms (`Retry{attempts, base}`, `with_retry`; waits grow fourfold, cap 5 s).
It repeats only what is safe: a connection that never opened (any request), a 503/429/408 (the server says it did not act), and for
reads and plain deletes also 500/502/504 or a dropped connection. A timed-out or dropped write is not repeated, nor a delete of a
named version, since a repeat could add a second version in a locked store or turn a conditional create into a false "exists".
Tested without sockets (decision table, wait schedule, unreachable port, status-to-error map). Not done: no circuit breaker, so a
dead RustFS costs 3 quick tries per call; a timeout still holds a caller for the full 30 s.

**S3/S4 pile (2026-11-08), one commit per slice, each with unit tests and a live test on a throwaway loopback RustFS 1.0.4.**

| # | What | API | Seen on RustFS |
|---|---|---|---|
| 1 | Read back Object Lock config | `ObjectBackend::get_lock_config(bucket) -> LockConfig{enabled, default}`, `put_default_retention`, `StoreService::lock_config(store)` | plain bucket: `enabled:false` (404 `ObjectLockConfigurationNotFoundError`); locked: `enabled:true`; a default retention reads back and a put with no headers inherits it |
| 2 | Lifecycle rules | `AdminBackend::put_lifecycle(bucket, &[LifecycleRule])` (empty list removes), `get_lifecycle`, `StoreService::set_lifecycle(store, rules, actor)` (chain `store.lifecycle`) / `lifecycle(store)`; rule = id, prefix, enabled, `expire_days`, `noncurrent_days`, `abort_multipart_days` | written and read back identically, no Content-MD5 needed; Object Lock still wins over an expiry |
| 3 | Extend retention after commit | `get_retention`, `put_retention`, `extend_retention(bucket, key, version, until, mode)` (only lengthens, keeps the current mode), `StoreService::extend_retention(who, ..)` (the `append` role may call it: it only adds protection) | an object put with no hold is stamped COMPLIANCE, lengthened, read back; RustFS refuses to shorten it even with bypass |
| 4 | Multipart upload with lock headers | `create_multipart`/`upload_part`/`complete_multipart`/`abort_multipart`, `put_stream(bucket, key, reader, part_size, &PutOpts)` (8 MiB parts, one part in memory), `StoreService::put_stream(who, store, key, reader, &opts)` | 11 MiB in three parts, retention applied to the finished object, bytes identical, `If-None-Match` enforced at complete, abort leaves nothing; the 512 MiB whole-body limit no longer applies; local backends still read it whole and refuse lock options |
| 5 | Retire a node on unpair | `StoreService::retire_node_store(node_store, actor)` and RPC `store.node.retire`: archives the store, records the id as retired (persists), `create_store`/`ensure_store` refuse it | data lands in the archive bucket, grants off, id refused |
| 7 | Dead RustFS | `Breaker{threshold 5, cooldown 10 s}` (`with_breaker`, `breaker_open`), `with_timeout(d)` view sharing the breaker | after 5 failed-to-connect requests in a row calls fail at once with `Unavailable("circuit open")` for the cooldown, then one probe; any HTTP answer (403, 404, 500) closes it |

Slice 6 (S3's ask) is in `clawft-kernel`, not the store: the exposure activity log (`ActivityLog`) did `next_seq + 1` unchecked, a panic in
debug and a wrap to 0 in release at `u64::MAX`, and `open()` did `last.seq + 1` on a file, so a log whose last record was `u64::MAX` could not be opened. Now `u64::MAX` is a
usable last number; after it, `append` keeps nothing, returns the record with `seq` `u64::MAX` and an empty `hash`, counts it in `dropped()`
and logs once; `is_exhausted()` says so; `open()` of such a file opens exhausted; and `verify` rejects a sequence that repeats or steps back.
Reaching it takes 1.8e19 appends, so this is a guard against a damaged or hostile file, not an expected state.

Notes: `put_stream` through the service logs the real size. A multipart part PUT is retried (same part number replaces the part); nothing
else that writes is. The breaker counts only "could not reach RustFS" (no connection, timeout, 502-504), not a 500 or 429.

**Follower wake hook in the receiver (S2's ask, 2026-11-08).** `store_ingest::WakeSink::notified(store, bucket, body)` is called after a bucket
notification has been logged, once per store of this daemon whose bucket the body names (other buckets, the audit stream, bad bodies and wrong
tokens never reach it; a panic in the sink is caught and does not change the answer to RustFS). Pass one with `serve_with` /
`start_from_env_with`; the daemon passes none yet. `weftos_store::wake::registry_hints` lives on S2's branch, not here, so the sink is the
seam: after the two branches merge, S2's implementation is `fn notified(&self, _store, bucket, body) { let h = registry_hints(body, bucket);
if !h.is_empty() { /* publish a wake on the project's relay topic */ } }`. The relay hop itself is the mesh transport card and is not started.

**Grant event sequence: where it is stamped and what bound applies (decision for the chain owner to confirm, 2026-11-08).** S3's review found
that the `seq = u64::MAX` freeze was not closed: my earlier guard covered the exposure activity log, a different sequence, and the ledger that
S3 feeds from the chain (`GrantLedger::apply` ignores any event with `seq <=` the last applied) would stop applying all later events,
revocations included, after one event claiming a huge `seq`. The decision, in `weftos_store::grant_seq` and `StoreService`:

1. **Stamped by `StoreService`, the one writer of grant changes**, not by the chain feed and not by the consumer. A counter `grant_seq` is
   part of the saved state and goes up by exactly one for each event that changes what a principal may do: `store.grant`, `store.revoke`,
   `store.grant.enable`, `store.grant.disable`, `store.principal.suspend`, `store.principal.resume`, `store.close` (all grants of a store
   went off at the start of an archive), `store.archive`, `store.node.retired`. It starts at 1, is gapless, is saved before the event is
   recorded, and is never reused after a restart.
2. **Stamped under the same lock as the change, and recorded to the chain before that lock is released**, so the order of the numbers is the
   order the changes were made and the order the chain sees them (two threads cannot record 6 before 5). The `ChainSink` therefore must be quick
   and must not call back into the service. A test makes 200 grants from 8 threads and requires the chain to see 1..=200 in order; moving the record out of
   the lock fails it.
3. **Upper bound on issuing: `CEILING = 2^62`.** At the ceiling every grant-affecting call is refused with a clear error before it changes anything; a state
   file that claims a `grant_seq` above it is refused at open as damaged.
4. **Bound on accepting, for every consumer: `grant_seq::admit(last, seq)`** is `last < seq <= last + MAX_GAP` and `seq <= CEILING`, with
   `MAX_GAP = 1_000_000`. A forged or corrupt `seq` far ahead is refused without moving `last`, so the next honest event (`last + 1`) still applies.
   The gap is allowed because a consumer may legitimately miss events (a feed filtered to its own principals, or an outage it repairs); 10^6
   is far above any real burst and far below the ceiling. The bound limits the harm of a forgery that slips under it to at most `MAX_GAP` honest events being treated as stale, not a permanent freeze. A consumer that is refused must resync from a snapshot, not widen the bound.
   For S3: `MaxSeqJump(MAX_GAP)` is the same rule except for the ceiling, so install `set_seq_guard` with a guard that calls `admit`, by default,
   and again after every `restore`. S3's edge-creds crate does not depend on `weftos-store`, so the two numbers may be copied; they are public constants.

What is NOT closed here: S1 stamps and bounds; whoever builds the chain feed must carry `seq` through (the field is in the chain payload now),
and the ledger must install the guard. Events recorded before this change have no `seq`. Reasoning for the chain owner to confirm: a bound only
at the consumer is not enough (a forged value just under it moves `last` far ahead and the ledger then accepts nothing honest until it passes that
value), so the stamp must come from one trusted writer and be gapless, and the consumer's bound is then a check that the writer's promise holds.

**S3 review of the S1 commits, fixes (2026-11-08).** HIGH 1 (retire after a cut-short archive reported success): `archive` now records when it
started and whether it finished (`archive_ts`, `archive_done`, `archive_count` on the store record), so a retry resumes under the same archive keys, and
`retire_node_store` returns `Ok` only when every object is copied, with the real count on a repeat. HIGH 2: see the grant event sequence above.
Also fixed from the same review: the breaker no longer counts a timeout from a short `with_timeout` view (the caller's impatience is not news
about RustFS), frees a probe slot after a local fault without closing the breaker, and gives up on a probe that never reports back after one cooldown;
the retired-id check in `create_store` is repeated under the lock that records the store (a retire landing in between wins, and the bucket just made is
removed); an `append` principal's `extend_retention` is capped at `APPEND_MAX_RETENTION_S` (5 years), since a Compliance hold cannot be undone by
anyone (write and admin are not capped; the cap is a policy number for Mathew to change); retain-until dates past year 9999 are refused before sending and `parse_iso`
rejects years outside 1970..=9999 and impossible days (Feb 31); `put_stream` on a backend without multipart refuses over 512 MiB and clamps `part_size` to 5 MiB..1 GiB.
Not changed: a failed `abort_multipart` after a failed upload is still silent (the upload is orphaned until the `abort_multipart_days`
lifecycle rule removes it, so set that rule on stores that stream large objects); the review's default-build note stands (`--features rustfs` is needed to
compile and test `rustfs.rs`).

**Archive retention (S1, for node, project and release stores; 2026-11-08).** Worktree archives are S2's (`store worktree gc --archive-keep-days`). For the rest,
`StoreService::archive_gc(&ArchiveKeep{node_days, project_days, release_days}, confirmed, actor)`, RPC `store.archive.gc`, CLI `weaver store archive-gc
[--node-days N] [--project-days N] [--release-days N] [--confirmed]`:
- **Nothing expires unless a number of days is given for that kind**: an archive is recovery data, as with S2's default. Zero days is refused (it
  would delete an archive the moment it was made).
- **A dry run unless `confirmed`**; the report lists each expired set (bucket, kind, age in days, objects, bytes), what was kept, and what was left alone.
- **Whole sets only.** The archive bucket keys are `<bucket>@<archive ms>/<key>`; a set is one archive of one store. A set is deleted when its age is at least
  the days for its kind, never partly, and a half-deleted set is finished by the next run. Worktree sets (`w-`), unknown prefixes and keys that are not archive keys are
  never touched (counted as `not_ours`).
- **An archive that is still being written is never deleted**, however old: a store record with `archived` set and `archive_done` false protects the set
  whose start time it holds (found by a mutation test).
- A deleting run records `store.archive.gc` on the chain (not a grant event, no `seq`). The store record, and the live bucket of a store archived but not deleted,
  are not the archive's to remove and are untouched.
Not done: no schedule (an operator or a timer runs it), and no per-store override. The one question left for Mathew is the numbers; none is set.

**Archive retention, scheduled and per store (S1, 2026-11-08).** Both are off and carry no day numbers; the numbers stay the owner's call.
- **Per-store override.** `set_archive_keep(store, Some(Days(n)) | Some(Forever) | None)`, RPC `store.archive.keep {name, days? | forever?}` (neither clears it), CLI
  `weaver store archive-keep <name> [--days N | --forever]`. It beats the kind's limit in both directions (shorter, longer, or exempt) and works when the kind has no limit.
  Zero days, an unknown store and worktree stores (S2's) are refused. It lives on the store's record (saved with the rest) and is a chain event, `store.archive.keep`.
- **Schedule.** Off unless `WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS` is set (at least 1). The days come only from `WEFTOS_STORE_ARCHIVE_{NODE,PROJECT,RELEASE}_DAYS` (a kind left out
  keeps for ever). Unless `WEFTOS_STORE_ARCHIVE_GC_CONFIRM` is `1`/`true`/`on`, a scheduled run only reports (to the daemon log). The first run is one interval after start
  (the last-run time is in memory); a failed run is retried at the next interval, not in a loop; a due run with no day numbers and no override does nothing, not even a listing.
  A bad value is logged and leaves the schedule off, it does not stop the daemon. Scheduled deletes are chain events with `actor: "schedule"`.

**Owner decision, 2026-11-08 11:21 ET: nothing expires yet.** Archive retention stays at its default (no expiry). The schedule stays off, no day numbers are set anywhere, and nothing is scheduled to delete archives; `archive_gc` and the per-store override exist for when the owner sets numbers.

**The daemon now has the receiver** (`clawft-weave/src/store_ingest.rs`, 2026-11-08): a loopback HTTP listener, off unless
`WEFTOS_STORE_INGEST_ADDR` is set, POST only, 1 MiB cap, authenticated by a 256-bit secret in the URL path
(`store-ingest-token`, 0600; the URLs are written to `store-ingest-urls`), because RustFS's webhook `auth_token` is unverified.
It feeds `ingest_audit_line` and `ingest_notification`, which now drop records for buckets that are not the daemon's stores and
the daemon's own root-key requests (already logged on the governed path), so a record reaches the activity log only if it came from a
principal's own key. Notifications still invalidate caches whoever wrote. Verified end to end against a throwaway
RustFS (`live_a_request_straight_to_rustfs_reaches_the_activity_log`, `--ignored`, `WEFTOS_STORE_TEST_RUSTFS_BIN`): a put made
straight to RustFS with a cog's key shows as that cog's `PutObject` (audit) and a notification event. Not wired yet: attaching
`PutBucketNotification` to buckets automatically when a store is created (the runbook does it by hand). Reading `rustfs.log` for
refusals is wired (see the close-out findings below).

**Adversarial review of S1 (2026-11-08), fixed:** a grant change saved but not pushed (RustFS down, a crash between the two)
is now made good by `StoreService::reconcile()` at daemon start, and backend pushes are serialised per service so an older snapshot
cannot undo a newer policy; a bucket made but never recorded is adopted when its tags are the store's own (a foreign bucket still
refuses); an archived store's grant cannot be switched back on; `svc.store get` checks the size before reading the object; the
cog service startup no longer lists objects on the async runtime; `weaver store put|get --as` now leaves a `store.as` chain
record naming the operator. **Known and not fixed:** `archive` can lose an object put by a request that passed the gate just
before it; listings and `FsBackend` etags read whole files (the local backend is the development path); a mesh-id change
renames the stores' `weftos://` names (the bucket names do not move; the migration is F17 below); `project add`'s store hook
joins a blocking thread; (the receiver now authenticates and drops unknown buckets; see above).

**Close-out findings F2, F9, F15, F16, F17, F18 (2026-11-08, owner order of 15:00), fixed on `feat/adr126-s1`:**
- **F2** Every grant change (grant, revoke, enable, disable, suspend, resume, archive's close) runs under one lock with its
  backend push. The chain event and activity lines are recorded only after the push succeeded; a change whose push failed
  stays saved (the service enforces it at once) with its events pending, and doing the same change again or `reconcile`
  (at start, and covering suspended and pending principals) finishes it and records it once. A repeated revoke no longer
  answers "holds no grant" while its push is outstanding. `ensure_store` on an existing store re-pushes its principals.
- **F9** The audit and notification receivers (path-secret authenticated, `WEFTOS_STORE_INGEST_ADDR`) were already called by
  the daemon; new: the daemon follows RustFS's own log (`WEFTOS_STORE_RUSTFS_LOG`) into `ingest_log_line`, which now reads both
  `s3_authorization_denied` and `SignatureDoesNotMatch` / `InvalidAccessKeyId` (the latter carry no account or bucket in
  RustFS 1.0.4, so they are logged as an unattributed `Authenticate` refusal and cannot suspend anyone). A principal whose own
  key is refused 20 times in 60 s (`WEFTOS_STORE_DENIAL_RULE=off|<max>/<s>`) is suspended through `suspend_principal`.
  Field names: the denial and signature lines are real RustFS 1.0.4 lines; the audit and notification bodies are exercised by
  the live test against a real RustFS (`live_a_request_straight_to_rustfs_reaches_the_activity_log`). Not validated: a
  production (photo-gallery) log, which was not read.
- **F15** `RustfsConfig` `Debug` redacts both root keys; the secrets directory is 0700 (a loose one of ours is tightened, another
  user's is refused); the secret's temp file is `create_new` 0600; plain `http://` is accepted only to a loopback host.
- **F16** SigV4 is checked against four AWS-documented canonical requests and signatures; every bucket string goes through
  `check_bucket`; a CopyObject `200` with an `<Error>` body is an error and the body must be a `CopyObjectResult` that agrees with the
  destination. Integrity: puts send `x-amz-checksum-sha256` (RustFS refuses a wrong one); a read is checked against Content-Length and
  the stored SHA-256, else the single-part ETag (MD5), else reported `Unverified` (multipart ETags prove nothing about content).
- **F17** The bucket is the store's identity. A second name for one bucket is refused; `ensure_store` and the daemon at start move
  a `local` store to its mesh-id name (record, grants, retired mark, pending events, exposure) with one stamped `store.migrate`
  event. Node store tokens must be the clean id (`pg.1` and `pg-1` no longer share `n-pg-1`). This is not the `n-<id>` to `node-<id>`
  bucket rename, which stays an open owner decision.
- **F18** `store.promote` refuses an empty, unclean or reserved (`.wt/`, `worktrees/`) prefix, never copies a reserved-area object,
  and refuses a worktree that holds overlay state under `.wt/`, so S2's landing path and its conflict check cannot be bypassed.

**S1 done-when rerun at the tip after F2/F9/F15-F18 (2026-11-08, `1153af0a`).** Against a throwaway
RustFS 1.0.4 container (`docker.io/rustfs/rustfs:1.0.4`, podman, loopback `127.0.0.1` only, tmpfs data, root
key made for the run; not the PG or photo-gallery RustFS). Commands: `cargo test -p weftos-store --features
rustfs --lib -- --ignored --test-threads=1` and `cargo test -p clawft-weave --features weftos-store/rustfs
--lib live_ -- --ignored --test-threads=1`, with `WEFTOS_STORE_TEST_RUSTFS_ENV` naming the throwaway env file.
- `weftos-store`: 16 of 16 live tests pass in 27.05 s (34 s wall including the test-binary build).
  This includes `live_service_done_when` (node and project stores; a disabled grant refused at the service and
  at RustFS), the scoped-then-disabled key (a disabled key's next request is a 403), `put_if_match`, the
  release roles, Object Lock, versions, multipart, and the F16 checksum and CopyObject check.
- `clawft-weave`: `live_rustfs_backs_the_daemon_service` (node store through the daemon service) and
  `live_a_cog_uses_svc_store_on_rustfs_and_a_disabled_grant_is_refused` (a cog's write appears in the
  activity log; disabling its grant refuses the next request at the service and at RustFS) pass, 0.81 s for both
  (72 s wall, mostly compiling).
- Not part of this run: `live_a_request_straight_to_rustfs_reaches_the_activity_log` needs a local `rustfs`
  binary (`WEFTOS_STORE_TEST_RUSTFS_BIN`), not a container, so it was not run here (it passed earlier on
  the binary); `conv_postmortem::live_postmortem_distills_durable_fact` needs Hermes :8090 and is not S1; it
  failed because that endpoint is not available.

**Touch point with the Mac-side S3/drive work.** The additive `put_if_match` / `delete_if_match` default
methods on the backend trait (`wt/adr126-s3-drive`, `2c43a95c5..1cade20f8`) touch S1's `backend.rs`.
`ObjectBackend::put_if_match` already exists here as a *required* method (`backend.rs`, implemented by the
RustFS, local-directory and in-memory backends, and by the two test backends in `backend_tests.rs` and the
`Flaky` backend in `service_tests.rs`); there is no `delete_if_match`. Signature identical (confirmed against
`1cade20f8`, per the owner's check of `backend.rs` in the Mac repo). At merge: one declaration of
`put_if_match` (S1's, required), S1's implementations stand, and the `delete_if_match` default comes in
unchanged. One detail to settle at merge: the Mac defaults return `BackendError::Unsupported` bare, while
S1's `BackendError::Unsupported` carries a message (`Unsupported(String)`), so the `delete_if_match` default
needs `Unsupported("conditional delete".into())` here.

## S2 status (2026-11-07, branch `feat/adr126-s2`, not landed)

- **Where the code is:** `crates/weftos-store/src/{overlay,promote,share,share_fetch,follower,testing,worktree_backend}.rs`
  and `tests/s2_{worktree,follower,done_when}.rs`. The Rust uses only `std`, `serde`, `serde_json` and
  `sha2` and spawns no processes. `worktree_backend.rs` implements `overlay::ObjectBackend` for every
  S1 `ObjectBackend` (local directory, RustFS, in-memory), so the S2 code runs unchanged on any of
  them; S1's backends report no modification time, which S2 no longer needs. Git work and the
  desktop mount are in `scripts/worktree-store.sh` (tests: `scripts/tests/worktree-store.test.sh`,
  `scripts/tests/worktree-union-mount.test.sh`). `sha2` is now a non-optional dependency of the crate.
- **Merged with S1** (`feat/adr126-s1`, 2026-11-07): the modules are wired into `lib.rs`.
- **The `weaver store worktree` verbs exist** (`crates/clawft-weave/src/store_worktree.rs`, CLI in
  `commands/store_cmd.rs`, RPC `store.worktree.*`, Admin like every `store.*` verb): `create`,
  `buckets`, `publish` (`--paths-z`, `--uncommitted-z`, `--changes-z`, `--bundle`, `--handoff`),
  `fetch`, `claim`, `promote`, `follow`, `search`, `leftovers`, `sync-docs`, `overlaps`. They run on
  the daemon's raw data plane (`StoreService::operator_objects`, added for this, as is the now-public
  `ensure_archive`), the same level as S1's own promote and archive, so they skip per-principal
  grants. `create`, `publish`, `claim` and `promote` are recorded in the activity log and on the
  chain, failures included. The daemon refreshes the project registry entry after publish and
  claim, which is the only writer of `worktrees/<slug>.json`. Paths in params (`--root`, `--dest`,
  `--cache`, the lists) are read and written by the daemon itself, so the CLI and daemon must share
  a host and a user. Promotion archives into `weftos-archive` under `<bucket>@<ms>/`. Two S1 files
  got small additive edits for this (`service.rs`, `service/lifecycle.rs`).
- **Verified (in `weftos-store` and `clawft-weave`, clippy `-D warnings` clean; daemon test `store_worktree::tests` drives every verb from create through claim and land on an `FsBackend`):** 56 S2
  integration tests plus `tests/s2_done_when.rs`, which runs the three done-when checks on S1's
  `FsBackend` with two machines sharing only the store: publish, hand off, follow, search, fetch,
  claim and merge across machines (a claimed merge cannot be overwritten by the writer); a new
  worktree reads project assets with nothing copied into its bucket; landing promotes once and
  archives the unmerged docs. Shell: 37 assertions including a real `git bundle` round trip, and 8
  more in `worktree-union-mount.test.sh`, which does a real `rclone mount` (FUSE, rclone 1.75.2)
  over the local-directory store layout: project files read through with no copy, new and edited
  files land only in the worktree bucket, `.wt/` is hidden, deleting a project-only file is
  refused, the project bucket is untouched. **Not verified:** a real RustFS (the same scenario is
  `two_machines_share_a_worktree_on_a_real_rustfs`, ignored until `WEFTOS_STORE_TEST_RUSTFS_ENV`
  names a throwaway server), the mount over an S3 remote, macOS or Windows, two physical machines,
  or `agent-git-flow` calling the script (it has no hook seam; the repo wraps `wt.sh` and `land.sh`).
- **Known limits:** claims are advisory; claim expiry compares the claimer's stamp with the
  evaluating daemon's clock (§4); `rclone union` cannot
  express tombstones, so deleting a project-only file through the mount is refused; rclone does not
  flush its write cache on unmount, so `worktree-store.sh umount` waits for the write-back delay;
  doc search matches path and title only, not content.

- **After the adversarial review (2026-11-08):** the findings fixed on `wt/adr126-s2-mac` are
  described in §4 and §4a (path rules, control state, slug and bucket checks, ceilings, claim
  host and skew, audit, digest). Evidence and per-finding tests are in
  `docs/research/adr126-s2-evidence-macos-2026-11-08.md`.
- **Still open:**
  - F2, grant and revoke locking, and F9, RustFS audit ingestion. Both are in the S1 layer and
    belong to the S1 session.
  - F7, F13, F14, F19 and F20 are fixed in the S2 layer (a land closes the handoff as merged
    whatever the claim state; a wider secret denylist; publish reads one open handle checked
    against the path; fetched manifests cannot name the follower's control files; `FsBackend`
    refuses keys that differ only by case). Still open in the S1 layer, for the S1 session: F15
    (`RustfsConfig` derives `Debug` with the root key; secrets dir mode), F16 (no SigV4 known-vector
    test; bucket strings unvalidated), F17 (the bucket name omits the mesh id, so `local` and
    `<hex>` names alias one bucket), F18 (S1 `store.promote` accepts `""`, `.wt/` and `worktrees/`
    prefixes). See `docs/research/adr126-s1-s2-adversarial-review-2026-11-08.md`.
  - `slug_of` in `worktree-store.sh` is now injective (`/` becomes `--`; branches containing `--`, `-/` or `/-` are refused and `WTS_SLUG` overrides). Existing stores created under the old slashed-branch slugs (`feat/s2` was `feat-s2`, now `feat--s2`) are not found by the script until renamed or re-created; none are known to exist outside drills.
  - The owner rejected the rclone mount. The union mount (`mount-config`, `mount`, `umount`) is
    proposed to be parked: kept in the code, dropped from the S2 done-when and from any gate, and
    marked parked in the script help and §4 (proposal in §1 of
    `docs/plans/mesh-store-next-steps-2026-11-08.md`, awaiting the owner). The desktop access
    path is the materialise-and-watch route in ADR-131.
## S3 results (2026-11-07)

S3 is built in `crates/weftos-edge-creds`, separate from S1's `weftos-store`. It does no I/O and
reads no clock, so the host supplies the parent link, the HTTP client and the time. It depends on
S1 only through two traits, `StsMinter` (RustFS AssumeRole and user disable/enable, card b76c74a7)
and `GrantCheck` (the chain-recorded grant).

- **Parent side (`EdgeBroker`).** For a node the link has authenticated, it checks the revoked set,
  a per-node rate limit (30 per minute) and the grant. It derives the bucket `n-<node id>` from the
  node id, never from the request. It then asks RustFS for an STS session as the node's own RustFS
  user, with an inline policy naming only that bucket. The user's long-lived key stays on the
  parent. For a presigned URL it mints a session scoped to one object and one action, and signs
  with that, so the edge can hold no credentials at all.
- **Edge side (`EdgeNodeStore`).** It is built with no key. It signs with SigV4 and the session
  token, refreshes inside the last 60 s, and retries once with a fresh lease on a 403. It refuses
  any credential with no session token or a lifetime above 15 minutes plus 60 s of skew, so a
  parent that leaked a long-lived key is rejected.
- **Revocation.** `revoke` records the node, then disables its RustFS user. A live session is
  refused at its next request (S0: about 0.22 s), and the parent refuses a new lease.

Verified against an in-memory store that checks SigV4, the session token, expiry, the user's
enabled flag and the session policy (`cargo test -p weftos-edge-creds`, 21 tests):

- the SigV4 header and presign code reproduces AWS's two published S3 example signatures;
- a Wasm-style node writes, reads, conditionally creates and deletes in its own bucket, and the
  store only ever saw STS access keys;
- a session aimed at another node's bucket is refused, and a presigned URL is bound to its key,
  bucket and host;
- expiry is refreshed transparently; a node revoked mid-session is refused on its next request;
- planted defects (revoke without disabling the user, a wildcard session policy, a removed TTL
  cap, skipped key validation) each fail a test.

**Live minter and grants (card b76c74a7).** `RustfsMinter` implements `StsMinter`:

- **Mint:** a signed (SigV4, service `sts`) `AssumeRole` form POST using the node user's own
  long-lived key, with the inline session policy and `DurationSeconds` fixed at 900. STS
  intersects the session policy with the user's attached policy, so a session never exceeds its
  user. The 2048-byte policy limit is checked before anything is sent.
- **Disable and enable:** an admin-signed `PUT <prefix>/set-user-status`. The prefix defaults to
  `/rustfs/admin/v3` and is configurable.
- **Pairing and unpairing:** `provision_node` creates the node's policy, user, policy attachment and
  bucket; `deprovision_node` removes the user and policy.
- **Keys:** the per-node user keys sit behind `NodeKeys` on the parent, are redacted in `Debug`
  and zeroed on drop, and never appear in anything sent to the edge.

`GrantLedger` implements `GrantCheck` from chain `GrantEvent`s (grant, revoke, revoke-all, with a
sequence number so a replayed or out-of-order event is ignored). A node is allowed only on its own
`weftos://<mesh>/nodes/<id>/store`, and write implies read. `apply_and_enforce` applies an event
and, when the node lost write, revokes it at once, rather than waiting for its session to expire.
The native HTTP client is the host's: a `reqwest` client here would trip the diff gate's `net`
rule, which only the owner can approve.

Tested end to end (37 tests) against a store that verifies signatures, session tokens, expiry and
the user-policy intersection, and serves STS and admin calls with the 900 s minimum and the
2048-byte limit. Seven planted defects in the new code each fail a test. Acceptance for the card:
a node gets a session for its own bucket only, another node is refused, and revocation refuses
the next request.

**Live run (2026-11-07, RustFS 1.0.4 in a loopback container, not PG).** The real broker, minter and
edge store ran against it from a throwaway harness kept outside the repo (its raw-socket client would
trip the diff gate's `net` rule). Results:

| Check | Result |
|---|---|
| Pairing: policy, user, attach, bucket | all four calls work with plain JSON bodies on `/rustfs/admin/v3`. The `/minio/admin/v3` prefix wants an encrypted body and refuses plain JSON |
| STS lease, edge write, read, delete | work; the session lasts 900 s |
| Conditional create | `If-None-Match: *` gives 412 on the second write |
| Presigned PUT | works, **only with `Host` as the sole header**. RustFS refuses a presigned request that carries an `x-amz-*` header outside the signed set (`presigned_unsigned_amz_header`). The first live run caught this; the edge is fixed and the mock now enforces it |
| Cross-node | node A's session writing to node B's bucket: 403. A's user asking for a session policy that names B's bucket, then writing: 403 |
| Revocation | `set-user-status` disabled: the first request after the call, on a session still 14 min from expiry, got 403, about 7 ms after the revoke started; the edge's next write was refused by the parent. Restore works |
| Removal | after `remove-user`, AssumeRole as that user: 403 |
| Session floor | RustFS accepted `DurationSeconds=60`. The 900 s minimum is MinIO's, so the floor is a config field defaulting to 900 |

Also checked: the crate compiles for `wasm32-unknown-unknown`.

**Not yet verified:** the spike's gate C6 on PG itself (this run used a local container); real edge
hardware; the link between a node and its parent (cogbus, mesh-local or a WebSocket), which is the
host's. Since built: key generation (`keygen`). Prepared, awaiting the owner: the native HTTP client (see below). Still not built: moving archived
buckets (S1's archive-on-unpair). `node_bucket` is the one-line ADR rule and should be replaced by S1's mapping when S1
lands.

### Adversarial review fixes (2026-11-08)

A read-only review of the crate found four defects, all fixed with a test that fails when the fix is removed:

- The 900 s cap was enforced only on the edge. The broker now refuses any session whose expiry is above `now + 900 + 60`, whatever the store returned.
- Revoke could fail open: disabling the RustFS user looked up the node key first, so a missing key left existing sessions alive. The disable now names the principal directly and signs with the admin key.
- `Debug` on `HttpRequest`, `HttpResponse` and `Presigned` printed session tokens, the node secret on the add-user path and presigned URLs. They now print shape only.
- Presigned keys containing `*`, `?` or `$` are refused, since they would widen the one-object session policy.
- Refusals sent to the edge carried store error text. The edge now sees "credential service unavailable"; the detail goes to the audit sink.

Node secrets come from `keygen` (160 bits of host entropy, constant input refused; `generate_node_key` behind the `keygen` feature).

Open from the review, not fixed: a grant event with `seq = u64::MAX` freezes the ledger, so only chain-authenticated events may reach `apply`; reusing a node id after unpair inherits the old bucket unless S1 archives it.

### Handoffs to other lanes (2026-11-08)

The two review items this crate cannot fix are now ignored regression tests in `crates/weftos-edge-creds/src/tests_live.rs`. Run `cargo test -p weftos-edge-creds --lib -- --ignored` to see both fail today. Remove the `#[ignore]` when the owner lands the fix.

| Gap | Test | Owner | What the fix needs |
|---|---|---|---|
| A grant event with `seq = u64::MAX` makes every later event `Stale`, so a later revoke is dropped | `a_far_future_seq_cannot_freeze_later_revocations` | S1 (weftos-store) and whoever feeds chain events to `GrantLedger::apply` | Only chain-authenticated events reach `apply`, or `apply` bounds a jump in `seq` |
| Reusing a node id after unpair inherits the old `n-<id>` bucket | `a_reused_node_id_does_not_inherit_the_previous_nodes_objects` | S1 (archive on unpair, section 3) | Archive and empty the bucket before the id can be paired again, or never reuse an id |

Ed25519 verification audit: the S3 crate and ledger verify no signatures (grant events arrive already chain-authenticated), and there is no `weftos-store` crate in this worktree yet. S1 and S5 must use `verify_strict` for any signature they check. In the mesh paths here, `node_facts_advert`, `placement_vocabulary`, `capability_claim`, `key_rotation` and `seed_bind` moved from `verify` to `verify_strict`; `clawft-mesh-local` cert checks already used it. Small-order key rejection tests: `cert_tests::small_order_machine_key_is_rejected` and `node_facts_advert::tests::small_order_key_with_a_trivial_signature_is_refused`. Still on plain `verify`, outside the mesh scope, for their owners: `http_facade`, `wasm_runner/types`, `workload_ctl/msg`, `workload_ctl/host_journal`, `clawft-delegation`, `exo-resource-tree` delegation, `tree_manager`, `weftos-cos` `mcp_catalog`, `clawft-weave` `project_forward`.

## Chain compaction (proposal, 2026-11-08)

**Status: proposed. A spike exists for the grant ledger (`crates/weftos-edge-creds/src/compaction.rs`), exercised against an in-memory loopback chain; nothing is wired to a live chain and no schedule is on.**

**Problem.** The grant ledger (S3) and the S5 gate state both rebuild by replaying every event they ever wrote. Start-up time and the cost of a new follower grow with the length of the chain, and revoked history is carried for ever.

**Design: snapshot plus tail.**

1. *Snapshot.* The daemon periodically folds the table into a `Snapshot`: `version`, `scope` (for example `grants/<mesh>`, or `gate/<account>/<mesh>` for S5), `through_seq`, the folded `state` in canonical order, `state_hash` (SHA-256 of the canonical state) and `events_head`.
2. *`events_head`.* A hash chain over the events folded so far: `head' = SHA256(head || "\n" || canonical_event_json)`, starting from a fixed genesis value. Events the sequence rule ignores do not extend it.
3. *Publication.* The snapshot is appended to the chain as one signed event (`store.snapshot`) by the same signer that signs grant events, using the domain `weftos/grant-ledger/snapshot/v1`. The signature covers the canonical snapshot JSON, so `through_seq` and `scope` cannot be moved.
4. *Restore.* A follower takes the newest snapshot it trusts and replays only the events with `seq > through_seq`. A tail event at or before `through_seq` is an error: it means two histories were mixed.
5. *Archive before prune.* The segment up to `through_seq` is written to cold storage and its range hash recorded before any chain segment is pruned. The snapshot alone cannot answer who held what, when. Pruning is a separate, owner-run step, never part of taking a snapshot.

**How a follower verifies a snapshot.**

- *Fresh follower (no old events).* Checks `version`, `scope`, `state_hash` against the state, then the signature with `verify_strict` under a key it already trusts for chain events. This trusts the signer for the past. A compromised signer could sign a snapshot that omits a revoke.
- *Follower that kept the events.* Also calls `verify_against_history`: re-fold the old events and compare `through_seq`, `events_head` and the state. This catches a validly signed but false snapshot, and a state that matches by a different history (the head differs).
- *Cross-check.* Two followers that disagree on `events_head` for the same `(scope, through_seq)` raise an alert; the archived segment settles it.

**What a snapshot must not do.** It carries no tokens or keys. A revoke is carried by the absence of the grant, and the existing sequence rule drops stale pre-snapshot events. The first snapshot after a signer rotation is signed by the new signer and must also chain to the old one by the chain's own key-rotation event, which the chain layer verifies, not this crate.

**S5 gate state.** The gate's `store.client.gate.<kind>` events (hold, grant, revoke, trust, cross-mesh install or revoke) fold the same way. S5 implements the fold and canonical bytes for its own state and reuses `head_extend`, the hashing rules and the verification steps above. Two points are S5's to settle: expiry (a cross-mesh grant that expired must not survive into a snapshot, so the snapshot is taken at a stated time and drops expired entries), and the per-account scope, since events for other accounts are ignored on replay.

**Open.**

- Cadence: every N events or every T hours, and whether a snapshot may be taken without a quiet point (the spike folds under the ledger lock).
- Where the signed snapshot lives on the chain and how chain-level pruning refers to it; this belongs to the chain owner.
- Whether `events_head` should be anchored in the existing chain anchor so a fresh follower has a second source besides the signer.

**Spike results.** The ledger now keeps `events_head`; `GrantLedger::snapshot`, `signed_snapshot`, `restore` and `verify_against_history` exist. Seven tests: snapshot plus tail equals full replay; a pre-snapshot event in the tail is refused; a follower refuses an edited, re-hashed, wrong-scope, rewound or foreign-signed snapshot; a follower with history catches a signed but false snapshot; the head catches a different history with the same state; the head depends on order and content; 1000 pair and unpair events fold to an empty snapshot. Each check was removed in turn and a test failed.

**Loopback wiring and schedule (2026-11-08).** `loopback::LoopbackChain` (feature `loopback-chain`, or tests) is an in-memory, append-only stand-in for the chain: grant events and signed snapshots, no I/O, no clock, no live chain. It adds `append`, `snapshot_now`, `recover` (newest snapshot that passes the follower checks, then an older one, then a full replay while nothing is pruned), `archive` (copies the events to cold storage and requires them to reproduce the signed snapshot) and `prune` (only after `archive`, only up to the newest usable snapshot). `SnapshotSchedule` is the scheduled-snapshot option: **off by default**, `every_events(n)` and/or `every_secs(t)`, never due when nothing is new; the chain takes a snapshot on `append` only if the host built it with an enabled schedule. Nothing in the crate turns it on. Twelve tests; a forged newest snapshot falls back, a forged one after a prune is an error rather than a silent partial ledger, prune is refused without a matching archive, and the schedule defaults off. Five planted defects (schedule always due, prune without the archive check, unchecked newest snapshot, ignoring the pruned bound, archive without the history check) each fail a test. `tick(now)` lets a host timer take a snapshot when only the time rule is due, and `compact(verifier, now)` is the owner-run snapshot, archive, prune sequence (the schedule never calls it; each stage refuses to go on if the one before did not hold). A repeat `snapshot_now` with nothing new returns the existing snapshot. A property test runs seven cadences against every compaction point and checks recovery equals a full replay. Still the chain owner's: the real signer, where the snapshot event sits, and the cadence to use (see the switches below).

**Bucket name parity with S1 (2026-11-08).** S1's `StoreName::Node` (weftos-store `name.rs`) derives `n-<id>` with an id of lowercase `[a-z0-9-]`, at most 61 characters. This crate's `node_bucket` accepts a strict subset (at most 48, no leading or trailing dash), so every id it accepts maps to the bucket S1 derives; `node_bucket_agrees_with_s1s_store_name_rule_and_is_never_looser` pins that. When S1's crate lands as a dependency, replace `node_bucket` with `StoreName::bucket` and drop the extra restriction only if the owner wants ids S1 allows.

**Native HTTP client, prepared and unmerged (2026-11-08).** Branch `prep/adr126-s3-reqwest-client` (commit c9b85e56, on top of this branch) adds `reqwest_transport::ReqwestTransport`, an `ObjectTransport` over `reqwest` behind the off-by-default feature `reqwest-client` (host side only, not wasm-clean). It sends the already-signed request with the signer's own percent-encoding, never follows redirects (a signed request must not be replayed elsewhere), and returns generic errors with no URL (a presigned query would leak). Four tests against a loopback listener; three planted defects (redirects followed, error text with the URL, URL encoding) fail a test. The diff gate rejects it, as expected: `new-dep` for `reqwest` in `crates/weftos-edge-creds/Cargo.toml` (already a workspace dependency, optional here) and `net` for `crates/weftos-edge-creds/src/reqwest_transport.rs` (the client and the test listeners). **The owner's choice is one step:** add the two approvals for those paths in `config/diff-gate-approvals.toml` and merge the branch; or decline, and the host keeps supplying its own `ObjectTransport`. Nothing on this branch is needed by the rest of the crate.

**Consume side of S1's archive-on-unpair and seq guard (2026-11-08).** S1 had not posted either call when this was written (its branch has no unpair archive and no seq guard), so S3 holds the contract as traits and tests it against mocks. When S1 posts commit ids, implement the traits over the real calls; nothing else changes.

- **`lifecycle::NodeLifecycle`** with `NodeArchiver` (S1: `archive_node` copies the bucket to the archive and empties it, idempotently; `bucket_is_empty`) and `NodeProvisioner` (`RustfsMinter` implements it). `unpair` applies the event and cuts the node off, seals the id, archives, and only then deletes the user and policy. A failed archive leaves the user disabled, not deleted, and the id sealed; calling `unpair` again finishes. `check_pairable` refuses a sealed id until its bucket is empty, and fails closed when the probe errors. The sealed set is memory only: after a restart `seal_from_history` rebuilds it from the chain's unpair events, and the empty-bucket probe is what unseals.
- **`grants::SeqGuard`** (`GrantLedger::set_seq_guard`; `MaxSeqJump(n)` as a stand-in). A refused event returns `Applied::OutOfRange` and changes nothing, head included. Default is no guard, because the chain's `seq` is shared with other events and a legitimate gap size is the chain owner's call. A ledger rebuilt by `restore` has no guard until the host installs it again. The two `#[ignore]` regression tests still describe the unguarded default; the guard is covered by separate tests.
- Eight tests; five planted defects (delete before archive, no sealing, probe fails open, guard ignored, state changed before the guard refuses) each fail a test.

**What gate C6 needs.** The repository does not define C6; the card (b76c74a7) states it as: a WeftOS principal gets a scoped STS session for one folder, another principal is refused, and revocation refuses the next request within 1 s, all with WeftOS-minted credentials, on the PG RustFS from S0. Everything but the PG instance is built and was run against a loopback RustFS 1.0.4. What is needed from the owner: (1) permission to run a harness against the PG RustFS (barred to agents today); (2) its endpoint and an admin key supplied at run time, not committed; (3) an HTTP client for the minter, either the owner-approved `reqwest` branch or the throwaway harness kept outside the repo; (4) two throwaway node ids and a bucket prefix agreed so the run cannot touch real stores; (5) the run window and who removes the test users afterwards. Expected result is the loopback table above, with the revoke time measured on PG.

**Chain-authenticated feed (2026-11-08).** S5 found that `GrantLedger::apply` trusts whatever it is handed. `authchain::ChainFeed` is now the way events reach the ledger: `ingest(&ChainGrantEvent)` accepts an event only if (1) its scope (a mesh, `grants/<mesh>`) is the feed's, (2) the signature verifies through the same `SnapshotVerifier` interface the snapshots use, under its own signing domain `weftos/grant-ledger/event/v1`, with the signature covering scope, event (so `seq` and `op`) and `prev_head`, (3) its `seq` is after the last applied one (otherwise `Replay`, harmless and safe to drop), and (4) `prev_head` equals the ledger's current `events_head` (otherwise `BrokenLink`: reordered, or an earlier event is missing, and nothing after the gap applies). The link reuses the head kept for compaction, so it survives a snapshot restore. Ingests are serialised, a refused event changes nothing, and `ingest_and_enforce` revokes at once when a node lost write, returning the failure if the RustFS disable fails. Ten tests; seven planted defects (no signature check, no link check, no scope check, signature without scope, signature without `prev_head`, no replay check, guard refusal returned as success) each fail a test; the `prev_head` case needed a new test (a relay rewriting `prev_head` to skip a revoke).

What this settles and what it does not: a forged, edited, relabelled, replayed, reordered or gap-spliced event is refused, and a relay cannot add a far-future `seq` (it is signed). It does not stop a trusted signer from signing `seq = u64::MAX`; the seq guard does that (`OutOfRange`), and a feed that refuses such an event cannot follow a chain that moved on, which is deliberate: an operator has to look. Still the chain owner's: the real signer and its key trust, how the chain wraps a grant event (this crate defines `ChainGrantEvent` as the shape it needs), who is allowed to sign grants for which meshes, and the snapshot cadence and placement.

### Chain-owner switches for S3 (2026-11-08)

Three decisions are the chain owner's. Each is written as one switch with its options and S3's recommendation. S3 has decided none of them and no code depends on a choice: the crate takes the answer through the existing interfaces (`SnapshotVerifier`, `ChainGrantEvent`, `SnapshotSchedule`, `LoopbackChain`).

**Switch 1. Who may sign grant events for which mesh. ANSWERED (owner, 2026-11-08 11:21): grants may be signed by any other node or person.** Built as `authchain::SignerRule::AnyOther` (the default): any identity the host's verifier trusts may sign, except that nobody may sign a `Grant` of access to themselves (`FeedError::SelfGrant`; their own revoke is allowed, it gives them nothing); `SignerRule::Anyone` switches that check off. Which identities the verifier trusts is still the host's. Cost to know about: with open signing, trust rests on the verifier's set of identities and on the self-grant check, which two colluding or sybil identities can pass, so the audit trail and the seq guard carry more weight; per-mesh key limits (options A to C below) remain available if the owner wants them back. The original options follow.

 Today the verifier only answers "did this signer sign these bytes". The feed also checks the scope, so a signer is currently trusted for every mesh the host's `SnapshotVerifier` accepts it for.

| Option | What it means | Cost |
|---|---|---|
| A. One signer key per mesh | The verifier maps `scope` (`grants/<mesh>`) to exactly one trusted key. | Smallest blast radius. Key rotation is per mesh. |
| B. The account's owner key signs all its meshes | One trusted key per account; scope must start with that account's meshes. | One key to protect. A leak affects every mesh of the account. |
| C. Delegated signers | The owner key signs a short-lived delegation to a signer for named meshes; the verifier checks the delegation. | Most flexible. Needs a delegation event, expiry and revoke on the chain. |

Recommendation: A to start. It needs no new chain event, and the `scope` field is already signed. Move to C when a mesh needs more than one signer.
Flip: the owner names A, B or C; the change is in the host's `SnapshotVerifier` implementation, not in this crate.

**Switch 2. Snapshot cadence. STILL OPEN (owner, 11:21): no cadence is approved; the recommendation below is not a decision and nothing sets one.** `SnapshotSchedule` is off by default and supports `every_events(n)`, `every_secs(t)` or both.

| Option | Setting | Effect |
|---|---|---|
| A. Stay off | No schedule; a person runs `snapshot_now` or `compact`. | No new load. A follower replays the whole chain. |
| B. By count | `every_events(n)` | Tail length stays under `n`. The cost follows write rate. |
| C. By count and time | `every_events(n).or_every_secs(t)` | Bounded tail and bounded staleness on a quiet mesh. |

Recommendation: C, with `n` near the number of events a follower can replay in about a second and `t` of one day. S3 sets no numbers; they are the owner's. Pruning is never scheduled: `compact` (snapshot, archive, prune) is owner-run in every option.
Flip: build the chain with `with_schedule(...)`; nothing else changes.

**Switch 3. Where the signed snapshot lives. STILL OPEN (owner, 11:21): not approved.**

| Option | Where | Cost |
|---|---|---|
| A. On the chain as an event | `store.snapshot`, signed by the grant signer. | One source of truth. Chain-level pruning needs a rule that refers to it. |
| B. In the mesh store, with a hash on the chain | The snapshot is an object; the chain holds its hash and `through_seq`. | Keeps the chain small. Followers need store access to bootstrap. |
| C. A and anchored `events_head` | As A, plus `events_head` in the existing chain anchor. | A fresh follower has a second source besides the signer. Needs the anchor to carry one more field. |

Recommendation: A now, and C when the anchor can carry the field. B has a bootstrap loop (a follower needs a snapshot to learn which store to read), so S3 advises against it for grants.
Flip: the owner names A, B or C; the wiring is `GrantLedger::signed_snapshot` on the chain side.

## Consequences

- **Gains:**
  - one governed place for everything that isn't in git;
  - agents scale out across machines without copying assets;
  - releases get a store that can't be rewritten.
- **PG becomes a dependency for the store.** While PG is down, nodes keep what they have
  cached, and writes queue or fail. A second RustFS node with replication is future work,
  not part of this ADR.
- **New external dependency:** RustFS reached 1.0 on 2026-09-16, so S0 validates the S3
  operations we use before anything relies on them.
- **No POSIX semantics.** Through a mount, a rename is a copy plus a delete, and there's no
  locking. §4's one-writer rule is how we live with that.

## Open questions for the owner

1. ~~Can the existing project directory be served in place?~~ Answered by S0: no. It's
   imported once and the bucket becomes the source.
4. ~~Listing a store isn't logged by RustFS (S0). Is that acceptable?~~ Answered by the owner on
   2026-10-29: yes. Anyone with read access is assumed to see all of a store's metadata with a
   single read, so listings don't need an audit line. Multipart part uploads do, and so do refusals
   and bad signatures. Whether RustFS can be extended to cover them is under study.
2. ~~How long do archived buckets (from a removed worktree, project or node) live before
   they're deleted?~~ Answered by the owner 2026-11-08: archives do not expire. S1 carries it as a
   setting, not a number (see "Archive retention"): the default is for ever; the operator names days
   per kind of store.
3. Do cloud agent sessions get write access to worktree stores, or only read?
