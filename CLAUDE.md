# Claude Code Configuration - Claude Flow V3

## Behavioral Rules (Always Enforced)

- Do what has been asked; nothing more, nothing less
- NEVER create files unless they're absolutely necessary for achieving your goal
- ALWAYS prefer editing an existing file to creating a new one
- NEVER proactively create documentation files (*.md) or README files unless explicitly requested
- NEVER save working files, text/mds, or tests to the root folder
- Never continuously check status after spawning a swarm — wait for results
- ALWAYS read a file before editing it
- NEVER commit secrets, credentials, or .env files

## Dashboard board is the authoritative work tracker

Every meaningful unit of WeftOS / clawft work goes through the WeftOS
dashboard board. Use `node scripts/dashboard-board.mjs ready` to read incoming
work, `create <stable-key> <title> <description> [source URL]` for a new item,
and `claim <ticket UUID|WEFT-N>` before starting code. Include source citation,
acceptance criteria, dependencies, and an observable completion condition in
the description. On completion, use `done <ref> <shipped, commit, tests, build>`.
Use `note` to record blockers or deferrals and `move` to set the state. Imported
WEFT-N IDs remain searchable references; Plane is historical after cutover.
Do not create or update WeftOS tickets in Plane.

## File Organization

- NEVER save to root folder — use the directories below
- Use `/src` for source code files
- Use `/tests` for test files
- Use `/docs` for documentation and markdown files
- Use `/config` for configuration files
- Use `/scripts` for utility scripts
- Use `/examples` for example code

## Project Architecture

- Follow Domain-Driven Design with bounded contexts
- Keep files under 500 lines
- Use typed interfaces for all public APIs
- Prefer TDD London School (mock-first) for new code
- Use event sourcing for state changes
- Ensure input validation at system boundaries

### Project Config

- **Topology**: hierarchical-mesh
- **Max Agents**: 15
- **Memory**: hybrid
- **HNSW**: Enabled
- **Neural**: Enabled

## Build & Test

**MANDATORY: Use `scripts/build.sh` for ALL build, test, check, and lint operations.**
Do NOT run `cargo build`, `cargo test`, `cargo check`, or `cargo clippy` directly
unless you are debugging a specific compilation issue that requires direct cargo
flags not exposed by the script. If that happens, extend `scripts/build.sh` with
the new capability so future builds use it.

```bash
# Build native CLI (release)
scripts/build.sh native

# Build native CLI (debug, fast iteration)
scripts/build.sh native-debug

# Build with extra features
scripts/build.sh native --features voice,channels

# Run workspace tests
scripts/build.sh test

# Fast compile check (no codegen)
scripts/build.sh check

# Lint (clippy, warnings as errors)
scripts/build.sh clippy

# Build WASM targets (prefer these over raw cargo)
scripts/build.sh wasi       # wasm32-wasip2 (edge / wasmtime hosts)
scripts/build.sh browser    # wasm32-unknown-unknown + --features browser

# Browser extras
scripts/build.sh serve          # Serve www/ test harness (default :8080)
scripts/build.sh test-browser   # Headless Chrome suite (wasm-pack + chromedriver)
scripts/build.sh browser --features browser-opfs  # extra features on top of browser

# Build everything (native + wasi + browser + ui)
scripts/build.sh all

# Full phase gate (use before committing). Check 0 is the pre-build diff gate
# (scripts/diff-gate.sh): it runs first and rejects new unsafe, deps, build.rs, net/process/FFI
# and CI-file changes before anything compiles. Agents must NOT add entries to
# config/diff-gate-approvals.toml or set DIFF_GATE_OWNER_APPROVED; only the owner does.
# See docs/development/diff-gate.md.
scripts/build.sh gate

# Preview what a command would do
scripts/build.sh native --dry-run

# See all commands and options
scripts/build.sh --help
```

- ALWAYS run `scripts/build.sh test` after making code changes
- ALWAYS run `scripts/build.sh check` (or `gate`) before committing
- If a new feature needs build flags not in the script, ADD them to `scripts/build.sh`
- Agents MUST use `scripts/build.sh`, not raw cargo, except for critical debugging

### Browser / WASM (W-BROWSER)

**W-BROWSER** scope (landed): compile + browser transport + www harness +
agent pipeline wired in-tab. Entry is wasm-bindgen (`init` / `send_message`),
not `fn main()`. Docs: [`docs/browser/`](docs/browser/).

- **Browser**: `scripts/build.sh browser` → `clawft-wasm` for
  `wasm32-unknown-unknown` with `--no-default-features --features browser`
  (and `release-wasm` profile + wasm-bindgen into `crates/clawft-wasm/www/pkg/`).
  Do **not** invent raw cargo browser flags; the script owns them.
- **WASI** (separate path): `scripts/build.sh wasi` → `wasm32-wasip2` for
  wasmtime / edge hosts — not the browser tab story.
- **Mutex**: `native` ⊻ `browser` — never enable both on the same compile unit.
  See ADR-083. For ad-hoc checks only:
  `cargo check --target wasm32-unknown-unknown -p clawft-wasm --no-default-features --features browser`.

## Security Rules

- NEVER hardcode API keys, secrets, or credentials in source files
- NEVER commit .env files or any file containing secrets
- Always validate user input at system boundaries
- Always sanitize file paths to prevent directory traversal
- Run `npx --no-install @claude-flow/cli security scan` after security-related changes

## Concurrency: 1 MESSAGE = ALL RELATED OPERATIONS

