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
3. **Add `crates/weftos-pq-sign`** with real ML-DSA-65 (FIPS 204, category 3) through RustCrypto's pure-Rust `ml-dsa` crate. The first draft used `pqcrypto-mldsa` (PQClean); the owner chose the swap on 2026-10-30 (see "Backend").
4. **Migrate ExoChain signing** to it, with an explicit legacy policy for placeholder signatures already on disk or on the wire, and a downgrade rule once a chain has cut over.

### Backend: RustCrypto `ml-dsa`, not PQClean

`cargo audit` flags the whole PQClean binding as unmaintained: RUSTSEC-2026-0166 (`pqcrypto-mldsa`), RUSTSEC-2026-0163 (`pqcrypto-internals`) and RUSTSEC-2026-0162 (`pqcrypto-traits`), all dated 2026-06-04, because PQClean is being archived and will get no further security fixes. The advisories point at RustCrypto `ml-dsa`. The workspace pins `ml-dsa` 0.1.1 (2026-06-05, the latest release), with features `alloc`, `getrandom` and `zeroize` and without `pkcs8`. Its one advisory, RUSTSEC-2025-0144 (a timing side-channel in `decompose`), is fixed from 0.1.0-rc.3 on. No `pqcrypto-*` crate is left in `Cargo.lock`, and no audit ignore was added.

What the swap changes:

- **No C, no feature switch.** The backend is pure Rust, so the optional `mldsa` feature and its fail-closed `Unsupported` mode are gone. The wasm32-wasip1 project guest now dual-signs with real ML-DSA-65 (checked with `scripts/build.sh test-wasm-project`: the guest writes `project.mldsa65`, a v2 checkpoint and the cutover record).
- **Same wire format.** Public keys (1952 bytes), signatures (3309 bytes) and the FIPS 204 context handling are the standard encodings. A signature PQClean made verifies under RustCrypto and the reverse, and PQClean's expanded secret key loads into RustCrypto with the same public key; a PQClean signature is kept as a test vector (`crates/weftos-pq-sign/testdata/pqclean-ml-dsa-65.vector`).
- **Key file v2.** RustCrypto treats the 32-byte FIPS 204 seed ξ as the private key and deprecates the 4032-byte expanded form. The key file is now `"WPQK" | v2 | algo 1 | 0 0 | pk | seed` (1992 bytes). A v1 file (`pk | expanded sk`, written only by the unreleased first draft of this change) is refused with a message saying to delete it and restart. Loading derives the public key from the seed and requires it to equal the stored one.
- **Hedged signing.** Each signature is FIPS 204 Algorithm 2 with fresh OS randomness (`sign_randomized`), not the deterministic variant.
- **Stack.** `ml-dsa` keeps its matrices on the stack: measured at 256-512 KiB per key generation, signature or verification in a release build and 0.5-1 MiB in a debug one. That overflowed kernel boot tests, so each operation runs on a scoped thread with an 8 MiB stack, and the expanded key is boxed. On wasm, which has no threads, the operation runs inline: the guest keeps the default 1 MiB linked stack and the runner keeps Wasmtime's default `max_wasm_stack`, and `test-wasm-project` (which signs on save and verifies on restart) passes with both a release and a debug guest, so neither limit was raised. Cost of the thread: about 80 µs per spawn against about 4 ms per ML-DSA verify in a debug build (2%). A chain file holds one checkpoint, so loading and verifying a chain is one ML-DSA verify and one thread whatever its length; measured in a debug build, a 10,000-event chain loads in 0.76 s (`load_from_rvf`, no ML-DSA) and verifies in 0.14 s (`verify_rvf_dual_signature`). No batching is needed.
- **Known answers.** Tests derive the IETF LAMPS example key (seed `00..1f`) and match its published public key byte for byte, verify the PQClean vector, and round-trip between the seed form and the expanded encoding in both directions.

### The crate: `weftos-pq-sign`

