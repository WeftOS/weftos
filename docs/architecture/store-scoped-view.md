# Store scoped view: the filesystem half of the Linux sandbox backend (2026-11-08)

Status: **interface spec plus a loopback prototype.** Owner item E-1. The prototype is the library
module `crates/weftos-store/src/drive/view.rs` and `drive/view/` on branch
`wt/adr126-e1-scoped-view`. The `weaver store view` verbs are specified here but not built. The
Linux bwrap side belongs to photo-gallery's `weftos-pg` session; see
[`docs/research/adr126-e1-scoped-view-handoff-weftos-pg-2026-11-08.md`](../research/adr126-e1-scoped-view-handoff-weftos-pg-2026-11-08.md).

Updated 2026-11-10: file modes. Each object carries one execute attribute; the view applies it
(section 2, "File modes"). Before this, every base file was 0444 and nothing in a root image could run.

Updated 2026-11-10 (attribute follow-ups): a refresh pulls a mode-only change made elsewhere; a conflict's remote
copy and `Keep::Remote` carry the attribute; multipart and Object Lock puts carry it (section 2, "File modes").

Updated 2026-11-10 (overlay lower): `lower/` is writable (section 2, "File modes"). Overlayfs copy-up keeps the
lower's mode, so a read-only lower made an in-place edit of an existing file fail with "Permission denied".
Only `base/` is read-only now.

Sources. Files marked "main checkout" are untracked files in `~/weftos`, not in this branch.

- Tapestry Phase 3 prep, section 5 E-1 and work item 10 ("E3 / R9 Linux backend"):
  `/Volumes/ai-models/research/tapestry-wt/p3-prep-docs/docs/tapestry/phase3-prep.md`.
- Tapestry plan, row R9 (main checkout): `docs/grok-build-fork/plan.md`.
- Owner decision Q9 (main checkout): `docs/grok-build-fork/decisions-2026-11-02.md`.
- Computer spike plan, section 1, points 2 and 3, the bwrap composition (main checkout):
  `docs/grok-build-fork/rustfs-computer-spike-plan-2026-11-02.md`.
- Seed drive plan, section 4 item 6, "the bwrap scoped view" (main checkout):
  `docs/plans/seed-drive-finish-and-integrate-plan-2026-11-07.md`.
- The drive the view is built on, with its design and live timings:
  [`docs/research/adr126-s3-materialise-watch-2026-11-08.md`](../research/adr126-s3-materialise-watch-2026-11-08.md).
- [ADR-131](../adr/adr-131-seed-drive.md) section 1 ("materialise rather than mount"), section 2
  (per platform), and "Alternatives considered".
- [ADR-126](../adr/adr-126-mesh-object-store.md) section 3 (the daemon owns the spaces and writes
  IAM). `crates/weftos-store/src/policy.rs` holds the Deny on `.wt/*` and `worktrees/*` (review F4).

## 1. Purpose and consumers

Tapestry R9 adds a Linux sandbox backend. bwrap and Landlock handle process, network and syscall
isolation. Owner decision Q9 says the filesystem side uses "a scoped rustfs view instead of bind
mounts". bwrap works through bind mounts, so the computer spike plan (section 1, point 3) restates
the intent. The rustfs part is *what* gets bound: a materialised, scoped tree from the store. It is
not a FUSE mount of the bucket. This document defines that thing.

**Consumer:** Tapestry Phase 3 work item 10 ("Linux backend, filesystem half"), blocked on E-1.
The launcher is item 9, the process, network and syscall half. It reads `view.json` and turns its
binds into bwrap arguments.

**macOS (out of scope):** upstream Tapestry already has Seatbelt. The same view works there with no
change to the library. Seatbelt gets `base/` as read-only paths and `work/` as the one writable
path, with `WorkStart::Clone` (the Mac has no overlayfs). The file modes are then the only thing
that keeps `base/` read-only (section 8).

## 2. What a session sees

A view root on the host has this layout. The session sees only the first two entries.

| Host path | In the sandbox | What it is |
|---|---|---|
| `<root>/base/` | `/` (or under a chroot path) | The read-only includes, each at its `at` path, plus empty mount points. Directories 0555, files 0444, or 0555 for an executable object |
| `<root>/work/` | `/work` | The writable tree. `Clone`: a full copy of the work prefix. `Empty`: an empty overlay upper; the upper is the change set |
| `<root>/lower/` | overlay lower of `/work` | `Empty` only: a mirror of the work prefix, in the writable modes (see "File modes") |
| `<root>/.state/` | never bound | One drive per include, the work drive, the lease record, the work index, the staged-path set, the overlay workdir |
| `<root>/view.json` | never bound | The manifest the launcher reads |

