# Cog protocol boundary

Branch `feat/cog-protocol-boundary`, parent `v0.8.3` (`fafd6168f8f97280ee24e5c2d2813b6e34918e15`). Worktree `/Users/mathewbeane/weftos-cog-protocol`. This note starts the boundary. It does not remove the kernel link.

## What is coupled today

`crates/weftos-cog-host/Cargo.toml` depends on `clawft-kernel` with features `native`, `ecc`, `exochain`, and `mesh`.

`crates/weftos-cog-host/src/licence.rs` is the ADR-106 start-time check. It calls `clawft_kernel::licence::check_run` and uses `RunRequest`, `RunPermit`, `RunVerdict`, `RunRefusal`, `SignedGrant`, `SignedApproval`, `SignedBinding`, `CheckoutGrantStore`, `ApprovalStore`, and `CognitumRunGate`. Revocation uses `clawft_kernel::mesh_swarm_revoke` and `clawft_kernel::revocation::RevocationList`. Trust anchors use `clawft_kernel::workload_pkg::TrustAnchors`.

`crates/weftos-cog-repo/Cargo.toml` depends on `clawft-types` with default features off, for runtime paths.

## Boundary to build

`weftos-cog-protocol` will hold versioned request, response, and event types. Cog Host will call the local user or project daemon on its Unix socket. The in-process `check_run` path stays until that client exists and the golden vectors match. Do not replace these path dependencies with a git dependency back into WeftOS.

The standalone copies drop `clawft-kernel` and `clawft-types` only after the client is in place. This branch has not added the crate yet.
