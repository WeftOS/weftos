# ADR-028: Mandatory Dual Signing (Ed25519 + ML-DSA-65) for ExoChain Events

**Date**: 2026-04-03
**Status**: Accepted
- **Updated**: 2026-10-30. **Finding: the ML-DSA-65 half is not post-quantum today.** In `weftos-rvf-crypto` 0.3.0 (`src/dual_sign.rs`), `MlDsa65Key` is an HMAC placeholder whose signing key equals its verify key (also noted at `crates/clawft-kernel/src/chain.rs:2766`). Anyone holding the "verify key" can forge the ML-DSA half. Ed25519 still gives classical authenticity. Fix tracked on the board (real FIPS 204 ML-DSA-65, for example via `pqcrypto-mldsa`, which ADR-128's T4 spike used).
- **Updated**: 2026-10-30. **Fixed by [ADR-129](adr-129-upstream-rvf-and-real-ml-dsa.md)** (Proposed): real ML-DSA-65 in `crates/weftos-pq-sign`, a versioned suite tag on chain signatures, and legacy placeholder signatures accepted on Ed25519 only and never reported as post-quantum. The `pqcrypto-dilithium` and `weftos-rvf-crypto` `DualSignature` references below describe the old design.
**Deciders**: K2 Symposium (D11), K5 Symposium Security Panel (D9, S5)
**Depends-On**: ADR-025 (Ed25519 Node Identity), ADR-029 (weftos-rvf-crypto Fork Strategy)
**Note (2026-04-28, WEFT-140)**: ADR-028 was previously shared with
"WeftOS Kernel Architecture", which lived under
`docs/architecture/`. That document has been moved into this
directory and renumbered to ADR-049; this dual-signing ADR keeps the
028 slot since it is the K2-symposium-derived decision dependency
chain referenced by the rest of the security ADR family
(ADR-025/029/030).

## Context

ExoChain events are the tamper-evident audit trail at the heart of WeftOS. Chain entries are permanent -- agent lifetimes may be short, but the chain persists indefinitely. If a sufficiently powerful quantum computer is built, Shor's algorithm would break Ed25519 signatures, allowing an attacker to forge chain events retroactively. The "harvest now, decrypt later" threat model means that chain signatures created today must resist quantum attacks that may become feasible in 10-20 years.

The K2 Symposium (D11) decided to enable post-quantum signatures immediately. The `rvf-crypto` upstream crate already supports ML-DSA-65 (Dilithium) dual-key signing via a `DualKey` type, but WeftOS maintains a fork (`weftos-rvf-crypto`, ADR-029) that extends this support. The K5 Security Panel (D9) formalized the requirement: cross-node chain events MUST carry both signatures; local events default to both but may fall back to Ed25519-only for performance-constrained environments.

## Decision

Chain events carry both Ed25519 and ML-DSA-65 (FIPS 204, formerly Dilithium) signatures. The dual signature structure is:

```
ChainEvent
  +-- hash: SHAKE-256(canonical fields)
  +-- ed25519_sig: Ed25519::sign(hash)        [64 bytes]
  +-- mldsa65_sig: ML-DSA-65::sign(hash)      [2420 bytes]
```

Verification rules:
- **Cross-node events** (events that cross a node boundary during chain replication): both Ed25519 and ML-DSA-65 signatures MUST be present and MUST verify. If either fails, the event is rejected.
- **Local events** (events produced and consumed on the same node): both signatures are produced by default. A node MAY be configured to produce Ed25519-only for local events via a governance-gated configuration flag, but this is not recommended.

The `DualSignature` type from `weftos-rvf-crypto` (version 0.3) provides the signing and verification implementation. The ML-DSA-65 implementation comes from the `pqcrypto-dilithium` crate, wrapped by `weftos-rvf-crypto`.

### Post-Quantum Roadmap

| Layer | Classical | Post-Quantum | Timeline |
|-------|-----------|-------------|----------|
| Chain event signing | Ed25519 | ML-DSA-65 (dual) | Now (K5, this ADR) |
| Node identity signing | Ed25519 | ML-DSA-65 (dual) | K6.0 |
| Noise handshake DH | X25519 | -- | K6.1 |
| Key exchange | X25519 | ML-KEM-768 (hybrid) | K6.4b |
| Message signing | Ed25519 | ML-DSA-65 (optional) | K6.3 |

## Consequences

### Positive
- Chain events are quantum-resistant today: even if quantum computers break Ed25519 in the future, the ML-DSA-65 signature remains valid, protecting the integrity of the audit trail
- Backward-compatible: existing tooling that only understands Ed25519 can verify the classical signature; the ML-DSA-65 signature is additive
- Leverages existing infrastructure: `chain.rs` already supports `DualSignature`; the K5/K6 change enforces it for cross-node events rather than introducing new code
- Aligns with industry practice: Chrome, Signal, and AWS use similar dual/hybrid approaches for post-quantum migration

### Negative
- Signature size increases from ~64 bytes (Ed25519 only) to ~2,484 bytes (Ed25519 + ML-DSA-65) per chain event -- a ~39x increase in signature storage
- ML-DSA-65 signing is slower than Ed25519: approximately 2-5ms per signature vs sub-millisecond for Ed25519, affecting chain append throughput
- The `weftos-rvf-crypto` fork (ADR-029) must be maintained to provide ML-DSA-65 support not present in upstream `rvf-crypto`, creating a long-term maintenance obligation
- Verification requires checking both signatures for cross-node events, roughly doubling verification time compared to Ed25519-only

### Neutral
- ML-DSA-65 is a NIST FIPS 204 standard (finalized 2024), not an experimental algorithm; the standardization risk is low
- The `pqcrypto-dilithium` crate used by `weftos-rvf-crypto` is a Rust wrapper around the reference C implementation; it is not constant-time on all platforms, but signing keys are per-node (not shared), limiting the side-channel attack surface
- Chain storage growth from larger signatures is bounded by chain compaction and archival policies, which are orthogonal to this decision