**File modes.** The object store keeps one attribute per object, `executable` (a bool): the file had
any execute bit when it was put. It is S3 user metadata `x-amz-meta-weft-exec: 1` on RustFS, the data
file's own execute bits on the directory backend, and a set in memory. Only the execute bit is the
object's; write bits are the view's policy, and setuid, setgid and the sticky bit are never carried.

| Where | Not executable | Executable |
|---|---|---|
| `base/` | 0444 | 0555 |
| `lower/` (overlay) and `work/` (clone) | 0644 | 0755 |

- **Put.** `store drive sync-up` and a view publish read the file's mode. A create or modify puts the
  bytes with the attribute (`put_attrs`, `put_if_match_attrs`). A chmod with the same bytes is a modify
  (a put of the same content, same etag, new attribute), and clearing the bit clears the attribute.
- **Materialise.** A drive fetches the attribute once per placed object (one HEAD) and sets 0755 on the
  placed file (the HEAD is skipped for an unchanged object; see "Refresh pulls a mode-only change"); the view then sets 0555/0444 (base) or keeps 0755/0644 (work, and the overlay `lower/`). Changing an execute bit
  in `base/` is reported in `status().tampered` and put back by refresh.
- **Old data.** An object with no attribute reads as not executable, so it materialises 0444/0644 as
  before. An `index.json` with no `exec` field loads as not executable and is written back without one
  for a non-executable file. A backend that does not carry attributes (the trait default) stores the
  bytes and reads back the default.
- **Refresh pulls a mode-only change.** A put of the same bytes with another attribute keeps the etag and
  size, so the etag alone cannot see it. `ObjectBackend::list_stamped` returns, with each object, an opaque
  modification stamp that moves when the bytes or the attributes change (RustFS: the listing's
  `LastModified`; the directory backend: mtime and the execute bit; memory: the execute attribute). The
  index entry records the stamp of the version it was checked against. A refresh asks for an object's
  attributes (one HEAD) only when its listing stamp differs from the entry's, so an unchanged tree costs
  one listing and no HEAD, and one flipped object costs one HEAD. An entry with no stamp (an index from
  before, a file this device just uploaded, a backend with no stamps) is checked once and then recorded.
  If the attribute differs and the local file is untouched, its mode is changed in place (0755, or the
  execute bits cleared); `DownReport::mode_updated` lists these, and they are in `updated` too. A file the
  user edited, deleted or chmod-ed is left alone and its stamp recorded; a local mode change then goes out
  with the next `sync_up` (last writer wins), and a local mode equal to the store's is adopted. In a view,
  base files are hard links to the drive's files, so the chmod reaches `base/` and the lock is re-applied;
  a `work/` file whose mode the session changed is adopted when it equals the store's and is a
  `work_conflicts` entry when the store also changed its bytes.
- **Conflicts carry it.** The remote copy of a conflict is written 0755 when the remote object is
  executable, the open conflict records `remote_exec`, and `Keep::Remote` places the remote bytes with that
  mode and sets the entry's `exec`. Recording a conflict costs one HEAD.
- **Multipart and Object Lock.** `PutOpts` has an `attrs` field. `put_locked` sends the metadata header; a
  multipart upload sends it with the create request (S3 takes user metadata there, not on the complete);
  `put_stream` passes it on, including the local backends' whole-body fallback.
- **Not covered.** A mode-only put inside the stamp's resolution of the previous put (RustFS
  `LastModified` is a timestamp text) is not seen by the stamp check. A third writer's mode change between
  the listing and the HEAD is picked up on the next refresh. Only the execute bit is carried.

**The overlay lower is writable.** `lower/` directories are 0755 and files 0644/0755, the same as `work/`.
Overlayfs copy-up gives the upper file the lower's mode and owner, so a 0444 lower made every copied-up
file 0444 and an in-place edit failed. A 0555 lower directory would likewise block creating a new file
in an existing directory. Only `base/` needs to be read-only. The lower is never written by the
session: overlayfs writes only to the upper, so the write bit does not let the sandbox change the lower's
bytes. On the host, `lower/` files are hard links to the work drive's files, so a host process that
writes through one would change the drive file; `status().tampered` and `refresh` repair detect and undo
that for content and execute bits, as before. Write bits are not tamper evidence (scan compares content
and the execute bit only). Plain (clone) mode and `base/` are unchanged.

