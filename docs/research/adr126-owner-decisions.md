# ADR-126: open owner decisions, S1 to S5 (one page, 2026-11-08)

Every call below is the owner's. Each row has a recommended default, what the answer unblocks, and what flips. Nothing here has been decided or switched; where a row says "off", that is how the code ships today. Sources are named so a row can be checked: `ADR` is `docs/adr/adr-126-mesh-object-store.md`, `status` is `docs/guides/adr-126-integration-status.md`.

Answer by row number ("1: 2, 2: B, 3: leave unset, ...") and each lane makes the flip.

## Switches that are already built (answer, then one-line flip)

| # | Decision | Options | Recommended default | Unblocks | Flip |
|---|---|---|---|---|---|
| 1 | **Who signs the worktree registry** (S2; `adr126-s2-registry-signing-and-claim-clock.md`) | 1. daemon node identity (ed25519); 2. store governance only (registry in a daemon-only store); 3. dedicated per-project key | 2 now, 1 once you name where followers read trusted keys; skip 3 | Signed registry entries, strict follower mode on real data | 2: `WEFTOS_WT_REGISTRY_STORE=1` (off today). 1: name the trusted-key location, S2 wires the daemon `Ed` signer |
| 2 | **ANSWERED 11:21: stay on A (format as-is); B only if the problem shows up; C only when the format must change; `feat/adr126-s2-format2` not merged.** **Registry format** (S2; decision note f3b1953d in `adr126-s2-l1-follower-view.md`) | A. keep two objects; B. follower retries a `bad` entry once after the settle delay; C. format 2, signature inside the entry | B if the noisy poll is ever seen; C only when the format has to change for another reason | Closing review finding #6 (one `unreadable` poll); C also removes orphan sidecars | A: nothing. B: S2 code change (small). C: ready as a branch plan, rollout order in the note, `WEFTOS_WT_REGISTRY_FORMAT=2` |
| 3 | **CI route for release uploads** (S4; `release::route`, `DEFAULT_CI_ROUTE`) | Daemon-mediated append (write-once through the service), or Direct (CI key puts straight to RustFS) | Daemon. Direct is only the shipped constant because a standalone CLI can only run that | Write-once release uploads without widening any key | One constant (`DEFAULT_CI_ROUTE`) for the verb; Direct also needs row 4's second switch |
| 4 | **CI direct put and sweeper put** (S1; `WEFTOS_STORE_CI_DIRECT_PUT`, `WEFTOS_STORE_SWEEPER_PUT`) | Each on or off | Both off (S1's and S4's recommendation) | CI direct: only matters if row 3 = Direct. Sweeper put: retention sweeps and locked restores | Env var in the daemon, restart; policy is pushed at start (`reconcile`) |
| 5 | **How the sweeper gets to put** (S1/S4; status, "The sweeper question") | (a) sweeper's own key plus `WEFTOS_STORE_SWEEPER_PUT=1`; (b) change S1's service gate so `Sweep` may put only under `tombstones/*` and `blobs/*` | (b). (a) lets the key overwrite or copy over any artifact; only a locked store's retention keeps the original | Retention through the service, locked restore | (a): row 4 switch. (b): S1 code change, not yet written |
| 6 | **ANSWERED 11:21: nothing expires yet; keep no-expiry, schedule no cleanup deletions.** **Archive retention days** (S1; `WEFTOS_STORE_ARCHIVE_{NODE,PROJECT,RELEASE}_DAYS`, `_GC_EVERY_HOURS`, `_GC_CONFIRM`) | Numbers per kind of store; unset keeps for ever. Per-store override: `weaver store archive-keep` | Set no numbers yet. When you do, run with `_EVERY_HOURS` and no `_GC_CONFIRM` first (dry run) and read the report | Archived buckets stop growing; the ADR's "how long do archives live" question | Env vars; confirm only after a dry run |
| 7 | **Lock modes and floors per release channel** (S4 immutability plan) | Governance or Compliance, and the floor in days, per channel | Governance by default; Compliance only for final releases, since nobody (you included) can undo it. Pick no numbers until the channels are named | `weaver release publish --lock ...` defaults; undeletable releases | Per-channel config; S4 wires the default |
| 8 | **Reused blob under a weaker lock** (S4 review 0910, M3) | Keep the old (weaker) lock and report it, fail the publish, or upgrade | Fail closed when the existing retention cannot be read; report a weaker mode in the receipt | Honest retention claims on a release | S4 code change after your answer |

## Approvals only the owner can add (`config/diff-gate-approvals.toml`)

