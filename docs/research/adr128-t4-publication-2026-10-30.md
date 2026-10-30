# ADR-128 T4: publication of sealed layers (2026-10-30)

Verdict: **PASS** on all three bars, with one sizing finding (SHAKE-256 throughput) that shapes layer size. Spike crate: `/Volumes/ai-models/research/weftos-publish-spike` (own `[workspace]`, `CARGO_TARGET_DIR=/Volumes/ai-models/research/weftos-tree-target`, built and tested under the `heavy` slot, release profile). Tailscale was down, so two nodes are two local processes with separate caches. Six integration tests, all green, including a 17-case fail-closed matrix.

## What is real and what is simulated

| Piece | Status |
|---|---|
| SHAKE-256 layer naming, Ed25519 + ML-DSA-65 dual signature, pinned-key verify | **Real.** Ed25519 via `ed25519-dalek` 2, ML-DSA-65 (FIPS 204, 3309-byte signatures) via `pqcrypto-mldsa` 0.1.2 |
| Subscriber logic (ACL, pin, verify, fetch, hash, install) | **Real code**, run as a separate OS process (`t4 subscriber`) |
| Announcement bus | **Stand-in**: append-only file, polled at 1 ms. The substrate (ADR-064 sync, real envelopes) is not exercised |
| Store | **Stand-in** for RustFS: a directory of objects keyed by name. S3 latency and auth are not exercised |
| ADR-057 ACL | **Mirrored**, not reused. The in-tree table is in `clawft-substrate/src/acl.rs`; pulling it in drags the substrate crate along. Same rule model (allow/deny identity strings, trailing `**` and one-segment `*` globs, deny wins). Subset only: no `inherit`, no `scope:` tokens from a capability service, `public` deliberately ignored for layers |
| ADR-063 envelope | **Mirrored in style, not wired.** Fixed-field, length-prefixed canonical bytes with a version tag. The announcement is not a `SubstrateEnvelope` (no `seq`, `ts_ms`, BLAKE3 id) and there is no path-ownership gate |
| Two nodes | One machine, one clock, two processes. Timing uses one `SystemTime` source, so the numbers carry no cross-host skew and no network |

**`weftos-rvf-crypto` could not be reused for the PQ half.** Its `MlDsa65Key` (v0.3.0, `dual_sign.rs`) is an HMAC-SHA3 placeholder: symmetric, verify key equal to signing key, "real Dilithium3 behind a future feature". It cannot prove publisher authenticity, and `clawft-kernel/chain.rs` currently signs ExoChain segments with it. That is a gap for ADR-028's claim, separate from T4. The spike uses `pqcrypto-mldsa` directly. The successor to `pqcrypto-dilithium` is the one in use here, and it is the final-standard ML-DSA, not the round-3 Dilithium3.

## Formats

**Layer name:** `SHAKE-256(b"weft-layer-v1\0" || layer_bytes)`, 32 bytes. The layer bytes can be the T5 canonical layer encoding; the spike treats them as opaque.

**Announcement** (the thing signed), canonical bytes in this order, strings as `"{len}:{s}\n"`:

```
weftos.layer.announce.v1\n
name <hex>\n
parents <n>\n   <hex>\n x n
size <u64>\n
<account>  <path_scope>  <signer key id>
```

The key id is the first 16 bytes of SHAKE-256 over both public keys. Both signatures cover exactly these bytes. Path scope is signed too (the task listed name, parents, size, account, key id; adding the scope stops a relay from re-scoping a layer). On the bus an announcement is one JSON line with hex fields (7012 bytes, dominated by the 3309-byte ML-DSA signature). Verifiers rebuild the signing bytes from parsed fields and never trust JSON.

**Subscriber pipeline**, stopping at the first failure, with nothing reaching the cache until the end:
parse (strict, unknown fields rejected) -> ACL on `layers/<account>[/<scope>]` -> signer key must be pinned (`<key_id>.pub`, file contents must hash to the file name) -> both signatures verify -> fetch by name -> size matches -> SHAKE-256 of bytes equals name -> write to cache via temp file and rename.

Account and scope are restricted to `[A-Za-z0-9._-]` segments, with no empty, `.` or `..` segments. Anything else is denied before the ACL table is read.

## Numbers (release build, M-series Mac, one machine)

Subscriber was running before publish. "announce->installed" is from just before the bus append to the subscriber's install event. 23 layers.

| Layer size | n | announce->installed | seal+put+announce+install |
|---|---|---|---|
| 64 KiB | 10 | 2.5 to 3.1 ms | about 3 ms |
| 1 MiB | 10 | 27 to 29 ms | about 29 ms |
| 32 MiB | 3 | 841 to 887 ms | 1.64 to 1.81 s |

