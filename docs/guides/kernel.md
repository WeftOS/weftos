# Kernel Operator Guide

This guide is the operator-facing entry point to the WeftOS kernel
(`clawft-kernel`) as it ships in 0.7.0. It covers the boot sequence,
the kernel services that an operator interacts with, the K-phase
status as of 0.7.0, governance and ExoChain auditing at the
operator level, and the day-to-day log lines that show up under
normal operation (including the DEMOCRITUS "still stuck" warning
that has historically been mistaken for a bug).

For developer-facing material — adding modules, extending traits,
unit-testing the kernel — see the WeftOS docs site at
[`docs/src/content/docs/weftos/kernel-guide.mdx`](../src/content/docs/weftos/kernel-guide.mdx).

For the architectural rationale behind the kernel, see ADR-049
(`docs/adr/adr-049-weftos-kernel.md`) and ADR-048 (Kernel Phase
Responsibilities).

---

## What the Kernel Is

`clawft-kernel` is the runtime that supervises agents, brokers IPC,
maintains the ExoChain audit log, enforces three-branch governance,
runs the DEMOCRITUS cognitive tick, and (optionally) the K6 mesh.
It is the same binary on every node — the operating mode is selected
by configuration (see ADR-042: Three Operating Modes).

The kernel runs as a long-lived daemon. The CLI front-ends (`weft`
and `weaver`) are thin clients: they enqueue commands over the
kernel's RPC socket and the kernel does the actual work, logs the
event to the chain, and routes any outbound traffic. ADR-021 (CLI
Kernel Compliance) is the load-bearing rule: **no CLI command
should bypass the daemon for state-changing operations**. If you
see a command that does, file it as a bug.

---

## Boot Sequence

Boot is a strict state machine driven by `Kernel::boot()` in
`crates/clawft-kernel/src/boot.rs`. An operator should expect to
see the following phases in order, each emitting a `BootEvent` that
is chain-logged when ExoChain is enabled:

| Phase | What happens |
|-------|--------------|
| 0 | Configuration loaded; feature gates evaluated; logging set up. |
| 1 | Process table + supervisor initialized (K1). |
| 2 | IPC + A2A router + topic router come up (K2). |
| 3 | Service registry started; default `SystemService`s (cron, health, etc.) registered (K2). |
| 3.5 | ExoChain primed: chain manager, tree manager, gate backends. |
| 4 | WASM tool runner ready (K3, behind `wasm-sandbox`). |
| 4.5 | Container manager (K4, behind `containers`). |
| 5a | App framework (K5) loads `weftapp.toml` manifests. |
| 5b | ECC substrate (K3c, behind `ecc`): causal graph, HNSW, calibration, cognitive tick. |
| 5d | Mesh networking (K6, behind `mesh`): listeners, peer discovery, heartbeat. |
| 6 | Kernel transitions to `Running`. |

A clean boot ends with `kernel state: Running` and a chain entry
of kind `kernel.boot.complete`. Anything earlier than that without
a corresponding entry means the boot stalled — `weaver kernel logs`
will show where.

### The chain won't restore: boot refuses

If the chain file exists but fails verification, boot stops with
`chain at <path> failed to restore (...); refusing to start a fresh chain
over it`. The kernel does not replace it with a new genesis, because the next
save would overwrite the only copy. Earlier builds logged "starting fresh"
and did exactly that, and still do: never run a pre-Phase 2 binary against a
`child-kernel` project's chain (see "Never downgrade a child-kernel project"
below).

Causes: file corruption, or events written by a newer binary that this build
cannot verify (events carrying a `rule_hash`, or RVF segments with flag bits
this build does not know).

What to do:

1. If a newer binary wrote it, upgrade the binary and restart.
2. Otherwise move or rename the chain file (and its `.rvf` / `.json` sibling)
   aside, for example `mv chain.rvf chain.rvf.bad`, and restart. A fresh chain
   starts; keep the old file for inspection.

`--new-chain` selects a different chain location; it does not touch the
existing file, so it does not clear this refusal.

---

## K-Phase Status (0.7.0)

K0–K5 are complete; K6 is at Phase 1. The full status table —
including which subsystem each phase owns and what is deferred —
lives in [`docs/weftos/k-phases.md`](../weftos/k-phases.md).

| Phase | Status | Highlights |
|-------|--------|-----------|
| K0    | Complete | Boot state machine, config, feature gates. |
| K1    | Complete | Process table, agent supervisor, RBAC. |
| K2    | Complete | IPC, A2A, topics, cron, services, health. |
| K2b   | Complete | Hardening: GovernanceGate, signed agent.spawn/exit/restart. |
| K2.1  | Complete | Symposium decisions: SpawnBackend, dual-signing scaffolding, ServiceEntry, MessageTarget::Service. |
| K3    | Complete | Wasmtime sandbox, fuel metering, ServiceApi, dual-layer A2ARouter gate. |
| K3c   | Complete | ECC: causal DAG, HNSW, cognitive tick, EML. |
| K4    | Complete | Container lifecycle, ChainAnchor trait + MockAnchor. |
| K5    | Complete | App framework, manifest install/start/stop, namespaced agents/tools. |
| K6    | Phase 1   | Types, traits, TCP/WS transport, 136 tests. Replication / split-brain handling deferred. |

---

## Operator-Facing Subsystems

### ExoChain (audit log)

ExoChain is the tamper-evident, hash-chained audit log behind every
state-changing operation in the kernel. It is enabled by default in
the 0.7.0 native binary. By default the chain lives at
`chain.rvf` in the kernel's runtime directory, signed with `chain.key`
next to it (see "Runtime directory" and "Chain storage location"
below). Operator commands:

