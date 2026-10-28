# Cheap and local model routing for agent work (2026-10-27)

Research only. Nothing was configured, started or installed. Owner rule: "Give simple subagents a cheaper model", plus "qwen next locally".

Provenance tags: `[primary]` = read from source on this Mac; `[local-doc]` = our own `~/llm` docs (measured by us); `[binary]` = string present in the installed Claude Code binary; `[experience]` = no source read. The ruvnet-brain worker was down, so no brain claims are used.

Source versions: `~/dev/ruflo` is on branch `main` at b93144803 (2026-10-18), `@claude-flow/cli` 3.57.0. The brief named `local/main-plus-prs`; that branch is not what is checked out there. Claude Code 2.1.311. Machine: Apple Silicon, 128 GB unified memory (`sysctl hw.memsize` = 137438953472).

## 1. What exists

### Ruflo (the 3-tier router)

- `hooks_route` and `hooks_pre-task` return a routing block. Tier 1 emits `[CODEMOD_AVAILABLE]` (not `[AGENT_BOOSTER_AVAILABLE]`); tiers 2 and 3 emit `[TASK_MODEL_RECOMMENDATION] Use model="<haiku|sonnet|opus>"`. [primary: ruflo/v3/@claude-flow/cli/src/mcp-tools/hooks-tools.ts:1802-1830]
- Tier 1 is now a deterministic TypeScript-compiler codemod, not a WASM model call (ADR-143). Only three intents are implemented: `var-to-const`, `remove-console`, `add-logging`. `add-types`, `add-error-handling` and `async-await` are explicitly not supported and go to a model. Executed with `hooks_codemod`, which accepts a file, a files list or a glob. [primary: hooks-tools.ts:5376-5403]. The label "Agent Booster" survives in `enhanced-model-router.ts` (`handler: 'codemod' | 'agent-booster' | 'haiku' | 'sonnet' | 'opus'`) and in older docs, so the signal name in our CLAUDE.md files is stale.
- `hooks_model-route` picks haiku/sonnet/opus with a lexical complexity score plus a Thompson-sampling bandit persisted to `.swarm/model-router-state.json`; `hooks_model-outcome` feeds results back; `hooks_model-stats` reports; `hooks_model-verify` is a $0 structural check (empty, truncated, refusal, repetition, TS/JSON parse) that returns an escalate verdict. The neural router is double-gated (`CLAUDE_FLOW_ROUTER_NEURAL=1`) and off by default. [primary: ruvector/model-router.ts:1-40; hooks-tools.ts:5190-5340]
- The router only names Claude tiers. It does not return a local model. Mapping a recommendation onto an actual subagent model is left to whoever spawns it. [primary: model-router.ts header; the Claude Code Agent tool is outside ruflo]
- `agent_execute` (ruflo's own LLM call, used for agents ruflo runs itself, not for Claude Code's Agent tool) supports providers `anthropic | openrouter | ollama`. Resolution: per-agent `provider`, then `RUFLO_PROVIDER`, then persisted `providers configure`, then key presence. With a non-ollama.com `OLLAMA_BASE_URL` it calls `<base>/v1/chat/completions` with no auth, so any local OpenAI-compatible server works. [primary: mcp-tools/agent-execute-core.ts:166-245, 326-347]
- Gap on this branch: tier aliases `haiku`/`sonnet`/`opus` on the Ollama path resolve to `gpt-oss:120b-cloud`; an explicit tag (`qwen3-coder-cc:latest` or `ollama:<tag>`) passes through. `OLLAMA_DEFAULT_MODEL` does not appear in `src/` at 3.57.0 (grep empty), so the fork fix recorded in our ADR-0025 is not in this checkout. [primary: agent-execute-core.ts:515-523; grep]
- `@claude-flow/providers` also ships an Ollama provider (`ollama-provider.ts`); `ruflo-ruvllm` plugin tools (`ruvllm_*`) are config/chat-template/MicroLoRA/SONA/HNSW helpers on `@ruvector/ruvllm-wasm`, which the CLI does not install. They do not serve or route to a model. [primary: v3/@claude-flow/providers/src; plugins/ruflo-ruvllm/README.md]
- `ruflo-deepseek-harness` is a DeepSeek chat/reason skill with a secret guard, not a subagent model switch. [primary: plugins/ruflo-deepseek-harness/README.md]
- MetaHarness: the catalog lists `@metaharness/router` (cost/perf model routing) as the backend behind the optional neural router. Not read in depth here; treat as unverified beyond the import in `neural-router.ts`. [primary: model-router.ts header; catalog: .planning/ruv/packages/new-libraries-2026-07.md:18]

