# Handoff — ADR-103 topology completion — 2026-10-04

This checkout contains the committed native Phase 4 and certified-leaf
integration on top of completed Phases 0–3. The native child-endpoint path is
green. The Wasmtime slice is integrated behind an off-by-default feature (2026-10-06).
The remaining completion work is running it under a real user daemon, real
Linux-container driver acceptance, final adversarial review, and a full gate. This document also inventories every operator migration
that the topology work introduced so the implementation status is not confused
with migrations performed on a real machine.

## Current state

- Branch `topology/adr103-completion` at
  `839f5f01507092fa4508f3e2d2b361c29a994318`.
- Phase 4 and certified-leaf integration are committed in `e74fb3834`; the
  follow-up nested-method classification and shutdown cascade are committed in
  `839f5f015`. `8375a11a2` merged the current `origin/0.8-metaharness` baseline.
- This branch has no upstream shown locally. No completion push is established.
- The topology source tree is committed. The remaining dirt is receipts and
  pre-existing/unrelated workspace state; do not sweep it into a topology
  commit or reset it indiscriminately.
- Dashboard cards are already claimed and remain in progress:
  - Phase 4: `0dc06c23-56a0-4718-b7d5-aefc2f05d98c`
  - Leaf: `444c7964-0a35-40f2-9462-11c020419cfe`
- Process discovery was unavailable in the final handoff check. An unrelated
  Claude `integrate-w1` suite and an installed coordinator daemon had previously
  been observed; do not stop or signal either. All acceptance runs must use
  isolated task-owned state and processes.
- Preserve the pre-existing unrelated dirt: `.mcp.json`,
  `docs/research/crosscut-latest.md`, `.grok/compose/`,
  `docs/plans/dashboard-fleet-terraform-integration.md`,
  `docs/research/fleet-manager/`, `docs/research/pg-source-intake.md`, and
  `scripts/pi/__pycache__/`.

## Verified state

| Area | State | Evidence |
|---|---|---|
| Baseline before Phase 4 import | Pass | `scripts/build.sh gate`: 22/22; 12,300 nextest tests passed, 27 skipped |
| Combined native source check | Pass | `scripts/build.sh check` completion receipt plus `completion-runs/combined-check-r2.log` |
| Combined native tests | Pass for focused Phase 4/leaf regressions | `completion-runs/focused-tests-r5.log`: 192 passed, 0 failed, 5,026 filtered |
| Seatbelt boundary | Pass before final combination | host runs r4/r5: 4/4; real lifecycle RPC r6: 1/1 |
| ESP-IDF leaf firmware | Pass build only | release r4, 56.30s; all 50 source hashes matched; no device flashed |
| Native weaver build | Pass | `completion-runs/p4-weaver-build-r2.log`, 2m10s |
| Real user-daemon/child smoke | Pass | `completion-runs/p4-child-smoke-merged-r1.log`: owner RPC, child method ceiling, bootstrap dispatch, cleanup |
| Wasmtime project kernel | Integrated, off by default (updated 2026-10-06) | compiled and unit-tested; real guest and runner pass `scripts/build.sh test-wasm-project`; not run under a real user daemon (`adr103-wasm-project-slice.md`) |
| Linux-container driver | Source and acceptance runner only | production driver tests passed before integration; real DinD gate never run |

The first combined run's ten failures were triaged and the focused r5 run is
green. Source fixes are present for
the leaf Noise compatibility regression, runtime-path fixture boundary,
`instance.nested.*` scope classification, owner `kernel.shutdown`, portable D10
fixtures, project child-listener fixture, and bounded owned-pipe shutdown. Three
project-supervisor readiness failures occurred under concurrent machine load.
The low-concurrency focused rerun passed; do not loosen those deadlines without
new failing evidence.

## Migration inventory

The product migration paths below are implemented. The table is authoritative
about execution: the owner-machine user-chain and `user.key` migrations have
receipts dated 2026-10-04; project-daemon conversion, machine-service key
adoption, live client-file promotion and hardware migration remain unperformed
or unverified. The native Phase 4 smoke used isolated task-owned state and proves
none of those remaining owner migrations. Treat all future commands that touch
`~/.clawft`, `~/.weftos`, `/var/lib/weftos`, or `/var/run/weftos` as owner-run
operations after a dry run and backup review.

