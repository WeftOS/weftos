# feat/adr126-s3: merge-ready note for s5 (2026-11-08)

Branch `feat/adr126-s3`, 14 commits over `target-0.8.5`. Nothing pushed. s5's integration branch already carries up to `e03a40e9`; the commit after that adds `tick`/`compact` to the loopback chain and the chain-owner switches in ADR-126, so re-merge it.

## What it changes

- New crate `crates/weftos-edge-creds` (ADR-126 S3: STS minting, SigV4, grant ledger, compaction, loopback chain, chain-authenticated feed, node lifecycle). Workspace `Cargo.toml` gets the member; `Cargo.lock` gets 14 lines. All dependencies were already workspace dependencies (`serde`, `serde_json`, `thiserror`, `hmac`, `sha2`, `zeroize`; optional `getrandom`; dev `tokio`). The diff gate saw no new dependency, unsafe, net or process code.
- Commit `429bb1fb` switches mesh signature checks to `verify_strict` in `clawft-kernel` (`capability_claim.rs`, `key_rotation.rs`, `node_facts_advert.rs`, `placement_vocabulary.rs`, `workload_runtime/seed_bind.rs`) and adds small-order key tests there and in `clawft-mesh-local/tests/cert_tests.rs`. These are the only files outside the new crate and ADR-126. Check them for conflicts if another lane touched the same lines.
- `docs/adr/adr-126-mesh-object-store.md`: S3 sections only.

A trial `git merge-tree` of `feat/adr126-s3` onto `target-0.8.5` is clean (no conflicts).

## What it needs from S1 and S2

- S1: `retire_node_store` is synchronous and copies nothing, so `lifecycle::NodeArchiver` needs an adapter before S3 can call it for real (review finding in `/tmp/weftos-owner-0440/s3-review.md`, S1 item 3). Until then the lifecycle runs against the mock. S1 must also say where the grant `seq` is stamped; `SeqGuard` has no default bound until then. The bucket-name rule is already tested against S1's `StoreName::node` (parity test).
- S2: nothing. The crate does not depend on S2.
- Nothing here depends on the chain owner's answers (ADR-126, "Chain-owner switches for S3"); the real signer, grant wrapping and cadence plug in through `SnapshotVerifier`, `ChainGrantEvent` and `SnapshotSchedule`.

## Tests to run (targeted only)

```
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p weftos-edge-creds --lib                       # 94 pass, 2 ignored (known-gap regressions)
cargo clippy -p weftos-edge-creds --all-targets --features mock,keygen,loopback-chain -- -D warnings
cargo check -p weftos-edge-creds --target wasm32-unknown-unknown
cargo test -p clawft-mesh-local --test cert_tests           # verify_strict small-order tests
cargo test -p clawft-kernel --lib node_facts_advert         # small-order test in the kernel
scripts/diff-gate.sh                                        # expect OK
```

The two ignored tests describe the unguarded seq default and stay ignored by design. Live RustFS/PG tests are `#[ignore]` and are not part of this run (gate C6 needs the owner's PG items listed in ADR-126).

## Left out on purpose

`prep/adr126-s3-reqwest-client` (native HTTP client) is unmerged; it adds `reqwest` and needs an owner diff-gate approval. Do not merge it.
