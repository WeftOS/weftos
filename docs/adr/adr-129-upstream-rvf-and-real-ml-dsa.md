# ADR-129: Upstream rvf-crypto and rvf-wire, real ML-DSA-65 in weftos-pq-sign, and a legacy policy for placeholder chain signatures

- **Status**: Proposed (2026-10-30; owner direction "go with your recommendation", relayed by the lead).
- **Supersedes**: ADR-029 (weftos-rvf-crypto fork strategy).
- **Fixes**: the ADR-028 finding of 2026-10-30, where the ML-DSA-65 half of the dual signature was an HMAC placeholder.
- **Finishes**: ADR-128 T0 (workspace on rvf-runtime 0.5 and rvf-types 0.3).
- **Deciders**: owner
- **Prior decisions**: the decision store, consulted on 2026-10-30, holds nothing on this topic.
- **Board**: card 7bed694c (real ML-DSA-65).

## Context

ADR-029 made `weftos-rvf-crypto` 0.3 and `weftos-rvf-wire` 0.2 published forks of the upstream ruvnet crates, to carry ML-DSA-65 dual signing (ADR-028). Three things are now known about them.

1. **The forks exist only as crates.io tarballs.** Their `repository` field points at `weave-logic-ai/ruvector`, but no branch there holds them. They cannot be rebased, reviewed or rebuilt from source we control.
2. **The ML-DSA-65 half was never ML-DSA.** In `weftos-rvf-crypto` 0.3.0 `src/dual_sign.rs`, `MlDsa65Key` is a 32-byte HMAC key, and `MlDsa65VerifyKey` holds the same 32 bytes. The "signature" is a SHAKE-256 keystream over `key || message || key`. Anyone who can verify can sign. Boot (`clawft-kernel/src/boot.rs`) derived that key as SHAKE-256 of the node's Ed25519 secret, and `chain.rs` pulled the bytes out of the opaque verify key with an `unsafe` `transmute_copy`. The chain had classical Ed25519 authenticity only.
3. **They block ADR-128 T0.** The forks depend on rvf-types 0.2, and `clawft-kernel` passes rvf-types values (`Scorecard`, `LineageRecord`, `SegmentFlags`) into them, so the workspace could not move to rvf-types 0.3. T0 landed partially (422e5b6be), with per-crate 0.5 / 0.3 overrides in `clawft-cow-memory` and `clawft-core`.

### What the forks changed

Diffed against the upstream crates.io packages (both cut from ruvnet/ruvector `70cc0b87`, `crates/rvf/rvf-crypto` and `crates/rvf/rvf-wire`):

| Fork | Base | WeftOS changes | Upstream has since added |
|------|------|----------------|--------------------------|
| `weftos-rvf-wire` 0.2.0 | an older `rvf-wire` | Package and lib renamed; rvf-types pinned to 0.2; one `use` reformatted. **No behaviour change.** ADR-029's "extended segment types for mesh framing (0x03-0x06)" are not in the tarball. | Decoder hardening: allocation bounded by payload in the HOT, INDEX and VEC codecs, checked adds on adjacency and delta ids, a tenth-byte check in varint decoding, rejection of unknown id encodings; the single tail scanner (`split_root_page`, `manifest_candidates`, `ScanMode`); the `stream` module. |
| `weftos-rvf-crypto` 0.3.0 | an older `rvf-crypto` | Renamed; rvf-types 0.2; the `dual_sign` module (the placeholder above); a public `build_signed_data_pub` accessor for it. | `stream` (descriptor signing), witness helpers (`encode_chain_entry`, `chain_entry_hash`, `verify_witness_chain_from`, legacy witness classification), and an attestation bounds fix (`rec_start > rec_end`, where the fork checked `rec_start > len`). |

Nothing in either fork needs keeping. The wire fork is strictly older than upstream. The only crypto addition is the placeholder.

## Decision

1. **Drop both forks.** The workspace depends on upstream `rvf-crypto` 0.3 (SHAKE-256, Ed25519 segment signing, witness chains, footers, lineage) and upstream `rvf-wire` 0.2, the versions rvf-runtime 0.5 uses.
2. **Move the workspace pins** to `rvf-runtime` 0.5 and `rvf-types` 0.3, and remove the per-crate overrides that T0 added. The kernel compiles unchanged on the new types once the forks are gone, and the lockfile carries one rvf-types. The standalone `tools/build-kb` and `crates/weftos-n6-leaf-hostcheck` move to upstream `rvf-wire` too. The no_std firmware `crates/weftos-n6-leaf` keeps rvf-types 0.2: it only writes segment headers, its own lockfile is untouched, and the wire format is unchanged.
3. **Add `crates/weftos-pq-sign`** with real ML-DSA-65 (FIPS 204, category 3) through `pqcrypto-mldsa` (PQClean). See the open question on that choice.
4. **Migrate ExoChain signing** to it, with an explicit legacy policy for placeholder signatures already on disk or on the wire.

