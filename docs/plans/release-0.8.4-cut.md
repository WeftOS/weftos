# v0.8.4 stable cut

**Date**: 2026-10-23
**Status**: Scope approved by the owner on 2026-10-23. Work is in progress.
**Base**: `target-0.8.4`. The latest prerelease is v0.8.4-rc.14 (`8282dd5ee`, 2026-10-13), deployed on all mesh nodes.

## Scope

v0.8.4 stable is everything in rc.1–rc.14, plus the commits landed since rc.14, plus the items below. The owner's rule is to bring in all finished work that is clean.

### Blockers (must land)

| Card | Item | State |
|---|---|---|
| `1a919ae6` | Daemon hang. A runtime deadlock (seen on the primary node, 2026-10-15 → 10-23), bounded shutdown, `stop --force` escalation, and a liveness watchdog. | lane |
| `85f29f99` | Governance: a Warning-level browser_policy match clears an earlier block, so the decision depends on rule order (G0). | lane |
| `4366dc71` | Teams webhook JWT is never signature-verified. | lane |
| `dec62889` | The release Docker image pushes, but the post-publish health smoke has failed since 2026-09-28. | lane |
| `0eff3101` | Release KB and SBOM workflows fail on some rc tags. | lane |
| `ba258d78` | `kernel restart` re-exec refuses its own PID. | Fixed in `edc3ad209` (already in 0.8.4). It was exercised live on 2026-10-23 when a member node re-execed and wrote its `REFUSED` exit marker. |

### Dependency and security cards

| Card | Item | State |
|---|---|---|
| `06eb23e8` | jsonwebtoken 9 → 10.3 (teams feature) | lane |
| `923ac3d6` | rpassword 5 → 7.5 (weft) | lane |
| `c6920f2b` | Lockfile refresh: xxhash-rust, rustls in the fuzz lock; `kb` and `fuzz-check` build lanes | lane |
| `0623d6a7` | Drop the unpatched `failure` crate via webrtc-audio-processing 2.1. Fallback: dismiss as not used. | lane (timeboxed) |
| `5e907457` | Record residual alerts. The owner dismisses them on GitHub. | lane (docs) |
| `975e420f` | Next.js/esbuild alerts. They close when 0.8.4 stable merges to master. | release step |

### Finished work brought in (if clean)

Each branch is rebased onto `target-0.8.4`, checked and clippy-clean before it lands:

- `wt/phonelink`: phone and glasses P0 (weftos-node-core, Android phone app, glasses app, build lanes). It includes `wt/phonep0` and `wt/glassesp0`.
- `feat/room-scope`: the weft-room-scope egui viewer.
- `feat/cog-protocol-boundary`: weftos.cog.v1 check-run types, the daemon check before Cog Host starts, and licence commands.
- `wt/infer-manifest`: advertise models only when checked against the daemon's trust.
- `wt/catalog-cog-awareness`: link sensors to their cogs in the console catalog.

### Already landed since rc.14

- Tauri shells retired.
- The workspace is publishable to crates.io, with auto-publish on stable tags.
- Research: harness2, TYGER, ESP32-P4, Trajectory Atlas (plan and egui benchmark), ISOP governance compilation, OlmoEarth for Urth, Ventuno Q NPU benchmark.
- Skills: weftos-design scoped to native.

### Not in 0.8.4

- `feat/post-v0.8.3`: cogs-extraction notes. They are confidential and belong in the private cogs repository, not here.
- `wip/adr103-wasm-project`: work in progress.
- `feat/spatial-*`: the owner's workspace.
- Push-to-talk (`437432d5`): its uncommitted work was lost, so it gets redone for 0.8.5.
- Long-running programmes: ADR-108 to ADR-121 follow-ups, the Wasmtime kernel, world model W1–W6, agent teams TM1–TM6, RuView/Urth, CoS multitenancy, Trajectory Atlas build, the ISOP spike G1–G4, the Phonon TTS spike.

## Release steps

1. Land each lane after review. The lead runs the full `scripts/build.sh gate` on the combined tip.
2. Update the CHANGELOG `[Unreleased]` section to 0.8.4.
3. Tag `v0.8.4`. That signs the release, publishes the 40 crates to crates.io and creates the GitHub release. Confirm the Docker, KB and SBOM workflows pass.
4. Merge into master (owner rule: every stable release). This closes `975e420f` and most open Dependabot alerts.
5. Roll out to every mesh node, member nodes first.
6. Clean up:
   - tag `f19dd0996`, then delete `release/0.8.3-crates` and its worktree;
   - close `f896c4f7`;
   - remove merged lane worktrees.
