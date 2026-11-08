# E3 scoped view alignment: spec, verbs, Tapestry launcher, PG end-to-end (2026-11-08)

Status: alignment note, docs only. Nothing here edits another party's branch. Date 2026-11-08, owner item E-1 (our side) and item 10 (Tapestry side).

Path legend, used for every pointer below.

| Tag | Path |
|---|---|
| SPEC | `/Volumes/ai-models/weftos-worktrees/adr126-e1-scoped-view/docs/architecture/store-scoped-view.md` (branch `wt/adr126-e1-scoped-view`, tip 4455d1cdc) |
| SPEC-V | same file on `/Volumes/ai-models/weftos-worktrees/adr126-e1-view-verbs` (tip 22431e4be), which rewrites 6.3 |
| LIB | `crates/weftos-store/src/drive/view.rs`, `view/spec.rs`, `view/tree.rs` in the first worktree |
| VERBS | `crates/clawft-weave/src/store_view.rs` in the verbs worktree |
| HAND | `docs/research/adr126-e1-scoped-view-handoff-weftos-pg-2026-11-08.md` |
| P3 | `/Volumes/ai-models/research/tapestry-wt/p3-prep-docs/docs/tapestry/phase3-prep.md` |
| R9 | `.../p3-prep-docs/docs/tapestry/r9-policy-layer.md` |
| PLAN | `docs/grok-build-fork/plan.md`; SPIKE = `docs/grok-build-fork/rustfs-computer-spike-plan-2026-11-02.md`; DEC = `decisions-2026-11-02.md`; SURVEY = `spike-results/host-survey.md` |

## 1. Purpose and the three parties

One contract for the filesystem half of the Linux sandbox backend (Tapestry R9, item 10, acceptance E3). The sandbox sees a materialised, scoped tree from the store, bound by bwrap. It is not a FUSE mount (SPEC 1, L26-36).

| Party | Owns | Reads from the contract | Status today |
|---|---|---|---|
| weftos-fs-research (this session) | spec, `drive::view` library, `weaver store view` verbs | writes `view.json`, runs the lease | library built (23 unit tests, 1 ignored live test passed on loopback RustFS, HAND L18-24). Verbs built on the verbs branch with unit and loopback tests, not merged (VERBS L1-24, SPEC-V 6.3) |
| Tapestry, session backend:4 | bwrap/Landlock launcher, items 9 and 10 | `view.json`, the `store.view.*` RPCs | item 9 (process, net, syscall half) has no dependency on E-1 and is buildable now. Item 10 is "blocked" on E-1 (P L55-56, L59, L109) |
| weftos-pg | Linux end-to-end on `weftos-ci` with a throwaway RustFS | `view.json`, bwrap, Landlock | five checks specified (HAND L70-87). No result file exists yet under `spike-results/` (listing shows host-survey, phase0, gates, rustfs-macos-wsl, stage-a, stage-b only) |

## 2. The contract, frozen as of today

### 2.1 Directory layout of a view root

| Path under `<root>` | Bound into the sandbox | Writer | Notes |
|---|---|---|---|
| `base/` | yes, read-only, at `/` (binds[0]) | library, modes 0555/0444 | read-only includes at their `at`, plus empty mount points (SPEC 2, L47) |
| `work/` | yes, at `/work`. `--bind` for `clone`, overlay upper for `empty` | session | the only writable path (SPEC 3, L95) |
| `lower/` | only as the overlay lower, `empty` only | library | read-only mirror of the work prefix |
| `.state/` | never | library | drives under `.state/drives/<i>` and `drives/work`, lease record, index, staged set, `ovl-work` |
| `view.json` | never | library, written last | the manifest (SPEC 5 step 1.8, L147) |

Never placed in `base/`, `lower/` or `work/`: `.drive/`, `.drive-tombstones/`, `.drive.tmp.*`, `.wt/`, `worktrees/`, lease objects (SPEC 2, L56-61, enforced by `is_control_key`, LIB `view/spec.rs` L99).

