# ADR-126 S3: materialise plus watch over the object store (2026-11-08)

Branch `wt/adr126-s3-drive`, based on `220952fe9`. Prototype of the ADR-131 section 1 to 3 path
(materialise from a local content-addressed cache, watch, seal after quiet, conflicts recorded)
over the ADR-126 store. Library only, in `crates/weftos-store/src/drive/`. No daemon verb: the
tests are the driver. Nothing ran against PG, aepod or any daemon.

## What was built

| Piece | Behaviour |
|---|---|
| `materialise(backend, bucket, prefix, dest)` | Lists the prefix, fetches into `<dest>/.drive/cas/<hh>/<blake3>`, places files with `std::fs::copy` (APFS `clonefile` on this Mac), writes `.drive/index.json` (path, size, mtime ns, blake3, store etag). A second call is a refresh: only objects whose etag or size changed are fetched, remote deletes are applied and logged |
| `Watcher::poll`, `watch_once` | Scan against the index: size and mtime first, hash only when one moved, so a touch is not a modify. A batch is reported once the scan result has stopped changing for `quiet` (default 2 s), and only once until a sync updates the index. `poll` takes the clock as an argument so tests do not sleep |
| `sync_up` | Create is a create-only put. Modify needs the remote etag to still be the one last synced against. Delete removes the object and writes a marker. Anything else is a conflict |
| `sync_down` | Same engine as the refresh. A remote change over a locally modified (or deleted) file is a conflict |
| Conflicts | Local file kept. Remote bytes written beside it as `<name>.remote-<8 hex of the remote hash>`. One line appended to `.drive/conflicts.jsonl`. The path is held open in the index: no upload, no overwrite, until `resolve(dest, path, Keep::Local or Keep::Remote)`. Four kinds: both modified, both created, remote modified/local deleted, remote deleted/local modified |
| Refusals | The existing `is_secret_path` denylist and private-key content check, `is_dangerous_path`, `validate_key`, `safe_join` (symlinked parents), the S2 `Limits` ceilings (16 MiB a file, 20,000 files, 512 MiB), case-only collisions, the drive's own `.drive/`, and the daemon's `worktrees/` area of a `p-` bucket. Every refusal is returned in `skipped` |
| `drive-notify` feature | Optional. `watch_once_events` wakes the loop on FSEvents or inotify. The scan still decides |

Files: `mod.rs`, `cas.rs`, `index.rs`, `scan.rs`, `watch.rs`, `remote.rs`, `materialise.rs`,
`sync_up.rs`, `conflict.rs`, `tests.rs`, `live_tests.rs`; each under 500 lines. `share.rs` changed
by two visibility words (`read_regular`, `looks_like_private_key` now `pub(crate)`).

## Numbers

Tests, `scripts/build.sh test weftos-store --features rustfs`:

| | Before | After |
|---|---|---|
| Passed | 114 | 132 (18 new) |
| Ignored | 11 | 12 (the live test) |
| With `--features rustfs,drive-notify` | n/a | 133 passed |

`scripts/build.sh clippy weftos-store --features rustfs,drive-notify` is clean.
`scripts/diff-gate.sh --worktree` passes: a `workspace = true` entry for a dependency the root
already declares is not counted as new, so `notify` could be added (optional feature only).

Live run, throwaway RustFS 1.0.4 on loopback, console off, random credentials in a 0600 file, data
on the internal APFS disk, machine load average about 7 to 9. One run, so read these as one sample.
Test: `drive::live_tests::live_drive_cycle`.

| Step | Time |
|---|---|
| Seed 500 objects of 1 to 4 KiB (sequential puts) | 4,582 ms (9 ms each) |
| First `materialise`, 500 files fetched | 435 ms |
| Second device, first `materialise` | 342 ms |
| Refresh, nothing changed (one listing, no GETs) | 55 ms |
| Watch, 3 local changes, default 2 s quiet | 2,019 ms |
| `sync_up` (2 puts, 1 delete plus marker) | 299 ms |
| One watch-and-sync cycle | 2,318 ms, of which 2,000 is the quiet period |
| Other device `sync_down` (2 GETs, 1 removal, 1 conflict) | 52 ms |

Clone check: `std::fs::copy` of a 400 MB file on this APFS volume took 147 microseconds and free
space did not drop, so the standard library clones on macOS and the crate needs no `libc`.

## Design calls

- **Scanner, not events, as the baseline.** The index already holds size and mtime, so a scan needs
  no dependency and cannot lose an event. FSEvents can drop events (ADR-131 section 3.2), and a
  rescan is the repair anyway. `notify` is allowed by the gate, so it is an optional wake-up only.
- **Conflict rule.** Never pick a winner. The local file is untouched and the remote copy name is
  derived from the remote hash, so the same remote version always lands in the same file. An open
  conflict blocks both directions for that path, so it cannot be re-recorded or lost.