```bash
weaver chain status          # current chain head, length, last entry kind
weaver chain verify          # walk the chain and verify hashes / signatures
weaver chain export <path>   # export to JSON for offline review
```

Per ADR-022 (ExoChain Mandatory Audit), every privileged operation
should produce a chain entry. If you run an action and `weaver chain
status` does not show a new entry, that is a regression — file it.

#### Runtime directory

Every runtime file of a kernel hangs off one root, resolved by one
function (`clawft_types::runtime_paths::RuntimePaths`, ADR-103 D4). The
socket, PID file, log, lock, `node.key`, chain files, anchor ledger,
`workloads.json`, `cluster_peers.json`, `apps.json` and
`revoked_hosts.json` all live directly under it.

Root resolution, highest first:

1. `$WEFTOS_RUNTIME_DIR`, when set and non-empty (full isolation for
   tests, probes and nested instances).
2. `<project>/.weftos/runtime`, where `<project>` is the nearest ancestor
   of the working directory that is a project root: it has
   `.weftos/project.toml`, or `.weftos/weave.toml`, or `weave.toml` next
   to a `.weftos/` directory (what `weaver init` creates), or an existing
   `.weftos/runtime/` directory, or a `.weftos/` directory in a git
   top-level (a `.git` file or directory, which covers git worktrees, so
   each worktree gets its own kernel). A bare `.weftos/` is not a project,
   and the walk never returns `$HOME`, so the `~/.weftos/` that holds apps
   and models is ignored.
3. `~/.clawft/` (legacy).

Legacy chain: the chain used to resolve from `$WEFTOS_RUNTIME_DIR` or
`~/.clawft` only, even for a project-local daemon. A kernel that resolves
to a project root with no chain yet, while `~/.clawft/chain.*` exists,
keeps using the legacy chain and its key and logs a WARN naming both
paths; starting a fresh genesis there would fork the history. Nothing is
moved until you run `weaver migrate user-chain` (below). To start a
fresh chain at the project path instead, run
`weaver kernel start --new-chain` (the legacy chain is left untouched), or
pin `kernel.chain.checkpoint_path`. A fresh chain is otherwise created
only when no chain exists at all. `weaver kernel start` reports success
only once the daemon serves (its socket accepts connections and
`kernel.pid` holds the spawned child's pid); if boot fails (for example the
chain lock is held) it prints the last log lines and exits non-zero. If the
daemon is still booting after 90 s (a large chain can take longer), it
prints "still starting (pid N); check `weaver kernel status`" and exits 0
without the started banner.

The Seed token store under `~/.clawft/secrets/` is read from there, with
a WARN, when the project has none yet; it is never moved automatically.

Migrating to the legacy chain safely: older kernels take no `chain.lock`,
so a new kernel cannot tell whether one is still writing
`~/.clawft/chain.*`. The first adoption is therefore explicit:

1. Stop every older weaver daemon (check `ps` or `weaver doctor daemon`).
2. Run `weaver kernel start --adopt-legacy-chain` once. It adopts the
   legacy chain and creates `chain.lock` beside it; later starts adopt it
   normally without the flag.
3. Or run `weaver kernel start --new-chain` for a fresh chain at the
   project path.

Without the flag (and with no `chain.lock` yet) the start is refused. Even
with the flag it is refused if the chain was modified within the last 120
seconds ("looks in use by an older kernel").

Migrating the legacy chain to the user chain: `weaver migrate user-chain
[--dry-run] [--from DIR] [--to DIR]` (defaults `~/.clawft` to
`~/.weftos/chain`). Stop every daemon that uses the legacy chain first.
It refuses if a kernel holds `chain.lock` or the chain was modified in the
last 120 s with no lock; takes the source `chain.lock` for the run; copies
`chain.rvf`, `chain.json`, `chain.key`, `chain.tree.json` and
`chain/anchors.jsonl` to a temp dir beside the destination with fsync;
restores the copy with the kernel's own loader and checks file hashes,
event count, head hash, integrity and the RVF signature against the
source; then renames it into place and writes `MIGRATED_FROM.json` there
and `MIGRATED-TO-WEFTOS.txt` beside the source. The source chain files are
never modified. Re-running is a no-op ("already migrated"); a destination
that holds a different chain is refused. `--dry-run` writes nothing. After
migration a boot that would still land on the migrated legacy chain is
refused unless `WEFTOS_RUNTIME_DIR` isolates it or `--adopt-legacy-chain`
is passed (WARN: that forks history). Rollback: delete the destination
directory and `MIGRATED-TO-WEFTOS.txt` beside the legacy chain; the legacy
chain is intact. A chain with no `chain.key`, or whose signature cannot be verified
against it, is refused unless `--allow-unsigned` is passed. If the marker write fails the command
exits non-zero; re-run it to finish.

Chain lock: whichever chain is in use is guarded by an exclusive lock
(`chain.lock` beside it) for the kernel's lifetime. A second kernel on the
same chain refuses to boot, naming the holder's PID.

Single instance: the daemon holds an exclusive advisory lock on
`<root>/kernel.lock` for its lifetime. A second kernel on the same root
exits non-zero with `another kernel owns <root> (pid N)`. With the lock
held, a leftover `kernel.sock` that refuses connections is unlinked and
rebound; one that accepts connections is never taken over.

