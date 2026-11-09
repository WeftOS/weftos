# ADR-126 integration status (S1 to S5), 2026-11-08

Where the mesh object store stands across the five lanes: what is built and how it was checked,
what waits for the owner, and what is out of bounds. The ADR
([adr-126-mesh-object-store.md](../adr/adr-126-mesh-object-store.md)) stays Proposed. Nothing here
is installed or running on any machine, and PG was never touched. This is a status report, not a
decision record; the decision-history lookup was unavailable when it was written, so prior decisions
about it are unknown.

## Done, and how it was checked

| Lane | Built | Checked by |
|---|---|---|
| S1 `feat/adr126-s1` | `weftos-store`: names to buckets, grants, governed access, audit, `weaver store`, daemon wiring, RustFS S3 and admin transport, audit and notification receiver, `put_if_match`, release-store roles (`append`, `sweep`), Object Lock and default retention, `list_versions`, lifecycle rules, multipart and streaming put, a circuit breaker and retries with backoff, retire a node's store on unpair, archive retention (`archive_gc`), a gapless `seq` on grant events, owner switches for sweeper put and CI direct put (off by default), `svc.store` | Unit tests; live against a throwaway loopback RustFS 1.0.4 (`--ignored`), including a disabled key getting 403 on its next request and RustFS audit records reaching the activity log |
| S2 `feat/adr126-s2` | Worktree stores: overlay, promotion with land preview and size caps, follower view and wake-ups, handoff and merge by `put_if_match`, union mount, registry store (off by default) with signatures bound to domain, bucket, slug and generation | Integration tests on S1's local-directory backend; shell tests with a real `git bundle` and a real `rclone mount`; live on loopback RustFS: shared worktree, land preview and caps, concurrent claims with one winner, a follower across three identities. The notification wake-up test needs a notify target and `scripts/store/notify-sink.py` and was not run here |
| S3 `feat/adr126-s3` | `weftos-edge-creds`: short-lived STS credentials, presigned URLs, grant ledger fed by chain events, chain-compaction design and spike | 87 unit and loopback tests (2 ignored live) against a mock store and a loopback chain, re-run on the S5 branch after the merge; a live run on loopback RustFS in a container (pairing, 900 s lease, revoke in about 7 ms) |
| S4 `feat/adr126-s4` | `weftos-release`: write-once release artifacts, signed manifests, retention | 28 integration tests on a minimal backend trait, re-run on the S5 branch after the merge. **No RustFS run recorded**; the S1 adapter, grants and `weaver` verb are S1's |
| S5 `feat/adr126-s5` (now carrying S1 through `19de450e`, S2 through `f3b1953d`, S3 through `4943d62d` (with `583623b4`, the owner-decisions page and the user-name options page) plus S1's notes file from `c241d1bd` (file only), the S3 chain-feed e2e test branch `628d0b5a`, and S4 at `f8419157`, all merged locally; merge commits `f38c2ffa` and `f281acc6`) | One store per client account under `/data1/<account>/store` as the client user; the isolation gate; WeftOS-account refusal unless granted; signed cross-mesh grants; gate state on the chain with snapshots, archive and key rotation; one RustFS instance per account, enforced | Unit tests (154 `clawft-weave` store tests; 98 `weftos-store` plus S2's integration tests); 8 live tests on two throwaway loopback RustFS instances, the same script also running S2's 4 live tests (`scripts/store/client-live-test.sh`, every slice under a timeout) |

## Seams between lanes

Tested over a loopback RustFS. **S5 now holds S1, S2, S3 and S4 itself** (merged 2026-11-08; the S3 merge conflicted only in the workspace `Cargo.toml` and the ADR text, the S4 merge was clean). S1 x S5, S2 x S5 and S1 x S2 run from this branch. S1 x S3 and S1 x S4 now run from this branch too: S3's e2e branch `test/adr126-s3-chain-feed-e2e` carries the integration tests (`crates/weftos-release/tests/adr126_integ_live.rs`, `adr126_s3_chain_feed_live.rs`) and `scripts/store/integ-live-test.sh`, and with them the test-only path dependencies in `crates/weftos-release/Cargo.toml` (see "Waiting on the owner": the diff gate flags two lines).