- **Tombstones.** One small JSON object per deleted key at `<prefix>.drive-tombstones/<path>`
  (device, time, deleted etag), removed when the key is written again, as in ADR-126 section 4. The
  section 4 location `.wt/tombstones/` is not used: it is control state the daemon alone may write
  (the policy denies `.wt/*` puts), and it serves the worktree-over-project overlay, not a single
  prefix. The marker today only names the deleter in a conflict record.
- **CAS layout.** `cas/<first two hex>/<blake3>`, written once through a temp file and rename. A
  placed file is a clone, so editing it never reaches the cache. Identical objects (same etag) are
  fetched once.
- **Compare-and-set.** Create is a create-only put and has no window. Modify and delete now send the
  etag with the write when the backend supports it (see "Conditional put and read-only"); the
  prototype first shipped head-then-write, whose window remains on a backend that answers
  `Unsupported`.
- **Error posture.** A bad path or a failed placement is a `skipped` entry, not a failed sync. The
  index is always saved, even after a backend error, so what was placed is recorded.

## What breaks it

- **Case-insensitive APFS.** Remote keys that differ only by case are reported and the first wins.
  A local case-only rename is a delete plus a create; untested.
- **Symlinks.** Never followed, never uploaded, always reported. Trees that depend on them lose them.
- **Large files.** Whole file in memory, 16 MiB ceiling, no multipart, no chunking. Build output and
  datasets are out.
- **Metadata.** No mode bits (placed files get the cache's), no empty directories, no mtime from the
  store (S1 `ObjectMeta` has none), so a restored `target/` still rebuilds. ADR-131 section 4 fixes
  this with attributes in the layer format.
- **Listing cost.** A refresh lists the whole prefix: 55 ms for 500, not measured at 20,000. The
  scanner's per-poll walk is also unmeasured at that size.
- **Crash leftovers.** `.drive.tmp.*` files from a killed run are ignored by the scan but not
  cleaned. Delete markers are never collected.
- **Not tested:** two real machines, a store with Object Lock or versioning, clock skew.

## Next steps

1. **First targets.** (a) The project bucket `p-<ulid>`, read-write, excluding `worktrees/`, which
   is already excluded. (b) The worktree bucket's `.wt/tree/` is a daemon-written publish area
   (`publish_tree`), and the policy denies writer puts under `.wt/*`. Use it read-only
   (`materialise` and `sync_down`) as the follower's fetch with `Options.read_only` set (built, see
   "Conditional put and read-only"), so a mistaken `sync_up` fails in the library, not at the Deny. (c) Photo-gallery's
   `/data1` import stays owner-gated and out of this work.
2. **Relation to the S2 follower.** `fetch_tree_with` and `sync_docs` already share the refusal
   helpers. Swap their bodies for `materialise`, so a followed tree gets the CAS, the index and
   conflict records. Keep the follower's registry poll as the cheap "what moved" signal, so the
   drive lists a worktree only after its head moved. The audit module already parses S3 bucket
   notifications, which is the push route that removes polling.
3. **Relation to the ADR-131 daemon.** The same functions must become its loop. What carries over
   unchanged: CAS, index, scanner, quiet rule, conflict records and `resolve`. What is replaced: the
   transport. `sync_up` puts per file; the daemon seals a signed layer and sends it to the inbox,
   and `sync_down` becomes a signed-head warm join. Put the transport behind a small trait (list,
   fetch, send changes) so this crate's tests keep using the store and the daemon uses layers.
4. **Conditional put.** Built and forwarded: `put_if_match` and `delete_if_match` on the S1 backend
   trait, used by `sync_up`; RustFS 1.0.4 honours `If-Match` on both (measured). The daemon's
   wrapping backend (`Op` in `store_worktree.rs`, the only non-test implementer of the trait besides
   the three real backends) now forwards both methods, and a test fails if it stops
   (`head_then_write` would be 1, `conditional` 0).
5. **aepod `s3`.** Edge and Wasm nodes hold no long-lived keys (ADR-126 section 5), so the drive
   needs a short-lived credential scoped to one `(bucket, prefix)`: get, list, put and delete on the
   prefix, put and delete on `<prefix>.drive-tombstones/`. That is a new statement in `policy.rs`,
   plus the STS or edge-token wiring aepod owns. The drive library takes any `ObjectBackend`, so
   the credential is the only new piece.
6. **Before anything touches PG** (owner or PG session): a scoped key for the drive principal, not
   the root credential; write access only to a named test prefix first; no restart of PG's RustFS;
   and a decision on the audit webhook, which capped small writes near 43 a second in the mesh mock
   (a 500-file first upload there would take about 12 s).
7. **Loose ends.** `weaver store drive materialise|sync-up|sync-down|resolve|watch` is wired (see
   "The `weaver store drive` verbs"). Open: tombstone collection, `.drive.tmp.*` cleanup, a
   daemon loop in place of `watch`'s one scan per call (ADR-131), and a scan of 20,000 files.

## Conditional put and read-only

Added after the prototype, in three commits on `wt/adr126-s3-drive`.