### Local setup already built (`~/llm`)

- Ruflo-to-local wiring exists and is the main finding: ADR-0025 and `~/llm/.mcp.json` set `RUFLO_PROVIDER=ollama`, `OLLAMA_BASE_URL=http://localhost:11434`, `OLLAMA_DEFAULT_MODEL=qwen3-coder-cc:latest`, blank `OPENROUTER_API_KEY` on the ruflo MCP server. `bin/ruflo-local env|configure|test` verifies it. [local-doc: ~/llm/docs/adr/0025-ruflo-local-models-across-hosts.md; ~/llm/USING_LOCAL_MODELS.md section 4]
- Claude Code itself on a local model: `bin/dsp-local` points Claude Code at Ollama's native Anthropic `/v1/messages` (`ANTHROPIC_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`), no proxy; it strips MCP servers because the harness prompt alone is about 88k tokens. [local-doc: ~/llm/bin/dsp-local header]
- Grok Build local models: `[model.local-*]` tables in `~/llm/.grok/local-models.toml` (keys `local-coder` = Qwen3-Coder-Next :8081, `local-planner` = Qwen3-Next :8082, `local-tiny`, `local-micro`, etc.), installed by `bin/grok-local --install`. [primary: that file]
- Codex: `~/.codex/config.toml` has no local model provider configured (grep found only marketplaces/MCP). [primary]
- weft headless: `providers.local.apiBase` + `agents.defaults.model = local/<repo-id>`, `delegation.claude_enabled:false`, `WEFTOS_RUNTIME_DIR=<empty dir>`. [memory note weft-headless-teammate-setup, verified at 7183d0fa]

### Qwen models on this Mac

Weights live on the model drive (mounted now); `~/.cache/huggingface/hub/models--*` entries are symlinks. `du -L` sizes of the HF directories (may include several revisions, so the `~GB` in ~/llm docs is the better working-set number):

| Model | Present | Quant | Size note | Fit on this Mac | Measured (local-doc) |
|---|---|---|---|---|---|
| Qwen3-Coder-Next 80B-A3B | yes | MLX 4-bit | ~45-55 GB resident (docs); du 84G | yes, 128 GB | SWE-Verified 70.6, ~90 tok/s, no thinking, daily coder |
| Qwen3-Next-80B-A3B Instruct | yes (4-bit and 8-bit), plus Thinking-4bit | MLX | ~55 GB (docs) | yes, not co-resident with Coder-Next | planner, HARD 100s, ~70 tok/s |
| Qwen3-Coder-30B-A3B Instruct | yes | MLX 8-bit (Ollama `qwen3-coder-cc`, 128k ctx) | ~32-38 GB | yes | HARD 100s, SWE 51.6; 4-bit loses tool use (57 vs 100) |
| Qwen3.6-35B-A3B | yes | MLX 4-bit | 38G | yes | not measured by us |
| Qwen3.8-27B | yes | MLX 4-bit | 30G | yes | not measured by us |
| Qwen3.5-4B / Qwen3-4B-Instruct-2507 | yes | MLX 4-bit | 5.7G / 4.2G | yes | Qwen3-4B printed tool names as text about half the time against a 16-tool prompt [memory: cos-local-model-gotchas] |

The aepod-xpc 4070 (12 GB CUDA) cannot hold any of the 30B-80B ones; it fits only 4B-9B class. Not needed here. No GGUF Qwen3-Next was found in the listings I ran, so llama.cpp is not the path; MLX or Ollama is. I did not inspect `ollama list` output (empty at check time, Ollama may be stopped) or the drive's gguf directory beyond a listing that returned nothing for Qwen.

Memory budget: Coder-Next 4-bit (~50 GB) plus the Haiku-class tier is Anthropic-side, so only one big local model is needed. `~/llm` doc rule: one big brain at a time; Coder-Next plus tiny worker plus embed plus STT/TTS is ~50 GB.

### Claude Code subagent model

