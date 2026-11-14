# weftos-worldmodel-dataset

Dataset archiver for the LeWM world-model loop (plan `docs/plans/world-model-data-and-training.md`, W1).
It takes a finished sensor session directory and copies it into a versioned, checksummed dataset.
Not published (`publish = false`).

## What it does

1. Finds the session files (names are configurable, see below).
2. Validates every `spatial.evidence.v1` line against the ADR-111 section 1.1 envelope and aborts on the first
   bad line with its line number. Unknown record types are counted, not fatal. `provenance.receipt` must be
   unique across the whole file (ADR-111 section 1.1); a repeat aborts and the error names both line numbers.
3. Validates SenML readings packs minimally: JSON array, at most 256 records, `bn` in scope and ending in `/`,
   `proof_` on every record, `bn` a valid id plus `/`, and no unknown must-understand field (a name ending in `_`
   other than `proof_`).
4. Builds `manifest.json` (`lewm.dataset.v1`): session, room (the single region id), `shell_measure` records,
   node `pose` records, protocol, consent, proof counts per type and overall, time span, SHA-256 and size per
   file, the layout used, archiver version and an RFC 3339 `created_at`.
5. Copies the files to `<dest>/ruview-demo/datasets/<session>/` through a temp directory, re-reads the copies,
   checks each against the checksum taken from the source, and only then renames into place.

```
weftos-worldmodel-dataset archive <session_dir> --dest <root> [--session ID] [--protocol TEXT] [--consent TEXT] [--overwrite]
weftos-worldmodel-dataset verify <root>/ruview-demo/datasets/<session>
```

## Store upload (second destination)

With `--store` (library: `ArchiveRequest::store = Some(StoreTarget {..})`) the archiver, after the filesystem copy
verifies, runs `weaver store put <store> <prefix>/<session>/<file> --as <principal> --file <path>` once per archived
file and then for `manifest.json`. `--store` is off by default so the plain copy works where no store is set up.

- The principal comes only from `--as`. Nothing is read from the environment.
- It stops at the first failure. Later objects are recorded as `skipped`; a failed object keeps the tail of stderr.
- Results go into a `store_upload` section of the manifest copy on disk and into the returned `ArchiveOutcome`.
  The file checksums are unchanged (`manifest.json` is not in its own file list) and `verify` still passes.
  `manifest.json` is uploaded last, after the section is written, so the on-store manifest is the one with the
  section; it lists the data files only, since it cannot record its own upload.
- `--store-dry-run` prints the commands and runs nothing; the manifest is not modified.
- A store failure keeps the verified filesystem copy and makes the CLI exit non-zero.
- `--weaver-bin` / `StoreTarget::weaver_bin` overrides the `weaver` on PATH (tests use a fake script).

Diff gate: spawning `weaver` is a `process` finding in `scripts/diff-gate.sh`. The feature is owner-approved; the
approval row in `config/diff-gate-approvals.toml` is added by the owner only.

## Finished marker and in-progress guard

`SessionLayout` has `finished_marker` (default `{session}.done`, `None` disables it) and `require_finished`
(default false). The marker is never archived as a CSI file. The manifest records `finished` (marker present).
With `require_finished` set and the marker absent, archiving refuses with `NotFinished`; a layout that requires
the marker but disables it is a `BadLayout` error.

Independently of the marker, every source file's size and mtime is recorded before validation and hashing and
checked again after the copy, before the checksum verify. A change means a recorder is still writing, and the
run is refused with `SourceChanged`.

Archiving refuses when consent is absent (no `consent.json` and no explicit consent), when the evidence spans
more than one region, and when the destination exists without `--overwrite`.

## Not checked here

- Per-type geometry rules. The reference validator is `weftos-spatial-core` in the spatial workspace.
- SenML quantity names, units, ranges, value fields, entities and time rules (`contracts/sensors/quantities.v1.json`).
- Anything about CSI file contents. The recording is hashed and copied as opaque bytes.

## Verify

`verify` re-hashes every file and also recounts the archived evidence and readings files, comparing lines, per-type
and proof counts, unknown types, room, time spans, `shell_measure`, `node_poses` and the readings counts with the
manifest. A copy whose bytes match but whose manifest was edited fails and prints each disagreement.

## Conformance test

`tests/contracts_vectors.rs` runs the `contracts/sensors/vectors` set (path via `CARGO_MANIFEST_DIR`) through the
archiver's checks:

- Every `valid` evidence and SenML vector must be accepted.
- Invalid evidence vectors must be rejected when the rule is an envelope rule: classes `version` and `size`, and
  `text`, `shape`, `range` when the offending field is an envelope field (the `env_*` files, except the
  coordinate bound, which is a per-type field). Per-type geometry vectors must not be rejected here and are
  counted as out of scope. The unknown-type vector must be accepted and counted.
- Invalid SenML vectors must be rejected for: not JSON, not an array, empty pack, too many records, over-long line,
  missing `bn`, `bn` without a slash or with bad characters, missing or bad `proof_`, unknown `_` field. The rest
  (units, ranges, values, entities, time, `bver`) are vocabulary rules and count as out of scope.

It prints one summary line per group and fails only on a wrong verdict. Run with `-- --nocapture` to see it.

## RuView side: not confirmed

Owner accepted these defaults on 2026-11-14 (plan section 1b); the open items are the RuView-side confirmations.

There is no RuView checkout here. The defaults follow the plan's description of RuView's session recording
(`<data-dir>/recordings/<session>.evidence.jsonl` next to the CSI recording) and everything else is a guess,
kept in `SessionLayout` so a wrong guess is a config change:

| Role | Default template |
|------|------------------|
| evidence (required) | `{session}.evidence.jsonl` |
| readings | `{session}.readings.jsonl` |
| CSI | `{session}.csi*`, `{session}*.csi.*` (top-level files only; the second pattern also matches a longer session id that shares the prefix) |
| protocol | `{session}.protocol.json`, `{session}.protocol.txt`, `protocol.json`, `protocol.txt` |
| consent | `{session}.consent.json`, `consent.json` |
| finished marker | `{session}.done` (a guess; whether RuView writes one is unconfirmed, so `require_finished` is off by default) |

`node_poses` is keyed by the pose record's `node_id`, not `source_id`, because one tape measure (one
`source_id`) places every node. The full records, including `source_id`, are kept verbatim.

Fixtures in `tests/fixtures/session/` are built from `contracts/sensors/vectors` with `t_ns` and receipts varied
and every proof tag left as the vector has it. The CSI file is 512 bytes of filler, not real CSI.