**No store control state is visible.** None of these is ever placed in `base/`, `lower/` or `work/`:
- drive state (`.drive/`) and delete markers (`.drive-tombstones/`) at any depth;
- drive temp names (`.drive.tmp.*`);
- a worktree bucket's `.wt/` area;
- a project bucket's `worktrees/` registry;
- the lease objects (`.drive/leases/`).

The drive itself refuses these only at the prefix root, so the view adds a check on the full key
(`is_control_key`). The drive's own refusals also apply: secret names, private-key content,
`.git` and other VCS files, Windows-hostile names, case-only collisions, symlinks and the size
ceilings. Every refusal is reported in the open, refresh or publish report, never silently
dropped.

A listing inside the sandbox, for a view with `docs/` at `/docs` and work prefix `proj/` cloned:

```
/            (read-only)
/docs/readme.md
/docs/guide/intro.md
/work/       (read-write)
/work/a.txt
/work/dir/b.txt
/tmp/        (empty mount point, if listed in mount_points)
```

## 3. Scope: the `ViewSpec`

```rust
pub struct ViewSpec {
    pub bucket: String,
    pub base: Vec<Include>,        // read-only: { prefix, at }
    pub work: Option<WorkSpec>,    // the one writable prefix: { prefix, start: Clone | Empty }
    pub deny: Vec<String>,         // extra refusals: "*.log", "target/"
    pub limits: ViewLimits,        // max_file, max_files, max_total (S2 defaults)
    pub lease_ttl_ms: u64,         // default 15 min, at most 24 h
    pub mount_points: Vec<String>, // empty top-level dirs in base/, e.g. dev, proc, tmp
    pub device: Option<String>,    // lease holder and drive device id
}
```

- **Read-only versus writable** is structural. `base` entries are read-only. `work` is the only
  writable prefix, and `work: None` is a read-only view: no lease, publish refused.
- **`at` paths must not nest.** An include at `""` must be the only one. An include may not land
  on a mount point. `work` is a reserved mount point whenever there is a work tree.
- **Deny patterns.** A pattern ending in `/` names a directory at any depth. Anything else is
  matched against each path segment, case-insensitively, with at most one `*`. They are applied on
  top of the drive's built-in denylist and cannot loosen it.
- **Size ceilings** apply per include (the drive skips what does not fit). `max_total` is also
  checked across all includes at open, which then fails with `OverBudget`.
- **Single writer.** A view with a work tree holds a lease on `(bucket, work prefix)`. A read-only
  view takes no lease, so any number of read-only views can share a prefix.