When the CLI cannot reach a kernel it names the socket it tried and
whether there is no socket file, a stale socket (connection refused) or a
permission problem. State-changing commands do not fall back silently:
`weft agent` requires `--local` to run in-process, and `weft cron
add/remove/enable/disable` fail without a daemon (`weft cron run` is not
implemented by the kernel).
Read-only commands may still read local files and say so on stderr.

#### Chain storage location (isolated runtimes)

The kernel resolves the chain checkpoint path once at boot and derives
the other chain files from it by extension:

| File | Purpose |
|------|---------|
| `chain.json` | JSON checkpoint (fallback format) |
| `chain.rvf` | RVF checkpoint (primary, signed) |
| `chain.key` | Ed25519 chain signing key (created on first boot) |
| `chain.tree.json` | Resource-tree checkpoint |
| `chain/anchors.jsonl` | External-anchor ledger, when anchoring is on |

Resolution order, highest first:

1. An explicit path in config: `kernel.chain.checkpoint_path` (and
   `kernel.chain.external_anchor.ledger_path` for the anchor ledger).
2. The runtime root (see "Runtime directory"): `$WEFTOS_RUNTIME_DIR`
   when set, else the project's `.weftos/runtime`, else `~/.clawft/`.
   The files go in `<root>/chain.json`, `<root>/chain.rvf` and so on.

Probe, demo and test daemons must run with `WEFTOS_RUNTIME_DIR`
pointing at a scratch directory. They still chain every action, but to
their own chain file and signing key under that directory, never to the
operator chain. The boot log shows the location that was picked:

```bash
WEFTOS_RUNTIME_DIR="$(mktemp -d)" weaver kernel start
weaver kernel logs | grep 'Chain storage'
```

Test code gets the same isolation: kernel unit tests pin a fresh temp
dir per boot, and integration tests build their config with
`ChainConfig::isolated_in(<tempdir>)`. The regression test is
`crates/clawft-kernel/tests/chain_runtime_isolation.rs`. It boots a
kernel with a fake `HOME` and `WEFTOS_RUNTIME_DIR` set, then checks
that the fake operator `chain.rvf` and `chain.key` are byte-identical
afterwards and that the isolated chain holds the new events.

#### Operator note: stray demo events from 2026-09-29

Before this isolation landed, `WEFTOS_RUNTIME_DIR` moved the daemon's
socket, PID file and log but not its chain. While card
mesh-placement-06 was being built, a demo daemon started with
`WEFTOS_RUNTIME_DIR` appended about 70 events to the operator chain at
`~/.clawft/chain.rvf`.

The same isolation work found a wider source of stray events. Until
this fix, three tests booted a kernel with `chain: None`:

- `crates/clawft-kernel/tests/feature_composition.rs`
- `crates/clawft-kernel/tests/e2e_integration.rs` (through
  `minimal_kernel_config()`)
- `crates/weftos/src/lib.rs` (`weftos_boots_and_reports_state`)

`chain: None` falls back to the default chain under `~/.clawft`, so
each of those tests loaded the operator chain on shutdown, appended its
own boot events, and saved it back (`chain.rvf` and `chain.tree.json`;
`chain.key` was not changed). As a result, every
`scripts/build.sh test clawft-kernel` or gate run on the machine before
this fix added test boot events to the operator chain. That includes the
runs on 2026-09-29, the last at about 15:48 local time. The tests now
use an isolated chain (`ChainConfig::isolated_in(tempdir)`).

The operator decided to **leave these events in place**. The chain is
append-only and hash-linked, so removing entries would break
verification for every later event. Do not try to rewrite or truncate
the chain to get rid of them. They are real entries recording demo
activity, not corruption.

To tell them apart from operator activity, look for events that are
all timestamped 2026-09-29 (the mesh-placement-06 build window) and
match these patterns:

