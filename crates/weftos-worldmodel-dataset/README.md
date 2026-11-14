# weftos-worldmodel-dataset

Dataset archiver for the LeWM world-model loop (plan `docs/plans/world-model-data-and-training.md`, W1).
It takes a finished sensor session directory and copies it into a versioned, checksummed dataset.
Not published (`publish = false`).

## What it does

1. Finds the session files (names are configurable, see below).
2. Validates every `spatial.evidence.v1` line against the ADR-111 section 1.1 envelope and aborts on the first
   bad line with its line number. Unknown record types are counted, not fatal.
3. Validates SenML readings packs minimally: JSON array, at most 256 records, `bn` in scope and ending in `/`,
   `proof_` on every record.
4. Builds `manifest.json` (`lewm.dataset.v1`): session, room (the single region id), `shell_measure` records,
   node `pose` records, protocol, consent, proof counts per type and overall, time span, SHA-256 and size per
   file, the layout used, archiver version and an RFC 3339 `created_at`.
5. Copies the files to `<dest>/ruview-demo/datasets/<session>/` through a temp directory, re-reads the copies,
   checks each against the checksum taken from the source, and only then renames into place.

```
weftos-worldmodel-dataset archive <session_dir> --dest <root> [--session ID] [--protocol TEXT] [--consent TEXT] [--overwrite]
weftos-worldmodel-dataset verify <root>/ruview-demo/datasets/<session>
```

Archiving refuses when consent is absent (no `consent.json` and no explicit consent), when the evidence spans
more than one region, and when the destination exists without `--overwrite`.

## Not checked here

- Per-type geometry rules. The reference validator is `weftos-spatial-core` in the spatial workspace.
- Receipt uniqueness across lines, and SenML quantity names, units and ranges (`contracts/sensors/quantities.v1.json`).
- Anything about CSI file contents. The recording is hashed and copied as opaque bytes.

## RuView side: not confirmed

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

`node_poses` is keyed by the pose record's `node_id`, not `source_id`, because one tape measure (one
`source_id`) places every node. The full records, including `source_id`, are kept verbatim.

Fixtures in `tests/fixtures/session/` are built from `contracts/sensors/vectors` with `t_ns` and receipts varied
and every proof tag left as the vector has it. The CSI file is 512 bytes of filler, not real CSI.
