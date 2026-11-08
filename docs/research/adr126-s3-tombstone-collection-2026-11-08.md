# ADR-131 drive: tombstone collection (design and prototype), 2026-11-08

Status: prototype behind a default-off switch. Code in `crates/weftos-store/src/drive/gc.rs`,
tests in `tests_gc.rs`, one live test in `live_hardening.rs`. The retention number is the owner's.

## 1. What a tombstone is for, and what it is not for

A writer is denied `.wt/*`, so a delete leaves a create-only marker at
`<prefix>.drive-tombstones/<path>` with `{device, ts_ms, etag}`. Reading the code as it stands:

- **Delete detection does not use the marker.** `sync_down` treats "in my index, missing from the
  listing" as a remote delete. An unmodified local file is removed, an edited one becomes a
  `RemoteDeletedLocalModified` conflict. `sync_up` of an edit finds the object gone (`NoObject`) and
  records the same conflict. Neither consults the marker to decide.
- **What the marker supplies:** the deleter's name in the conflict record ("remote deleted by X"),
  and the rule in `write_tombstone` that a stale writer's older marker never replaces a newer one.

So dropping a marker costs the name in a later conflict and nothing else. That is the property the
rest of this note relies on, and it is pinned by `a_device_that_syncs_after_collection_loses_only_the_deleters_name`.

A marker is cleared when the path is created again (`clear_tombstone` on every create), so only
markers of paths that stayed deleted accumulate. That is the backlog collection exists for.

## 2. Retention assumed

A marker older than R is collected. The drive has no device registry, so "every device has synced
past this delete" cannot be known; R stands in for it. The age is read from the marker body
(`ts_ms`), because `ObjectMeta` has no mtime. A body without a usable `ts_ms` is never collected
(counted as `undated`). All markers written by `write_tombstone` carry one, so this only matters for
foreign objects.

Candidates for R and what each trades:

| R | Trade |
|---|---|
| Hours (above the 30 min claim TTL / 10 min claim skew in `share.rs`) | Smallest backlog. A laptop closed overnight loses the deleter's name. |
| 7 days (placeholder) | Covers a weekend and a week away. A backlog of at most a week of deletes. |
| 30+ days | Names survive long absences; the backlog grows with delete churn. |
| Tied to a lease/claim horizon | The horizons that exist (claim TTL 30 min, skew 10 min) are minutes, far too short for a person's device; do not tie it to them. |

The default is `PLACEHOLDER_RETENTION` = 7 days. It is a placeholder only: it is a round number
above every machine-to-machine horizon in the code and below a month. The owner picks the real one.

## 3. Who collects, and when

`collect_tombstones(backend, bucket, prefix, &Options, now_ms) -> GcReport`, called from `sync_up`
after a successful push when `Options.collect_tombstones` is set (default false). With the switch
off the function returns an empty report without touching the store; `sync_up` does not call it.
Choice of `sync_up` over a separate daemon verb for the prototype: it needs no new scheduling and
the function is public, so the ADR-131 daemon loop can call it directly instead. Owner question 2.

Per pass: list `<prefix>.drive-tombstones/` only, sort by key, read at most
`Options.tombstone_gc_cap` bodies (default 500, one `get` each), and delete each marker older than R
with `delete_if_match` on the listing's etag. Consequences:

- **Replaced since the listing:** the `If-Match` is refused, the marker survives, `raced` counts it.
- **Gone already:** `vanished`. **Backend without conditional delete:** nothing is deleted (`unsupported`), never a blind delete.
- **Idempotent:** a second pass over the same store collects nothing new.
- **Bounded:** past the cap the report says `capped`; the next pass continues. Key order means a long
  run of young markers at the front is re-read each pass before older ones behind them; with the
  default cap and a one-week window this is a small cost, but it is a known weakness.
- **Scope:** only keys under `<prefix>.drive-tombstones/` are ever read or deleted. A GC error never
  fails the sync; it lands in `UpReport.gc_error`.

## 4. Interaction with the hardening rules; a device offline longer than R

What the hardening lane proved (delete, recreate, delete again; stale device cannot win) rests on
the create-only put, the conditional replace and the newer-marker rule. Collection only removes
markers that are at least R old, so a marker involved in a live race (written within seconds) is
never a candidate unless R is shorter than the race.

For a device offline longer than R that still holds the file, nothing in the sync rules changes,
because they never read the marker to decide:

- file unmodified locally: removed on `sync_down`, as before;
- file edited locally: conflict on `sync_down` or on `sync_up`, the edit is kept and not re-uploaded,
  as before; the record just lacks "remote deleted by X";
- file untracked and created again at the same path: create-only put, as before.

I therefore did **not** add a `stale_local` state. The lead's suggestion (keep and report instead of
re-uploading) is what an edited file already gets, and an unmodified file is correctly removed.
Adding a new blocking state would change behaviour for no new safety. If the owner wants the
deleter's name guaranteed, that is a longer R, not a new state.

One narrow case stays: a delete that is itself older than R by the time a second delete of a
re-created path happens writes a fresh marker, as before.

## 5. Not solved

- No device registry, so R is a guess about how long devices stay away; nothing proves they synced.
- No clock authority: `ts_ms` is the deleting device's wall clock. A device with a clock far behind
  writes markers that look old at once; a clock far ahead writes markers that live long. Bounded by
  the same newer-marker rule, not fixed.
- Every create still sends one extra tombstone DELETE (RustFS answers 204 either way). Collection
  does not change that.
- A death between a delete and its tombstone still loses the deleter's name.
- Two real machines are untested; the live test ran on a loopback RustFS with a shrunk R.

## 6. Owner questions

1. The retention number R (placeholder 7 days).
2. Does collection belong in `sync_up` (as now) or only in the ADR-131 daemon loop?
3. Should anything stronger than the existing conflict behaviour apply to a file kept past a
   collected delete? This design says no (section 4); say if a `stale_local` state is wanted anyway.

## Results

`scripts/build.sh test weftos-store --features rustfs`: before 148 passed, 20 ignored (per the
hardening section of the materialise note); after 157 passed, 21 ignored. 9 new in-process tests,
1 new live test (all 8 `live_hard` tests pass on a throwaway loopback RustFS; timing noisy, load 6-7).
