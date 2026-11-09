# ADR-126 S4 merge note (close-out, 2026-11-08)

Local only: nothing here was pushed, tagged, released or installed; no production service was touched. All live runs used a throwaway loopback RustFS started and stopped by the test script.

**Branch to merge:** `integ/adr126-s1-s4` (tip `e0447e0e` at this note). It is S1's `feat/adr126-s1` (tip `19de450e` when merged) plus the S4 commits below.
`feat/adr126-s4` itself is still the original `f8419157` and does **not** build the `store` feature alone: the store wiring needs the `weftos-store` crate that only S1 carries.
Merge S1 first, or take the integ branch whole. A trial merge of the integ branch into `origin/master` was clean at the 9:10 pass; rerun it at merge time because `master` moves.

## S4 commits (oldest first, all on the integ branch)

`28c2a65e` ReleaseBackend over weftos-store, `93f5accc` sweeper copy, `0e2c7334` `weaver store release publish`, `d3a4b45b` blocking-client fix, `c0a651ea` plan and spike, `14164e1a` Object Lock,
`bcc7d4be` non-root CI keys, `3322d291` history and CI routes, `6358df22` mock seams, `3058f458` real S1 calls, `2e08bd5c` service-route retention extension and release archive report (dry run),
`304b8da8` Windows-host build id rules and key portability test, `41e9c7c7` live measurement of S1 archive on a locked release store, `ee064a99` key length guard, `db3fdd96` per-segment cap on artifact paths,
`33f9406c` length guard derived from the validators with planted-defect proofs, `e0447e0e` live test reads retention through S1's calls instead of curl.

## Names