| Migration | Plan and operator guide | Implemented state | Real-state status |
|---|---|---|---|
| Legacy chain adoption or deliberate fresh chain | ADR-103 A1; `docs/research/daemon-topology/phase-0-review.md`; `docs/guides/kernel.md` “Legacy chain adoption guard” | `chain_storage.rs` plus `--adopt-legacy-chain` and `--new-chain`; lock-aware, refuses a silent fork, covered by unit and `legacy_adoption_boot` tests | Not run here. The user reported starting a new chain, but that does not prove a legacy adoption/migration path. |
| `~/.clawft` chain to `~/.weftos/chain` | `docs/plans/weave-topology-p1-plan.md` package E and section 4; `docs/guides/kernel.md` “Owner migration” | `chain_migrate.rs` and `weaver migrate user-chain [--dry-run]`; hashes and verifies the copied head/signatures, uses atomic placement, is idempotent, leaves the source untouched, and installs fork guards | Product code completed in Phase 1 and carried through the green Phase 0–3 baseline gate. **Run on the owner's Mac 2026-10-04:** 89,287 events, signature verified, source `chain.rvf` hash unchanged, `MIGRATED-TO-WEFTOS.txt` marker added. |
| Existing project-rooted daemon to supervised child kernel | `docs/plans/weave-topology-p2-plan.md` section 4; `docs/guides/kernel.md` “Project kernels (Phase 2)” | `project_migrate.rs` and `weaver project migrate-kernel <id> [--dry-run|--revert]`; refuses a live old daemon, copies only workload/app state, preserves the old runtime, switches manifest `serve.via`, and starts a fresh project chain linked to the user-chain head | Completed and adversarially reviewed in Phase 2. The Phase 4 smoke used a fresh isolated project; no real legacy project daemon was migrated. |
| Collapsed user mesh to machine mesh service | `docs/plans/weave-topology-p3-plan.md` section 5; `docs/guides/weftos-deployment-sops.md` “Moving to the machine mesh service” | `weaver mesh install-service` prints a reviewed script and requires an explicit `--adopt-node-key` or `--fresh-node-key`; service install/admission was completed and reviewed in Phase 3 | Not installed or migrated here. On macOS the user must be in the `_weftos` daemon group and log in again before using the group-owned runtime directory. |
| Migrated chain key to `~/.weftos/user.key` | `docs/plans/weave-topology-p3-plan.md` package U and section 5; deployment SOP step 2 | `user_key.rs` and `weaver migrate user-key [--dry-run]`; atomic 0600 copy, public-key/user-id equality proof, divergence refusal, source `chain.key` retained | **Run on the owner's Mac 2026-10-04:** `user.key` 0600, public key equal to `chain.key`; `~/.weftos/weave.toml` written (mesh `0.0.0.0:9489`, the coordinator's port). The user daemon is not started yet: stop or migrate the coordinator first. |
| User-key rotation | ADR-103 A13; `docs/guides/kernel.md` “Rotating the user key” | `user_key_rotate.rs` and `weaver migrate user-key --rotate`; offline lock, dual-signed hash-linked handover, crash-resumable staging, retired-key verification, chain-backed recovery | Product code complete; not run on owner state. Existing children must restart/rebind after rotation as described in the guide. |
| Existing machine node identity during service install | Phase 3 plan D-4 and deployment SOP | `--adopt-node-key ~/.weftos/run/node.key` preserves node id and remote pins; `--fresh-node-key` deliberately creates a new identity. The generated script is printed only and must be reviewed before admin execution | Owner choice remains outstanding; no machine-service key adoption was performed here. |
| Legacy unsigned bare-metal leaf to certified leaf | `docs/plans/adr-103-leaf-implementation.md` | Certified leaf protocol, claim synchronization, reconnect cleanup, and ESP-IDF producer/NVS backlog are implemented; unsigned legacy mode remains off by default | Migration tooling and hardware rollout are still deferred. ESP-IDF built, but no device was flashed or power-loss tested. |
| Legacy workspace registry to project manifests | `docs/plans/weave-topology-p1-plan.md` packages A/B and section 4; `docs/guides/kernel.md` owner migration | User-daemon startup reads `~/.clawft/workspaces.json` and idempotently seeds pending manifests under `~/.weftos/projects/`; `weft project init` adopts a seeded root and keeps its stable project id | Product code and tests are complete. This migrates registry metadata only: it does not copy repository files, classify clients, or enroll projects in the dashboard/fleet manager. |
| Mac development and archived client source intake to photo-gallery | `docs/plans/dashboard-fleet-terraform-integration.md`; `docs/research/pg-source-intake.md` | Non-destructive, resumable `rsync` into `/data1/weavelogic-internal/_archive/`, followed by receipts, checksum verification, classification, and later promotion into governed roots | Copies of `~/dev/` and `~/aepod-xpc-sync/` were recorded as started on 2026-10-04; current completion and checksum verification are not established. Do not treat archive landing as project enrollment. |
| Live `<client workspace>/` projects to governed client homes | `docs/plans/dashboard-fleet-terraform-integration.md`; `docs/research/pg-source-intake.md` | Map each live source to client, project, owner and predecessor; then copy it under `/data1/weavelogic-clients/<client>/...` with a distinct project UID/runtime and bind dashboard UUID to certified WeftOS ULID explicitly | Inventory only. No general live-client transfer, UID provisioning, dashboard binding, or fleet enrollment is recorded as complete. |

