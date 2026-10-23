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