- **S5 x S2 x S1** (on this branch): a client's worktree store is promoted into its project store through the
  service; the promoted bytes are readable through the client gate by a granted cog, exist on the client's
  instance only, the scratch file stays out, and the WeftOS account is refused before and after (for the
  project store and the worktree store).

- **S1 and S5.** The client gate in front of S1's service; per-account instances share nothing (the
  other instance does not know the bucket, and the first account's cog key and root credential are
  refused there); a revoked key is refused by RustFS itself on its next request; a cross-mesh grant is
  a real RustFS policy and revoke and expiry remove it; a second account on an instance is refused with
  a clear error; the client store rides the daemon's own S1 service when both are on the same instance.
- **Why one instance per account is enforced, not just advised.** A principal's RustFS key has a single
  policy, owned by whichever service granted it last. Two services on one instance overwrite each
  other (live test `two_services_on_one_instance_overwrite_each_others_key_policy`).

### All five lanes together (scratch branch `integ/adr126`, local only; the same code is now on `feat/adr126-s5`)

`/home/aepod/weftos-wt/s5-integ` is S5 with S2, S4 and S3 merged, then brought up to date with S1 through
`19de450e` (diff-gate fixes, archive retention) and S2 through `727c0202` (review fixes, the new signing
format). Both updates merged without conflicts. Only the ADR text conflicted (S2), the workspace
`Cargo.toml` and the ADR (S3), and S4 merged clean the first time; `scripts/build.sh check` passed on the
merged tree. Nothing from it is on the S-lane branches. Re-run with
`WEFTOS_STORE_TEST_RUSTFS_BIN=<rustfs> scripts/store/integ-live-test.sh` (every slice under a timeout,
`SLICE_TIMEOUT` seconds, default 1200; about 10 minutes, mostly the `clawft-weave` build; it starts and stops
its own throwaway instances).

| Seam | Result on loopback RustFS |
|---|---|
| S1 and S4: a signed release published through the service by a CI principal holding only `append` | passes: reads back, retry is idempotent, a rebuild with other bytes is `AlreadyPublished`, a second put of a key is refused, and the CI key itself cannot overwrite, create, copy or delete at RustFS |
| S1 and S4: the retention sweeper, through the service | **refused whatever the owner switch says** (`holds sweep ...; PutObject needs append`): S1's switch widens only the sweeper's own S3 key, not the service gate |
| S1 and S4: the retention sweeper using its own S3 key | passes both ways: RustFS refuses its put (403) while `sweeper_put` is off, and the same sweep completes once it is on |
| S1 and S3: S1's chain events driving S3's grant ledger | passes: after create, revoke, re-grant read and re-grant write, the ledger and the node's own RustFS key agree. S1's `seq` stamps are checked for gaps and used (finding 3) |
| S1 and S2: shared worktree, land preview and caps, concurrent claims with one winner, a follower across three identities (S2's own live tests) | pass |
| S5, S2 and S1: promote a client's worktree store into its project store | passes: the bytes are on the client's instance only, the scratch file stays out, and the WeftOS account is refused before and after |

Re-run history: the first update left the old sweeper test failing; it is now the pair above, so the owner
question is stated as the exact choice below rather than as a red test.

**The sweeper question, as the owner's choice.** Retention needs the sweeper to put (a tombstone first, a blob
back after a racing publish). Either (a) run S4's sweeper with the sweeper principal's own S3 key and set
`WEFTOS_STORE_SWEEPER_PUT=1` in the daemon (S1's switch; off by default; the key can then overwrite or copy over
any artifact, and only a locked store's retention keeps the original), or (b) change S1's service gate so
`Sweep` may put under `tombstones/*` and `blobs/*` (a code change in S1, not yet written). Nothing is flipped.

Findings (for the lanes named; none is fixed here):

1. **S1 `Sweep` is read plus delete; S4's sweeper must also put** (a tombstone, and a blob back after a racing
   publish), so retention cannot run through S1's service (`access.rs`, `weftos-release/src/retention.rs`).
   This is the owner's "sweeper put" item; the two ways to settle it are above. A locked restore needs it too.