- All operations MUST be concurrent/parallel in a single message
- Use Claude Code's Task tool for spawning agents, not just MCP
- ALWAYS batch ALL todos in ONE TodoWrite call (5-10+ minimum)
- ALWAYS spawn ALL agents in ONE message with full instructions via Task tool
- ALWAYS batch ALL file reads/writes/edits in ONE message
- ALWAYS batch ALL Bash commands in ONE message

## Swarm Orchestration

- MUST initialize the swarm using CLI tools when starting complex tasks
- MUST spawn concurrent agents using Claude Code's Task tool
- Never use CLI tools alone for execution — Task tool agents do the actual work
- MUST call CLI tools AND Task tool in ONE message for complex work

### 3-Tier Model Routing (ADR-026)

| Tier | Handler | Latency | Cost | Use Cases |
|------|---------|---------|------|-----------|
| **1** | Agent Booster (WASM) | <1ms | $0 | Simple transforms (var→const, add types) — Skip LLM |
| **2** | Haiku | ~500ms | $0.0002 | Simple tasks, low complexity (<30%) |
| **3** | Sonnet/Opus | 2-5s | $0.003-0.015 | Complex reasoning, architecture, security (>30%) |

- Always check for `[AGENT_BOOSTER_AVAILABLE]` or `[TASK_MODEL_RECOMMENDATION]` before spawning agents
- Use Edit tool directly when `[AGENT_BOOSTER_AVAILABLE]`

## Swarm Configuration & Anti-Drift

- ALWAYS use hierarchical topology for coding swarms
- Keep maxAgents at 6-8 for tight coordination
- Use specialized strategy for clear role boundaries
- Use `raft` consensus for hive-mind (leader maintains authoritative state)
- Run frequent checkpoints via `post-task` hooks
- Keep shared memory namespace for all agents

```bash
npx --no-install @claude-flow/cli swarm init --topology hierarchical --max-agents 8 --strategy specialized
```

## Swarm Execution Rules

- ALWAYS use `run_in_background: true` for all agent Task calls
- ALWAYS put ALL agent Task calls in ONE message for parallel execution
- After spawning, STOP — do NOT add more tool calls or check status
- Never poll TaskOutput or check swarm status — trust agents to return
- When agent results arrive, review ALL results before proceeding

## V3 CLI Commands

### Core Commands

| Command | Subcommands | Description |
|---------|-------------|-------------|
| `init` | 4 | Project initialization |
| `agent` | 8 | Agent lifecycle management |
| `swarm` | 6 | Multi-agent swarm coordination |
| `memory` | 11 | AgentDB memory with HNSW search |
| `task` | 6 | Task creation and lifecycle |
| `session` | 7 | Session state management |
| `hooks` | 17 | Self-learning hooks + 12 workers |
| `hive-mind` | 6 | Byzantine fault-tolerant consensus |

### Quick CLI Examples

```bash
npx --no-install @claude-flow/cli init --wizard
npx --no-install @claude-flow/cli agent spawn -t coder --name my-coder
npx --no-install @claude-flow/cli swarm init --v3-mode
npx --no-install @claude-flow/cli memory search --query "authentication patterns"
npx --no-install @claude-flow/cli doctor --fix
```

## Available Agents (60+ Types)

### Core Development
`coder`, `reviewer`, `tester`, `planner`, `researcher`

### Specialized
`security-architect`, `security-auditor`, `memory-specialist`, `performance-engineer`

### Swarm Coordination (claude-flow prompt roles only — WEFT-199)
`hierarchical-coordinator`, `mesh-coordinator`, `adaptive-coordinator`

These names are **claude-flow / Ruflo swarm prompts**, not separate
in-tree Rust coordinator types. WeftOS runtime multi-agent fan-out is
[`SwarmCoordinator`](crates/clawft-core/src/agent_bus/coordinator.rs)
with **flat** topology only (`SwarmTopology::Flat`). See
`docs/architecture/swarm-topology.md`.

### GitHub & Repository
`pr-manager`, `code-review-swarm`, `issue-tracker`, `release-manager`

### SPARC Methodology
`sparc-coord`, `sparc-coder`, `specification`, `pseudocode`, `architecture`

## Memory Commands Reference

```bash
# Store (REQUIRED: --key, --value; OPTIONAL: --namespace, --ttl, --tags)
npx --no-install @claude-flow/cli memory store --key "pattern-auth" --value "JWT with refresh" --namespace patterns

# Search (REQUIRED: --query; OPTIONAL: --namespace, --limit, --threshold)
npx --no-install @claude-flow/cli memory search --query "authentication patterns"

# List (OPTIONAL: --namespace, --limit)
npx --no-install @claude-flow/cli memory list --namespace patterns --limit 10

# Retrieve (REQUIRED: --key; OPTIONAL: --namespace)
npx --no-install @claude-flow/cli memory retrieve --key "pattern-auth" --namespace patterns
```

## Quick Setup

```bash
# Prefer project .mcp.json (pinned via package.json weftos.rufloPin = 3.42.4;
# real source is ~/dev/ruflo feat/grok-host — see weftos.rufloPinNote).
# Manual add: never @latest — schema owns .swarm/agentdb-memory.db (WEFT-684).
claude mcp add claude-flow -- npx --no-install @claude-flow/cli mcp start
npx --no-install @claude-flow/cli daemon start
npx --no-install @claude-flow/cli doctor --fix
```

## Claude Code vs CLI Tools

- Claude Code's Task tool handles ALL execution: agents, file ops, code generation, git
- CLI tools handle coordination via Bash: swarm init, memory, hooks, routing
- NEVER use CLI tools as a substitute for Task tool agents

## Support

- Documentation: https://github.com/ruvnet/claude-flow
- Issues: https://github.com/ruvnet/claude-flow/issues