### The crate: `weftos-pq-sign`

- `mldsa65::SigningKey` holds the secret and its verifying key. It is not `Clone`, its `Debug` prints only a public key id, and the secret bytes are zeroized on drop. `generate()`, `sign(msg, ctx)`, `verifying_key()`, `to_keypair_bytes()` (zeroizing buffer, `pk || sk`), and `from_keypair_bytes()`, which proves the halves match by signing and verifying a probe.
- `mldsa65::VerifyingKey` (1952 bytes) has `from_bytes`, `as_bytes`, `key_id` and `verify(msg, ctx, sig)`, and **no signing method**. A `compile_fail` doctest pins that. Its bytes are rejected by `from_keypair_bytes`.
- `mldsa65::Signature` is exactly 3309 bytes. Every FIPS 204 context string is checked to be at most 255 bytes.
- `dual::DualSignature { ed25519, ml_dsa65, suite }` is the old kernel serde shape plus `suite: SigSuite`, which defaults to the legacy suite when absent. `SigSuite` is `LegacyPlaceholderV1` (`"ed25519+hmac-placeholder/v1"`, tag 1) or `Ed25519MlDsa65V2` (`"ed25519+ml-dsa-65/v2"`, tag 2). A binary form `"WDS" | tag | ed25519[64] | len u16 | pq` is length-checked on decode.
- `DualSignature::verify` returns a `Verdict`: `Valid(PqStatus)` or `Rejected(RejectReason)`. `PqStatus` is `Verified`, `LegacyPlaceholder`, `NotChecked` (a real PQ half, but no key given) or `Absent`. Ed25519 is verified with `verify_strict` and is always mandatory. `Verdict::is_post_quantum()` is true only for `Valid(Verified)`.
- `keyfile` stores `"WPQK" | v1 | algo 1 | pk | sk`, written 0600 through a `create_new` temporary file that is synced and renamed. It never overwrites. Loading refuses symlinks, non-regular files and any group or world permission bit, and does not repair them.
- The PQClean backend is the default feature `mldsa`. Without it the types still compile, but no key can be built and every sign or verify returns `Unsupported`, so verification fails closed. The wasm32-wasip1 project guest uses that mode, because PQClean's C build needs a WASI SDK there.

### Migration and legacy policy (clawft-kernel)

**Checkpoints.** An RVF chain file ends with its checkpoint segment, an Ed25519 footer, and, for a dual-signed node, a second footer with `sig_algo = 1`.

- **Writing.** A node with both keys adds `"sig_suite": "ed25519+ml-dsa-65/v2"` to the checkpoint CBOR, and the second footer is a real ML-DSA-65 signature over the whole checkpoint segment as written (header, payload, padding) under the context `weftos/exochain/checkpoint/ml-dsa-65/v2`. The tag lives inside the bytes the Ed25519 footer signs, so it cannot be added or stripped without breaking Ed25519.
- **Verifying** (`ChainManager::verify_rvf_dual_signature`, now returning `Verdict`):
  - Ed25519 is mandatory.
  - A v2 checkpoint must carry a well-formed ML-DSA-65 footer that ends the file and verifies; otherwise `Rejected(MlDsa65)`. A stripped or swapped footer is a rejection, not a downgrade.
  - A checkpoint without the tag is legacy: the second footer is not checked and the verdict is `Valid(LegacyPlaceholder)`, or `Valid(Absent)` without one. Legacy is **never** reported as post-quantum.
- **Loading** (`load_from_rvf`) is unchanged and still does not check signatures. It logs `pq_legacy` when a legacy second footer is present.

**Event signatures.** `ChainManager::dual_sign` produces a v2 `DualSignature`, with ML-DSA-65 under `weftos/exochain/dual-sig/ml-dsa-65/v2` when the node has the key. `verify_dual_signature` returns `Verdict`. A value with no `suite` field (written by an old node) deserializes as legacy and is accepted on Ed25519 alone, flagged `LegacyPlaceholder`. Callers that need post-quantum assurance, such as ADR-028's cross-node rule, must check `is_post_quantum()`. `is_valid()` replaces the old `bool`. There are no callers outside `chain.rs` today.