Readable `node-<id>` is the node user name (owner decision at the close-out; S1's `opt/adr126-s1-node-names` / `-aliases`). S4 creates no node users and names none: its principals are `cog:` (CI) and the sweeper, which S1 keys as `wk-<hash>`, and its object keys are
`blobs/`, `releases/`, `tombstones/`, `quarantine/`. Nothing in S4 changes with the naming. Checked on a throwaway integration branch (below): S3's `principal_for` and S1's key id for a node principal are the same `node-<id>` string, and the S4 live tests pass unchanged.

## Diff gate (owner only; nothing here was edited)

Run as `scripts/diff-gate.sh --base feat/adr126-s1 --head HEAD` (S4 over S1): **3 findings, all new path dependencies**, to be approved by the owner in `config/diff-gate-approvals.toml`:
`clawft-weave -> weftos-release` (feature `store`), `weftos-release -> weftos-store` (optional, feature `store`), and `weftos-release` dev-dependency `weftos-store` (feature `rustfs`).
No `unsafe`, no `build.rs`, no CI-file change, no net or process finding. (The `process` finding that used to be here, a test shelling out to `curl`, is gone as of `e0447e0e`: the test reads the retention and the refused delete through S1's calls.)
S5's integration tests add a dev-dependency `weftos-release -> weftos-edge-creds`; that one is S5's to list. Against current `origin/master` the gate also reports master's own newer CI files and S1's network code; those are not S4's.

## Proof (all local, targeted)

- Mock and unit: `cargo test -p weftos-release --features store` (release_store 48, ci_route 10 with the live ones ignored, key_portability 5, store_backend 5), `clawft-weave` release verb unit tests 7; clippy `-D warnings` clean for both crates (at `2e08bd5c`; the later commits are tests and a validator line).
- Live, throwaway loopback RustFS 1.0.4 (ignored tests, `WEFTOS_RELEASE_TEST_RUSTFS_ENV`), rerun at `e0447e0e`: `store_live` 3 (round trip, locked versioned bucket with retention read back, S1 seams), `ci_route` 2 (daemon route with extension and the 5-year cap, S1 archive of a locked release store), `store_ci_key` 1. RustFS stopped after each run.
- Cross-lane, throwaway integration branch `closeout/adr126-s4-integ` (S4 + S1 latest incl. readable names + S5; not for merge): `scripts/store/integ-live-test.sh` (S5): the S1 x S4 and S1 x S3 chain-feed live tests, the S2 live tests and the S5 client test passed; one test, `s1_s3_node_bucket_names_agree_but_rustfs_user_names_do_not`, failed because it pins the old naming gap and the gap is now closed by `node-<id>`;
  rewritten as `s1_s3_node_bucket_and_rustfs_user_names_agree` and passing, with the two remaining id-rule gaps documented (below). S5's backends there also needed the sweeper's `copy` call added.
- Reviews of other lanes with failing/guard tests are on `review/adr126-keys-0925` and `review/one-creator-1140` (S4's own branches, not for merge); reports in `/tmp/weftos-owner-0440/s4-*.md`.

## Behaviour a reviewer should know

- Default CI route is `Direct` (`DEFAULT_CI_ROUTE`, one line in `crates/weftos-release/src/route.rs`). The verb refuses `--ci-route daemon` with a message saying the CLI has no daemon client yet.
- A locked publish refuses an unlocked bucket before writing. A reused blob gets the later date (asked blind on the service route) but keeps its first lock mode.
- Build ids are lowercase `[a-z0-9._-]`, no trailing `.`, not a Windows device name; artifact path segments are at most 255 bytes. Both are guarded by tests that derive the limit from the validator.
- The 512 MiB file limit is gone (multipart). A retry re-downloads an existing blob once to verify it (no streaming get).
- Archive retention is a dry-run report only; no number is set anywhere in S4, and nothing S4 does can delete an archive.
- Do not run S1's `archive` or node retire on a release store: it hides the release behind delete markers and cannot finish on a locked bucket (measured). Retire releases with tombstone and sweep.

## TODOs from the earlier note: closed, and what is left

Closed: the 2e08bd5c-era items (service-route extension, release archive report); the Windows-host key and length guards; the readable-name statement; the diff-gate list (now exact); the curl `process` finding; S4's integration tests against a throwaway RustFS.
Not S4's to close (owner or other lanes), none blocks the merge:
- **Owner:** CI route pick (flip `DEFAULT_CI_ROUTE`), the sweeper put grant (a locked restore needs `put`; the `sweep` role is read plus delete), the three diff-gate entries above, lock modes and floors per channel, archive days (owner decision: nothing expires yet).
- **S1:** delete-marker expiry in lifecycle rules, a streaming or ranged get, a retention read on the service route (removes the blind extension ask), refuse `archive` on a locked store, key rules shared across lanes (Windows-host findings K1 to K10).
- **S3 / S1 together:** align node id validation (S1 takes 1 to 61 characters and a trailing `-`, S3 takes 1 to 48 and neither; with `node-<id>` as the user name S3's rule decides which nodes can lease), one creator of the node's bucket and user (S4 review X2/X3), revocation through S1's `suspend_principal` (X4).

## Rebase plan onto landed S1 (rehearsed 2026-11-08)

Rehearsed on the throwaway branch `rehearse/adr126-s4-on-s1-2a8a1327`: the 19
non-merge S4 commits of `integ/adr126-s1-s4`, replayed with
`git rebase --onto 2a8a1327 feat/adr126-s1` onto the local S1 tip `2a8a1327`.

- Result: **no conflicts**. `weftos-release` tests pass on it (default features
  53 tests; `--features store` 68 tests, 2 ignored live tests not run).
- Merge commits of S1 into `integ/adr126-s1-s4` are dropped by the replay;
  S1 arrives from the landed branch instead. The S1 content S4 used is a
  subset of `2a8a1327` (its newest commits are doc and `put_if_match` note only).
- Expected conflict spots if the landed S1 differs from `2a8a1327`:
  `crates/clawft-weave/src/commands/mod.rs` and `store_cmd.rs` (keep both
  sides: S4 adds `release_cmd`), `crates/clawft-weave/Cargo.toml` (S4 adds the
  `weftos-release` dep), workspace `Cargo.toml` members. Resolution: keep both.
- `weftos-release/Cargo.toml` dev-deps: keep one `weftos-edge-creds`/`weftos-store`
  entry each (a merge with S5 once duplicated keys).
- S4 does not re-apply an S1 commit, so no patch-id skipping is needed here
  (S5 does; its rebase must follow S1 landing).
- Real-branch step, after the MaxBook reports S1 landed: `git rebase --onto
  <landed S1 tip> feat/adr126-s1 integ/adr126-s1-s4` (or the same on
  `feat/adr126-s4`, which holds only `f8419157`), rerun
  `scripts/build.sh test weftos-release --features store`, and report.
- Not done: no merge to target-0.8.5, no push, no deploy, no weaver install.
