# ADR-126 S2: merge-ready note (close-out, 2026-11-08)

Branch state, what to merge, what to leave, and the tests to run. Nothing here has been pushed or merged by S2.

## Branches

| Branch | Head | Status | Take it? |
|---|---|---|---|
| `feat/adr126-s2` | `3fdea5e3` (this note's commit follows it) | Registry format stays as it is (**A**, owner decision 2026-11-08). Contains option B (off by default) and the gate drills with their three fixes. | **Yes.** It merges into the current `feat/adr126-s5` tip (`4e592452`) with no conflicts (checked with `git merge-tree`). s5 already holds S2 up to `f3b1953d`; the new commits since are listed below. |
| `feat/adr126-s2-format2` | merges `feat/adr126-s2` (B and the drills) plus option C | Option C (signature inside the entry), ready, **unmerged by the owner's decision**. Use only when the registry format has to change for another reason. Switch: `WEFTOS_WT_REGISTRY_FORMAT=2`; `WEFTOS_WT_REGISTRY_REQUIRE_FORMAT_2=1` refuses format 1. | **No.** Also merges cleanly into s5's tip, so it stays a one-step option. |
| `feat/adr126-s2-drills` | `e74f2eba` | Fast-forwarded into `feat/adr126-s2`; nothing of its own left. | Redundant; may be deleted. |

New on `feat/adr126-s2` since s5's last merge (`f3b1953d`):

- `63426543` option B: `poll_verified_settled`, `WEFTOS_WT_FOLLOW_RETRY_MS` (0 to 10000; unset or 0 = off). Not turned on by default. Use B only if the one noisy `unreadable` poll between the registry entry and signature writes is actually seen.
- `92e44f6c` gate drills (single machine, loopback RustFS) and three fixes in `weftos-store`: concurrent follow-state saves could tear the file (unique temp name per save); `overlaps` reported most one-sided edits as conflicts (83 to 91 percent false positives; now uses the change lists when both manifests have them); a publish killed after the manifest write left orphan objects (cleanup now lists the tree prefix, one extra list per publish).
- `e74f2eba` the drills' write-up (numbers, what each drill proved, what still needs two machines).
- `3fdea5e3` the owner's format decision recorded in the research note.

Files touched (S2-owned except where noted): `weftos-store` `follower.rs`, `registry_sign.rs`, `share.rs`, tests `s2_follower_drills.rs` and `s2_registry_sign.rs`; `clawft-weave` `store_worktree.rs` and its tests (the `WEFTOS_WT_FOLLOW_RETRY_MS` setting); the research note. No S1, S3, S4 or S5 files.

## Checks run

- Diff gate on the S2 head: `DIFF_GATE_BASE=feat/adr126-s1 scripts/diff-gate.sh`: OK (36 files, 0 failed, 0 approved). No approvals added.
- Throwaway scratch merge of `feat/adr126-s2` into s5's earlier tip (`fe69361f`): `weftos-store --features rustfs` and `clawft-weave --lib store` 361 tests passed, 0 failed; clippy `-D warnings` clean on both crates.
- Flake hunt (below).

## Tests to run after merging

```
cargo test -p weftos-store --features rustfs
cargo test -p clawft-weave --lib store_worktree
cargo clippy -p weftos-store -p clawft-weave --all-targets -- -D warnings
# live, throwaway loopback RustFS only (never the production instance):
WEFTOS_STORE_TEST_RUSTFS_ENV=<env file> cargo test -p weftos-store --features rustfs -- --ignored s2_
```
The notification wake-up test also needs `scripts/store/notify-sink.py` and `WEFTOS_STORE_TEST_NOTIFY_FILE` (see the research note).

## Flake hunt (S2 suites)

`weftos-store --features rustfs`, 12 rounds each at `RUST_TEST_THREADS` 1, 4 and 64 (36 rounds), with other builds running on the machine. 35 rounds fully green. One round (threads 1, round 6) failed 4 tests of `s2_follower_drills.rs` in 0.5 s: `writer_kill::mem`, `writer_kill::fs`, `merger_kill::fs`, and `overlaps_report_only_paths_both_worktrees_changed_even_with_different_bases`. The panics were all `unwrap` or "unexpected sync failure" on file-system or store writes, which no test logic touches differently from the 35 passing rounds. Then 25 more rounds of that suite at one thread: all green. `/tmp` is a 48 GB tmpfs that was at 82 percent with other lanes writing RustFS data, so the likely cause is a transient out-of-space or cleanup while the tests ran in the shared `/tmp`. **Not proven**: the error text was lost to the output filter. The timing-sensitive tests (`follower_retry`, the `elapsed` bounds, the concurrent-claim tests) did not fail in any round. If it recurs, run with `TMPDIR` on a disk path and keep the full panic text.

## What S2 does not close

Two physical machines and a live daemon (barred by the no-install rule; what they need is written in the research note), the S1-side hook that feeds `registry_hints` from `store_ingest` and the mesh-relay hop, a decision on the manifest `schema` field being checked on read and on `verify_entry` checking the project, and the publish-killed-after-overwriting-a-changed-doc window (fails safe; needs a format change).

## Rebase onto landed S1 (rehearsal, 2026-11-08)

Rehearsed on throwaway branch `rehearse/s2-onto-s1-2a8a1327` (worktree `s2-rehearse`) against S1 tip `2a8a1327`. The real branch `feat/adr126-s2` was not touched.

- Method that works: `git rebase --rebase-merges <landed S1 tip>`. A plain `git rebase` replays 30 commits including the two earlier S1 merges and conflicts at the first commit (ADR doc). `--onto` from the second merge drops S2 code; do not use it.
- Expected conflicts: only `crates/weftos-store/src/sigv4.rs`, at the second S1 merge commit. S1 moved the canonical-request code into `canonical_request()` and `authorization()`; S2's side only carries a rustfmt reflow of the same `format!`. Resolution: take S1's file whole (`git checkout <S1 tip> -- crates/weftos-store/src/sigv4.rs`), `git add`, `git rebase --continue`. S2 has no sigv4 change of its own.
- A plain merge of `2a8a1327` into S2 would conflict in `sigv4.rs` and `docs/adr/adr-126-mesh-object-store.md` (doc conflict: S1 and S2 status text both edited; keep both).
- Registry format stays A; no format2/B/C commit is involved.
- When the MaxBook reports S1 landed: `git rebase --rebase-merges <landed tip>` on `feat/adr126-s2`, resolve `sigv4.rs` as above, run `scripts/build.sh check` and the targeted `weftos-store` and `clawft-weave` store tests.

## Landed onto S1 (branch `land/adr126-s2-on-s1land`, 2026-11-08 night)

The landed S1 (`integ/adr126-s1-land` b8c42295f) also carries the Mac S2 line (`s2-mac`, `s2-meta`, `s2-hooks`) that fixed the same ground independently. Owner call: the Mac branches win where the two overlap, S2 is replayed on top, the stricter check stays where they differ. Registry format stays A.

Mac won (kept as landed): worktree verb path guard (F1) and live/archived/foreign-bucket checks (F6, F8); activity-log and chain record of create/publish/claim/promote (F10); `mark_landed` (a git land is the merge whoever held the claim, F7) and the registry entry staying as `merged` until the store is archived and deleted (`forget`); registry-entry bucket check (F3); manifest cannot name control files (F19); `FollowState::save` unique temp name; follow digest and parallel fetch (`poll_fast`); `overlaps` with both bases and change-list narrowing; injective branch-to-slug rule in `worktree-store.sh`; claim stamps from the future are not live (`claim_is_live`).

S2 replayed on top: registry signing and trust (`registry_sign`, `SignCfg`, trust in verb output); option B follower retry (off by default); claim watch (observed-time expiry) and claim epoch; CAS handoff writes (`mark_landed` now conditional too); land preview, conflict policy, size cap, publish quota, `usage` and `gc` verbs; the registry-only store switch; gate drills and their three fixes; the publish sweep by listing.

Where they differ and the stricter rule was kept: a caller that names a claim epoch is refused when the claim was taken over (Mac's land would not check); with a trusted-key file `follow` reads and verifies every entry (no digest shortcut); `claim_merge` treats a far-future stamp as not live (Mac) in addition to my observed-time expiry.

Dropped: my test "a landed worktree leaves the registry" (opposite of Mac's decision that the merged entry stays until deletion); `promote` no longer removes the registry entry itself, `forget` removes it with its signature sidecar when the store is archived and deleted.

Checks on the tip: `weftos-store` all tests (182 lib, plus s2_* suites), `clawft-weave --lib store` 102 passed 5 ignored (live), clippy clean on both crates with `--tests`, `scripts/tests/worktree-store.test.sh` 74/74 and `worktree-union-mount.test.sh` 8/8, diff gate against the landed S1 OK (0 failed, 0 approved).