- **Who derives it.** The daemon derives the spec from the governance decision for the session's
  principal (Tapestry R9's `governance.check`; ADR-126 section 3 grants). The owner decided 2026-11-08 that any other node or person may sign grants, not only the daemon; the daemon derives the spec from a grant it verifies. The session never
  supplies a spec, a prefix or a path. The spec hash goes into `view.json` and is checked on every
  reload.

## 4. Materialised, not mounted

- **Why.** ADR-131 section 1 measured the per-`stat` round trip of a FUSE mount on the Mac:
  - warm `git status` was 2.1 to 2.5 s against 0.09 to 0.22 s locally;
  - the Linux kernel tree took 31 to 34 s against 0.31 s;
  - NFS from a VM took 26.5 s;
  - JuiceFS needs its own metadata engine;
  - `rclone mount` has no locking and last close wins (ADR-131 "Alternatives considered").

  A materialised tree gives the session native `stat`, `open`, `mmap`, `fsync` and `rename` at
  local speed, and works offline. The computer spike measured warm `git status` inside a
  materialised computer at 0.86 to 1.67 times local (seed drive plan, C4 row).
- **What bwrap binds.** Local directories only:
  - `--ro-bind <root>/base /`;
  - for `Clone`, `--bind <root>/work /work`;
  - for `Empty`, `--overlay-src <root>/lower --overlay <root>/work <root>/.state/ovl-work /work`.

  No FUSE, no network file system, no store credential inside the sandbox.
- **Cost.** `base/` and `lower/` are hard links to the drive's files, so they take no extra bytes on
  any file system. The work clone is `std::fs::copy`: a clone on APFS, a full copy on ext4 without
  reflinks. The drive's content-addressed cache holds each blob once.
- **macOS limits.** There is no bwrap and no overlayfs. 0555/0444 (0555 on an executable) modes are advisory, because the
  owner can `chmod` them back. A base file written in place through its hard link changes the
  drive's copy too. That is detected (`status().tampered`) and repaired from the cache on the next
  refresh. The Seatbelt profile is what really denies writes outside `/work`.

## 5. Lifecycle

1. **Open** (`ScopedView::open`):
   1. validate the spec and the root;
   2. take the lease (create-only put);
   3. materialise each include into its drive under `.state/drives/`;
   4. mirror the read-only includes into `base/`;
   5. create the mount points;
   6. fill `work/` (clone) or `lower/` (overlay);
   7. lock `base/` read-only;
   8. write `view.json` last.

   Any failure releases the lease and removes what was made, by exact path.
2. **Hand over.** The daemon gives the launcher the path of `view.json`. The launcher binds what it
   lists and nothing else.
3. **Refresh** (`refresh`), on a timer, a store notification, or a `store.view.refresh` call:
   1. a base drive file that changed under the view is put back from the cache;
   2. each base include is pulled (`sync_down`) and re-mirrored. Changes to `base/` show live
      through the read-only bind;
   3. `Clone`: unedited work files are brought up to date, and edited ones are left alone and
      reported as `work_conflicts`;
   4. `Empty`: the work prefix is not refreshed. The overlay lower must not change under a mounted
      overlay (`work_not_refreshed`).

   Refresh never writes to the store.
4. **Publish** (`publish`):
   1. check the lease (ours and unexpired);
   2. check that the work drive's tree still matches its index, apart from changes this view
      staged;
   3. collect the work changes. `Clone` compares the work tree with the work index. `Empty` reads
      the upper: files and whiteouts;
   4. copy each change into the work drive. Its index entry is set to the base the edit was made
      against;
   5. run the drive's `sync_up`. Creates are create-only. Modifies and deletes send `If-Match`, and
      deletes write a tombstone. A remote change since the base becomes a conflict record (412),
      even when a refresh already moved the drive forward.
5. **Resolve** (`resolve(path, Keep::Local | Keep::Remote)`). `Keep::Local` makes the next publish
   write the session's version over the recorded remote one. `Keep::Remote` makes the work file the
   remote version (`Clone` only).
6. **Close** (`close(Publish | Discard)`):
   1. `Publish` first publishes, and refuses with `Unpublished(n)` if conflicts stay open (nothing
      is removed);
   2. release the lease (a conditional delete, so a lease another holder took is left alone);
   3. remove `view.json`, `base/`, `lower/`, `work/` and `.state/` by exact path;
   4. remove the root only if it is then empty.

   The report is what the daemon writes to the activity log and the chain.
7. **Renew** (`renew`) extends the lease with a conditional put. A session longer than the TTL
   renews before each publish.

## 6. Interface

### 6.1 Rust API (built)

```rust
ScopedView::open(&backend, &spec, root) -> Result<(ScopedView, OpenReport)>
ScopedView::load(root) -> Result<ScopedView>
view.refresh(&backend) -> Result<RefreshReport>
view.publish(&backend) -> Result<PublishReport>      // ViewError::ReadOnly without a work tree
view.resolve(path, Keep) -> Result<()>
view.renew(&backend) -> Result<()>
view.status() -> Result<ViewStatus>                  // local only: lease, pending, conflicts, tampered
view.close(&backend, CloseMode::Publish | CloseMode::Discard) -> Result<CloseReport>
lease_key(bucket, prefix) -> String                  // ".drive/leases/<32 hex>.json"
is_control_key(bucket, key) -> bool
```

The errors are typed (`ViewError`): `Spec`, `Root`, `AlreadyOpen`, `NotAView`, `LeaseHeld`,
`LeaseLost`, `LeaseExpired`, `LeaseTakeoverUnsupported`, `ReadOnly`, `OverBudget`, `Tampered`,
`Unpublished`, `Unsupported`, and `Drive(DriveError)` for store, I/O and JSON failures. The drive API
is unchanged. `SkipWhy` gained one variant, `Denied`.

### 6.2 `view.json` (built)

```json
{
  "schema": 1,
  "root": "/abs/root",
  "base": "/abs/root/base",
  "work": "/abs/root/work",
  "work_start": "clone",
  "binds": [
    {"kind": "ro", "src": "/abs/root/base", "dest": "/"},
    {"kind": "rw", "src": "/abs/root/work", "dest": "/work"}
  ],
  "lease": {"key": ".drive/leases/<hash>.json", "id": "<ulid>", "holder": "<device>", "expires_ms": 0},
  "spec_hash": "<blake3 of the spec>",
  "bucket": "p-...", "base_includes": [{"prefix": "docs", "at": "docs"}], "work_prefix": "proj/",
  "mount_points": ["work", "tmp"],
  "deny": ["*.log"],
  "refresh": {"rpc": "store.view.refresh", "root": "/abs/root"},
  "opened_ms": 0, "updated_ms": 0
}
```

For `Empty` the second bind is
`{"kind": "overlay", "lower": ".../lower", "upper": ".../work", "workdir": ".../.state/ovl-work", "dest": "/work"}`.
`dest` assumes `base/` is `/`. A launcher that binds `base/` under a chroot path prefixes every
`dest`. `deny` is the spec's deny list as applied (added 2026-11-08 for the Tapestry launcher, which checks that the globs it asked for took effect; the drive's built-in denylists are not repeated; older manifests without it read as empty). The manifest carries no credential and no store endpoint. The launcher must treat any other
field as advisory.