**Conditional writes.** `ObjectBackend` gained two default methods, `put_if_match(bucket, key, data,
etag)` and `delete_if_match(bucket, key, etag)`. The default answers `BackendError::Unsupported`, so
existing backends compile unchanged. A different etag is the new `PreconditionFailed` (HTTP 412), a
missing key is `NoObject`. Delete needed one too: `sync_up`'s delete path has the same window as
modify, and a conditional put cannot delete. `MemBackend` is atomic under its lock. `FsBackend`
serialises writes inside the process with a lock, so compare and rename cannot interleave; another
process on the same directory is not covered. `RustfsBackend` sends `If-Match: "<etag>"`.

`sync_up` modify and delete try the conditional write first. If the store refuses (412 or key
gone), it takes the old head path to classify the case, so the conflict kinds and records are the
same as before. If the backend answers `Unsupported`, it does the old head-then-write. `UpReport`
counts both: `conditional` and `head_then_write`. The race test: a remote change after
`materialise` makes the next `sync_up` produce `BothModified` and `RemoteModifiedLocalDeleted`
conflicts and leaves the remote bytes untouched.

**RustFS 1.0.4 answer: it honours `If-Match` on PUT and on DELETE.** Throwaway server on loopback,
console off, random credentials in a 0600 file, test
`drive::live_tests::live_if_match_is_honoured_or_not`. Observed:

| Request | Result |
|---|---|
| PUT with the current etag | succeeds, new etag returned, bytes replaced |
| PUT with a stale etag | 412, object unchanged |
| PUT with an etag on a missing key | 404, nothing created |
| DELETE with a stale etag | 412, object still there |
| DELETE with the current etag | succeeds |

The test was run three times: the first two printed nothing because nextest hides output (set
`NEXTEST_SUCCESS_OUTPUT=final` to see it); all three passed with the same result. One server
version, one node, single drive, so this says nothing about distributed mode. The modify and delete
window is closed on RustFS; it stays open only on a backend that answers `Unsupported`, where a
writer landing between the head and the write still wins silently.

**Read-only.** `Options.read_only` makes `sync_up_with` return `DriveError::ReadOnly` before it
scans or touches the store. `materialise_with` and `sync_down_with` work as before. A worktree
bucket's `.wt/tree/` is a daemon-written publish area, so followers use it with `read_only` set;
this is in the `drive` module docs too.

**Forwarding and tombstones.** The daemon's wrapper `Op` forwards the two methods, so the drive
through the daemon reports `conditional` (test in `store_worktree_tests.rs`). Tombstone writes are
create-only: the put refuses an existing marker; then a marker that is newer than this writer's
clock, or that names the same deleted etag, stays; an older one is replaced with `put_if_match`
against the etag just read, and left alone on a backend with no conditional write. A stale writer
therefore never overwrites a newer tombstone. This does not stop a tombstone landing after a
revive (a delete whose marker write is delayed past a re-create); only the next `sync_up` of the
key clears it.

Plain `sync_up(backend, dest)` has no read-only form (use `sync_up_with`).

## The `weaver store drive` verbs

Thin calls into the library through the daemon RPC (`store.drive.<verb>`, `store_drive.rs`), wired
like the S2 verbs: an Admin act on the raw data plane, paths checked by the same `Guard` as the
worktree verbs (absolute, no `.` or `..`, resolved through symlinks, outside the daemon's own state),
the library's secret denylist and ceilings (`Limits::default()`) unchanged.

```
weaver store drive materialise --name <store> --dest <abs dir> [--prefix P] [--read-only]
weaver store drive sync-down   --root <abs dir> [--name <store>] [--read-only]
weaver store drive sync-up     --name <store> --root <abs dir> [--read-only]
weaver store drive resolve     --name <store> --root <abs dir> --path <rel> --keep local|remote
weaver store drive watch       --root <abs dir>
```

- `--root` is the existing drive; `--dest` is accepted for it, and two different values are refused.
  A `--name` given with `--root` must match the store the drive is bound to.
- `--read-only` sets `Options.read_only` for that call: `sync-up` then fails with the read-only
  error before it scans or contacts the store.
- `materialise`, `sync-up` and `resolve` write an activity-log entry (`DriveMaterialise`,
  `DriveSyncUp`, `DriveResolve`) and a chain event (`store.drive.<verb>`), success or failure, with
  counts in the payload and not file lists. `sync-down` and `watch` read and record nothing.
- `watch` is one scan per call and does not block. It prints the pending local changes, skipped
  paths and the open-conflict count. A shell loop drives it: call every second, and `sync-up` once
  two reports in a row are the same and non-empty (the quiet rule, kept by the caller until the
  ADR-131 loop exists).
- The output is the report as JSON. `weaver store drive sync-up` shows `conditional` and
  `head_then_write`, so a wrapper that drops the conditional write is visible.

Tests: `store_drive_tests.rs` (round trip with `conditional` reported, conflict and `resolve`, relative
and protected paths refused, read-only refusal, secret skipped, wrong-store refusal, records) and
`store_worktree_tests.rs` (wrapper forwarding).
