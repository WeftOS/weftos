# cargo audit residual ignores

Ignores live in `CARGO_AUDIT_IGNORES` in `scripts/build.sh` (gate check 12).

## wasmtime: no residual ignores

The wasmtime advisories RUSTSEC-2026-0222, -0269, -0314 and -0316 were previously ignored
with reachability arguments while the repo was on wasmtime 45.0.3 and Rust 1.93. They are
cleared: `rust-toolchain.toml` pins Rust 1.95 and `Cargo.lock` resolves wasmtime and
wasmtime-wasi to 48.0.5 (patched line 48.0.3+). The four ignores, their expiry dates
(`CARGO_AUDIT_EXPIRIES`) and the grep guard that backed them (`audit_guard_wasmtime_usage`)
were removed, so `scripts/build.sh audit` now fails if any of these advisories reappears.

The remaining ignores (paste, bincode, quick-xml, event-listener, lexical-core) are
unmaintained or transitive items documented inline in `scripts/build.sh`.

## failure: removed (2026-10-23)

`clawft-voice-aec` now uses `webrtc-audio-processing` 2.1 (bundled build, needs `meson` on
PATH), which no longer depends on `failure`. `cargo tree -i failure --workspace --all-features`
finds nothing, and the RUSTSEC-2020-0036 / RUSTSEC-2019-0036 ignores are gone from
`CARGO_AUDIT_IGNORES`. Dependabot #328 and #329 close once this lands on the default branch.

## lexical-core 0.7.6 (Dependabot #330, RUSTSEC-2023-0086, tolerable_risk)

- Reach: `imap` 2.4.1 -> `lexical-core`, behind the `email` feature, which is off in every
  build (default, release, CI, and the shipped binaries). Not compiled into anything we ship.
- No patched 0.7.x exists. The ignore stays in `CARGO_AUDIT_IGNORES` for the lockfile entry.
- Disposition: `tolerable_risk`; the GitHub dismissal is made by the lead or owner.
- Revisit when: `imap` 3 is stable (move the `email` feature to it and drop the ignore), or
  the `email` feature is enabled in any shipped build, whichever comes first.

## 2026-10-23: advisories published after rc.14

Fixed by lockfile bumps (no ignore): wasmtime and wasmtime-wasi 48.0.5 -> 48.0.7 (still 48.x),
rustls 0.23.46, rustls-webpki 0.103.16, webpki-roots and webpki-root-certs 1.0.10, tokio 1.54.1
(RUSTSEC-2026-0386). `clawft-splatd` moved from zip 2.4.2 to zip 9.0.5 (RUSTSEC-2026-0367/-0368/-0369);
its extractor was already hand-rolled (`enclosed_name`, regular files only), and now also
refuses symlink entries, with a test for the symlink and traversal refusals.

New ignores in `CARGO_AUDIT_IGNORES`:

- **RUSTSEC-2023-0071** (rsa 0.9.10, Marvin timing attack, no fix exists). Reached through
  `jsonwebtoken` 10 `rust_crypto` in `clawft-channels` (`teams` feature). Marvin is a timing
  leak of private-key operations. Production code only verifies RS256 signatures with
  public keys (`EncodingKey` appears only in `teams/jwt_tests.rs` with throwaway test keys).
  Revisit when a fixed `rsa` is published or `jsonwebtoken` drops it.
- **RUSTSEC-2026-0367, -0368, -0369** (zip 1.1.4: `extract()` SUID/SGID and symlink issues,
  fix >= 9.0.0). Reached only through `candle-core` 0.8.4, behind the optional `candle`
  feature of `weftos-worldmodel-impls`. The only candle use in the tree is
  `use candle_core::Device` (`candle/mod.rs`); no npz/zip loading and no `extract` call exists.
  No candle release uses zip 9 (0.9.1: zip 1; 0.9.2 and 0.10: zip 7; 0.11.0: zip 8.6).
  Revisit when candle moves to zip >= 9, or if the candle feature starts loading archives.
- **RUSTSEC-2026-0374** (basic-toml unmaintained). Only via `askama_derive` <- `askama` <-
  `uniffi_bindgen`, a build-time template macro reading its own config; not resolved in
  any host-target build (`cargo tree -i basic-toml` is empty). All askama_derive releases
  through 0.16.1 still depend on basic-toml. Revisit when askama drops it.

## 2026-10-25: advisories published after rc.15

- **RUSTSEC-2026-0387, crypto-bigint 0.5.5** (timing variability in constant-time
  operations; fixed only in 0.7.6+). Reach: `jsonwebtoken` 10 with `rust_crypto` pulls
  `p256`/`p384` -> `elliptic-curve` 0.13 -> `crypto-bigint` 0.5, behind the `teams` feature
  of `clawft-channels`. The Teams verifier rejects every algorithm except RS256 before
  decoding (`crates/clawft-channels/src/teams/jwt.rs`, the `header.alg != Algorithm::RS256`
  check), so no ECDSA or big-integer elliptic-curve code runs, and verification only
  touches public keys and public tokens. Ignored in `CARGO_AUDIT_IGNORES`.
  Revisit when `jsonwebtoken` ships on `elliptic-curve` 0.14 / `crypto-bigint` 0.7, or if
  any ES* algorithm is accepted.
- **clipboard-win 5.4.1 yanked**: lockfile bump to a non-yanked release (no ignore).