### 6.3 `weaver store view` verbs (built 2026-11-08, unit and loopback tests only)

Wired like `weaver store drive`: an Admin act through the daemon RPC `store.view.<verb>` (code:
`crates/clawft-weave/src/store_view.rs`, flags in `commands/store_view_args.rs`), the root checked by
the S2 `Guard` (absolute, resolved, outside daemon state), the view bound to the store `--name` names.
The spec is given by the operator for now (`--spec` or flags); deriving it from the session's grant is
still the daemon's job later. Differences from the first sketch, following the library: `resolve` and
`renew` are verbs too, and `close` requires exactly one of `--publish` or `--discard` (the library
refuses a publishing close only while conflicts stay open, never for plain unpublished edits).

```text
weaver store view open    --name <store> --root <abs> (--spec <file.json> | --include P[=AT].. [--work P [--work-start clone|empty]]
                          [--deny PAT].. [--mount-point DIR].. [--lease-ttl-ms N] [--device D]) [--read-only]
weaver store view status  --root <abs> [--name <store>]
weaver store view refresh --root <abs> [--name <store>]
weaver store view publish --name <store> --root <abs>
weaver store view resolve --name <store> --root <abs> --path <rel> --keep local|remote
weaver store view renew   --root <abs> [--name <store>]
weaver store view close   --name <store> --root <abs> (--publish | --discard)
```

`--spec` is a `ViewSpec` JSON file without `bucket`, read by the CLI and sent as a param (the daemon
opens no path the caller names for it); a `bucket` that differs from `--name`'s is refused. `--spec` and
the spec flags are alternatives. `--include P=AT` places prefix `P` at `AT` under `base/` (default
`base/` itself). `--read-only` drops the work tree: no lease, `publish` refused. Output is JSON.

Limits and bucket (2026-11-10). The spec's `limits` are three flags, `--max-file BYTES`, `--max-files N`,
`--max-total BYTES`; an omitted one keeps the daemon default, and none given sends no `limits` key. They
are CLI sugar for the same `ViewSpec` field a `--spec` file carries, so the daemon still has one
source of truth. There is no `--bucket`: the bucket is always the one `--name` binds to, and the CLI
refuses the flag (a spec file naming another bucket is refused by the daemon).

