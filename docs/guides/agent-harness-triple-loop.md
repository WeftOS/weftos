# Agent harness and the triple loop

How WeftOS (and any sibling repo) runs incoming work the way Forge does:
a **master controller**, three standing loops, one board, jobs through
process-compose. Published twin: [Agent Harness](/weftos/guides/agent-harness).

**Do not build an environment-wide process manager yet.** Per-project
`process-compose.yaml` is the unit. The overlay is planned in
[ADR-098](../adr/adr-098-environment-process-compose.md).

## Why this exists

A chat that picks up a ticket and dies is not a harness. Incoming work,
research, and development have to keep turning when the human is elsewhere.
WeftOS has the dashboard board, MetaHarness, and a flywheel runner. This document
wires those into the same **triple loop** the Forge lead uses.

```
                    master controller (lead)
                              │
          ┌───────────────────┼───────────────────┐
          ▼                   ▼                   ▼
   1. research loop     2. development loop   3. ops / compose
   (always on)          (plan + do ≥ 90)      (desired state)
          │                   │                   │
          └──── one board (WeftOS dashboard) ─────┘
```

The master controller is a long-lived Grok (or Claude) session that **does
not implement everything**. It drains, scores, routes, and judges. Lane
agents do the work.

## Add the harness (checklist)

Drop these in a repo root. WeftOS already has the first four; the rest
are the 2026-08-17 contract.

| path | role |
|---|---|
| `AGENTS.md` | Master + lanes, loop verbs, hard rules |
| `.grok/rules/agent-harness.md` | Short rule the host actually loads |
| `.grok/rules/metaharness.md` | Score / flywheel / no silent promote |
| Dashboard + `scripts/dashboard-board.mjs` | Incoming WeftOS work SoT |
| `process-compose.yaml` | This project's servers + jobs |
| `compose/desired.yaml` | Up/down flags |
| `compose/manifest.yaml` | Project id, namespace, ports (overlay contract) |
| `compose/ports.yaml` | Ports this project claims |

Then teach every lane: **jobs go through compose**, restarts need
`--agent` and `--reason`, never invent a one-off Python runner.

WeftOS mapping of Forge verbs:

| Forge | WeftOS |
|---|---|
| `forge-loop.mjs say/hand/work` | `scripts/grok-team-bus.mjs` + dashboard ticket notes |
| `forge-harness` board | Dashboard (`dashboard-board.mjs ready/claim/done`) |
| `/harness-score` | `scripts/metaharness/score.sh` |
| `/harness-research` | `scripts/metaharness/crosscut.mjs` + dashboard board hunt |
| `/harness-plan` / `/harness-do` | dashboard claim → worktree → `done` |
| flywheel forever | `node scripts/metaharness/loop-runner.mjs --forever` (opt-in) |

## Master controller

You (the lead session) sit **outside** the inner cogs.

Every turn:

1. Theme work (not wait-black) if this host uses Ghostty surfaces.
2. Drain dashboard ready (`node scripts/dashboard-board.mjs ready`) and any team-bus inbox.
3. Run the **score** cog (`scripts/metaharness/score.sh`) as its own subtask.
4. Run the **research** cog. Never skip. Fail open.
5. If a development ticket is ready, cut or claim **one** singular task.
6. Hand durable facts to the lane that owns them (`memory_store` / team bus).
7. Do not start `:3000` on Forge or a second process-compose. Do not take
   down a desired-up webserver you do not own.

You spawn named subagents (architect → coder → tester → reviewer) for
fat work. You do **not** wait for Ruflo to write code.

## Loop 1 — research (always on)

Purpose: keep incoming work and leftover hunt alive so plan has something
to cut. This is the “full-time loop” for a large board.

| beat | WeftOS command |
|---|---|
| Index | `node scripts/metaharness/weftos-brain.mjs index` |
| Crosscut | `node scripts/metaharness/crosscut.mjs` |
| Ready set | `node scripts/dashboard-board.mjs ready` |
| Optional Darwin dry | `node scripts/metaharness/darwin-loop.mjs` (no promote) |

Rules:

- Fail open. A missing MCP does not stop the turn.
- Do not invent tickets that are already on the dashboard board.
- Do not start `loop-runner.mjs --forever` unless the user asked — it
  spends tokens on a timer.
- Receipt: `.metaharness/brain/crosscut-latest.json` or a dashboard ticket note.

When ready is empty, hunt leftovers (docs drift, score gaps, fusion
anchors) and file **one** dashboard ticket with acceptance, ruin, and
`blocked_by`.

## Loop 2 — development (plan + do)

