# ADR-126 S4 merge note for s5 (2026-11-08)

**Branch to merge:** `integ/adr126-s1-s4` (local, unpushed). It is S1's `feat/adr126-s1` (tip `19de450e`) merged with the S4
work. `feat/adr126-s4` itself is still the original `f8419157` and does **not** build the `store` feature alone: S4's
store wiring needs the `weftos-store` crate that only S1 carries. Merge S1 first or take the integ branch whole.

A trial merge of the integ branch into `origin/master` is clean (no conflicts) as of this note.

## S4 commits (oldest first, all on the integ branch)

`28c2a65e` ReleaseBackend over weftos-store, `93f5accc` sweeper copy, `0e2c7334` `weaver store release publish`,
`d3a4b45b` blocking-client fix, `c0a651ea` plan and spike, `14164e1a` Object Lock, `bcc7d4be` non-root CI keys,
`3322d291` history and CI routes, `6358df22` mock seams, `3058f458` real S1 calls, plus the commit carrying this note
(service-route extension, release archive report).

## Diff gate (owner only; nothing here was edited)

New path dependencies the gate will flag, to be approved by the owner in `config/diff-gate-approvals.toml`:
`weftos-release -> weftos-store` (optional, feature `store`), dev-dependency `weftos-store` (feature `rustfs`) plus `tempfile`,
`clawft-weave -> weftos-store` and `clawft-weave -> weftos-release`. No `unsafe`, no `build.rs`, no CI-file change.

## Proof

Targeted, no workspace suite: `cargo test -p weftos-release --features store` (release_store 48, ci_route 10, store_backend 5),
`clawft-weave` release verb unit tests (7), clippy `-D warnings` clean for both crates. Live on a throwaway RustFS 1.0.4
(ignored tests, `WEFTOS_RELEASE_TEST_RUSTFS_ENV`): `store_live` 3, `ci_route` daemon route 1, verb 2, non-root key 1.
RustFS was stopped after each run.

## Behaviour a reviewer should know

- Default CI route is `Direct` (`DEFAULT_CI_ROUTE` in `crates/weftos-release/src/route.rs`, one line). The verb refuses `--ci-route daemon`
  with a message saying the CLI has no daemon client yet.
- A locked publish refuses an unlocked bucket before writing. A reused blob gets the later date but keeps its first lock mode.
- The 512 MiB file limit is gone; large files go up as multipart. A retry re-downloads existing blobs once (no streaming get).
- Archive retention is a dry-run report only; no number is set anywhere in S4.
- A locked restore by the sweeper needs a put grant the owner has not given; the `sweep` role is read plus delete only.

## Open items

Owner: route pick, sweeper put, diff-gate entries, lock modes and floors per channel, archive days.
S1: delete-marker expiry in lifecycle rules, streaming or ranged get, retention read on the service route.
