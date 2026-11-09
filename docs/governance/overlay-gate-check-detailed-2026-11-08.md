# OverlayGate did not implement `check_detailed` (2026-11-08)

Status: fix on branch `fix/overlay-gate-check-detailed`, not merged.
**This is a governance-gate change. It needs the G0 review chain before merge:
WeftOS Claude review, then Cogs Bot, then Mathew signs off.**

## The bug

On a project kernel the gate is an `OverlayGate` (`crates/clawft-kernel/src/overlay_runtime.rs`).
It implemented `check` and `governance_snapshot` but not `GateBackend::check_detailed`,
so it fell through to the trait default, which returns `None` ("no governance engine").
The `governance.check` RPC (`crates/clawft-weave/src/governance_check_rpc.rs`) turned that
into `no_governance_engine`. Tapestry's R9 layer read that as "governance unavailable" and
ran a tool that an overlay deny rule forbids: a silent fail-open on project kernels.

## Evidence

The R8/R9 live check of 2026-11-08 (`docs/tapestry/p2-r8-r9-live-checks.md` at tapestry
commit 4adc6cee). The probe lane patched `check_detailed` (12 lines), saw the overlay deny come
back, and reverted it (scratch worktree `p2-live-weaver`).

## The fix

`OverlayGate::check_detailed` delegates to the inner `GovernanceGate` under the same read lock
`check` uses. The inner gate's engine already holds the merged parent + overlay rules, so
the verdict (decision, reason with rule id, evaluated rules, grant flag) uses the same
strictest-wins composition as `check`. `evaluate()` ordering (G0) is not touched.

## Other wrappers with the same fall-through

Every `GateBackend` impl was listed. Production impls without `check_detailed`:
`CapabilityGate` (no engine; `None` is correct), `WorkloadGate` (workload.* audit gate, no
`GovernanceEngine`; `None` is correct, callers must treat it as cannot-decide), and
`TileZeroGate` (test-only). `OverlayGate` was the only wrapper around an engine. Test doubles
elsewhere are not wrappers. `daemon.rs` hands the overlay gate to the agent tool gate, so it
inherits the fix.

## Tests (`crates/clawft-kernel/src/overlay_runtime_tests.rs`)

- overlay deny returns from `check_detailed` as Deny with rule id `d1`, agrees with `check`,
  writes nothing to the chain;
- overlay `require_approval` returns EscalateToHuman;
- parent `WL-DENY` still wins through an overlay that adds an unrelated deny.

RPC level (`crates/clawft-weave/tests/governance_check_project_kernel.rs`, one-test binary):
boots a real `profile = "project"` kernel in-process (child root from
`overlay_runtime::test_support::install_child_fixture`, overlay written to the child's
overlay path), then calls `governance.check` through `daemon::handle_connection` over a unix
socket, so routing, the capability check and `governance_check_rpc::handle` are all on the path.
It asserts: overlay `[[deny]]` d1 gives `deny` naming `d1`; overlay `[[require_approval]]` gives
`escalate_to_human`; an uncovered action gives `permit`; and no answer is `no_governance_engine`.
Checked to bite: with the `check_detailed` impl removed (parent commit's overlay_runtime.rs) the
test fails with `no_governance_engine`; with it, it passes.

Not covered: a real `weaver`/daemon process and the Unix socket auth path of a spawned daemon
(the test calls the connection handler directly); overlay reload while serving.