### 2.2 `view.json`, schema 1

Code is `ViewManifest` at LIB `view/spec.rs` L231-249, built at `view/tree.rs` L329-357, schema constant `VIEW_SCHEMA = 1` at `view.rs` L48. All of it is serialised with no skip rules, so a read-only view carries `null`, not a missing key.

| Field | Type | Writer | Reader | Notes |
|---|---|---|---|---|
| `schema` | u32, always 1 | library | launcher, `ScopedView::load` | launcher must refuse any other value (HAND L59). Load checks it at `view.rs` L376 |
| `root` | absolute path | library | launcher, load | load refuses a manifest whose `root` differs from the directory opened |
| `base` | absolute path | library | launcher | equals `<root>/base` |
| `work` | path or null | library | launcher | null when the spec has no work tree |
| `work_start` | `"clone"`, `"empty"` or null | library | launcher | `WorkStart` is snake_case, default `clone` (spec.rs L46-54) |
| `binds` | list of `Bind`, base first | library | launcher only | see 2.3 |
| `lease` | `{key, id, holder, expires_ms}` or null | library, rewritten by renew and publish | supervisor | null for a read-only view. Advisory to the launcher |
| `spec_hash` | blake3 hex | library | load | load recomputes it from `.state` spec, refuses on mismatch |
| `bucket` | string | library | audit only | not needed to build binds |
| `base_includes` | list of `{prefix, at}` | library | audit only | |
| `work_prefix` | string or null | library | audit only | normalised with a trailing slash |
| `mount_points` | list of names | library | launcher | `work` first when there is a work tree, then the spec's. Already present as empty dirs in `base/` |
| `refresh` | `{rpc, root}` | library | supervisor | `rpc` is the constant `store.view.refresh` (`view.rs` L57) |
| `opened_ms`, `updated_ms` | u64 | library | audit only | `updated_ms` moves on every renew and publish |

The manifest carries no credential and no store endpoint. A launcher treats any field not in its read column as advisory (SPEC 6.2, L233-235).

### 2.3 `Bind`

Tagged by `kind` (spec.rs L206-214). `dest` assumes `base/` is `/`. A launcher that binds `base/` under a chroot path prefixes every `dest`.

| `kind` | Fields | Launcher turns it into | Produced when |
|---|---|---|---|
| `ro` | `src`, `dest` | `--ro-bind src dest` | always, `dest` is `"/"` |
| `rw` | `src`, `dest` | `--bind src dest` | `work_start` is `clone`, `dest` is `/work` |
| `overlay` | `lower`, `upper`, `workdir`, `dest` | `--overlay-src lower --overlay upper workdir dest` | `work_start` is `empty`. `upper` is `work/`, `workdir` is `.state/ovl-work` |

### 2.4 What is never bound, what the launcher adds itself

- Never bound: `.state/`, `view.json`, the daemon socket directory, any host path not in `binds`.
- Launcher's own choice, not in the manifest: `--dev`, `--proc`, `--tmpfs /tmp`, unshare flags, Landlock rules (write beneath `/work`, and `/tmp` if granted; HAND L57-62, SPEC 8 L287).
- The sandbox must not be able to reach the daemon socket. `store.view.*` are Admin acts (SPEC 6.3 L239), so the supervisor outside the sandbox calls them. R9 already uses the admin-scope local socket (R L48).

## 3. Sequence

"Exists" means built and tested on a branch, not necessarily merged to `target-0.8.5`.