Inner cogs. Both must hit **90**. The historical Plane plan-pass was 0.80 / doer 0.85;
the meta bar here is 90.

**Plan** cuts a singular task from the dashboard board or from a brief:

- One doer can finish it.
- Acceptance is a command or an observable.
- Ruin is named.
- Dependencies are `WEFT-N`, not vibes.

**Do** claims first (`node scripts/dashboard-board.mjs claim <ref>`), implements in
a worktree if another coder is live, then `done` with tests/build.

```bash
node scripts/dashboard-board.mjs claim <ticket-uuid-or-WEFT-N>
node scripts/dashboard-board.mjs goals
node scripts/dashboard-board.mjs link <ticket-uuid-or-WEFT-N> <goal-uuid>
# … implement …
node scripts/dashboard-board.mjs done <ticket-uuid-or-WEFT-N> \
  "Shipped …; commit <sha>; tests scripts/build.sh test; build scripts/build.sh check"
```

Jobs (typecheck, gate, score) run through this project's
`process-compose.yaml` (`job_*`, `disabled: true`) or
`process-compose process start job_…` once that project's PC is up.

The dashboard client reads a host-local `wfb_` credential from
`~/.config/weftos/board-token` (mode 600), or `WEFTOS_BOARD_TOKEN`. Give each
harness host its own credential. Each host's heartbeat reporter reads a
separate `wft_` credential from `~/.config/weftos/node-token` and its dashboard
node UUID from `~/.config/weftos/node-id`; it runs as `dashboard-heartbeat` in
process-compose or a host-local systemd user service. Keep these host-local
files off Grokbot sync.
The dashboard retains imported `WEFT-N` references but no longer writes Plane.
Shasta software harnesses use a Shasta-scoped dashboard credential from their
own repo. Shasta restoration, BakeOS production tasks, and a client
product board's tickets retain their native board authority; their dashboard
subscriptions are read-only views. BakeOS publishes native `MS-` IDs, and
a client project's board publishes its own native IDs under the engagement lead's
scoped export decision.
FlipsOS publishes redacted native ticket numbers; its first verified snapshot
contains 102 tickets. Goal publication awaits its schema migration.

## Loop 3 — ops / process-compose

Each **project** runs **one** process-compose instance.

| project | HTTP | MCP SSE |
|---|---|---|
| Forge (`forge-krause-automation`) | `:18080` | `:18081` |
| WeftOS | `:18090` | `:18091` |
| 1v1sion (`1v1sion.com`) | `:18100` | `:18101` |
| WeftOS tailnet router (ADR-116) | `:18000` | — |

Those ports are claimed in `compose/ports.yaml`. Do not reuse Forge's
18080/18081 here. Kernel HTTP `:8080` is the product daemon, not PC.

```bash
# WeftOS (from repo root)
process-compose up -D -t=false --keep-project -f process-compose.yaml \
  -p 18090 --address 127.0.0.1
# inspect
process-compose process list -p 18090 --address 127.0.0.1 -o json
```

Mutations need a reason. Prefer a small wrapper later; until then log
the action in `.grok/compose/` or ExoChain when the kernel is up.

## Environment-wide pane (planned, not built)

process-compose **can** merge files (`-f a -f b`, `extends`) the way
docker-compose does: last file wins on the same process name; maps merge;
paths are relative to the **first** file; `.env` is CWD-only.

It is **not** terraform. Namespaces do not isolate names. Two projects
that both define `web` become one process. Two `mcp_server` blocks
collapse to one listener. Relative `working_dir: src/…` breaks when the
merged CWD is not that repo.

Verified 2026-08-16 on v1.120.0: merging two yamls that each had a
process named `web` produced **3** processes, not 4 (last `web` won).

So we **do not** scan the disk and feed every `process-compose.yaml`
into one instance. That would collide ports (`:3000`, `:8080`, `:18081`),
overwrite jobs, and start the wrong `web`.

Overlay ships only after every participating project has:

1. `compose/manifest.yaml` with a unique `id` and `prefix`
2. Process names already prefixed (`weft-kernel`, `forge-ui`)
3. Absolute `working_dir`
4. **No** `mcp_server` in the leaf (or a unique port reserved in
   `compose/ports.yaml`)
5. `overlay_ready: true`

Until then: one PC per project, one TUI/API per project, a registry
file that only **lists** them. See ADR-098.

## Hard rules

- Never commit to `master` / `main`.
- MetaHarness does not become a `weft` link dependency (ADR-096).
- No silent flywheel promote.
- No invented torque / LOTO / manufacturer PN (Forge). No invented
  dashboard ticket acceptance.
- Do not take down a desired-up webserver you do not own.
