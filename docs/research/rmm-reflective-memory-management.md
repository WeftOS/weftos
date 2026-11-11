# Reflective Memory Management (RMM) — Google Cloud AI Research → WeftOS

**Status:** Research captured 2026-09-11. **Updated 2026-10-04: Phase 1 (retrospective, WEFT-732) and Phase 2 (prospective, WEFT-733) implemented**, both opt-in (`agents.memory_recall`, `agents.memory_consolidation`) — see §5 "Phase 1 as built" and "Phase 2 as built". Phase 3 (Gumbel, promote gate) not started. Board ticket **WEFT-732** → **WEFT-733**, cycle 0.8.x, `ws06-memory`.
**Date:** 2026-09-11
**Paper:** Tan, Yan, Hsu, Han, Wang, Le, Song, Chen, Palangi, Lee, Iyer, Chen, Liu, Lee, Pfister —
*In Prospect and Retrospect: Reflective Memory Management for Long-term Personalized Dialogue Agents*,
ACL 2025. arXiv:2503.08026v2 (28 Jul 2025). https://arxiv.org/abs/2503.08026
**Companion papers:**
- ReasoningBank (same Google Cloud AI group) — arXiv:2509.25140, ICLR 2026 —
  distill *strategies* from success/fail traces (not which memories were used)
- LongMemEval — arXiv:2410.10813, ICLR 2025 — the eval RMM reports on
**Audience:** ws06-memory, HybridRouter / SONA, AgentDB / RVF, agent loop context assembly
**Relates to:** ADR-058 (session RVF + HNSW context tier), ADR-059 (Qwen3 embedder + reranker),
ADR-096 (no silent champion swap), WEFT-46 (Sona skill rerank), WEFT-347 (MemoryConsolidator),
**WEFT-732** / **WEFT-733** (this landing),
`crates/clawft-core/src/agent/{memory,context}.rs`,
`crates/clawft-core/src/agent/context_router/sona_rerank.rs`

---

## 0. TL;DR

1. **RMM is two loops around a frozen retriever.** Prospective reflection
   organizes memory into *topic units* (merge-or-insert, not turn/session
   cuts). Retrospective reflection trains a **tiny linear reranker** from
   the generator’s own citations (`+1` cited, `-1` retrieved-but-ignored)
   with Gumbel noise + REINFORCE. The dense retriever is **not** fine-tuned.

2. **That is the hole in our long-term memory path.** `ContextBuilder`
   still dumps the entire `MEMORY.md` into `# Relevant Memory:`
   (`crates/clawft-core/src/agent/context.rs`). `MemoryStore` is append-only
   markdown. `SonaSkillReranker` already has MicroLoRA + ReasoningBank, but
   it reranks *skills*, `observe()` takes one scalar, and that path is **not
   wired into AgentLoop**. `NoopLearner` discards trajectories.

3. **Do not RL-update the retriever.** Paper ablation (Contriever,
   Gemini-1.5-Flash): Retrospective Reflection *without* the reranker
   collapses LongMemEval Acc **58.8 → 31.0**. That is the failure mode
   SONA’s day-0 fail-open already encodes for skills. Keep it for memory.

4. **Tweet numbers vs paper numbers.** Table 1 with GTE is real:
   **69.8% Recall@5, 70.4% Acc** on LongMemEval vs 62.4 / 63.6 RAG.
   “100% quality improvement rate” in the paper means *every LongMemEval
   question is designed to need memory*, not a 100% lift. The 43% / 37% /
   28% “production” figures circulating with the paper are **not in the
   paper**.

5. **Land on existing crates, do not add a memory product.** Frozen
   first-stage = `VectorStore` / HNSW / RVF. Reranker = generalize
   `SonaSkillReranker`. Attribution = citation ids on generate.
   Distillation = ReasoningBank / AgentDB reflexion (already planned).
   Reranker-weight promote = MetaHarness flywheel with `confirm=true`
   (ADR-096). `MEMORY.md` dump is the fail-open fallback.

6. **Plane (filed with this note, cycle 0.8.x, ws06-memory):**
   - [WEFT-732](docs/plans/weft-rmm-retrospective.md) — retrospective
     (retrieve Top-K → citation-rerank Top-M, stop the dump)
   - [WEFT-733](docs/plans/weft-rmm-prospective.md) — prospective
     (session-end topic extract + merge/insert), blocked-by WEFT-732

---

## 1. How this entered the tree