| # | Step | Our side | Tapestry launcher side | PG end-to-end | State |
|---|---|---|---|---|---|
| 1 | Decide scope | daemon builds the spec from the governance grant (SPEC 3, L106-109). Until then the operator supplies `--spec` (SPEC-V 6.3) | `governance.check` already gates tool calls (R L5-12); it does not yet produce a view spec | operator-written spec | grant derivation: planned. Operator spec: built |
| 2 | Open | `weaver store view open --name <store> --root <abs> --spec <file>`, RPC `store.view.open`. Reply names the `view.json` path (VERBS L113-122) | reads `view.json`, validates schema and paths under `root` | `open` on a throwaway RustFS | library and verb built. Launcher: item 10, blocked. PG check: not run |
| 3 | Run | none, the library is idle | spawn bwrap from `binds` in order, Landlock inside the child, record the ABI | checks 1, 2, 4, 5 | launcher: items 9 and 10. PG: not run |
| 4 | Refresh | `store.view.refresh {root}` on a timer, on a store notification, or before a command. Never writes the store (SPEC 5, L152-161) | supervisor makes the call; `base/` changes appear live through the ro bind | step times recorded (HAND L94) | verb built. Supervisor wiring: Tapestry's |
| 5 | Renew | `store.view.renew {root}`, needed before publish once the 15 minute TTL is near (SPEC 5 step 7, L184) | supervisor calls it. Nothing in `view.json` tells it to | not in the five checks | verb built. Caller undecided, see section 6 |
| 6 | Stop the sandbox | none | kill bwrap and wait for exit before step 7. Reading an overlay upper under a live mount is not defined by the spec | "leave the session" (HAND L77) | proposed order, see section 6 |
| 7 | Publish | `store.view.publish --name <store> --root <abs>`: lease check, tamper check, collect changes, `sync_up` with `If-Match` (SPEC 5, L162-172) | reports the publish report to the user | overlay check: puts, delete, tombstone in the store listing | verb built. PG: not run |
| 8 | Resolve | `store.view.resolve --path <rel> --keep local\|remote` after a 412 conflict | surfaces the conflict list | not in the five checks | verb built |
| 9 | Close | `store.view.close --name <store> --root <abs> --publish\|--discard` (exactly one) | removes nothing itself, the library removes `view.json`, `base/`, `lower/`, `work/`, `.state/` by exact path | root left empty | verb built |

Activity and chain records: `ViewOpen`, `ViewPublish`, `ViewResolve`, `ViewClose`, counts only (VERBS L88-95, SPEC-V 6.3). `status`, `refresh`, `renew` record nothing.

## 4. Alignment table

### 4.1 Tapestry requirements

| Requirement (source) | Answered by | Gap |
|---|---|---|
| Item 10: filesystem half via the scoped view, depends on item 9 and E-1 (P L56) | SPEC 2, 4, 6.2, `view.json` section 2 here | none on our side. Item 9 comes first |
| E3 exit: Linux sessions run under bwrap/Landlock with the rustfs view as their filesystem (PLAN L126, P L10-11) | HAND L70-87, checks 1 to 5 | no PG result yet |
| E-3: CI runners for Linux backend tests (P L111) | HAND names `weftos-ci`; SURVEY L9-23 shows `weftos-ci` has bwrap 0.11.1 with `--overlay`, Landlock ABI 7, kernel 6.18 | none |
| Policy decides before the tool runs, fails closed (R L32-44) | SPEC 8 (L279-288): view and bwrap enforce after that decision | R9 says "Not a sandbox" (R L68), consistent. No view-specific deny reason exists yet |
| Process and network isolation are Tapestry's (P L36-39) | SPEC 8 leaves them to the launcher | none |
| Tapestry has no way to learn whether the manifest is a root image or a project view | HAND L40-47 describes both, SPEC 2 L47 says `/` "or under a chroot path" | `view.json` has no field for it. `binds[0].dest` is always `"/"` (tree.rs L330). Needed: a launcher setting or a manifest field, owner call |
| Spec comes from governance, not the session (SPEC 3 L106) | VERBS take an operator spec (SPEC-V 6.3) | who builds the spec for a Tapestry session today is undecided |