- `mldsa65::SigningKey` holds the seed, the key expanded from it, and its verifying key. It is not `Clone`, its `Debug` prints only a public key id, and the seed and expanded key are zeroized on drop. `generate()` / `try_generate()`, `from_seed(&[u8; 32])`, `sign(msg, ctx)`, `verifying_key()`, `to_keypair_bytes()` (zeroizing buffer, `pk || seed`), and `from_keypair_bytes()`, which proves the halves match by deriving the public key from the seed.
- `mldsa65::VerifyingKey` (1952 bytes) has `from_bytes`, `as_bytes`, `key_id` and `verify(msg, ctx, sig)`, and **no signing method**. A `compile_fail` doctest pins that. Its bytes are rejected by `from_keypair_bytes`.
- `mldsa65::Signature` is exactly 3309 bytes. Every FIPS 204 context string is checked to be at most 255 bytes.
- `dual::DualSignature { ed25519, ml_dsa65, suite }` is the old kernel serde shape plus `suite: SigSuite`, which defaults to the legacy suite when absent. `SigSuite` is `LegacyPlaceholderV1` (`"ed25519+hmac-placeholder/v1"`, tag 1) or `Ed25519MlDsa65V2` (`"ed25519+ml-dsa-65/v2"`, tag 2). A binary form `"WDS" | tag | ed25519[64] | len u16 | pq` is length-checked on decode.
- `DualSignature::verify` returns a `Verdict`: `Valid(PqStatus)` or `Rejected(RejectReason)`. `PqStatus` is `Verified`, `LegacyPlaceholder`, `NotChecked` (a real PQ half, but no key given) or `Absent`. Ed25519 is verified with `verify_strict` and is always mandatory. `Verdict::is_post_quantum()` is true only for `Valid(Verified)`.
- `keyfile` stores `"WPQK" | v2 | algo 1 | pk | seed`, written 0600 through a `create_new` temporary file that is synced and renamed. It never overwrites. Loading refuses symlinks, non-regular files, any group or world permission bit (it does not repair them) and a v1 file.

### Migration and legacy policy (clawft-kernel)

**Checkpoints.** An RVF chain file ends with its checkpoint segment, an Ed25519 footer, and, for a dual-signed node, a second footer with `sig_algo = 1`.

- **Writing.** A node with both keys adds `"sig_suite": "ed25519+ml-dsa-65/v2"` to the checkpoint CBOR, and the second footer is a real ML-DSA-65 signature over the whole checkpoint segment as written (header, payload, padding) under the context `weftos/exochain/checkpoint/ml-dsa-65/v2`. The tag lives inside the bytes the Ed25519 footer signs, so it cannot be added or stripped without breaking Ed25519.
- **Verifying** (`ChainManager::verify_rvf_dual_signature`, now returning `Verdict`):
  - Ed25519 is mandatory.
  - A v2 checkpoint must carry a well-formed ML-DSA-65 footer that ends the file and verifies; otherwise `Rejected(MlDsa65)`. A stripped or swapped footer is a rejection, not a downgrade.
  - A checkpoint without the tag is legacy: the second footer is not checked and the verdict is `Valid(LegacyPlaceholder)`, or `Valid(Absent)` without one. Legacy is **never** reported as post-quantum.
- **Loading** (`load_from_rvf`) still does not check signatures, but it enforces the downgrade rule below. It logs `pq_legacy` when a legacy second footer is present.

**Downgrade.** A chain file holds one checkpoint and is rewritten whole on every save, so a "legacy checkpoint appended later" can only be a whole file re-signed with Ed25519 alone, by someone who can forge or holds Ed25519 but not ML-DSA-65. The rule is enforced at two levels.

- *In the chain.* The first dual-signed `save_to_rvf` appends a hash-linked event, source `chain`, kind `pq.cutover`, with the suite and the ML-DSA-65 key id. Every later checkpoint over a history containing that event must be v2: `verify_checkpoint` returns `Rejected(Downgrade)` for a legacy or Ed25519-only one, `load_from_rvf` refuses it, and `save_to_rvf` refuses to write one (a node that lost its ML-DSA key cannot save its chain until the key is back). Keeping any post-cutover history means keeping the event, because the events are hash-linked.
- *On the node.* After its shutdown save, a node writes a cutover record beside its chain file (`chain.rvf` gives `chain.pq-cutover`: chain id, sequence and event hash). While the record exists, `load_from_rvf` and `verify_rvf_dual_signature` refuse any file at that path that lacks exactly that cutover event, which catches a history rewritten from before the cutover, and boot no longer falls back to the unsigned JSON checkpoint. Without a record (a legacy or pre-cutover chain) boot is unchanged, JSON fallback included (`boot::tests::boot_json_fallback_is_refused_only_after_the_pq_cutover`). A corrupt record fails closed. The project guest keeps the same record.