- source `app`, kinds `app.install`, `app.start`, `app.stop` and
  `app.remove`, with payload `app_name` set to the demo app (`demo-app`
  in the card's fixtures);
- source `workload`, kinds `workload.*` (for example `workload.install`
  and `workload.start`), from the same demo run;
- the boot, governance-gate and shutdown events the demo daemon itself
  wrote in the same time window, next to the `app` and `workload`
  events.

To review them without changing anything, export the chain and filter
the export. The JSON export is a top-level array of
`{sequence, chain_id, timestamp, source, kind, hash}` records, with
timestamps in RFC 3339 UTC. Widen the date filter by a few hours if
your local day straddles midnight UTC:

```bash
weaver chain export --format json --output chain-review.json
jq '.[] | select(.timestamp | startswith("2026-09-29"))
    | select(.source == "app" or .source == "workload")
    | {sequence, timestamp, source, kind}' chain-review.json
```

The JSON export does not include payloads. To confirm that an `app.*`
event names the demo app, look it up by sequence in
`weaver chain local --count 200`. The stray events form contiguous
sequence runs, so the boot and gate events around them belong to the
same demo daemon.

### User daemon (Phase 1)

ADR-103 Phase 1 adds a per-user daemon that collapses the `machine` and
`user` roles into one process. It serves every project of one user; the
project-local runtime (`<project>/.weftos/runtime/`) keeps working
unchanged until Phase 2.

```bash
weaver kernel start --profile user     # or WEAVER_PROFILE=user
weaver kernel status --profile user
weaver kernel stop --profile user      # restart likewise
```

`--profile` is global to `weaver kernel`, so `stop`, `restart` and
`status` resolve the same root as `start`. Without it everything behaves
as before.

| Item | User profile |
|------|--------------|
| Runtime root | `~/.weftos/run/` (socket, `kernel.pid`, `kernel.log`, `kernel.lock`, `node.key`). The project walk-up is never used; `WEFTOS_RUNTIME_DIR` still overrides for isolation. |
| One per user | The `kernel.lock` in that root. A second start exits nonzero naming the holder's pid. |
| Config | `~/.weftos/weave.toml` layered over the legacy `~/.clawft/config.json`, so the `.toml` wins on conflict. The working directory is set to `~/.weftos`, so a project-local `weave.toml` or `.clawft/config.json` is not picked up. An explicit `--config FILE` is used as-is. |
| Chain | `~/.weftos/chain/` when it exists (written by `weaver migrate user-chain`). Otherwise the legacy `~/.clawft` chain under the Phase 0 guard: the first start needs `--adopt-legacy-chain`, and the daemon never starts a silent fresh genesis. `--new-chain` starts a fresh `~/.weftos/chain/` and leaves the legacy chain untouched. |
| Projects | On start the manifest store `~/.weftos/projects/` is seeded from `~/.clawft/workspaces.json` (idempotent). |

The plan's decision D-1 applies: until Phase 3 the user key is the
chain key (`chain.key`). The handshake reports its id (the node-id-style
hash of the chain verifying key) as `user_key_id`. It becomes
`~/.weftos/user.key` in Phase 3.

#### Handshake in `kernel.status`

`kernel.status` carries the same payload as `kernel.handshake` (which
stays for compatibility) under a `handshake` key:

```json
{"state": "running", "...": "...",
 "handshake": {
   "proto": {"current": 1, "min": 1},
   "node_id": "<32 hex>", "user_id": "501", "user_key_id": "<32 hex>",
   "profile": "user", "roles": ["machine", "user"],
   "project_id": null, "bound_via": "none",
   "runtime_dir": "/Users/me/.weftos/run", "pid": 4242,
   "version": "0.8.1", "sha": "...", "binary": "..."}}
```

`user_id` is the local uid and is unverified in Phase 1 (peer
credentials arrive in Phase 3). A default daemon leaves `profile`,
`roles`, `user_id` and `user_key_id` empty. `weaver kernel status`
prints the profile, runtime root, bound project, node, user and
protocol.

#### `project.*` RPCs

| Method | Capability | Params |
|--------|------------|--------|
| `project.list` | Read | none |
| `project.show` | Read | `{id}` or `{root}` |
| `project.register` | Admin | `{root, name?}` |

They read and write the manifests in `~/.weftos/projects/`.
`project.register` adopts an absolute, existing project root exactly
like `weft project init`: an existing `project.toml` id wins, a seeded
manifest is adopted, else a ULID is minted. It refuses a relative root,
`/`, `$HOME`, and a root whose id is registered for another live root
(`root_conflict`). Errors carry `error_kind`: `project_not_found`,
`invalid_project`, `bad_root`, `root_conflict`, `invalid_params`.

#### Owner migration (one machine, in this order)

This is the Phase 1 move to the user daemon. The later move of the mesh into the
machine mesh service (Phase 3) is a separate procedure:
[weftos-deployment-sops.md](./weftos-deployment-sops.md), "Moving to the machine mesh
service (owner migration)".

1. **Install the new binaries.** Nothing signals old daemons.
2. **Stop every older daemon**, from its own project directory with its own
   binary (`weaver kernel stop`, or `kill`). Confirm with `lsof -i :9470`
   that the mesh port is free and that no `kernel.pid` remains. This must
   precede the chain copy: the migration refuses a locked or recently
   written chain, but it cannot see a writer that uses a different runtime
   dir.
3. **Check `~/.clawft/config.json`.**
   - If it sets `kernel.chain.checkpoint_path`, remove it or point it at the
     migrated location. An explicit path bypasses the chain guards; one that
     points into a migrated directory is refused at boot (exit 78) unless
     `--adopt-legacy-chain` is passed, and `weaver migrate user-chain` warns.
   - Check `gateway.host`. The default is now `127.0.0.1` (with a `Host`
     check); set `0.0.0.0` only if you want LAN exposure.
4. **Create `~/.weftos/weave.toml`.** Copy the `[kernel.mesh]` and Noise
   settings from the old project's `weave.toml`. Without it the user daemon
   runs with the mesh off and the old daemon's mesh peers see it disappear.
5. **Migrate the chain.** Run `weaver migrate user-chain --dry-run` and read
   the plan (five files, the head seq and hash, signature `verified`), then
   run it without `--dry-run`. Afterwards `~/.weftos/chain/` holds the chain
   and `MIGRATED_FROM.json`, and `~/.clawft/` holds `MIGRATED-TO-WEFTOS.txt`;
   the source bytes are unchanged. It is refused if `chain.key` is missing
   or the signature does not verify; `--allow-unsigned` overrides that and is
   not recommended.
6. **Start the user daemon** with `weaver kernel start --profile user`. If
   you skipped step 5 and the legacy chain is still in use, add
   `--adopt-legacy-chain` the first time. `weaver kernel status --profile
   user` should show profile `user`, roles `machine, user`, runtime
   `~/.weftos/run` and project `(unbound)`.
7. **Optional: run it as a service.** Run
   `weaver service unit --kind launchd --out ~/Library/LaunchAgents/ai.weftos.user.plist`
   (or `--kind systemd`), then the printed `launchctl bootstrap` line. Stop
   the foreground daemon first; the lock refuses two user daemons. After
   that `weaver update --restart` restarts through the service manager. A
   refused boot exits 78; systemd does not retry it
   (`RestartPreventExitStatus=78`). launchd cannot filter on exit codes, so the
   daemon leaves `~/.weftos/run/REFUSED` after a permanent refusal or a clean
   stop and the plist keeps the job alive only while that file is absent.
   Only an instance launchd starts (the plist sets
   `WEFTOS_SERVICE_MANAGER=launchd`) removes the file on boot. If you start the
   daemon by hand while the job is parked (`weaver kernel start --profile
   user`), the file stays and the command prints a hint; to hand control back,
   `weaver kernel stop --profile user` and then
   `launchctl kickstart gui/$(id -u)/ai.weftos.user`. Regenerate a plist made
   before this marker existed.
8. **Register each project** with `weft project init` (adopts the seeded
   manifest and prints the ULID), then `weft project show .`. `weft` reaches
   the user daemon from a registered project directory, or from anywhere
   once `~/.weftos/run` has a daemon, with no `--runtime`.
9. **What to expect afterwards.**
   - `weaver kernel start` in a project without flags is refused (exit 78)
     while it would land on a migrated legacy chain. Only two flags override
     it: `--adopt-legacy-chain`, which forks history, or `--new-chain`, which
     starts a fresh project chain.
   - Against the user daemon, mutating commands outside a project return
     `project_required` (`read_only`). The log streams, `substrate.read`,
     `cluster.facts` and `voice.trace` that the egui tray and
     `weft voice watch` use are allowed; `ipc.subscribe_stream` needs a
     project.
10. **Tokens.** `weft token issue` prints a `wft_` secret once, plus a
    playground link; `weft token list|revoke` manage them. Literal `auth`
    scopes work only from the daemon's uid on the unix socket.

Rollback:

1. `weaver kernel stop --profile user`.
2. Remove `~/.weftos/chain` and `~/.clawft/MIGRATED-TO-WEFTOS.txt` (the
   marker, otherwise the old daemon is refused).
3. Restart the old daemon with the old binary in its project directory.

The `~/.clawft` chain files are byte-identical; that directory only gained
`chain.lock` (and `MIGRATED-TO-WEFTOS.txt` until you remove it).

### Project kernels (Phase 2)

ADR-103 Phase 2 (amendment A7) runs a project as its own kernel, a child of
the user daemon. The child has its own key (`<root>/.weftos/project.key`,
which is its node key and chain key), a certificate signed by your user key,
its own chain under `<root>/.weftos/chain/`, the parent's signed governance
plus an optional tighten-only `overlay.toml`, and no mesh listener, voice or
local embedding model (it uses the user daemon's). The user daemon starts it
on demand, restarts it when it crashes, stops it when it is idle and keeps
its ephemeral files in `<run_root>/<id>/` (`~/.weftos/run/<id>/`).

Nothing is converted automatically. A project becomes a child only when you
opt it in.

#### Owner procedure, per project, in this order

1. **User daemon running, project registered.** `weaver kernel status
   --profile user` answers and `weft project list` shows the project.
2. **Stop the project's old daemon** from the project directory with the
   binary that started it (`weaver kernel stop`). Confirm it is gone: no
   process in `lsof <root>/.weftos/runtime/kernel.sock`, and
   `<root>/.weftos/runtime/kernel.pid` names no live process. It is never
   signalled for you; `migrate-kernel` refuses while it runs.
3. **Dry run, then migrate.** `weaver project migrate-kernel <id> --dry-run`,
   read it, then `weaver project migrate-kernel <id>`. It copies (never
   moves) `<root>/.weftos/runtime/{workloads.json,apps.json}` to
   `<root>/.weftos/state/`, sets `[serve] via = "child-kernel"` in the
   manifest and prints the rollback line. Keep that line.
4. **Ignore the child's files in git.** `weft project init` adds
   `.weftos/chain/`, `.weftos/project.key`, `.weftos/project.cert.json` and
   `.weftos/state/` to an existing `.gitignore`; for a tree initialised by an
   older build, add `.weftos/project.cert.json` and `.weftos/state/` by
   hand. Commit `.weftos/overlay.toml` if you write one (below).
5. **Start it.** `weaver kernel start --project <id>`, or just run a `weft`
   command inside the project (the resolver asks the user daemon to start
   it). Neither starts the user daemon itself.
6. **Check it.** `weaver kernel status --profile user` lists the child as
   running. `weft project show .` prints a handshake whose `node_id` equals
   the certificate's `project_key_id` (`project.cert.show`). The user chain
   shows `project.register` at the first start and `project.anchor` at the
   child's first clean stop (and then at most every five minutes once 100
   new events have accumulated).

Rollback: `weaver kernel stop --project <id>`, then
`weaver project migrate-kernel <id> --revert` (or set `[serve] via =
"user-daemon"` in the manifest), then restart the old daemon in the project
directory. `<root>/.weftos/runtime/` was never modified.

#### Governance overlay (`<root>/.weftos/overlay.toml`)

The child runs the user daemon's rules, signed and pushed as
`<run>/<id>/parent-policy.json`, merged with the project's overlay. The
overlay can only tighten:

```toml
schema = 1

[[deny]]                        # add denies (globs; a trailing * is a prefix)
id = "project.no-shell"
actions = ["tool.shell_exec", "workload.place*"]
reason = "this project never runs shell tools"

[[require_approval]]            # force human approval for these actions
actions = ["workload.start*"]

[limits]                        # numbers may only go down, flags only up
risk_threshold = 0.5            # effective = min(parent, overlay)
max_processes = 32
spawn_budget = 4
human_approval_required = true  # false -> true only
```

`deny` and `require_approval` are unioned with the parent's; each numeric
limit is the smaller of the parent's and the overlay's; a flag is on if
either turns it on. An overlay may not contain `permit` or `deactivate`,
reuse a parent rule id, or raise a limit. Any of those, or a parse error,
refuses the boot with the offending key named. `human_approval_required =
true` in an overlay never turns a parent deny into an approval prompt: the
parent's blocking rules stay hard denies, and only actions the parent
permits ask for approval. Editing the file does nothing until the
`governance.reload` RPC on the child's socket or a restart. `max_processes` and
`spawn_budget` take effect at boot only.

#### Revoke and rekey

- `project.rekey` replaces the project key. The old child is stopped and the
  next start runs under the new certificate. No marker is written.
- `project.revoke` is **terminal for the project id**. The child is stopped,
  its tokens die, the marker `<run_root>/<id>/revoked` is written, and no
  key is ever certified for that id again (`project_revoked`). Deleting the
  marker does not re-enable the id. To run the tree again: stop nothing
  further (the child is already down), move `<root>/.weftos/chain/` aside,
  delete `<root>/.weftos/project.key` and `project.cert.json` (the revoked
  key may never certify another project), run `weft project init --fork
  --force` in the tree (a new id; the old manifest is archived), then
  migrate the new id as above.
- If either RPC answers `revoke_marker_unwritten` or
  `identity_change_incomplete`, the change is in force and the old child was
  stopped, but a file could not be written or removed. The message names it;
  fix the cause (for a missing marker, write `revoked` into that path by
  hand).

#### Project chain reset and anchors

A project chain moved aside restarts its anchors at `seq = 1`, which the user
daemon refuses while it holds the old head (`anchor_seq`; `kernel.status`
shows `anchor_pending: true`). After resetting the chain on purpose, the owner
runs `weaver project anchor reset --project <id> --reason "..."` on the user
daemon. The reset is chained and signed; the old statements stay as history and
the project anchors again from genesis. Do not run it for a project whose chain
was not reset.

#### Rotating the user key

`weaver migrate user-key --rotate` (user daemon stopped) replaces
`~/.weftos/user.key` and records a handover signed by the old and the new key in
`<manifests>/user-key-rotations.jsonl`. Certificates and anchor records sealed
by the old key keep verifying up to the rotation point; the old key's later
signatures are refused. Afterwards start the user daemon, restart each project
kernel (`weaver kernel restart --project <id>`; a running child pins the old
key), and run `weaver mesh bind rebind` if the machine mesh service is
installed. A project kernel that keeps running after the rotation fails its
next `mesh.register` (`pop_failed`) and is restarted by the supervisor's
liveness pass after about 30 s beyond three missed beats; it is not
re-certified in place. The old key is kept as `user.key.retired-<n>`; delete it yourself
once you are satisfied. After a suspected compromise, also rekey or revoke the
affected projects. Details: ADR-103 A13.

#### Never downgrade a child-kernel project

A pre-Phase 2 `weaver` run against a `child-kernel` project does not refuse
the project chain: it cannot verify the newer events (`rule_hash`), treats the
chain as unreadable and **starts a fresh chain over it**, destroying the
project's history. Before running an older binary on such a project, move
`<root>/.weftos/chain/` aside (and set `[serve] via` back with
`migrate-kernel --revert`).

### Governance (three-branch)

Governance is a permission-and-deferral layer in front of the chain.
ADR-033 (Three-Branch Governance) defines the model: legislative
(rules / capabilities), executive (the running agent / service), and
judicial (post-hoc review of `Defer` outcomes). Effects are scored
via the `EffectAlgebra` (ADR-034) so high-blast-radius actions can
be auto-deferred for human review.

```bash
weaver governance status            # current rule set summary
weaver governance pending           # actions Deferred and awaiting review
weaver governance review <id>       # accept or reject a deferred action
```

### Process table & supervisor (K1)

```bash
weaver agent list                  # PID / name / state for every agent
weaver agent inspect <pid>         # capabilities, parent PID, recent events
weaver agent spawn <type>          # spawn a new agent (subject to RBAC)
weaver agent stop <pid>            # graceful stop (SIGTERM-equivalent)
weaver agent restart <pid>         # supervised restart, chain-logged
```

### Health & lifecycle

```bash
weaver kernel start                # bring the kernel up
weaver kernel stop                 # graceful shutdown (drains agents)
weaver kernel status               # boot state + uptime
weaver kernel liveness             # self-check; exit 1 when wedged or stale
weaver health                      # aggregated health across services
weaver kernel logs                 # last N boot/runtime events
```

#### Bounded stop

`weaver kernel stop` sends SIGTERM and waits 10 s. The daemon's own
shutdown fits inside that wait:

- The shutdown runs as named steps: RPC accept loop (2 s), child endpoint
  (1 s), agent service (5 s), nested quiesce (3 s), nested project kernels
  (10 s), project hooks (2 s), agent supervisor (5 s), then the kernel.
  Each step logs when it starts and how long it took, or that it timed out
  and was abandoned. Early steps never use the last 3 s, which the kernel
  step keeps for itself (service stops, then the chain checkpoint). Inside
  the kernel step every service's `stop()` runs concurrently, with 3 s
  each.
- The whole shutdown has a budget of 7 s, or 13 s for a nested instance
  (its owner allows 15 s). A guard on its own OS thread ends the process
  1 s after the budget. It needs no async runtime, so it fires even when
  every runtime thread is blocked. It logs `shutdown exceeded its budget`,
  removes `kernel.pid`, and exits 0 on a stop (so `Restart=on-failure`
  does not bring back what was stopped) or 1 on a restart.
- `kernel.pid` is removed only when the process is about to exit. A live
  daemon always has its pid file.

When the daemon does not exit:

- `weaver kernel stop` fails (exit 1), says so, and leaves the pid file
  in place. Earlier versions deleted the pid file and socket anyway. That
  left a running daemon that nothing could find.
- `weaver kernel stop --force` sends SIGKILL after the 10 s and waits up
  to 5 s more for the process to exit.
- Without a usable pid file, both forms find the daemon through the
  run-dir lock: the daemon is the process holding `kernel.lock`, and the
  lock file names its pid.

#### Liveness and the watchdog

Every 10 s the daemon runs a round of probes, each with a 5 s limit:

- scheduler lag;
- a blocking-pool round trip;
- the kernel lock;
- every mesh peer-table lock (`MeshRuntime::lock_probe`);
- a `kernel.liveness` call over the daemon's own socket (accept loop, gates and dispatch).

`kernel.liveness` returns the last round. `weaver kernel liveness [--json]`
exits 0 only when all of these hold:

- the daemon answers within 10 s;
- the last round passed;
- that round is less than three intervals old.

The generated systemd units set `WatchdogSec=60` and `NotifyAccess=main`.
These are the user daemon unit (`weaver service unit --kind systemd`) and
`weftos-mesh` (`weaver mesh install-service`). The process sends
`WATCHDOG=1` after every round that passes. Rounds run every 10 s at most,
and at least three times per watchdog interval. A wedged process stops
sending it, so systemd kills it after 60 s and `Restart=` starts it again.

- **Boot:** while the daemon boots, a keepalive feeds the watchdog for up
  to 5 minutes.
- **Child processes:** children inherit the variables but send nothing,
  because `WATCHDOG_PID` names the parent.
- **No systemd:** without `NOTIFY_SOCKET` and `WATCHDOG_USEC` (macOS, or
  a manual start) nothing is sent, and only the self-check runs.

launchd has no watchdog. `KeepAlive` restarts a process that exits, not
one that hangs. On macOS, run the self-check from a periodic job and kill a
daemon that fails it. `KeepAlive` then relaunches it, because a SIGKILL
leaves no `REFUSED` sentinel:

```bash
weaver kernel liveness --profile user || weaver kernel stop --profile user --force
```

Run it every few minutes, for example from a LaunchAgent with
`StartInterval`.

### Windows transport (WEFT-11 + WEFT-559)

Local RPC uses a platform transport in `clawft-rpc` / `clawft-weave`:

| Platform | Transport | `DaemonClient::connect` | Daemon accept loop |
|----------|-----------|-------------------------|--------------------|
| Unix     | UDS `kernel.sock` under the runtime dir | Connects or returns `None` | Wired (`clawft-weave::daemon`) |
| Windows  | Named pipe `\\.\pipe\clawft-kernel-<hash>` derived from the logical socket path | Connects or returns `None` (WEFT-11) | Wired (WEFT-559) |
| Other    | — | Always `None`; `call` errors clearly | N/A |

- Path derivation: `pipe_name_for_path(socket_path())` — project-local
  runtimes stay isolated the same way UDS paths do.
- Server helpers: `create_listener` / `create_listener_next` in
  `clawft_rpc::named_pipe`; the daemon accept loop re-creates the next
  instance after each client connects.
- `weaver kernel start` (background or `--foreground`), `stop` (RPC
  `kernel.shutdown` then `taskkill`), and `restart` (stop+start) work
  on Windows. cargo-dist includes `x86_64-pc-windows-msvc`.
- Residual: soak-test the named-pipe roundtrip and release artefacts on
  a Windows host / `windows-latest` CI (see build guide).

Details:
[`weftos-deferred-requirements.md`](./weftos-deferred-requirements.md)
(Windows transport section).

### Mesh (K6)

When the `mesh` feature is enabled, the kernel also listens on a
mesh transport for cross-node IPC, chain replication, and SWIM
heartbeats (ADR-039). The mesh runs as a phase-5d boot step and is
configured under `[kernel.mesh]` in `~/.clawft/config.json`.

Transports (selectable via `transport`):

| Value | Backend | Notes |
|-------|---------|-------|
| `tcp` (default) | `TcpTransport` | Dev / UDP-blocked nets |
| `ws` / `websocket` | `WsTransport` | Browser / proxy-friendly |
| `quic` | `QuicTransport` (quinn) | ADR-026 primary; needs `quic` feature + **UDP** |

Application crypto is optional Noise (`noise = true`, snow XX). See
[mesh-quic.md](./mesh-quic.md) for QUIC config, firewall, and tests.

```bash
weaver mesh status                 # peer count, view, listener address
weaver mesh peers                  # detailed peer table with last-heard timestamps
```

---

## DEMOCRITUS Cognitive Tick

DEMOCRITUS (named for the atomist; see ADR-047 Self-Calibrating
Tick) is the kernel's cognitive loop. It runs behind the `ecc`
feature flag and drives the ECC substrate through a SENSE → THINK
(fast EML) → DETECT DRIFT → THINK (exact RFF) → LOG → COMMIT cycle.
Operators encounter DEMOCRITUS in two places:

1. **Boot logs**: at phase 5b you will see lines like
   `DEMOCRITUS: tick interval calibrated to N ms` and
   `DEMOCRITUS: causal graph idle`.
2. **Runtime logs**: periodic INFO/WARN lines describing causal
   activity, drift detection, and cycle / stuck-state warnings.

### Reading the "still stuck" log line

`crates/clawft-kernel/src/cognitive_tick.rs` emits a `WARN` line of
the form:

```
DEMOCRITUS: still stuck after N checks: Stuck { net_change: 0.0, ... }
```

(or `Oscillating { ... }`) when the cycle detector observes that the
λ₂ coherence history has flat-lined or oscillated for a window.
**This is not a bug** in the overwhelming majority of cases.

It happens during normal operation in three scenarios:

- **Empty causal graph**: the kernel boots, ECC is enabled, but no
  agent has yet produced an event for the cognitive tick to chew on.
  The graph is empty, so coherence does not move, and the cycle
  detector reports "stuck" exactly as designed. The `idle-graph
  gate` (post-v0.6.19) suppresses this in steady state, but transient
  empty-graph windows during boot still surface one or two warnings.
- **Idle conversation**: the operator-visible agent has not received
  a new prompt for many ticks. The graph stops growing, λ₂ stops
  changing, and the detector reports "stuck" until the next event
  perturbs the graph.
- **Steady-state convergence**: the conversation has settled into a
  stable attractor. From the cycle detector's point of view this is
  indistinguishable from "stuck"; from the operator's point of view
  this is healthy behaviour.

What v0.6.19 did about it:

- **Edge-triggered logging**: entering and leaving the stuck phase
  always log; in-phase repeats log on an exponential-backoff
  schedule (`stuck_checks_since_log` doubles after each warning,
  capped at 256 checks). You should see the warning *less often*
  the longer it persists, not more.
- **Idle-graph gate**: when `causal.node_count() == 0` the loop
  skips the cycle-detector branch entirely, so a freshly-booted
  kernel does not spam the warning before any event has landed.
- **Bounded coherence history**: the rolling window cannot grow
  without bound, so memory does not leak through long stuck
  phases.

When the warning *does* mean something:

- The warning fires *immediately on entry* with `ConversationState::Stuck`
  *and* you have an active conversation — i.e. messages are being
  produced but coherence is not moving. That suggests the agent is
  looping on itself or wedged on a thinking step.
- The warning fires every tick despite the backoff (it shouldn't —
  if it does, the backoff state machine is broken).
- The warning correlates with elevated tick latency in
  `weaver health` or with a flapping process under
  `weaver agent list`.

In all three of those cases, capture the surrounding chain entries
(`weaver chain export`) and file an issue. Otherwise: ignore the
line, or filter it at your log aggregator.

The cognitive-tick code that emits these lines is gated as of commit
`5f888a1a` (v0.6.19) — see the kernel-governance audit
(`.planning/reviews/0.7.0-release-gate/02-kernel-governance.md`,
"Open questions and known limitations") for the full backstory.

---

## Configuration

Operator-facing kernel configuration lives in
`~/.clawft/config.json` under the `kernel` key (with workspace and
project-level overlays per the
[Configuration Guide](./configuration.md)). The most-touched fields:

```jsonc
{
  "kernel": {
    "features": ["exochain", "ecc", "mesh"],
    "mesh": {
      "enabled": true,
      "transport": "tcp",
      "listen_addr": "0.0.0.0:9489",
      "seed_peers": ["10.0.0.2:9489"]
    },
    "chain": {
      "path": "~/.clawft/chain"
    },
    "governance": {
      "default_policy": "permit",
      "deferral_threshold": 0.7
    },
    "ecc": {
      "tick_interval_ms": "auto",
      "stuck_suppress_cap": 256
    },
    "ipc_tcp": {
      "enabled": false,
      "listen": "127.0.0.1:9420"
    }
  }
}
```

The mesh listener defaults to port **9489** ("the weave"; ADR-103 D1) and
`listen_addr` is the configurable address (`listen` is accepted as an
alias). When `mesh.enabled` is true and the address cannot be bound, for
example because another kernel on the same machine already holds the port,
boot fails with an error naming the address. With `mesh.enabled = false`
nothing is bound and nothing fails.

A node's id is derived from its Ed25519 node key, not generated per boot:
`node_id = hex(SHA-256(pubkey)[..16])`, 32 hex characters (ADR-025, ADR-103
D11). The same id appears in the mesh handshake, cluster membership,
heartbeats and `kernel.status`, and it is stable across restarts while
`<runtime>/node.key` is kept.

See [`docs/weftos/kernel-modules.md`](../weftos/kernel-modules.md)
for the full per-module reference and `kernel-modules.md` for
which features pull in which crate dependencies.

---

## Where to Go Next

- **Full architecture**: [`docs/weftos/architecture.md`](../weftos/architecture.md).
- **Per-phase status (live)**: [`docs/weftos/k-phases.md`](../weftos/k-phases.md).
- **ADRs that govern kernel design**: ADR-021 (CLI Kernel
  Compliance), ADR-022 (ExoChain Mandatory Audit), ADR-023
  (Assessment as a Kernel Service), ADR-033 (Three-Branch
  Governance), ADR-047 (Self-Calibrating Tick), ADR-048 (Kernel
  Phase Responsibilities), ADR-049 (WeftOS Kernel Architecture).
- **Audit (release-gate snapshot)**:
  [`.planning/reviews/0.7.0-release-gate/02-kernel-governance.md`](../../.planning/reviews/0.7.0-release-gate/02-kernel-governance.md).
- **Developer-facing kernel guide**:
  [`docs/src/content/docs/weftos/kernel-guide.mdx`](../src/content/docs/weftos/kernel-guide.mdx).

If you need a concept that is not covered here, check the
audit doc first — it is the most current snapshot of what
0.7.0 actually ships.