Captured from [@marfinxx](https://x.com/marfinxx/status/2098383477385150860)
(2026-09-11) quoting the earlier [trace-engineering thread](https://x.com/marfinxx/status/2094016175617241109)
(2026-08-30). The screenshot is ACL 2025 RMM, not a new 2026 paper.

The two threads compose and should stay paired:

| Thread | Object | Job |
|--------|--------|-----|
| Trace engineering + ReasoningBank | execution DAG / distilled strategies | *how* to act next time |
| RMM | topic bank + citation-attributed retrieval | *which* memories actually drove the answer |

Both papers say the same anti-pattern: **never pass raw traces into working
context.** Distill first.

---

## 2. What the paper actually does

### 2.1 Problem

Long-term personalized dialogue fails in two structural ways:

1. **Fixed granularity.** Turn / session / time-interval cuts do not match
   topic shifts, so facts fragment across entries and retrieval returns
   incomplete or contradictory slices.
2. **Fixed retriever.** Off-the-shelf dense retrieval cannot adapt to a
   user’s interaction pattern, and labeled query–memory pairs are too
   expensive to collect per user.

### 2.2 Algorithm (paper Algorithm 1)

```
Retrieve:   M_K ← frozen retriever fθ(q, B)          # Top-K from memory bank
Rerank:     M_M ← gϕ(q, M_K)                         # lightweight linear + Gumbel
Generate:   a, R_M ← LLM(q, S, M_M)                  # response + per-memory citations
Update:     gϕ ← REINFORCE(gϕ, R_M)                  # +1 cited / −1 ignored
On session end:
            extract topic snippets from S
            for each snippet: merge into B or insert
```

Memory bank entry = `(topic summary as search key, raw dialogue segment)`.

### 2.3 Prospective reflection (topic organization)

After each session:

1. LLM extracts topic-aligned snippets + summaries (not turn cuts).
2. For each new snippet, retrieve Top-K nearest existing entries.
3. LLM decides **merge** (same topic / updated fact) vs **insert** (new topic).

This is the knowledge-update path (preference reversed, allergy changed).
Append-only `MEMORY.md` cannot do it without leaving contradictory
paragraphs in the dump.

Paper granularity study (100 LongMemEval samples, GTE): turn and session
are both mediocre; mixing them is worse (noise); oracle-per-instance best
granularity is the ceiling; **PR approaches that ceiling** without an
oracle.

### 2.4 Retrospective reflection (citation → reranker)

Reranker (not the retriever):

```
q'  = q  + Wq q
m'i = mi + Wm mi
si  = q' · m'i
s̃i  = si + Gumbel(ui)          # exploration
pi  = softmax(s̃i / τ)
```

Sample / take Top-M. Generator is prompted to **cite memory ids in the
same call** as the answer (citations conditioned on the response beat
pre- or post-hoc citation). Reward: `+1` if cited, `-1` if retrieved but
not cited. Update `ϕ` with REINFORCE; baseline `b` is a hyperparameter.

Citation-as-reward vs Gemini-1.5-Pro judge on LongMemEval: overall F1
**86.7%** (useful 90.2, not-useful 85.9). Good enough as an unsupervised
signal. Not a second LLM-as-judge on the hot path.

### 2.5 What the paper measured (Table 1, 3-run average)

| Method | Retriever | MSC METEOR | LongMemEval R@5 | LongMemEval Acc |
|--------|-----------|------------|-----------------|-----------------|
| No history | — | 5.2 | — | 0.0 |
| Long context | — | 14.8 | — | 57.4 |
| RAG | GTE | 27.5 | 62.4 | 63.6 |
| MemoryBank | specific | 20.1 | 58.6 | 59.6 |
| LD-Agent | specific | 25.4 | 56.8 | 59.2 |
| **RMM** | **GTE** | **33.4** | **69.8** | **70.4** |
| RAG oracle | oracle turns | — | 100.0 | 90.2 |

Ablation (Contriever): RAG 58.8 Acc → +PR 59.6 → +RR w/o reranker **31.0**
→ +RR with reranker 60.2 → full RMM 61.2.

Defaults: no reranker ⇒ Top-K=5; with reranker ⇒ Top-K=20, Top-M=5.
Raising to K=50 / M=10 (GTE) → R@10 74.4, Acc 73.8.

Generator note: Gemini-1.5-Flash beat Gemini-1.5-Pro *inside RMM* (stronger
models abstain more on personal facts). Do not assume “bigger generator =
better memory use.”

### 2.6 Paper limitations (their §Limitations)

- REINFORCE on the reranker is extra compute (they still argue it is
  cheaper than retriever fine-tune).
- Text-only.
- Merge/insert may need more work on very long evolving histories.
- Privacy: the bank is personal data; they flag DP / consent, they do not
  ship a control.

---

## 3. What circulating commentary added (do not treat as paper results)

The 2026-09-11 write-up around the paper claimed production lifts
(irrelevant-context −43%, historical hallucination −37%, long-horizon
completion +28%) and a “holy grail of unsupervised persistent memory.”
Those numbers are **not in arXiv:2503.08026**. Keep them out of tickets
and ADRs. The paper is unsupervised *citation rewards on a reranker*,
not a generally unsupervised memory AGI.

“100% Quality Improvement Rate” = on LongMemEval, memory improved the
answer in 100% of questions (the benchmark is built that way). On MSC
the same statistic is 86%.

---

## 4. WeftOS as-is (verified 2026-09-11)

| Piece | Today | RMM needs |
|-------|-------|-----------|
| `MemoryStore` | `MEMORY.md` + `HISTORY.md`, append/overwrite | Topic nodes with stable ids; merge vs insert |
| `ContextBuilder` | Injects **entire** `MEMORY.md` as `# Relevant Memory:` | Retrieve Top-K, inject Top-M after rerank, each with `m_i` id |
| `memory_tool` | `memory_read` / `memory_write` against the markdown file; optional `VectorStore` + `HashEmbedder` | Same tools, but read path must not imply “dump the file into context” |
| `VectorStore` (`vector-memory`) | Brute-force cosine, in-memory | Frozen first-stage retriever (keep; do not RL) |
| `SonaSkillReranker` (WEFT-46, `hybrid-rerank`) | MicroLoRA residual + ReasoningBank **skill** boost; day-0 fail-open | Same engine, **memory** candidates; Gumbel on logits |
| `observe(query, skill, quality)` | One scalar; **not wired into AgentLoop** | Per-candidate `(memory_id, ±1)` from citations |
| ReasoningBank / AgentDB reflexion | Pattern traces / episodes with critique | Complementary distillation (ReasoningBank paper), not a substitute for attribution |
| `NoopLearner` | Discards `Trajectory` | Closed loop lives here or in SONA `observe` |
| WEFT-347 MemoryConsolidator | Done — periodic distill ConversationStore → `MEMORY.md` | Prospective should *replace* “append more markdown” with topic merge |
| ADR-058 L2 graft | Session-scoped HNSW over ExoChain; v1 granularity = turn / tool output | Prospective is the “semantic re-chunking is additive” follow-on ADR-058 already reserved |
| ADR-059 | `Qwen3-Reranker-0.6B` named as rerank model | RMM’s linear `Wq`/`Wm` is the *online* adapter; do not silently replace the 0.6B cross-encoder. SONA MicroLoRA is the closer analogue |

The smoking gun:

```text
crates/clawft-core/src/agent/context.rs  (build_messages)
  match self.memory.read_long_term().await {
      Ok(memory) if !memory.trim().is_empty() => {
          // entire file → system prompt, labeled "Relevant Memory"
      }
  }
```

WEFT-665 already showed why this is dangerous: graft debris in `MEMORY.md`
was reloaded every turn and the model “recalled” empty blocks.

---

## 5. Landing plan (reuse, do not invent)

```
session end
  → topic extract + merge/insert into AgentDB / RVF nodes     (prospective)
turn
  → frozen HNSW / VectorStore Top-K=20
  → Sona memory reranker Top-M=5  (Gumbel explore, MicroLoRA residual)
  → generator emits answer + memory-id citations
  → cited +1 / ignored −1 → sona.observe per candidate        (retrospective)
  → ReasoningBank distill (success/fail); never raw traces in context
```

### Phase 1 — retrospective (ship first)

1. Stop dumping `MEMORY.md`. If `vector-memory` / HNSW is on: retrieve
   Top-K, inject Top-M with stable ids (`m1`…`mM`). If retrieval is off
   or the reranker is untrained: **fail-open** to the current dump (same
   contract as SONA day-0).
2. Prompt the generator to cite those ids in the same completion.
3. Generalize `SonaSkillReranker::observe` from `(query, skill, quality)`
   to a list of `(id, ±1)`. Wire it from AgentLoop. Do **not** wait for
   Gumbel; binary citation rewards into the existing observe path is the
   first closed loop.
4. Keep the retriever frozen. Feature-gate (`hybrid-rerank` or a sibling
   `memory-rerank`) so default `weft` does not take SONA as a hard dep
   (ADR-096 removable-harness rule).

### Phase 1 as built (2026-10-04, WEFT-732)

- `crates/clawft-core/src/agent/memory_recall.rs`: `MemoryRecall` (select → render → attribute),
  `MemoryRetriever` (frozen; `OnnxRetriever` = nomic-embed-text-v1.5 cosine under `onnx-embeddings`, and an explicit opt-in `LexicalRetriever` = SimHash cosine that logs a warning; with no model and lexical not requested, recall is off, an error is logged and the full `MEMORY.md` is injected; since 2026-11-11 there is no silent hash fallback),
  `MemoryReranker` (`IdentityMemoryReranker` default; `SonaSkillReranker` implements it under
  `hybrid-rerank`, scoring snippets by text, `+1`→quality 1.0 / `-1`→0.0 per candidate).
- `ContextBuilder::build_messages_with_query` injects at most `top_m` (default 5) of `top_k`
  (default 20) snippets as `[m1]`…, with the citation instruction in the same block. With recall
  off, no query, or nothing retrieved: the full `MEMORY.md` dump (fail-open).
- `AgentLoop` attributes the reply per session: cited `+1`, retrieved-not-cited `-1`, never
  retrieved untouched; `[mN]` markers are removed from the user-facing reply (the sink keeps the
  reply as written). The retriever is never updated.
- Config: `agents.memory_recall { enabled = false, top_k = 20, top_m = 5 }` — off by default, so
  no behaviour change until an operator opts in (ADR-096).
- Interpretation of "day-0 fail-open": an untrained reranker keeps the retriever's order (WEFT-46
  contract); retrieval off / empty keeps the dump. Snippets are blank-line paragraphs of
  `MEMORY.md` (topic nodes arrive with Phase 2).

### Phase 2 — prospective (blocked by Phase 1 ids)

1. Session-end hook (post M3 store collapse this is ConversationSink /
   chain, **not** retired `SessionManager`).
2. Extract topic snippets; nearest-neighbor against the bank; merge or
   insert. This is where WEFT-347’s consolidator should grow, not a
   second consolidator.
3. Knowledge-update tests: a later fact must replace, not sit beside,
   the earlier paragraph.

### Phase 2 as built (2026-10-04, WEFT-733)

- `crates/clawft-core/src/agent/learning/topics.rs`: the topic bank, a managed
  `<!-- topics -->` section of `MEMORY.md` with one node per topic (`<!-- topic:key=… -->` then
  `- fact _(from conv)_`). Facts are split into clauses, keyed deterministically by
  `topic_key` (negation and filler dropped; single-valued attributes such as where you live,
  your name or what you prefer key on the attribute alone), then merged (same key: the newer
  fact replaces the text) or inserted (new key). Same transcript twice changes nothing.
- WEFT-347's consolidator grows a `topic_merge` mode instead of a second consolidator; the
  store's existing sanitizing applies to every write.
- `AgentLoop` runs the consolidator after each turn on the conversation sink when its cadence
  is due (`agents.memory_consolidation { enabled = false, every_k_turns = 10, topic_merge = true }`).
  Before this, nothing ran the consolidator in production.
- Retrieval sees one snippet per node; comment lines are stripped from snippets.
- Deterministic keying is the "equivalent" the ticket allows; an LLM summarizer can replace
  `topic_key` behind the same shape. Known limit: two phrasings of one single-valued fact that
  share no attribute cue (for example "home is Austin" vs "I live in Denver") still insert two
  nodes.

### Phase 3 — exploration + promote gate

1. Gumbel noise on rerank logits so new topic nodes get sampled.
2. Reranker weights are a **champion swap**: MetaHarness evaluate →
   receipt → `confirm=true` (ADR-096). No silent flywheel promote.
   The paper’s retriever-collapse ablation is the incident we are
   preventing.

### Explicit non-goals

- Fine-tuning the dense retriever / embedder with these ±1 rewards.
- A new memory crate or a parallel `MEMORY.md`.
- Passing raw traces, WAL, or OTel spans into the working prompt.
- Treating Qwen3-Reranker-0.6B and the SONA linear adapter as the same
  object. Offline cross-encoder (ADR-059) and online citation-RL
  (this note) can stack; they are not substitutes.
- Shipping tweet production percentages as WeftOS metrics.

---

## 6. Mapping onto rUv / AgentDB (grounded)

AgentDB already documents Reflexion episodes, skill-library
auto-consolidate, causal edges, explainable/causal recall, and a nightly
learner (`agentdb/docs/guides/FRONTIER_MEMORY_GUIDE.md`). Those are the
**store and the critique**. What WeftOS does not wire is **generation
attribution as the retrieval reward**.

ReasoningBank (arXiv:2509.25140, `ruvector-sona`) distills *strategies*
(`title / description / content`) from self-judged success and failure.
RMM distills *which episodic nodes to surface*. Both belong; neither
replaces the other.

SONA day-0 fail-open (empty ReasoningBank **and** MicroLoRA residual ≈ 0
⇒ preserve primary order) is the production form of the paper’s “do not
fine-tune the retriever on sparse rewards.”

---

## 7. Annotation index (so this is findable)

Canonical note: this file.

| Location | What to look for |
|----------|------------------|
| Board `ws06-memory` | **WEFT-732** retrospective (Phase 1 built 2026-10-04), **WEFT-733** prospective (Phase 2 built 2026-10-04). Specs in `docs/plans/weft-rmm-*.md` |
| `crates/clawft-core/src/agent/memory_recall.rs` | WEFT-732 Phase 1: retrieve / rerank / cite / attribute |
| `crates/clawft-core/src/agent/learning/topics.rs` | WEFT-733 Phase 2: topic keys, merge-or-insert bank |
| ADR-058 | Update 2026-09-11 — RMM is the reserved “semantic re-chunking” step; do not dump L3 into L1 |
| ADR-096 | Reranker weights = flywheel champion, `confirm=true` |
| `docs/brain/05-rvf-brain-and-research.md` | Research stream **RMM / citation-attributed memory** |
| `docs/brain/04-bugs-gaps-and-current-state.md` | Gap: MEMORY.md dump |
| `docs/guides/routing.md` | Learning-backend checklist + NoopLearner |
| `docs/guides/rvf.md` | SONA row |
| `docs/research/rvf-context-router.md` | Companion pointer |
| `docs/research/ruv-ecosystem-synergy-flywheel.md` | Memory / patterns table |
| `docs/plans/wave-0i-WEFT-46-result.md` | Follow-on: observe → memory citations |
| `crates/clawft-core/src/agent/memory.rs` | Module pointer |
| `crates/clawft-core/src/agent/context.rs` | Dump site |
| `crates/clawft-core/src/agent/context_router/sona_rerank.rs` | observe generalization |
| `crates/clawft-core/src/vector_store.rs` | Frozen first-stage |
| `crates/clawft-tools/src/memory_tool.rs` | Tool path vs context dump |
| Ruflo memory | `weftos/research` key `rmm-reflective-memory-management`; `patterns` key `pattern-rmm-citation-rerank-memory` |

---

## 8. Suggested tests (when implementation starts)

Phase 1:

- Untrained reranker preserves retrieval order (day-0 fail-open).
- `vector-memory` off still dumps `MEMORY.md` (compat).
- `vector-memory` on injects at most Top-M snippets, each tagged `m_i`.
- Cited ids get `+1`, retrieved-uncited get `-1`, never-retrieved get no update.
- Poisoned `MEMORY.md` (WEFT-665 class) is not re-injected in full when
  retrieval is on.

Phase 2:

- Merge: later “user is not allergic to penicillin” supersedes the earlier
  allergy node rather than sitting beside it.
- Insert: a genuinely new topic does not smash an unrelated node.
- Session-end is idempotent on the same transcript.

Do **not** claim LongMemEval 70.4% as a WeftOS gate. That number is Gemini-1.5
+ GTE on their bank. Our gate is: dump gone, citations flow, fail-open holds,
merge works.

---

## 9. Citation

```bibtex
@inproceedings{tan2025rmm,
  title={In Prospect and Retrospect: Reflective Memory Management for Long-term Personalized Dialogue Agents},
  author={Tan, Zhen and Yan, Jun and Hsu, I-Hung and Han, Rujun and Wang, Zifeng
          and Le, Long T. and Song, Yiwen and Chen, Yanfei and Palangi, Hamid
          and Lee, George and Iyer, Anand and Chen, Tianlong and Liu, Huan
          and Lee, Chen-Yu and Pfister, Tomas},
  booktitle={ACL},
  year={2025},
  eprint={2503.08026},
  archivePrefix={arXiv}
}
```

ReasoningBank companion: Ouyang et al., arXiv:2509.25140, ICLR 2026.
LongMemEval: Wu et al., arXiv:2410.10813, ICLR 2025.