Error kinds. A `root` the path guard refuses (not absolute, `.` or `..`, a symlink, inside the
daemon's own state, or not a directory) returns `error_kind: "path_refused"`; missing or malformed
params return `"invalid_params"`. Clients branch on the kind, not the message text. The HTTP facade
(`daemon_facade.rs`) does not route `store.view.*`, so the kind reaches RPC clients (`weaver`, Tapestry)
only. Other failures keep no kind.

The `weaver store` CLI surfaces the kind on stderr as its own line, `error_kind: path_refused`,
printed before the unchanged `Error: <message>` line (exit status 1 as for every failure). Untyped
failures print no such line. Scripts match `^error_kind: ([a-z0-9_]+)$` rather than the message.

| Verb | RPC | Prints | Records |
|---|---|---|---|
| `open` | `store.view.open` | root, `view.json` path, spec hash, lease, files, bytes, fetched, recovered, took-over holder, skipped | activity `ViewOpen`, chain `store.view.open` |
| `status` | `store.view.status` | the `view.json` manifest plus lease, expiry, per-include counts, pending changes, conflicts, tampered | none |
| `refresh` | `store.view.refresh` | per-include down reports, repaired, work updated/removed/conflicts | none (never writes the store) |
| `publish` | `store.view.publish` | the up report (put, deleted, conflicts, conditional writes), applied, held | `ViewPublish`, chain `store.view.publish` |
| `resolve` | `store.view.resolve` | the path and side kept | `ViewResolve`, chain `store.view.resolve` |
| `renew` | `store.view.renew` | the lease with its new expiry | none (the drive records no claim verbs either) |
| `close` | `store.view.close` | mode, publish report, lease released, removed paths, root removed, names left | `ViewClose`, chain `store.view.close` |

Success and failure are recorded alike, as the drive verbs do. The chain payload is
`{store, actor, result}` where every list in the result is replaced by its length, at any depth: counts,
spec hash and lease id, never file lists. A call that fails before it names a valid store records
nothing.

## 7. Failure modes

| Failure | Detection | State the view is left in |
|---|---|---|
| Store unreachable at open | backend error from the lease put or a listing | Error. Lease released if it was taken; every created path removed; a root that existed before is kept, empty |
| Store unreachable at refresh | backend error from `sync_down` | Error. View intact, `base/` locked again; an include that failed keeps its previous content |
| Store unreachable at publish | backend error from the lease check or `sync_up` | Error. Work tree untouched, the change still pending; uploads that landed before the failure are recorded |
| Lease lost mid-session (taken over, or deleted) | the lease etag differs or the object is gone, checked before every publish and renew | `LeaseLost`. Nothing is written. `close(Discard)` leaves the other holder's lease alone |
| Lease expired by our clock | `now >= expires_ms` | `LeaseExpired`. `renew` succeeds if the object is still ours, then publish works |
| Stale etag on publish (412) | `If-Match` refused | A conflict record: local file kept, remote copy beside it in the work drive, path held open. `close(Publish)` refuses with `Unpublished` until `resolve` |
| Remote delete over a local edit | `put_if_match` returns `NoObject` | `RemoteDeletedLocalModified` conflict, the deleter named from the tombstone |
| Path traversal in a hostile listing (`..`, `.`, empty segment, backslash, NUL) | the drive's `check_rel` | Key skipped as `BadPath`, reported. Nothing is written outside the root |
| Symlink escape (a symlink planted where a base directory belongs) | the mirror prunes symlinks first, and `safe_join` refuses symlinked parents | The symlink is removed and replaced by the real directory. Nothing is written through it |
| Symlink in `work/` pointing outside | the scan and `read_regular` (no follow, same inode) | Skipped as `Symlink`, never uploaded |
| Write outside the writable prefix | bwrap's read-only bind (Linux), the modes (Mac), Landlock | The session cannot reach the store at all. A base file changed on a Mac is reported as tampered and repaired on refresh. A work drive file changed outside a publish makes publish refuse with `Tampered` |
| Control names created in `/work` (`.drive`, tombstones, `.wt`) | `is_control_key`, the drive's scan | Not published. A top-level `/work/.drive/` is ignored silently by the drive's scan; anything deeper is reported `Reserved` |
| Disk full mid-materialise | I/O error from the drive | Same as an unreachable store at open: error, lease released, paths removed |
| Crash mid-open | marker in `.state/`, no `view.json` | The next open on that root releases the recorded lease and cleans up, then opens fresh (`recovered: true`) |
| Crash mid-publish | staged paths in `.state/staged.json`, the drive's saved index | The next publish retries: staged work changes are allowed by the tamper check and `sync_up` uploads them |
| Torn index (`index.json`) in an open view | JSON error on load | `Drive(Json)`. The view stays loadable; `close(Discard)` still removes it |
| Leftover `.drive.tmp.*` | the drive's scan ignores them; the mirror's prune removes them from `base/` | Harmless. Removed with the view |
| Close with unpublished work | `CloseMode` | `Publish`: published first. `Discard`: dropped, by the caller's explicit choice. `status().pending` shows it beforehand |
| Clock skew on the lease | a lease stamped more than 10 min ahead of our clock is not live (the S2 claim rule, `CLAIM_SKEW_MS`) | Taken over with a conditional put. A holder whose clock is behind by more than the TTL loses its lease early; the next publish fails with `LeaseLost` rather than writing |
| Backend without conditional writes | `Unsupported` | A stale lease takeover is refused (`LeaseTakeoverUnsupported`). Renew and release fall back to head-then-write (not atomic). Data writes use the drive's head-then-write fallback |
| Opaque directory in an overlay upper (`rmdir` then `mkdir` of the same name in the session) | **not detected**: it is an xattr, and reading one needs a dependency the crate does not have. Decided 2026-11-08: read it with the `xattr` crate, no unsafe; implementation is pending the owner's diff-gate approvals entry for the new dependency | Lower files under that directory are not deleted on publish until then. Question 4 (decided) |

## 8. Enforcement map

| Layer | Enforces |
|---|---|
| Store IAM (`policy.rs`, ADR-126 section 3) | A principal reaches only its granted buckets. Writer puts and deletes on `.wt/*` (worktree bucket) and `worktrees/*` (project bucket) are explicitly denied. A revoked grant refuses the next request |
| The daemon | The session holds no store credential. The daemon derives the spec and holds the lease. Every store call is the daemon's, through `ScopedView`. The verbs' `Guard` checks the root |
| The view and drive library | Control keys never placed or published. Secret, VCS and hostile names refused. Size ceilings. Symlinks never followed or uploaded. Conditional writes and tombstones. The single-writer lease. Base repair and tamper refusal. Exact-path removal |
| bwrap | The mount namespace: the session sees `base/` read-only at `/`, `work/` at `/work`, and nothing else of the host. `.state/` and `view.json` are not bound. Network and IPC unsharing (launcher's choice) |
| Landlock (inside the bwrap child) | Write access only beneath `/work` (and `/tmp` if granted), so a missed bind cannot widen it |
| Seatbelt (macOS) | Writes only to `work/`. The only real read-only guarantee for `base/` on the Mac |

**Not enforced by anything today:**
- **The lease against a misbehaving daemon.** It is advisory between daemons. IAM does not stop a
  principal with write access to the bucket from putting or deleting `.drive/leases/*`. Object-level
  safety comes from the conditional writes, not the lease. Open question 1.
- **The read-only include prefixes at the store.** A principal's grant is per bucket (Read or
  Write), not per prefix. A view's daemon with a Write grant could write the base prefixes; only the
  library declines to. Per-prefix IAM is open question 2.
- **Tombstone collection, and a tombstone landing after a revive** (stated in the drive research
  doc).
- **On the Mac, the base modes.** The owner can undo them; detection and repair only.
- **Overlay opaque directories** (section 7).

## 9. Open questions for the owner

1. **Lease objects under IAM.** Add `.drive/leases/*` (and `.drive-tombstones/` writes from
   anything but the drive principal) to the Deny statement in `policy.rs`, so only the daemon's own
   key can write them?
2. **Per-prefix grants.** Should a view's credential be scoped to its prefixes (ADR-126 section 5
   short-lived STS, "get, list on the base prefixes; put, delete on the work prefix and its
   tombstones"), so the read-only half is enforced at the store too? That is aepod's STS wiring.
3. **Lease TTL and renewal.** Is 15 minutes, renewed before each publish, right? Should the daemon
   renew on a timer while a session runs?
4. **Overlay mode.** Decided 2026-11-08, implementation pending the approvals entry: read the
   `trusted.overlay.opaque` / `user.overlay.opaque` xattr with the `xattr` crate, no unsafe. It is a
   new dependency, so the owner's own entry in the diff-gate approvals comes first; this is a
   follow-up, not done.
5. **Refresh while an overlay is mounted.** `Empty` never refreshes the work prefix during a session.
   Is "refresh the work prefix at the next open" acceptable, or do sessions need live remote
   changes in `/work` (which means `Clone`)?
6. **Where the root lives.** One view root per session under the daemon's runtime directory (outside
   `.weftos` state, which `Guard` refuses), on the same file system as the drive cache so hard links
   and clones work?
7. **The Q9 wording.** Should R9 be reworded to "a materialised, scoped view from the store, bound
   by bwrap" (the spike plan's correction)?