2. **S3's broker names the RustFS user `node-<id>`; S1 creates and scopes it as `wk-<hash>`**
   (`weftos-edge-creds/src/policy.rs` `principal_for` against `RustfsBackend::key_id_for`). S3's revoke and
   disable would act on a user S1 never made. The bucket rule (`n-<id>`) agrees.
   **S3's live result** (2026-11-08, loopback RustFS; test `s1_s3_node_bucket_names_agree_but_rustfs_user_names_do_not`,
   which passes on this branch): the bucket name agrees (`n-n1`) but the user does not: S3 uses `node-n1`, S1
   uses `wk-` plus 16 hex of blake3 of `node:n1`. S3's revoke would disable a user S1 never made while S1's
   stays enabled. The options page is on this branch (taken at `4943d62d`, merge `0a5c6bea`):
   [adr126-rustfs-user-name-options.md](../research/adr126-rustfs-user-name-options.md). It lays out
   **A, S3 adopts `wk-<hash>`** (branch `opt/adr126-s3-wk-names`, default unchanged until a host opts in; no
   migration, names unreadable in RustFS output) and **B, S1 adopts `node-<id>`** (branch
   `opt/adr126-s1-node-names`; readable names, but two live rules while existing users are re-created, and
   RustFS cannot rename a user). **Recommended default: A**, together with one creator of the user (S1
   creates and scopes, S3 only assumes roles and disables/enables; not built). **Neither option branch is
   merged here; the choice is the owner's.** S1's notes on the page were still empty when taken. Both lanes
   currently create the user and policy, so an unreconciled run lets each overwrite the other's policy. S5 is not
   affected (it uses S1's names).
   **S1's notes** have landed, in the same commit as its option B code (`c241d1bd` on `opt/adr126-s1-node-names`).
   Only the notes file is taken here ([adr126-rustfs-user-name-options-s1-notes.md](../research/adr126-rustfs-user-name-options-s1-notes.md));
   the commit and the branch are not merged. They change the picture in three ways: S1 has **deployed nothing**, so
   no `wk-` user exists outside throwaway instances and the page's migration argument against B is zero today (S1
   built `store.rekey` for any daemon that did issue hashed keys); under B both lanes address one user, so S3's
   `add-user` after S1's would replace the secret and leave S1's stored secret stale; and S1's view is that either
   works, with B costing about 10 lines and A costing S3 a `blake3` dependency and unreadable names. The page's
   recommended default (A) rested partly on the "users in the field" point, so that recommendation is weaker than
   it reads; the remaining choice is readable names (B) against one generic rule (A).

   **Required for either option: one creator of the user and its policy.** S1 creates and scopes the user (user,
   policy `p-<key id>`, secret under its 0600 secrets dir, enabled state from the grants); S3 only assumes roles
   and disables/enables the user, reading the secret through `NodeKeys` from S1's secrets dir; S3's
   `provision_node` reduces to creating the node bucket. It is **not built** on any branch. Today (neither option
   merged) both lanes create the user and a policy, so a live run overwrites one side's policy or secret. Under A
   it prevents a policy fight on `wk-...`; under B it is also what stops a stale secret on `node-<id>`. Owners:
   S3 (reduce `provision_node`, read `NodeKeys`) and S1 (expose the secret path/reader). Gate C6 should not run
   until it exists.
   **Run, 2026-11-08 (11:40 pile, throwaway worktree, nothing merged into any branch).** S1's and S3's one-creator
   branches (`opt/adr126-one-creator-s1` at `d6113ca9`, `opt/adr126-one-creator-s3` at `1638b5f0`) merged onto
   `feat/adr126-s5` without conflicts in a throwaway worktree (`scripts/store/one-creator-integ.sh`, which removes
   the worktree afterwards). Non-live tests, all green: `weftos-edge-creds` 95 (with `mock,keygen,loopback-chain`)
   and 94, 2 ignored; `weftos-release` 28 plus 1; `weftos-store --features rustfs` **126** (it was 98 before S1's
   change) plus S2's suites 2, 24, 6, 9, 45; `clawft-weave --filter store` **158** plus 1; clippy clean for the
   three crates. Live, on throwaway loopback RustFS instances started and stopped by the script (none left):
   S1 x S4 (4), S1 x S3 chain-authenticated feed (1), S1 x S3 bucket-name parity (1), S2 (4), the S5 promotion
   test (1), and **S1's own 16 live tests, including `live_one_creator_provisioning_secret_and_a_double_create`**
   (one lane creates the user, a second create is shown to replace the secret, and the one-creator path keeps it).
   `s1_s3_node_bucket_names_agree_but_rustfs_user_names_do_not` still passes because it pins today's mismatch:
   no naming option is merged. A first attempt failed only because `/tmp` (a tmpfs) filled with the scratch
   worktree's build output; the rerun built on the disk.
   **The loopback counterpart of C6, and what it does not cover.** Covered against a real RustFS: S3's policy
   agrees with what RustFS enforces (a principal reaches its own folder, another is refused, a disabled key is
   refused on its next request), grant events fed through the chain-authenticated feed match RustFS, and one
   creator per user holds. **Not covered:** the STS AssumeRole exchange over HTTP with the real minter, because
   the minter's HTTP client is the unapproved `reqwest` branch (S3's tests use a mock store for that leg), and the
   1-second revoke timing on the PG instance. Gate C6 proper stays barred (PG).
