# feat/adr126-s3: merge-ready note (close-out, 2026-11-08)

Branch `feat/adr126-s3`, commits over `target-0.8.5` listed below. Nothing pushed, no release, no tag, no install.

## What it changes

- New crate `crates/weftos-edge-creds` (ADR-126 S3: STS minting, SigV4, grant ledger, compaction and loopback chain, chain-authenticated feed, node lifecycle). Workspace `Cargo.toml` gets the member and `Cargo.lock` 14 lines. Dependencies are all existing workspace ones (`serde`, `serde_json`, `thiserror`, `hmac`, `sha2`, `zeroize`; optional `getrandom`; dev `tokio`). The crate has no dependency on S1 (`weftos-store`) or any other ADR-126 crate, and S3 does not depend on `weftos-edge-creds` from anywhere in this branch.
- `verify_strict` on mesh signature paths in `clawft-kernel` and `clawft-mesh-local` (commit `429bb1fb`): the only files outside the new crate, the workspace manifests and the docs.
- Grant policy (owner, 11:21): any other node or person may sign a grant. `authchain::SignerRule::AnyOther` is the default: no one may sign a grant of access to themselves; own revoke is allowed; `SignerRule::Anyone` turns the check off. Which identities are trusted at all is the host's `SnapshotVerifier`.
- Snapshot cadence and location are still open. `SnapshotSchedule` is configurable and OFF by default (`every_events`, `every_secs`, both); `LoopbackChain::tick` and the owner-run `compact` exist; nothing sets a cadence. The options are in ADR-126, "Chain-owner switches for S3" (switch 2 and 3 marked open).
- `validate_key` refuses a path segment over 255 bytes (RustFS 1.0.4 answers `InvalidArgument`; measured), with a test that fails when the check is removed or loosened.
- Docs: ADR-126 S3 sections, `docs/research/adr126-owner-decisions.md` (20 rows, answered ones marked), `docs/research/adr126-rustfs-user-name-options.md` (owner chose readable `node-<id>`, option B; S3 needs no change).

A trial `git merge-tree` of `feat/adr126-s3` onto `target-0.8.5` was clean at `e03a40e9`; the later commits add only files under the new crate and docs.

## Unmerged branches (the owner picks; none is needed for this branch)

- `opt/adr126-s3-wk-names` (option A naming): superseded by the owner's choice of B. Do not merge.
- `opt/adr126-one-creator-s3`: `provision_node` creates only the node bucket; S3 only assumes roles and disables or enables the user (S1 is the single creator). Merge with S1's `opt/adr126-one-creator-s1`, and only after the owner has answered the naming/alias question.
- `opt/adr126-nodekeys-adapter`: optional daemon feature `edge-creds` (off by default) with `S1NodeKeys`; needs a diff-gate approval entry for the new optional dependency.
- `prep/adr126-s3-reqwest-client`: native HTTP client; needs a diff-gate approval entry (`reqwest`).
- `test/adr126-s3-chain-feed-e2e`: the S1 x S3 chain-feed live test (already merged into s5's integration branch).

## What it needs from S1 and S2

- S1: `retire_node_store` is now resumable and returns Ok only after a full copy; an adapter from it to `lifecycle::NodeArchiver` is still the host's. S1's grant `seq` is gapless, stamped by `StoreService` (ceiling 2^62); install the `SeqGuard` with that bound when wiring. `node_key` is keyed without the mesh (see the options page).
- S2: nothing.

## Tests to run (targeted only)

```
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p weftos-edge-creds --lib            # 99 pass, 2 ignored (known-gap regressions, by design)
cargo clippy -p weftos-edge-creds --all-targets --features mock,keygen,loopback-chain -- -D warnings
cargo check -p weftos-edge-creds --target wasm32-unknown-unknown
cargo test -p clawft-mesh-local --test cert_tests
cargo test -p clawft-kernel --lib node_facts_advert
scripts/diff-gate.sh                              # expect OK
```

Live S1 x S3 tests run through `scripts/store/integ-live-test.sh` on the integration branch against a throwaway loopback RustFS only; they passed on `feat/adr126-s5` (see the integration status). Gate C6 on the PG instance has not been run and is not part of this close-out.

## Rebase plan onto landed S1 (rehearsed 2026-11-08)

Rehearsed on the throwaway branch `rehearsal/adr126-s3-on-s1-2a8a1327` (S1 tip `2a8a1327`; merge base `527ea4db`; 22 S3 commits replayed). The real branch was not rebased.

- Files touched by both lanes: `Cargo.toml`, `Cargo.lock`, `docs/adr/adr-126-mesh-object-store.md`. Nothing else overlaps.
- `Cargo.toml` (1 conflict, first commit): workspace `members` list. Keep both lines: S1's `crates/weftos-store` and S3's `crates/weftos-edge-creds`.
- `Cargo.lock`: auto-merged. After the rebase run a targeted `cargo check -p weftos-edge-creds` so the lock is regenerated if it drifts; never hand-edit it.
- ADR (conflicts in 4 of the 22 commits: the `Updated` header line, the S1 status section against the S3 results section, and three later appends): keep both sides each time, S1's text first. The two sections are independent.
- Result of the rehearsal: `crates/weftos-edge-creds` is byte-identical to `feat/adr126-s3`; `cargo test -p weftos-edge-creds --lib --features mock,keygen,loopback-chain`: 100 passed, 2 ignored (by design). No conflict markers remain.
- `scripts/diff-gate.sh` on the rebased tip rejects only S1's own entries (daemon dependency on `weftos-store`), which are the owner's S1 approvals; S3 adds nothing new to the gate.
- Option branches (`opt/*`, `prep/*`, `test/*`) stay unmerged and are not rebased until the owner picks; `opt/adr126-nodekeys-adapter` will conflict in `crates/clawft-weave/Cargo.toml` with S1's daemon edit (keep both dependencies).
- When the MaxBook reports S1 landed: `git rebase <landed-tip>` on `feat/adr126-s3`, resolve as above, rerun the targeted test, report the new tip.
