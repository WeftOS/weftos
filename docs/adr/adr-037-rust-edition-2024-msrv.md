# ADR-037: Rust Edition 2024 and MSRV 1.93

**Date**: 2026-04-03
**Status**: Accepted
**Updated**: 2026-10-27. The workspace `rust-version` is now `1.95` (root `Cargo.toml`, `[workspace.package]`); the toolchain pin moved from 1.93 to 1.95 on 2026-10-02. The text below records the decision as taken on 2026-04-03 with MSRV 1.93.
**Deciders**: Project lead (workspace Cargo.toml configuration)

## Context

The WeftOS workspace contains 22 crates (plus the excluded `gui/src-tauri`) that all inherit `edition` and `rust-version` from `[workspace.package]` in the root `Cargo.toml`. The project uses advanced Rust features including `let`-chains (used in `mesh_heartbeat.rs` for `if let Some(since) = self.suspect_since && since.elapsed() > config.suspect_timeout`), `#[non_exhaustive]` enums throughout kernel types, and async trait methods. The choice of edition and MSRV determines which language features are available and which Rust toolchains can build the project.

Rust Edition 2024 was stabilized with Rust 1.85 (February 2025). MSRV 1.93 is well ahead of the edition stabilization point, reflecting the project's use of features stabilized after the edition itself.

## Decision

The workspace uses Rust Edition 2024 with MSRV 1.93, configured in the root `Cargo.toml`:

```toml
[workspace.package]
version = "0.X.Y"          # see workspace Cargo.toml for the current value
edition = "2024"
rust-version = "1.93"
```

All 22 workspace crates inherit these settings via `edition.workspace = true` and `rust-version.workspace = true`. The excluded `gui/src-tauri` crate (`weftos-gui`) independently sets the same values (`edition = "2024"`, `rust-version = "1.93"`).

This is an aggressive choice:
- Edition 2024 enables the latest language semantics (updated borrow checker rules, `gen` block syntax if stabilized, new prelude additions).
- MSRV 1.93 excludes contributors and CI environments running older Rust versions.
- The build script (`scripts/build.sh`) is the mandatory build entry point (per project CLAUDE.md) and does not pin a specific toolchain, so builders must have >= 1.93 installed.

## Consequences

### Positive
- Access to Edition 2024 language features across all 22 crates, including `let`-chains used in mesh heartbeat state transitions and other ergonomic improvements
- Single edition and MSRV for the entire workspace eliminates edition mismatch between crates
- Matches the `gui/src-tauri` crate's independent configuration, ensuring workspace-wide consistency
- MSRV 1.93 is recent enough to avoid workarounds for language features that are now stable

### Negative
- Excludes contributors on older toolchains -- anyone with Rust < 1.93 cannot build any workspace crate
- Downstream consumers who depend on weftos crates via crates.io inherit the MSRV 1.93 requirement, which may conflict with their own MSRV policies
- Edition 2024 has fewer ecosystem battle-testing hours than Edition 2021; subtle edition-specific behavior changes may surface in edge cases

### Neutral
- MSRV is enforced by `cargo`'s `rust-version` field; CI will catch MSRV violations via `cargo check`
- After 1.0 release, MSRV bumps become semver-breaking changes under the Rust ecosystem convention; pre-1.0 they are acceptable as minor version bumps per ADR-001