3. **S1 records a node's default write grant only inside `store.create`**, never as a `store.grant` event, and
   `store.create` is the one grant-affecting event without a `seq` (S1 now stamps a gapless `seq` on the rest,
   which closes S3's `u64::MAX` freeze on S1's side). S3's ledger therefore cannot be rebuilt from S1's chain
   without special-casing the create and shifting S1's numbers by one, as the test glue does. Stamping the
   create, or recording the default grant as a stamped `store.grant`, removes it.
4. **Principal and access formats differ**: S1 `node:<id>` and five access levels; S3 a bare node id and
   read/write. No shared adapter; `append` and `sweep` have no S3 equivalent.
5. **Node id rules differ**: S1 allows 61 characters and a leading or trailing `-`; S3 allows 48 and neither.
6. **No adapter from S4's `ReleaseBackend` to S1's `StoreService`** exists in either crate (S4 leaves it to S1).
7. **An `append` principal's own S3 key cannot write**, so every CI write goes through the daemon; S4's docs
   assume CI holds PutObject on `blobs/*` and `releases/*` directly.
8. **`ClientStore` has no promote verb.** Promotion is S1's operator path (every `store.` verb is Admin) and
   skips the client gate by design; the test promotes through the shared service.

The integration tests added test-only path dependencies to `crates/weftos-release/Cargo.toml` (two lines the
diff gate's `new-dep` rule flags on the merged tree), so they stay on the scratch branch; a small dedicated
test crate would avoid that if they are moved.


Stated dependencies now exercised: S2 uses S1's `put_if_match` (concurrent claims have exactly one winner and a
handoff cannot erase a claim); S1 stamps the `seq` S3's ledger needs. Still not exercised: S3's chain-authenticated
feed (`e03a40e9`) against the real daemon chain signer (S3's tests use a stand-in signer), and S2's notification
wake-up against a real notify target.

## Owner decisions of 2026-11-08 (11:21 to 11:43) and what they mean for S5

Read from `/tmp/weftos-owner-0440/decisions-1122.txt`, `decisions-1139-no-edge-creds.txt`,
`decisions-1140-edge-creds-flagged.txt` and `decisions-1143-names.txt`. S5 changes nothing in code for any of
them; this is status only.

- **User names: option B approved (11:43).** A node's RustFS user is the readable `node-<id>`. This supersedes the
  options page's recommended default of A and the "recommended default: A" line above; both are kept as history.
  Only one lane creates each RustFS user, because a second create replaces its secret: **S1 creates and scopes
  it, S3 only assumes roles and disables/enables it** (the one-creator split above). The owner also asked S1
  whether it can keep an alias/mapping so `wk-<hash>` is still recognised and shown while `node-<id>` is the
  canonical RustFS name; S1 reports feasibility in `/tmp/weftos-owner-0440/aliases-1143-note.txt`. **No
  naming-option or one-creator branch is merged until the alias question has an answer or a note.** S5 itself is
  unaffected: its gate maps to S1 principals, not RustFS user names.
- **`weftos-edge-creds` as a dependency: 11:39 hard no, superseded at 11:40.** S1 may take the crate **only behind a
  feature flag**; the daemon must start and run with the flag off and without S3. On this branch the crate is
  a workspace member and `crates/weftos-release` has it (and `weftos-store`) as test-only path dependencies from
  S3's e2e branch; `clawft-weave` and `weftos-store` do not depend on it. Those two `weftos-release` lines are
  still the diff gate's `new-dep` findings and still need the owner's approvals entry.
- **S1 archive retention: nothing expires yet.** S1's archive-gc schedule is off by default and carries no day
  numbers; nothing here schedules deletions.
- **S2 registry and signature format:** stay on A. B only if the problem shows up; C only when the format must
  change for another reason. `feat/adr126-s2-format2` is not merged and B is not on by default.
- **S3 grants:** may be signed by any other node or person. Snapshot cadence and placement are **still open**; the
  S3 compaction schedule stays off by default and no cadence is assumed anywhere in this status.
- **Close-out and release note:** local commits only; no push, deploy, tag, testing release or install over the
  production weaver; the RustFS and Tapestry release happens once, later, by the owner. Live checks use only
  throwaway loopback RustFS, never the photo-gallery instance.

## Object keys against Windows-host RustFS limits (S4's check, 2026-11-08)

Source: `/tmp/weftos-owner-0440/s4-keys-0925.md`. weftos-drive found that RustFS on a Windows host rejects keys
containing `: * ? " | < >` and folds case; S4 extended the check to a trailing `.` or space, device names
(`con`, `nul`, `aux`, `com1`...) and case-only differences. Evidence lives on S4's local branch
`review/adr126-keys-0925` (not merged here): `crates/weftos-store/tests/s4_keys_0925.rs` and
`crates/weftos-edge-creds/tests/s4_keys_0925.rs`, one `#[ignore]`d failing test per problem. Nothing was run
against a real Windows-host RustFS; the limits are drive's findings plus the standard Windows file-name rules.
These are items for the owning lanes; S5 changes none of them.

| # | Owner | Where | Problem |
|---|---|---|---|
| K1 | S1 | `weftos-store/src/backend.rs` `check_key` | Refuses only empty, NUL, backslash and `.`/`..`/empty segments; accepts `: * ? " | < >`, control characters, trailing `.`/space and device names. On a Windows host the put fails opaquely. Archive keys `<bucket>@<ms>/<key>` inherit it. |
| K2 | S2 | `weftos-store/src/overlay.rs` `validate_key` | Same gap, for overlay writes, promote and archive prefixes and registry slugs. |
| K3 | S2 | `weftos-store/src/share.rs` `publish_tree` | Case-colliding paths (`src/Foo.rs`, `src/foo.rs`) are two objects here and one on a folding host; the second put replaces the first while the manifest lists both. Fold case in the `seen` set. |
| K4 | S1/S2 | `weftos-store/src/name.rs` `check_slug` | Slugs allow uppercase and device names. `Alpha` and `alpha` are two worktrees but one registry and signature file on a folding host. Lowercase-only slugs plus the device-name and trailing-dot rules. |
| K5 | S3 | `weftos-edge-creds/src/policy.rs` `validate_key` | Same gap as K1 for every edge put/get/delete. |

Checked and fine: bucket names (lowercase only in every lane), the fixed internal keys (`.wt/...`,
`worktrees.sig/`, the registry prefix), S2's tree share (already refuses these names; it is the model to
copy, so K1, K2 and K5 could share one rule), S4's release keys (fixed in `304b8da8`, on
`integ/adr126-s1-s4`, not on `feat/adr126-s4`), and S5, which writes no keys of its own. Not covered yet: path length
(Windows MAX_PATH 260 against keys of up to 512 bytes in S1 and 1024 in S3), Unicode normalisation, and bucket
tags; the 10:25 pile has S4 on these and appends to the same file. Fold in what it adds.