Before the cutover, legacy checkpoints still verify on Ed25519 and are flagged `LegacyPlaceholder` or `Absent`. Tests cover a legacy checkpoint after the cutover (with and without a placeholder footer: rejected), one before it (verifies, flagged), the save refusal, the node record against a rewritten history, and the existing stripped and swapped footer cases. Limits: the record is on the same disk as the chain and the keys, so an attacker who can write that disk is out of scope (they could replace the ML-DSA key too). A verifier given only a file, with no record, cannot tell "never cut over" from "rewritten from before the cutover"; peers need per-signer state, which arrives with the identity work in open question 3. Per-event `DualSignature`s have no cutover rule yet for the same reason.

**Event signatures.** `ChainManager::dual_sign` produces a v2 `DualSignature`, with ML-DSA-65 under `weftos/exochain/dual-sig/ml-dsa-65/v2` when the node has the key. `verify_dual_signature` returns `Verdict`. A value with no `suite` field (written by an old node) deserializes as legacy and is accepted on Ed25519 alone, flagged `LegacyPlaceholder`. Callers that need post-quantum assurance, such as ADR-028's cross-node rule, must check `is_post_quantum()`. `is_valid()` replaces the old `bool`. There are no callers outside `chain.rs` today.

**Keys.** The ML-DSA-65 key lives beside the node's Ed25519 key with the extension `mldsa65`: `~/.weftos/user.key` becomes `user.mldsa65`, `chain.key` becomes `chain.mldsa65`, and a project's `project.key` becomes `project.mldsa65`. It is generated on the first boot after the upgrade and reused afterwards, with the same owner-only protection as the Ed25519 key, but stricter: a loose mode is refused, not repaired. Nothing is derived from the Ed25519 key any more. The old placeholder key was never stored, so there is nothing to migrate or destroy. If the key cannot be loaded or created, boot logs it and the node signs Ed25519-only; once its chain has cut over, that node cannot save the chain until the key is restored.

**Upgrade path.**
1. Upgrade the binary. On the next boot each node generates its ML-DSA-65 key; the boot log names the key id and path, never the key.
2. Old `.rvf` checkpoints still load and verify as legacy. The next save appends the cutover event and rewrites the checkpoint with v2 signatures, so a node's own chain becomes post-quantum signed at its first checkpoint after the upgrade, and cannot be downgraded after it.
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
- The `unsafe` `transmute_copy` in `chain.rs` is gone. The new crate is `#![forbid(unsafe_code)]`, and the backend is pure Rust with no `build.rs`.
- The wasm project guest signs with ML-DSA-65 too.

**Negative.**
- `ml-dsa` 0.1 is young and unaudited, as PQClean's binding was. It is behind `mldsa65.rs` only, so another backend stays a one-file change.
- Each ML-DSA operation spawns a short-lived thread on native targets, because of the stack use above.
- ML-DSA-65 adds about 3.3 KB to each checkpoint and each dual signature, plus a 2 KB key file per node.
- The first dual-signed save adds one event to every chain, and after it a node without its ML-DSA key cannot save its chain.
- `verify_rvf_dual_signature` and `verify_dual_signature` now return `Verdict` instead of `bool`. That is a source break for any out-of-tree caller.
- `ml-dsa` computes intermediate signing values on the stack, which this crate cannot zeroize.

## Open questions for the owner

1. ~~`pqcrypto-mldsa` is flagged unmaintained.~~ Resolved 2026-10-30: the owner chose RustCrypto `ml-dsa` (see "Backend").
2. **Diff-gate approvals.** The new dependencies (`ml-dsa` and its RustCrypto tree, the new crate, and the swap from the forks to upstream `rvf-crypto` / `rvf-wire`) are `new-dep` findings for the owner to approve.
3. Should ADR-028's cross-node rule now be enforced: reject a cross-node event unless `is_post_quantum()`? That needs ML-DSA verifying keys in the node identity, which is out of scope here.