The safe owner order is documented, rather than inferred. Chain and `user.key`
migration are recorded complete. The remaining sequence is to stop or migrate
the coordinator holding port 9489, explicitly adopt or replace the machine node
key while installing the service, start the user daemon, and then opt individual
projects into `migrate-kernel`. Do not delete source keys, chains, repositories,
or old project runtime directories as part of the automated steps.

### Repository and client-file intake

The project-file migration is a separate program from the daemon/runtime
migrations above. Its authoritative working plan is
`docs/plans/dashboard-fleet-terraform-integration.md`; the source-by-source
ledger is `docs/research/pg-source-intake.md`.

- `~/dev/` is staged to
  `/data1/weavelogic-internal/_archive/dev/`. It contains current development
  repositories and worktrees, including `weftos-dashboard` and client repositories.
  The recorded detached copy started, but no completion receipt or checksum
  comparison is present. Everything in this landing area remains unclassified.
- `~/aepod-xpc-sync/` is staged to
  `/data1/weavelogic-internal/_archive/aepod-xpc-sync/`. It contains about 44 GB
  of archives, including client-labelled material. Do not extract it or assume
  an archive is the newest source until provenance and predecessor relationships
  are reviewed.
- `<client workspace>/` was inventoried (client roots listed in the local-only
  `~/weavelogic.terraform` inventory, not here). Those live
  roots have **not** been generally transferred. Each selected project needs a
  destination under `/data1/weavelogic-clients/`, its own UID/runtime, explicit
  company/project ownership, and a decision about predecessor/successor history.
- The WeftOS internal pilot is the one completed source-root move recorded by
  the plan: staged source was copied from `/srv/weftos/projects/weftos/` to
  `/data1/weavelogic-internal/weftos/`, revision and checksum checks matched,
  binaries matched, and `weft project init --name weftos` registered project id
  `01M43SGRRXEJJ1SAZNXVD0TPAP`. The later
  `weaver project migrate-kernel ... --dry-run` was only a dry run; the child
  kernel migration was not applied.
- Archive intake, project promotion, dashboard association, WeftOS registration,
  and fleet enrollment are distinct states. Never infer one from another. The
  dashboard UUID and WeftOS ULID require an explicit binding; names and slugs do
  not establish identity.
- Sources remain in place after a verified copy. No intake step authorizes
  deletion. Every batch needs source/destination, exact options, timestamps,
  exit status, file count or checksum comparison, and skipped/unreadable paths.

## Integrated work that must be preserved

- Sandbox enum and supervisor support `Logical` (default, with `native` alias),
  `Seatbelt`, and `LinuxContainer`. Container protected-path validation and the
  nested-parent checks are both retained.
- Linux-container guest executable is `/usr/local/bin/weaver`; `/weftos` remains
  a directory for project/run/trust/parent mounts. The former `/weftos`
  entrypoint collided with its child mount paths.