Overall announce->installed: p50 27 ms, p95 849 ms, max 887 ms. The 2 s bar holds at every size tried.

**Finding:** time scales with size because the hash does: SHAKE-256 runs at roughly 40 MB/s here (1 MiB costs about 25 ms). The publisher hashes once and the subscriber hashes again, so a 32 MiB layer spends 1.7 s of the 1.8 s end-to-end budget on hashing, and a 64 MiB layer would break the 2 s bar from seal start. Signature work is not the cost (Ed25519 plus ML-DSA verify is well under 1 ms against the 2.5 ms floor). Production needs either layers kept below roughly 16 MiB, or a layer name defined over chunk hashes (T2 already chunks; hash the chunk-hash list, verify chunks as they stream). Raise this for B2.

## Tamper (nothing installed, cache empty in every case)

| Attack | Outcome |
|---|---|
| One byte flipped in the stored object | `hash_mismatch` (fetched, rejected) |
| Object truncated by one byte | `size_mismatch` |
| Object swapped for same-size different bytes | `hash_mismatch` |
| Ed25519 half flipped | `bad_signature` |
| ML-DSA half flipped | `bad_signature` |
| ML-DSA truncated, ML-DSA missing (Ed only), Ed25519 missing (PQ only) | `bad_signature` |
| size, path scope or parents changed after signing | `bad_signature` |
| Rogue key signs validly but claims the pinned key id | `bad_signature` |
| Pinned signature spliced onto a different layer | `bad_signature` |
| Valid dual signature from a key that is not pinned | `unpinned_key` |
| Garbage line on the bus | `malformed` |

Both signature halves are required, so a classical-only or PQ-only forgery fails. Every signature, key and malformed-input case was rejected before any fetch: the store's fetch log stayed empty. An honest layer published after each attack installed, so the node was alive and the negatives are not vacuous.

## Isolation, fail closed

Two subscribers, account A (`node:nA`) and account B (`node:nB`), same bus and store, three layers (A with scope `src/secret`, B, A with no scope). B's cache held only B's layer, A's held only A's two, and the store's fetch log shows `nB` fetched exactly one object, its own. B logged two `denied_acl` events and fetched nothing of A's.

Fail-closed matrix, 17 cases, each on a fresh bus, each followed by a positive control (valid ACL written, a fresh layer installs, the denied layer stays absent). All 17 denied with the layer not fetched and not cached:

missing ACL file; empty file; malformed JSON; wrong JSON shape; unknown field; no rules; rule without `allow`; allow for another identity; `public` allow (ignored for layers); B-only rule against an A layer; prefix account `BB` against a `B` rule; deny overriding allow; allow scoped to a different subtree; hostile account `../B`; hostile scope `src/../../A`; absolute scope `/etc`; empty account. Also covered: removing the ACL file mid-run denies the next announcement (the table is reread per announcement), and the denial reason is distinguishable (`denied_acl`) from a missing object (`fetch_failed`), as ADR-057 requires.

## Not proven, needed for production (B2)

- Real substrate transport (ADR-064), the ADR-063 envelope and its path-ownership gate, and the `substrate.read.denied` ExoChain event. The spike logs denials to a local JSONL only.
- Real RustFS fetch with its own authorization; the store-side check that a subscriber may read the object (here the store has no auth, so isolation rests on the subscriber's ACL gate plus unguessable-by-ACL names, which is not enough on its own for client layers: B2 needs a per-account bucket per ADR-128 §8).
- Replay and ordering: an old valid announcement is accepted again (installation is idempotent, but there is no revocation or expiry). Key rotation and revocation of a pinned signer are untested.
- The wire cost of two signatures (about 3.4 KB per announcement) over a real mesh link.
- U1 lineage cross-check against RVF is not done; the layer name here is the spike's own SHAKE-256.
- Fix the placeholder ML-DSA in `weftos-rvf-crypto` before anyone relies on ExoChain dual-signing for authenticity.

## Reproduce

```
cd /Volumes/ai-models/research/weftos-publish-spike
CARGO_TARGET_DIR=/Volumes/ai-models/research/weftos-tree-target \
AGF_SLOT_CLASS=heavy AGF_SLOTS=1 ~/.claude/skills/agent-git-flow/scripts/slot.sh run \
  cargo test --release -- --nocapture --test-threads=1
```

Test keys are generated per test into `.run/` under the spike dir with mode 0600 (the test asserts it) and removed when each test ends; none are printed, none remain.
