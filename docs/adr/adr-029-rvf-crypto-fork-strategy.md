# ADR-029: weftos-rvf-crypto as Published Fork of rvf-crypto

**Date**: 2026-04-03
**Status**: Superseded by [ADR-129](adr-129-upstream-rvf-and-real-ml-dsa.md) (2026-10-30). The forks were crates.io-only renames of older upstream sources; the wire fork had no WeftOS change and the crypto fork's ML-DSA-65 was an HMAC placeholder. WeftOS now uses upstream `rvf-crypto` / `rvf-wire` and `crates/weftos-pq-sign`.
**Deciders**: Architecture review, Sprint 14 (informed by ADR-012 context, K2 D11)
**Depends-On**: ADR-012 (Inline sha3 to Eliminate rvf-crypto Path Dependency)

## Context

The upstream `rvf-crypto` crate (published on crates.io by ruvnet) provides Ed25519 signing and SHAKE-256 hashing for the RVF ecosystem. However, it does not include ML-DSA-65 (Dilithium) post-quantum signature support, which WeftOS requires for dual signing of ExoChain events (ADR-028). Three options were considered:

1. **Contribute ML-DSA-65 upstream**: Submit a PR to `rvf-crypto` adding `pqcrypto-dilithium` support. Rejected because upstream `rvf-crypto` serves a broader audience that does not need post-quantum primitives, and the `pqcrypto-dilithium` dependency is heavy (~2MB compiled, C FFI).

2. **Vendor the code**: Copy the relevant rvf-crypto source into the WeftOS workspace. Rejected because vendoring loses upstream bug fixes and makes the provenance of cryptographic code ambiguous.

3. **Published fork**: Maintain `weftos-rvf-crypto` and `weftos-rvf-wire` as independent crates on crates.io with the `weftos-` prefix. These are forks that extend the upstream API surface with WeftOS-specific features while tracking upstream changes.

ADR-012 previously identified the `rvf-crypto` path dependency as a release blocker (critical blocker B1) and established the pattern of feature-gating RVF dependencies. This ADR formalizes the fork as the long-term strategy.

## Decision

WeftOS publishes two forked crates on crates.io:

| Crate | Version | Upstream | Key Addition |
|-------|---------|----------|-------------|
| `weftos-rvf-crypto` | 0.3 | `rvf-crypto` | `DualSignature` with ML-DSA-65 via `pqcrypto-dilithium`, `DualKey` type |
| `weftos-rvf-wire` | 0.2 | `rvf-wire` | Extended segment types for mesh framing (TYPE bytes 0x03-0x06) |

These are forks, not wrappers. They re-export the upstream API surface and extend it. The `weftos-` prefix naming convention is mandatory for all forked crates to avoid confusion with upstream.

Workspace dependency declarations in `Cargo.toml`:

```toml
weftos-rvf-crypto = { version = "0.3", default-features = false, features = ["std", "ed25519"] }
weftos-rvf-wire = { version = "0.2" }
```

The fork maintenance process:
1. Track upstream `rvf-crypto` and `rvf-wire` releases via GitHub watch
2. Merge upstream changes into the fork, resolving conflicts in the ML-DSA-65 additions
3. Publish new fork versions within one week of upstream releases
4. CI runs both upstream and fork test suites to detect divergence

## Consequences

### Positive
- ML-DSA-65 dual signing is available as a first-class feature without waiting for upstream adoption
- Published on crates.io: downstream consumers and CI can depend on `weftos-rvf-crypto` without path dependencies or git URLs, resolving the ADR-012 release blocker
- The `weftos-` prefix clearly distinguishes fork crates from upstream, preventing accidental dependency confusion
- Fork retains upstream API compatibility, so migrating back to upstream (if it adds ML-DSA-65) requires only a crate rename in `Cargo.toml`

### Negative
- Two crates must be maintained in sync with upstream `rvf-crypto` and `rvf-wire` changes -- this is an ongoing maintenance obligation proportional to upstream release frequency
- Version conflicts are possible if other workspace crates or downstream consumers depend on upstream `rvf-crypto` directly; the workspace must use `[patch]` or ensure exclusive use of the fork
- The fork diverges from upstream with every WeftOS-specific addition, increasing the merge cost over time
- Publishing forked cryptographic crates carries reputation risk: users must trust that `weftos-rvf-crypto` has not introduced vulnerabilities in its extensions

### Neutral
- The fork strategy is a common pattern in the Rust ecosystem (e.g., `rustls` forks of `ring`, various `tokio` forks during major version transitions)
- If upstream `rvf-crypto` adds ML-DSA-65 support in the future, the fork can be deprecated and replaced with a version bump to upstream
- The `weftos-rvf-wire` fork extensions (mesh frame types) are additive and do not conflict with upstream wire format; they occupy previously unassigned type bytes