- The dedicated child UDS denies mint, revoke, shutdown, and owner-only methods.
  The owner endpoint permits Admin `kernel.shutdown` under the user profile's
  read-only outside-project policy; `deny_all` still refuses it.
- D10 nested users use private HOME/runtime/key/chain/config, signed contracts,
  explicit mesh grants, reciprocal Noise-bound admission, and an owned stdin
  liveness pipe for bounded graceful cascade. Recovered processes are never
  signaled through an unverified PID.
- Mesh merge conflicts were resolved by retaining both `leaf_ingress` and
  `MeshAuthentication`, and by processing signed-leaf frames before reciprocal
  nested hello detection. Review this joint path; do not choose one side.
- Certified leaf support includes no-std signing/ACK/discovery/offline replay,
  service claim synchronization, reconnect cleanup, and ESP-IDF producer/NVS
  backlog. Legacy unsigned bare metal remains off by default.

## Open threads, in order

1. **Exercise Wasmtime under a real user daemon.** The driver is integrated behind
   the off-by-default `clawft-weave/wasmtime-project` feature and the guest and
   runner are built by `scripts/build.sh wasm-project`; `test-wasm-project` drives
   them with a signing-parent fixture. What remains is an isolated user-daemon run
   with operator pins, the dedicated child socket and a daemon restart (adoption).
   Full native-kernel parity is not claimed: agents/workloads, shared ParentLink
   services, subscriptions, nested Wasmtime, key rotation, quotas, packaging, and
   power-loss validation remain outside this slice
   (`docs/research/daemon-topology/adr103-wasm-project-slice.md`, "Not done").
2. **Do not enable `wasmtime-project` in a release build** until item 1 is done.
3. **Run actual Linux-container driver acceptance.** The implemented runner is
   `scripts/dev/p4-container-driver.py`; its contract is documented in
   `completion-runs/container-lifecycle-plan.md`. It requires a disposable,
   explicitly named Docker-in-Docker context where parent and dockerd share the
   same Linux PID/mount namespace and `/case` path. It must use a pinned child
   image containing the evaluated `/usr/local/bin/weaver`. Do not mount the
   operator's Docker socket and do not substitute the logical-inside-container
   harness or a `--version` smoke.
4. **Adversarial review and final gate.** Review the final leaf/D10/Wasm/container
   composition, run the full `scripts/build.sh gate`, curate receipts, update the
   two board cards to done only after acceptance, then commit the remaining work
   on `topology/adr103-completion`. Merge it into `0.8-metaharness` only after the
   gate and review, and push normally. Never commit to `main`/`master` or force
   push.
5. **Schedule owner migrations separately.** After the release candidate is
   accepted, follow the operator guides in the migration table. Start with dry
   runs and backups. Do not let an automated completion agent execute migrations
   against the owner's real home, installed daemon, machine service, or hardware.
6. **Finish repository intake before fleet enrollment.** Determine whether the
   two recorded detached archive copies completed, capture checksums and skipped
   paths, then classify selected internal and client projects. Provision a
   distinct project identity and UID/runtime before promoting each source from
   archive or `<client workspace>/`. Do not bulk-enroll archive directory names as fleets.

## Resume here

Run from `~/weftos`. Long jobs must be detached and polled.

```bash
# 1. Confirm the committed native tree is clean.
git diff --check

# 2. Build and exercise the Wasmtime guest and runner (release; needs python3
# cryptography and the wasm32-wasip1 target; never installs targets).
scripts/build.sh wasm-project
scripts/build.sh test-wasm-project

# 3. Wasmtime driver tests in the weaver crate.
scripts/build.sh test clawft-weave --features wasmtime-project

# 4. Container gate starts in plan-only mode. Read its --help before --run.
python3 scripts/dev/p4-container-driver.py --help

# 5. After real-daemon Wasmtime and real-container acceptance plus adversarial review:
nohup env CARGO_INCREMENTAL=0 scripts/build.sh gate \
  > docs/research/daemon-topology/completion-runs/final-gate.log 2>&1 < /dev/null &
```

Keep long jobs detached and polled; a release build of the guest and runner
adds several GiB of `target/`.

## Dead ends — do not retry

- **Treating an engine/version smoke as container acceptance.** It proves only
  CLI reachability. The actual production supervisor must create, inspect,
  restart, adopt, revoke, and clean the child in disposable DinD.