### Non-live re-run on the merged tip (`e5f3e3ec`, 2026-11-08, after the 10:25 pile)

All green, no live tests and no RustFS: `weftos-edge-creds` 95 (with `mock,keygen,loopback-chain`) and 94 (default),
2 ignored (S3's known-gap regressions); `weftos-release` 28 plus 1 (4 live ignored); `weftos-store --features rustfs`
98 plus S2's suites (2, 24, 6, 9, 45); `clawft-weave --filter store` 154 plus 1 (12 live ignored);
`clawft-mesh-local` cert test 1; `clawft-kernel` `node_facts_advert` 8; clippy clean for `weftos-edge-creds` and
`weftos-release`. S4's `#[ignore]`d key tests (K1 to K5) are not on this branch.

### Close-out checklist (S5, 2026-11-08)

- Merge-readiness: `feat/adr126-s5` merges onto `target-0.8.5` with no conflicts (rehearsal below, re-done after
  later merges would need the same dry run); the gate's only failures are S1's receiver lines, the approvals-file
  change and the two `weftos-release` test-dependency lines, all owner-side.
- No TODO of S5's is left open in this page. What remains is the owner's or another lane's: naming option and
  alias answer, the one-creator branches (unmerged), the sweeper choice, the approvals entries, key findings
  K1 to K5 and the open key checks (S4), S3's seg-length and snapshot cadence, and PG gate C6.
- Local commits only; no tag, no release, no push, nothing installed over the production weaver.

## Rehearsal: merging `feat/adr126-s5` onto `target-0.8.5` (2026-11-08)

Done on a throwaway worktree at `target-0.8.5` (`527ea4db`, the same commit as `origin/target-0.8.5`), then aborted
and removed. No real merge, no push.

- **Conflicts: none.** `git merge --no-commit --no-ff feat/adr126-s5` merged automatically. `target-0.8.5` is
  S5's merge base, so the result is the S5 tree exactly (checked: no difference against `feat/adr126-s5`).
- **Size:** 148 files, +30 203 / -25 lines. Outside `crates/`, `docs/` and `scripts/` it touches `Cargo.toml`,
  `Cargo.lock`, `config/diff-gate-approvals.toml` (S1's draft-entry commit, not mine) and the
  `contracts/cogs/` store vectors (S1).
- **Diff gate against `target-0.8.5` (view only):** 148 files, 8 failed, 3 approved. The 8 are S1's
  `store_ingest*` lines and the approvals-file change, as before; S3's and S4's merges add nothing the gate
  flags. Landing on target needs the owner's approvals step first.
- **What a real merge would still need:** the owner's approvals entries (above), then the usual `scripts/build.sh
  gate`, which I have not run (targeted tests only).

## Waiting on the owner

- **Diff-gate approvals for S1's receiver** (`store_ingest.rs` and `store_ingest_tests.rs`: listener, test
  sockets, and a live test that spawns RustFS). S1 prepared the entries: appending
  `docs/guides/rustfs-store-ingest-approvals.draft.toml` (on `feat/adr126-s1`) to
  `config/diff-gate-approvals.toml` turns every store_ingest finding to "approved" (S1 checked it on a scratch
  copy). On this branch the gate flags exactly those lines, the approvals-file change itself, and two `new-dep` lines
  (`crates/weftos-release/Cargo.toml:24-25`, the test-only path dependencies on `weftos-store` and
  `weftos-edge-creds` that came in with S3's e2e branch). Gate view after the 2026-11-08 re-merge: 153 files, 10 failed, 3 approved. S5 adds nothing the gate flags: its live tests start no
  process and bind no socket (a shell script does).
- **S1: sweeper put, and CI direct put** (`WEFTOS_STORE_SWEEPER_PUT`, `WEFTOS_STORE_CI_DIRECT_PUT`; both off,
  both tested both ways, recommendation: leave off). The sweeper side is tested end to end above and is the
  choice (a) or (b) in "The sweeper question". The CI append key stays read-only at RustFS, so CI writes go
  through the daemon unless the owner flips the second switch.
- **S3: a `reqwest` client for the STS minter** trips the gate's `net` rule; only the owner can approve it.
- **S2: card 4c4319e0 holds the move** until a real RustFS run, a Windows node, latency, leftover, failure
  and follower drills have passed, and ADR-131 D1 and D2 pass first.
- **ADR open questions:** how long archived buckets live before deletion; whether cloud agent sessions get
  write access to worktree stores.
- **S5: pruning the chain** after `store.client.archive` and `archive_verify` agree is the chain owner's
  step. The shared WeftOS RustFS instance should be pre-marked with an owner so no client account can claim
  it by opening first.

## Barred (standing rules for these lanes)

No `git push`; no Phase A deploy; no `terraform apply`; no install or deploy of a store-capable `weaver`
on any machine; no restart of weftos, shasta, meshd, rustfs or cog-host; no MaxBook weaver, gateway or MLX
restart; no Sansone migration; no blind S3 mount (materialise and watch only); no live PG install; no
full-workspace test run; the approvals file is the owner's.

## Running the live checks

```
# S1: WEFTOS_STORE_TEST_RUSTFS_ENV names a throwaway server's env file
scripts/build.sh test weftos-store --features rustfs --ignored

# S5: starts and stops its own two loopback RustFS instances
WEFTOS_STORE_TEST_RUSTFS_BIN=/path/to/rustfs scripts/store/client-live-test.sh
```

## Flake hunt (weftos-store and the store tests of clawft-weave)

One flake found and fixed (2026-11-08 pass, after S3 and S4 were merged); the earlier pass found none.

- **Second pass:** `weftos-store --features rustfs`, `clawft-weave --filter store`, `weftos-edge-creds` and
  `weftos-release`, 6 rounds each at `RUST_TEST_THREADS` 1, 4 and 64: 72 runs, 71 green. The earlier pass's 60
  runs (above) were all green.
- **The failure:** `rustfs::tests::a_dead_rustfs_fails_fast_after_the_breaker_opens_and_a_short_timeout_view_shares_it`
  (S1's test), once, at 4 threads: "no connection attempt: 51.9 ms" against a 50 ms bound. The breaker was
  open and no attempt was made; the thread was descheduled while another build ran. Fix: the bound is now 150 ms,
  still under the view's 200 ms timeout, so a hung attempt still fails it. 40 of 40 runs pass at 64 threads.
  **This is a one-line change to an S1-owned file** (`crates/weftos-store/src/rustfs_tests.rs`) on the S5
  branch; S1 should take the same change so the next merge does not conflict.

- **First pass, unit and integration tests:** `weftos-store --features rustfs` (52 tests) and `clawft-weave --filter store`
  (137 tests, which include S1's and all of S5's), 10 rounds each at `RUST_TEST_THREADS` 1, 4 and 64, with
  another build running on the machine for part of it: 60 runs, all green.
- **Live tests:** S5's 7 and S1's 8, six runs each against fresh throwaway loopback RustFS instances
  (`scripts/store/client-live-test.sh`, which now also takes a command to run): 12 runs, all green. S1's
  suite took 88 to 178 s per run, the spread mostly load from other builds.
- **Read for latent risk:** wall-clock use is limited to S1's Object Lock live tests (fixed one-hour
  windows, so not time-sensitive); the only test statics are `fs.rs`'s temp-file counter (production code)
  and one single-use clock in S5's expiry test. One caution, not a flake: S1's lock tests leave
  Compliance-locked versions that nothing can delete for an hour, so run them only on a throwaway instance.


## Rebase onto landed S1 (rehearsal, 2026-11-08)

Rehearsed on throwaway branch `rehearsal/s5-on-s1-2a8a1327` (worktree removed; the real branch is untouched, nothing pushed or merged to target-0.8.5). S1 tip 2a8a1327 is 14 commits past the S1 base (19de450e) that S5 already contains.

- **Merge of S1 2a8a1327 into S5: 3 conflicts, all resolved, then `weftos-store --features rustfs` 150 passed, 0 failed.**
  - `crates/weftos-store/src/rustfs_tests.rs` (circuit-open test): take S1's bucket name `bkt`, keep my 150 ms bound (S1 still has the flaky 50 ms; S1 should take the 150 ms change).
  - `crates/weftos-store/src/sigv4.rs` (`canonical_request` split out of `authorization`): take S1's side whole; mine is the same code, formatted differently.
  - `docs/adr/adr-126-mesh-object-store.md` (Updated lines): keep both sides.
- **`git rebase --rebase-merges` onto 2a8a1327 is not a good route:** S5 holds 107 commits including the S2, S3 and S4 merges, and it stops at the first replayed S1 merge (21787976) on the same sigv4 and ADR conflicts. Land by merge (or by rebasing onto target-0.8.5 once S1, and whatever else precedes it, has landed): the conflict set above is the one to expect, and it will not grow unless S1 changes more before landing.
- **S5's own first commit re-applies S1's first commit** (`f89d3608`, the weftos-store crate): once S1 is in the target, that commit and the S1 merges drop out of a rebase as already applied. If git does not drop `f89d3608` as patch-identical, skip it by hand (`git rebase --skip`).
- S2 stays on registry format A; no S2 file is in the conflict set.
- When the MaxBook reports S1 landed: rebase (or merge) the real `feat/adr126-s5` onto that tip, apply the resolutions above, re-run `scripts/build.sh test weftos-store --features rustfs` and `clawft-weave --filter store`, and report.
