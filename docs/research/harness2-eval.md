# Eval: google-research/harness2 (Harness^2), 2026-10-18

Source: a local clone of the repo (read only, nothing executed), plus GitHub API and the arXiv abstract page (https://arxiv.org/abs/2610.14224).

## Verdict

BORROW THE PATTERN, do not adopt the code. The Propose-Probe-Compose loop, the per-domain experience library, and the "no ground truth, use contrastive execution evidence" idea map onto WeftOS's existing MetaHarness/flywheel plan (ADR-096 draft). The code is a Python benchmark-sweep harness that drives OpenCode/Codex over three public benchmarks; it is not an embeddable runtime and is Linux plus Python 3.12 only. Watch for a `--bench local` implementation (issue #5).

## What it is

- Framework for "recursive agent harnessing" with model weights frozen (README.md). A harness = system prompt, guardrails, memory, tool guidance, subagents, skills, scripts, plugins (the eight editable components, README "Harness components"; encoded as `SurfaceSpec` in harness2/core/tree.py).
- Task-level recursion (harness2/core/recursion.py): k reference executions of the base harness, then an editor proposes invariant (general) and domain-specific edits, each candidate is probed by executing the same task, and a composer builds the refined harness from behavioural contrasts between executions. Parallel mode = k general + k domain candidates then k compositions; sequential mode = k chained steps. Invalid proposals get one retry, then inherit the current harness (recursion.py docstring, `STAGE_ATTEMPTS = 2`).
- Selection (core/selection.py): picks among anonymised final outputs (id_map, shuffled), then a reflector writes a structured experience (cue / lesson / evidence; `has_experience` in core/library.py).
- Harnessing-level recursion (core/library.py): per-domain experience library on disk, `INDEX.md` plus the full adopted harness tree plus `EXPERIENCE.md` per task. Later tasks in a domain read only earlier entries. Whole tree is carried, never a summary.
- Seams: `BenchSpec` / `BenchPrompts` (core/spec.py), `Substrate` protocol (substrate/base.py: surface(), materialise(), launch). A guard in `BenchPrompts.__post_init__` rejects prompt text that mentions benchmark, grader, rubric or score, so improvers are told about the work, not the measurement.
- Path hygiene in tree.py: `normalize_rel` rejects absolute paths and `..`; name regexes constrain skills/subagents/scripts; reserved script names.

## Evidence / numbers

- Paper (abstract, arXiv 2610.14224): one step improves GLM-5 and Gemini 3.5 Flash over base harnesses; gains up to 11.5 points WorkBuddy-Bench, 4.5 JobBench, 11.8 percentage points LAB partial pass rate; "further gains" with larger budget. Four model-harness configurations, three benchmarks (JobBench, LAB legal, WorkBuddy office/web/code). Per-config tables not read; not stated here.
- No ground-truth verifier is used during refinement (paper abstract, README). Judging is separate (`harness2-judge`).
- Caveats from the source itself: executor network access is unrestricted and Codex runs with sandbox disabled; docs/EXPERIMENTS.md reports that a Codex rollout on JobBench downloaded its own task rubric unprompted in validation. Benchmarks are public, so contamination is possible unless egress is blocked. dependencies/PINNED.md says historical paper runs may have used different tasks, deployments and local tooling than this release, so the repo is not a bit-exact reproduction.
- Test coverage: one file, tests/test_workflow.py, 229 lines. Demo `harness2-demo` runs deterministic editors with real Python executions and no credentials.
- Open issues (51) include report undercounting spend after interrupted runs (#50), stale grades after failed re-judge (#44), traceback on bad args (#38), proposals for `--bench local` (#5), `--max-usd` spend limit (#4), opt-in human feedback (#2). There is no spend cap today.

## License and maturity

- Code: Apache-2.0 (LICENSE; GitHub API spdx Apache-2.0). No non-commercial terms on the repo code. README disclaimer: "not an officially supported Google product", excluded from Google VRP.
- Bundled third_party runners and patches keep upstream licenses (harvey-labs, job-bench-eval, workbuddy-bench, opencode; third_party/licenses/). Benchmarks and datasets retain their own terms; I did not audit those individually, so treat them as "not checked".
- Created 2026-10-06, last push 2026-10-14, 3 commits total (initial release 2026-10-08, "updates", arXiv link fix 2026-10-13). 4,431 stars, 395 forks, 2 contributors, 0 releases, version 0.1.0. Very new, single code drop, not yet a maintained project.
- Stack: Python 3.12-3.13, zero runtime dependencies in pyproject (optional openpyxl/python-pptx for rendering, litellm/fastapi bridge). Shells out to Node 20+, Bun 1.3.11 (OpenCode), Codex CLI, gcloud/Vertex AI, Docker (WorkBuddy), Pandoc/Poppler (LAB). About 10.8k lines under harness2/core, substrate, benchmarks.

## Fit for WeftOS (inference, not source)

What WeftOS has: ADR-096 draft "MetaHarness as foundational evolution layer" and docs/research/metaharness-foundation.md (freeze model, evolve harness, evaluate -> receipt -> signed promote via @metaharness/flywheel, darwin mutation surfaces, router). ADR-017 GEPA prompt evolution. ADR-045 tiered router permissions (in-tree routing); note that in this checkout adr-026 is the QUIC transport ADR, so the "ADR-026 model routing" name in CLAUDE.md is the 3-tier routing described there, not a file. ADR-097 data governance.

Overlap and difference:
- Darwin/flywheel in ruv's stack evolve a harness against a scored fitness signal. Harness^2 explicitly targets the case where there is no verifier at edit time and uses contrastive execution evidence instead. That is complementary: WeftOS agent work (CoS, mesh tasks, agent-git-flow lanes) mostly lacks ground truth.
- The editable-surface list (system prompt, guardrails, memory, tool guidance, subagents, skills, scripts, plugins) is a cleaner mutation-surface taxonomy than ADR-096's 7 darwin surfaces to compare against; worth diffing, not copying.

Pieces worth taking and where they land:
1. Propose-Probe-Compose as a flywheel candidate generator: probe = re-run the same task (or a replay) under the candidate; compose = merge by contrast. Lands in ADR-096 flywheel stage, as a candidate producer; promote still goes through the signed receipt and `confirm=true` gate.
2. Per-domain experience library with cue/lesson/evidence triples, full adopted tree retained, entries append-only. Lands next to the CoS memory and agent-git-flow lane notes; store entries in RVF/AgentDB namespaces per domain, with evidence required (reject entries without evidence, as `has_experience` does).
3. Anti-leak prompt guard (improver prompts may not mention scoring). Cheap to copy into any WeftOS improver or GEPA prompt builder.
4. Invariant vs domain-specific edit split, and "invalid proposal gets one retry then inherits current harness" fail-safe. Matches the existing never-silent-promote rule.
5. `tree.py` validators (path-escape rejection, name regexes, reserved names, size caps on always-on files) as a model for validating agent-proposed edits before they touch disk.
6. Lessons from its weaknesses: no spend cap (#4), no network egress control by default, anonymised-output selection by an LLM only. WeftOS should require cost ceilings and ExoChain-logged egress for any self-modifying loop.

Not worth taking: benchmark adapters, OpenCode/Codex install modes, Vertex/gcloud plumbing, Python runtime. Nothing here runs on the Rust kernel or in the browser/WASM path.

## Proposed next step

1. Add a short "Prior art: Harness^2" subsection to docs/research/metaharness-foundation.md and cite it from ADR-096 (docs only; owner should decide, I edited nothing).
2. Spike (about one day, offline): read harness2/core/prompts.py (1,163 lines, not read in full here) and extract the editor/composer/reflector prompt skeletons, then run them through the existing `metaharness_gepa` / flywheel on one WeftOS task type with a small `--max-usd` equivalent.
3. Revisit in about 4 weeks: if `--bench local` (issue #5) lands, try it on a folder of agent-git-flow tasks; if the repo stays at 3 commits, treat the paper as the only durable artifact.

Unread / not established: full prompts.py, schedule.py, context.py internals; paper body tables; third-party benchmark licenses; whether results replicate.