- **The logical pair inside one outer container.** Useful supplemental lifecycle
  evidence, but parent and child share namespaces; its receipt explicitly cannot
  set `container_driver_accepted`.
- **Using `/weftos` as the executable.** It conflicts with the required
  `/weftos/project`, `/weftos/trust`, `/weftos/parent`, and `/weftos/run` mounts.
- **Process-group identity as a Seatbelt boundary.** Both `setpgid` and `setsid`
  succeeded in host probes. The dedicated child-only UDS is the boundary.
- **Broadening Seatbelt until dyld works.** The minimal necessary permission was
  `(allow file-read-data (literal "/"))`; the resulting strict boundary tests
  passed. Do not replace it with broad writable access.
- **Disabling Noise for leaf compatibility.** The service must retain Noise and
  admit authenticated CAP_LEAF publishing for its own substrate prefix.
- **Running broad parallel suites to diagnose supervisor startup.** Three
  readiness failures happened under heavy concurrent load. Use the focused
  two-worker run first.
- **Reusing stale worktree binaries.** Several worktrees had sources newer than
  their binaries after cache reclamation. Build from the integrated main tree.
- **Starting a second process-compose or touching installed daemons.** The repo's
  process-compose ports are 18090/18091; Forge ports and the installed coordinator
  are outside this acceptance work.

## Key paths

- `docs/research/daemon-topology/completion-runs/status.md` — short completion status.
- `docs/research/daemon-topology/completion-runs/coordination.md` — detailed chronological receipts and decisions.
- `docs/research/daemon-topology/completion-runs/combined-tests-r1.log` — initial 5,356/10/7 combined run and source of the triage.
- `docs/research/daemon-topology/completion-runs/focused-tests-r5.log` — green focused native run, 192/192.
- `docs/research/daemon-topology/completion-runs/p4-weaver-build-r2.log` — green native build.
- `docs/research/daemon-topology/completion-runs/p4-child-smoke-merged-r1.log` — positive real user-daemon/child smoke.
- `docs/research/daemon-topology/completion-runs/d10-review-fixes.md` — D10 review fixes and targeted tests.
- `docs/research/daemon-topology/completion-runs/leaf-review-r1.md` — leaf adversarial review history.
- `docs/research/daemon-topology/completion-runs/container-lifecycle-plan.md` — exact real-driver topology and evidence requirements.
- `scripts/dev/p4-child-endpoint-smoke.py` — isolated real daemon/child endpoint smoke.
- `scripts/dev/p4-container-driver.py` — disposable DinD real-driver acceptance runner.
- `docs/plans/weave-topology-p1-plan.md` — user daemon and legacy user-chain migration plan.
- `docs/plans/weave-topology-p2-plan.md` — project child-kernel migration plan.
- `docs/plans/weave-topology-p3-plan.md` — machine mesh service and user-key migration plan.
- `docs/guides/kernel.md` — chain, project-kernel and user-key operator procedures.
- `docs/guides/weftos-deployment-sops.md` — machine-service owner migration procedure.
- `docs/plans/dashboard-fleet-terraform-integration.md` — repository placement,
  UID isolation, dashboard binding and fleet-enrollment plan.
- `docs/research/pg-source-intake.md` — live development/client source inventory
  and transfer status ledger.
- `docs/research/daemon-topology/adr103-wasm-project-slice.md` — Wasmtime project kernel: boundary, integration with the other drivers, verification and what is not done.

## Gotchas

- The topology sources are committed, but the checkout has unrelated dirt and
  untracked receipts. Do not reset it, copy an old worktree wholesale, or
  regenerate `Cargo.lock` from one stale branch.
- The old `leaf-disconnect-fix.patch` is stale; its changes are already in main.
- Keep `D10_TEST_ROOT` short on macOS because Unix sockets are limited to roughly
  103 bytes.
- Tests must never use real `~/.weftos` or `~/.clawft`; use private HOME/runtime
  directories and the nextest wrapper.
- No physical ESP32 flash, power-loss test, legacy bare-metal migration, or real
  Linux-container run has occurred. State those limits in release notes.
- Do not delete main's pre-existing large incremental cache merely to gain space.
  Earlier cleanup was limited to task-created worktree caches.