- The Agent tool `model` parameter accepts `sonnet | opus | haiku | fable` (tool schema in this session). Agent definitions take a `model:` frontmatter field (`developer.md` uses `inherit`). [primary: this session's tool schema; ~/.claude/agents/developer.md:6]
- Environment overrides exist in the binary: `CLAUDE_CODE_SUBAGENT_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_BASE_URL`. [binary: `strings` on claude 2.1.311]. Their precedence and exact semantics I did not verify; check with `claude-code-guide` before relying on them.
- Honest limit: these are process-wide. `ANTHROPIC_BASE_URL` redirects every request in that Claude Code process, including the lead. There is no supported per-subagent "run this one on local Qwen" switch. A subagent on a local model needs the whole session on a local-compatible endpoint (what `dsp-local` does, and then the lead is local too) or a different executor (Ruflo `agent_execute`, `weft`, Grok).

## 2. Recommended routing table

| Task kind | Tier | Model | From a Claude Code lead | From a Ruflo swarm | From a weft / Codex / Grok lane |
|---|---|---|---|---|---|
| Mechanical edit: var-to-const, remove console, add logging | 1 | Codemod (no LLM) | `hooks_codemod` MCP call, or Edit | `hooks_route` then `hooks_codemod` | n/a, use the codemod or `sed` |
| Bulk read/summarise, grep-and-report, format conversion, file listing, doc stubs, commit-message drafts, classification | 2a | Local Qwen3-Coder-Next 4-bit (:8081) | Not as an Agent-tool subagent (see section 4). Run `weft agent -m` or `ruflo agent_execute` from Bash/MCP and read the result | `agent_spawn` with `config.provider="ollama"`, `config.model="<tag>"`, then `agent_execute` | weft: `providers.local.apiBase=http://127.0.0.1:8081/v1`; Grok: `/model local-coder` (already installed). Codex: no local provider configured |
| Scoped search, simple single-file edit, test fix with a clear failure, anything where a wrong answer is cheap to detect | 2b | Haiku | `Agent` with `model:"haiku"` | `hooks_model-route` says haiku; spawn accordingly | Anthropic not available in weft with `claude_enabled:false`; use local |
| Standard implementation, multi-file changes, code review of subagent output | 3 | Sonnet | `Agent` with `model:"sonnet"` (existing default [memory: agent-model-routing]) | `hooks_model-route` says sonnet | n/a |
| Architecture, gnarly debugging, governance/security logic, multi-constraint integration, a cheap-tier attempt that bounced | 4 | Fable/Opus | no override (lead model) or `model:"fable"` | opus per router | n/a |
| Embedding, STT, TTS, tiny classification | local | Qwen3-Embedding-0.6B, Granite-H-Tiny, Llama-3.2-1B | via `~/llm/bin/*` | n/a | n/a |

Escalation rule: run `hooks_model-verify` (or compile/test) on tier 2 output; escalate one tier on a failed check. Do not use local Qwen for anything touching secrets, governance rules, signing or release code, or when the answer is not mechanically checkable.

Which Qwen for tier 2a: the doc-measured candidates are Coder-Next 4-bit (best, but a 50 GB resident model for "simple" work, and it occupies :8081 which Grok and the CoS also use), Coder-30B-A3B 8-bit via always-on Ollama `qwen3-coder-cc` (already the ruflo default, shared, HARD 100s), and Nemotron-9B-OpenCode 8-bit (9.7 GB, tool_hard 7/7). The brief asked about "Qwen3-Next": we have Qwen3-Next-80B-A3B (Instruct/Thinking), but our own measurements rank it as a planner, and Coder-Next as the coder. Recommendation: use Coder-Next for tier 2a when :8081 is already up for another reason, otherwise `qwen3-coder-cc` on Ollama, which needs nothing started. Qwen3-Next-80B stays for planning-class local work, not "simple" work. The sub-9B Qwens are not recommended for tool use.

## 3. Proposed setup (for owner approval; none applied)

1. Already in place for Ruflo-executed agents: the `~/llm/.mcp.json` env block above. To get the same in WeftOS, copy those env keys into the ruflo MCP server entry in `~/weftos/.mcp.json`. Verify with `~/llm/bin/ruflo-local test`. Spawn local agents with an explicit tag, because tier aliases resolve to a cloud name on this ruflo checkout:
   - `agent_spawn` config: `{"provider":"ollama","model":"qwen3-coder-cc:latest"}`
2. Make haiku the default for simple Claude Code subagents globally: put `model: haiku` in the frontmatter of simple agent definitions, and pass `model:"haiku"` on Agent calls for read-only search/summary. Optional process-wide default via `CLAUDE_CODE_SUBAGENT_MODEL=haiku` in `~/.claude/settings.json` `env`. This would override per-call choices if it behaves as its name suggests (unverified), so confirm before adding; it also would cut the sonnet default the owner set on 2026-07-06 for scoped implementation work.
3. weft lane on local Qwen, in the project `.clawft/config.json`: `providers.local.apiBase` = `http://127.0.0.1:8081/v1`, `agents.defaults.model` = `local/mlx-community/Qwen3-Coder-Next-4bit`, `delegation.claude_enabled` = false; run with `WEFTOS_RUNTIME_DIR=<empty dir>`.
4. Start the local server only when a batch of tier-2a work exists: `~/llm/bin/serve mlx-community/Qwen3-Coder-Next-4bit --port 8081 --host 0.0.0.0 --temp 1.0 --top-p 0.95 --top-k 40 --max-tokens 32768 --prompt-cache-bytes 4G` (detached, per the long-running rule). Check `~/llm/bin/monitor` and that nothing else holds :8081 first.
5. Optional: call `hooks_model-route` with `preferCost:true` before fan-out and record outcomes with `hooks_model-outcome` so the bandit learns; the `[CODEMOD_AVAILABLE]` path needs no setup.
6. Codex: no change proposed. Local Codex needs a provider block in `~/.codex/config.toml` that I did not research.

## 4. Not possible or not worth it

- Per-subagent local model from a Claude Code lead: not supported. The Agent tool enum is Anthropic aliases only; `ANTHROPIC_BASE_URL` is process-wide. Pointing it at Ollama/a proxy moves the lead to local too (`dsp-local`), which is a different session, not a cheaper subagent. Agentic-flow's Anthropic-compatible proxy would have the same process-wide shape; I did not clone or read agentic-flow, so that is [experience], not verified.
- `dsp-local` as the daily driver: the harness prompt is ~88k tokens, MCP is stripped, and local tool-use is below frontier. Fine for experiments.
- Ruflo `hooks_model-route` cannot return a local model; it only names haiku/sonnet/opus. Treat its output as advice about the Claude tier, and pick local yourself.
- "Agent Booster" in our CLAUDE.md: the real Tier 1 is three codemods; most "simple" tasks (add types, error handling, async/await) are not covered. Expect tier 1 to apply rarely.
- Qwen3-Next 80B for simple work: wasteful (55 GB resident) and blocks Coder-Next. Sub-9B Qwens: unreliable tool calls against our prompts. Qwen on the 4070: only small models fit.
- Running local while the MLX server is cold: first request loads weights (~45 s for a smaller model); not worth it for one or two small tasks.
- Ruflo 3.57.0 here lacks the fork's `OLLAMA_DEFAULT_MODEL` fix; until upstreamed or the fork is the one running, always pass an explicit model tag. A stale npx cache can also pin an older cli (ADR-0025).
- Walk-away condition for the ruv pieces: if tier-2 routing does not save measurable spend after a week of `hooks_model-stats`, the boring alternative is a static rule in CLAUDE.md ("search/summarise = haiku, implementation = sonnet") with no router state to maintain.

## 5. Proposed paragraph for the global "Cost and session hygiene" rule

Give simple subagents a cheaper model. Read-only search, summaries, file listing, format conversion and single-file mechanical edits go to Haiku (`model: "haiku"` on the Agent call); deterministic edits (var-to-const, remove-console, add-logging) go to the Ruflo codemod (`hooks_codemod`) with no LLM. Bulk or repetitive work that can be checked mechanically may run on the local Qwen tier instead: Qwen3-Coder-Next 4-bit on `:8081` (start only when needed, one big local model at a time) or the always-on Ollama `qwen3-coder-cc`, reached through a Ruflo `agent_execute` agent with `provider: "ollama"` and an explicit model tag, a `weft agent -m` lane on `local/...`, or Grok `/model local-coder`. The Claude Code Agent tool cannot run a subagent on a local model, so do not expect it to. Verify cheap-tier output (tests, compile, or `hooks_model-verify`) and escalate one tier on failure; never use the local tier for secrets, governance, signing or release work. Keep Sonnet for scoped implementation and Fable for escalation.