**Keys.** The ML-DSA-65 key lives beside the node's Ed25519 key with the extension `mldsa65`: `~/.weftos/user.key` becomes `user.mldsa65`, `chain.key` becomes `chain.mldsa65`, and a project's `project.key` becomes `project.mldsa65`. It is generated on the first boot after the upgrade and reused afterwards, with the same owner-only protection as the Ed25519 key, but stricter: a loose mode is refused, not repaired. Nothing is derived from the Ed25519 key any more. The old placeholder key was never stored, so there is nothing to migrate or destroy. If the key cannot be loaded or created, boot logs it and the node signs Ed25519-only.

**Upgrade path.**
1. Upgrade the binary. On the next boot each node generates its ML-DSA-65 key; the boot log names the key id and path, never the key.
2. Old `.rvf` checkpoints still load and verify as legacy. The next save rewrites the checkpoint with v2 signatures, so a node's own chain becomes post-quantum signed at its first checkpoint after the upgrade.
3. Distributing ML-DSA-65 verifying keys to peers is not part of this change. Until a peer has a node's verifying key, that node's v2 signatures verify as `NotChecked` there. The key is available from `ChainManager::ml_dsa_verifying_key()` for the mesh identity work to publish.

### Mixed-version behaviour

| Writer → reader | Checkpoint file | `DualSignature` |
|-----------------|-----------------|-----------------|
| old → new | `Valid(LegacyPlaceholder)` if Ed25519 verifies; never post-quantum | no `suite` field → legacy; `Valid(LegacyPlaceholder)` on Ed25519 |
| new → old | `verify_rvf_signature` (Ed25519) passes. The old `verify_rvf_dual_signature` returns `false`, because it recomputes the HMAC | the unknown `suite` is ignored. An old node never had a peer's placeholder key (it was derived from that peer's secret), so in practice it verifies Ed25519 only and accepts |
| new → new | `Valid(Verified)` with the writer's ML-DSA key | `Valid(Verified)` with the key, `NotChecked` without |

No old node can produce a real ML-DSA signature, and no new node treats an old one as post-quantum. Mixed fleets keep exactly the Ed25519 assurance they had before.

## Consequences

**Positive.**
- Chain checkpoints and event signatures get real FIPS 204 ML-DSA-65; a verifying key cannot sign, and tests prove it.
- No crypto fork to maintain. Upstream decoder and witness fixes arrive with each rvf release.
- ADR-128 T0 is finished: one rvf-types (0.3) in the lockfile, and no per-crate overrides.
- The `unsafe` `transmute_copy` in `chain.rs` is gone. The new crate is `#![forbid(unsafe_code)]`.

**Negative.**
- `pqcrypto-mldsa` is C (PQClean) behind FFI with a `build.rs`, and it needs a C compiler on every native target. The wasm project guest gets no ML-DSA.
- ML-DSA-65 adds about 3.3 KB to each checkpoint and each dual signature, plus a 6 KB key file per node.
- `verify_rvf_dual_signature` and `verify_dual_signature` now return `Verdict` instead of `bool`. That is a source break for any out-of-tree caller.
- `pqcrypto-mldsa` copies the secret key into a stack buffer for each signature, which this crate cannot zeroize.

## Open questions for the owner

1. **`pqcrypto-mldsa` is already flagged unmaintained.** `cargo audit` reports RUSTSEC-2026-0166 (`pqcrypto-mldsa`), RUSTSEC-2026-0163 (`pqcrypto-internals`) and RUSTSEC-2026-0162 (`pqcrypto-traits`), all dated 2026-06-04: PQClean is being archived and will get no further security fixes. The advisory recommends the pure-Rust RustCrypto `ml-dsa` crate. CI's `cargo audit --deny warnings` fails on these until they are ignored or the backend changes. The backend is confined to `crates/weftos-pq-sign/src/mldsa65.rs`. Signatures and public keys are FIPS 204 encodings and would not change; whether `ml-dsa` loads PQClean's expanded secret-key encoding is not yet checked, and if it does not, existing key files would be regenerated on the switch. A pure-Rust backend would also bring ML-DSA to the wasm guest and drop the C build.
2. **Diff-gate approvals.** The new dependencies (`pqcrypto-mldsa`, `pqcrypto-traits`, the new crate, and the swap from the forks to upstream `rvf-crypto` / `rvf-wire`) are `new-dep` findings for the owner to approve.
3. Should ADR-028's cross-node rule now be enforced: reject a cross-node event unless `is_post_quantum()`? That needs ML-DSA verifying keys in the node identity, which is out of scope here.