### 4.2 PG end-to-end checks (HAND L70-87)

| Check | Spec section | Gap |
|---|---|---|
| 1. C2 isolation: `/` read-only, writes only in `/work`, `.state` invisible, no network | SPEC 2 (L47-54), 8 (L283-287) | none. `.state` is invisible because it is not in `binds` |
| 2. Overlay end to end: edit, create, `rm`, `rm -r`, leave, publish, check store | SPEC 5 step 4 (L162-172), 7 (L260-262) | whiteouts are detected as char device 0/0 but never exercised (HAND L29-30). This is the first real run |
| 3. `rmdir d && mkdir d` opaque directory | SPEC 7 last row (L277), 9 item 4 | library does not read the xattr. Expected result: lower files under `d` survive publish. Owner decision on `libc` |
| 4. `clone` route on a host without unprivileged overlayfs | SPEC 4 (L129-131), 7 | no surveyed host lacks `bwrap --overlay`. PG fails bare `unshare -Urm` but bwrap overlay works (SURVEY L22-23, L29). Force the route with `work_start: clone`, not with the host |
| 5. Hard links: `base/` and `.state/drives/*` on one file system | SPEC 4 (L129-131) | `/mnt/d` is 9p on WSL and cannot hold the root (SURVEY L31). Put the root on the distro ext4 |

### 4.3 Q9 / R9 wording fix, proposed for the Tapestry doc owner, not applied

Sources: DEC L9 ("scoped rustfs view instead of bind mounts"), PLAN L68 and L172, SPIKE L18 (already says the wording needs a fix).

Proposed one-line edit for PLAN R9 (L68) and L172: replace "the filesystem side uses a scoped view from our own rustfs implementation instead of bind mounts" with "the filesystem side bind-mounts a materialised, scoped view of the store, not a FUSE mount of the bucket".

## 5. Failure modes both sides must agree on

Library behaviour is SPEC 7 (L255-277). This table adds what each side sees.

| Failure | Daemon / verb result | Launcher must | PG should check |
|---|---|---|---|
| Lease lost mid-session (taken over or deleted) | publish and renew fail `LeaseLost`, nothing written. `close --discard` leaves the other holder's lease alone | keep the sandbox running, show the error, offer discard. Never retry by editing `view.json` | not in the five checks |
| Lease expired by clock | `LeaseExpired`. `renew` succeeds if the object is still ours | call renew, retry publish once | no |
| Store unreachable at refresh | error, view intact, `base/` locked again, failed include keeps old content | keep running on stale base, log it | no |
| Store unreachable at publish | error, work untouched, change still pending | keep the work tree, retry later | no |
| 412 on publish | conflict record, local file kept, remote copy beside it in the work drive, path held open | show conflicts, call `resolve`. Do not call `close --publish` until clear | recommended, not in the five checks |
| Close with unpublished work | `--publish` publishes first and refuses with `Unpublished` while conflicts stay open, removing nothing. `--discard` drops work | pass one flag explicitly, never default to discard | root left empty after close |
| `.state` exposure attempt | not bound, so absent. A symlink planted in `/work` is skipped, never uploaded (SPEC 7 L266) | never add a bind outside `binds` | check 1 |
| Control names created in `/work` (`.drive`, `.wt`) | top-level `/work/.drive/` ignored silently, deeper ones reported `Reserved` (SPEC 7 L268) | none | optional |
| Opaque directory in overlay upper | not detected, lower files survive publish (SPEC 7 L277) | optionally report opaque dirs if owner picks that route | check 3 |
| `refresh` while overlay mounted | `work_not_refreshed: true`, base still refreshes (HAND L64-68) | do not expect remote work changes in `/work` | overlay check |
| Crash mid-open or mid-publish | next open recovers; next publish retries staged paths (SPEC 7 L270-271) | on restart call `status`, then `open` or `publish` | no |

## 6. Open decisions