| # | Decision | Recommended default | Unblocks | Flip |
|---|---|---|---|---|
| 9 | **S1 receiver entries** (listener, test sockets, live test spawning RustFS) | Approve. Entries are narrow (this file, these patterns); S1 checked them on a scratch copy | Landing S1's `store_ingest` on target | Append `docs/guides/rustfs-store-ingest-approvals.draft.toml` (on `feat/adr126-s1`) |
| 10 | **S4 path dependencies** (`weftos-release` dev-deps on `weftos-store`, `weftos-edge-creds`; listed in S4's merge note) | Approve; test-only, in-tree crates | Landing S4's integration tests | Entries listed in `adr126-s4-merge-note-2026-11-08.md` |
| 11 | **`reqwest` for S3's STS minter** (branch `prep/adr126-s3-reqwest-client`, `c9b85e56`; trips the `net` rule) | Approve if you want the native minter run from the daemon; otherwise leave unmerged. S3 works against any host-supplied HTTP client without it | A native STS minter for gate C6 and a real node | Approval entry; then merge the prep branch (one commit, 4 tests) |

## Chain-owner decisions (S3 and S5)

Written as switches in ADR-126, "Chain-owner switches for S3". S3 decided none.

| # | Decision | Options | Recommended default | Unblocks | Flip |
|---|---|---|---|---|---|
| 12 | **ANSWERED 11:21: any other node or person may sign grants (built as `SignerRule::AnyOther`).** **Who may sign grant events for which mesh** | A. one key per mesh; B. account owner key; C. delegated signers | A | The real `SnapshotVerifier` and `ChainGrantEvent` wiring | Host's verifier implementation |
| 13 | **STILL OPEN (11:21): do not treat the default as approved.** **Snapshot cadence** | A. stay off; B. every N events; C. count and time | C, with N about one second of replay and T one day; numbers are yours | Bounded follower replay time | `with_schedule(...)`; pruning stays owner-run in every option |
| 14 | **STILL OPEN (11:21).** **Where the signed snapshot lives** | A. on the chain; B. in the mesh store with a hash on the chain; C. A plus `events_head` in the chain anchor | A now, C when the anchor can carry the field; against B (a follower needs a snapshot to learn which store to read) | A chain-side snapshot publisher | Chain-side wiring of `GrantLedger::signed_snapshot` |
| 15 | **Confirm S1's grant `seq` rule** (c43e240a: stamped by `StoreService` under the change lock, gapless, ceiling 2^62, `admit()` bound for consumers) | Confirm, or name a different bound | Confirm; install `MaxSeqJump` or the `admit` bound as the ledger's `SeqGuard` | Closing the `u64::MAX` freeze end to end | S3 installs the guard in the daemon glue |
| 16 | **S5: pruning the chain** after `store.client.archive` and `archive_verify` agree | Owner-run step only | Keep it manual; no schedule | Chain size control | Owner runs it |
| 17 | **S5: pre-mark the shared WeftOS RustFS instance with an owner** so no client account can claim it by opening first | Do it before any client account opens a store | Do it | Safe multi-account use of the shared instance | Owner (or S5 on your word) writes the marker |

## Run permissions

| # | Decision | Recommended default | Unblocks | Flip |
|---|---|---|---|---|
| 18 | **Gate C6 on PG** (card b76c74a7). Needs five things from you: permission to run; endpoint and admin key supplied at run time (never stored); which HTTP client; throwaway node ids and a bucket prefix; a run window | Allow one short window with throwaway ids and a prefix that is deleted afterwards; native client only if row 11 is approved | Proof that WeftOS-minted credentials work on the PG RustFS: scoped session for one folder, another principal refused, revocation refused within 1 s | The five items, in ADR-126 "What gate C6 needs" |
| 19 | **S2 card 4c4319e0 holds the move** until a real RustFS run, a Windows node, latency, leftover, failure and follower drills pass, and ADR-131 D1/D2 pass first | Keep the hold | Moving S2's follower view to real use | Owner releases the card |

## Open question from the ADR

| # | Decision | Recommended default | Unblocks | Flip |
|---|---|---|---|---|
| 20 | **Do cloud agent sessions get write access to worktree stores, or only read?** (ADR open question 3) | Read only; write per worktree on request | Agent grants in S2's overlay and the S1 grant policy | A grant at the service; no code change |

## Not owner calls (so they are not here)

Review findings the lanes fix themselves: S2's planted-generation rollback bypass; S4's publish-then-edit and size-change poisoning; S1's adapter for `NodeArchiver`; the RustFS user-name mismatch (`node-<id>` in S3's broker versus `wk-<hash>` in S1); the Windows-host key rules. See `/tmp/weftos-owner-0440/s3-review-0910.md`.