Items 1 to 7 are copied from SPEC 9 (L302-322). Items 8 to 12 are new from this pass.

| # | Decision | Tag |
|---|---|---|
| 1 | Deny writes to `.drive/leases/*` and tombstones from anything but the drive principal in `policy.rs` | owner |
| 2 | Per-prefix store credentials (STS) so the read-only half is enforced at the store | owner |
| 3 | Lease TTL of 15 minutes, and whether the daemon renews on a timer while a session runs | owner |
| 4 | Overlay mode: take `libc` for the opaque xattr (diff gate), or the launcher reports opaque dirs, or ship `clone` only on Linux | owner, with PG result as input |
| 5 | `empty` never refreshes the work prefix during a session: acceptable, or use `clone` for live remote changes | Tapestry |
| 6 | Where the root lives: under the runtime dir, outside `.weftos` state, same file system as the drive cache | owner, PG verifies on `weftos-ci` |
| 7 | Reword R9 per section 4.3 | Tapestry doc owner, after owner confirms (SPIKE L18) |
| 8 | Who builds the spec for a Tapestry session until governance derives it: operator file, or a `store.view.open` param from the supervisor | owner |
| 9 | Root-image versus project view: add a manifest field, or the launcher decides by its own config | owner, Tapestry |
| 10 | Who renews the lease. The manifest carries `expires_ms` but no renew endpoint. Proposal: the supervisor renews before each publish and when under 1/3 of the TTL remains | Tapestry |
| 11 | Close order: kill the sandbox, then `publish`, then `close`. The spec shows the lifecycle but not this ordering explicitly | owner confirms, Tapestry implements |
| 12 | Where `view.json` validation lives. HAND L59 asks the launcher to refuse paths outside `root`; the library validates only schema, spec hash and root on load (view.rs L376) | Tapestry |

## 7. Hand-offs

- **Tapestry backend:4 (read-only pointers, no edits by us).** Read section 2 of this note and SPEC 6.2. Build the manifest reader and bwrap argument builder against `ViewManifest` (spec.rs L206-249). Start with item 9 now; item 10 needs only the manifest and the RPC names. Raise decisions 5, 9, 10 and 12.
- **weftos-pg.** Run the five checks in HAND L70-87 on `weftos-ci` (bwrap 0.11.1, ABI 7, SURVEY L11-23). Put the root on ext4. Force the `clone` route with the spec, not the host. Record whether hard links held, and the exact bwrap command. Write results to `docs/grok-build-fork/spike-results/`. Add a read-only check that `kernel.sock` is unreachable from inside the sandbox.
- **Still owed by this session (verbs lane).** Merge or hand over `wt/adr126-e1-view-verbs`. Update SPEC on the library branch, which still says the verbs are "specified, not built" (SPEC L5-6, L237) and lists five verbs where the code has seven. Fold decisions 8 to 12 into SPEC 9 if the owner agrees.

## 8. Where sources disagree

1. SPEC (library branch) and HAND say the verbs are not built. The verbs branch has all seven (open, status, refresh, publish, resolve, renew, close). Code wins; SPEC-V 6.3 is correct.
2. SPEC 6.3 lists five verbs (open, status, refresh, publish, close). Code and SPEC-V add `resolve` and `renew`.
3. SPIKE step 3 says the working tree is "the overlay upper" but its bwrap command uses `--bind <work> /work` (SPIKE L11 row 3 and step 3 at L55-60). The library has both: `clone` is `--bind`, `empty` is `--overlay`.
4. DEC Q9 and PLAN R9 say "instead of bind mounts". bwrap works by bind mounts. SPIKE L18 already notes this.
5. HAND check 4 assumes a host without unprivileged overlayfs. SURVEY shows no such host: all have bwrap overlay, and PG only blocks a bare `unshare`.
6. SPEC 6.2 shows an example with a work tree. In code a read-only view writes `work`, `work_start` and `lease` as `null`.
