# "AISP Cell" holographic memory claim: evaluation for WeftOS

Date: 2026-10-12. Status: research, no code changes. Verdict: pattern-only, with one small optional experiment (section 5).

## 1. What was claimed

An infographic, "Holographic Memory Test: Delete a fifth of the memory. It finds its way back.", describes "the AISP Cell". Knowledge is stored as a layered mesh (outer membrane = big picture, inner layers = families of ideas, core = foundations). Every piece has a permanent ID made of an address (place in the mesh plus type) and a 32-byte content fingerprint. The test deletes 20% of a store at random, then asks for each missing piece: where it belonged, what it was, what it belonged to, where it came from. A piece counts as recovered only when all four answers are right.

Reported recovery at 20% loss:

| Design | Recovered |
|---|---|
| Full Cell | 97.75% |
| Cell without addresses | 96.00% |
| Flat database handed the Cell's map | 91.11% |
| Trained AI index (KGE), about 5 min training | 41.70% |
| Cell without its layered index | 32.59% |
| Flat database | 6.35% |

Plus "zero wrong answers" and "structure beats size".

## 2. Primary source: not found

I could not find the repo, paper or post that ran this test. State plainly: the source is unidentified.

What I checked (2026-10-12):

- The ruvnet-brain `search_ruvnet` tool: the card router declined the query as ambiguous, so no retrieval ran. Not evidence either way.
- `gh search repos/code/issues/prs` for "AISP", "holographic", "cell memory" across ruvnet and GitHub at large; ruvnet gists filtered on "aisp|holograph".
- A clone of `ruvnet/ruvector` at 31549f7 (2026-10-11): no "AISP" string, no "holographic memory", no "Cell" memory crate. The only "holographic" code is `crates/ruvector-kge` (HolE embeddings, circular correlation) and `docs/research/gnn-v2/14-semantic-holography.md`.
- A clone of `bar181/aisp-open-core` (d8f2f38, 2026-06-09), plus bar181's profile and repo list: no Cell, no memory-recovery test.
- Web searches: nothing matching 97.75% / "AISP Cell".

What "AISP" is, as far as public sources show:

- AISP = "AI Symbolic Protocol" (also "AI Symbolic Programming"), by Bradley Ross (GitHub bar181), repo `bar181/aisp-open-core` v5.1, licence MIT (c) 2026 Bradley Ross. It is a spec language for LLM-to-LLM instructions with a 512-symbol vocabulary, claiming ambiguity under 2%. It is not a memory system. Brad is a founding director of the Agentics Foundation, which is also ruv's circle, so a connection is plausible, but I found no link to ruvnet code. `ruvnet/metaharness` ADR-048/049 use AISP only as a notation for swarm contracts.
- The "KGE" baseline name matches `ruvnet/ruvector` `crates/ruvector-kge` (HolE/RotatE). That is a guess about which KGE was used; unverified.

So the numbers are an unsourced infographic. Treat everything below as an evaluation of the claim as shown, not of code I read. Likely next step if it matters: ask the owner where the image came from (author, repo link).

## 3. What the claim does and does not show

Reading the setup from the image alone:

1. "Recovered" is metadata inference, not content recovery. The four answers are place, type, parent and provenance. The 32-byte fingerprint of a deleted piece is gone with the piece; nothing says its content comes back. This is orphan placement: given the surviving structure, say where the hole was.
2. The structure does the work, and it is explicit redundancy. If children carry a parent address and parents carry child lists, 80% survivors identify most holes by construction. That is the Merkle-tree/git-fsck/doubly-linked-list effect, not a new "holographic" property. The decisive row is "flat database handed the Cell's map" at 91.11%: once the baseline gets the same index it nearly matches. The Cell's own addresses add about 1.75 points (97.75 vs 96.00).
3. The fingerprint is not the source of the gain. Removing addresses costs 1.75 points; the table has no "without fingerprints" row.
4. The baselines are weak by design. A flat table with no links cannot recover placement, so 6.35% is close to guessing; it shows the metric rewards structure, not that the Cell is better than a competent store. The KGE was trained "for about five minutes, then guesses", an under-trained baseline, and KGEs predict links poorly on sparse hierarchies without tuning. No baseline is a plain adjacency/parent index in a normal database, which is the fair comparison and which row 3 suggests would score about 91%.
5. "Zero wrong answers" next to 97.75% means the remaining 2.25% were abstentions or partial answers. That is good behaviour but needs the abstention rate and the definition of "wrong" stated.
6. Unreported: dataset, record count, hierarchy depth and fan-out, random seed and number of trials, confidence intervals, denominators, how queries were formed, whether the Cell's index was rebuilt from survivors only (a leak if built before deletion). Deleting whole subtrees or correlated chunks is not tested; random 20% is the easy case for parent-pointer redundancy.
7. Reproducibility: none from what is public. "Inspired by image diffusion" is a metaphor; nothing in the setup is denoising.

Net: plausible, even expected, for a redundant parent-pointer structure on random loss. Not evidence of a new technique, and the "structure beats size" slogan is fair but is the claim that indexes beat unindexed data.

## 4. Fit for WeftOS

What we already have:

- Addressing: ADR-114 `weftos://<mesh>/<kind>/<id>[/path][?rev=sha256:...]` with kind and id (the "address + type"), and `rev` as a sha256 of the complete artifact bytes (the "fingerprint"). `crates/weftos/src/init/digest.rs` computes sha256 Merkle-style package and team digests (one line per file, sorted, team digest also covers member package digests), so a hierarchical content digest exists for packages.
- Brain entries (`agents/schemas/brain-metadata.schema.json`, ADR-121 decision 9): kind, summary, why, where[] (repo paths, card ids, weftos:// names), installation, package (sha256 of the writing agent), author, instance, card, supersedes[], scope. That already records type, provenance and some links. There is no explicit parent/children or layer field and no per-entry content digest.
- Index: `crates/clawft-core/src/embeddings/hnsw_store.rs` is a flat HNSW (`instant-distance`) with brute-force under 32 entries, JSON persistence, id + embedding + metadata. No hierarchy.
- Branching: `crates/clawft-cow-memory` (agenticow port over `rvf_runtime::RvfStore`) gives checkpoint, rollback, promote, lineage and diff. Lineage is a kind of provenance chain.

What adopting the layered cell would mean concretely:

- Schema: add to each entry `layer` (membrane/family/core, or simply depth), `parent` (an entry id or weftos:// name) and `digest` (sha256 of the entry bytes). Parents also carry a child-digest list (a Merkle node). Optional: `type` is already `kind`.
- Index: a tree/DAG index beside HNSW (parent -> children, plus a map from digest to entry). HNSW stays for semantic search; the tree is the repair map.
- Recovery procedure: on open or sync, walk the tree; a parent listing a child digest that is not present is a known hole; its address, type and sibling set are read off the parent, its provenance from the siblings and `where`/`card`. Content comes back only from a replica or archive by digest (the mesh sync and the primary node), never from the survivors.

Cost: one extra field set per entry, a rebuildable tree index, write-path maintenance of parent child-lists (a hot parent becomes a write hotspot and a conflict point under mesh sync), a schema bump (`weftos.brain-metadata/2`) and migration of existing brains. About a few days of work; low risk because the tree is derivable and can be rebuilt.

What we would gain: detection and placement of missing or corrupted entries (an fsck for a brain), explicit evidence of what is missing rather than silent absence, stronger provenance chains. What we would not gain: content resilience (that is replication/backup, which the mesh and primary node already own), or better retrieval (HNSW does that).

Do we have the failure this solves? Real brain loss in WeftOS so far has been poisoned or corrupted content and store splits (see handoff notes), not random 20% record loss; random loss of that kind would be a disk or sync fault where we would restore from replicas first. The value is therefore mostly integrity and audit, which our sha256 pins and the chain already partly cover.

## 5. Verdict: pattern-only (prototype only if the experiment is cheap)

Deciding facts: no source or code exists to adopt; the headline result is explained by index redundancy (a flat DB given the map scores 91%); the metric measures placement metadata, not content recovery; the baselines are weak; we already hold address + type + sha256 in spirit. Adopt the idea, not the artifact: a `parent` link and a per-entry `digest` in the next brain-metadata revision, if and when an integrity/fsck feature is wanted. Do not take a dependency on "AISP Cell".

Walk-away condition: if the owner can point to a real repo with the harness and the dataset and the 91% baseline row survives, nothing changes (the claim then only confirms the index effect); only an honest result where the Cell beats a plain parent index would change the verdict.

Strongest non-Cell alternative: ordinary parent pointers plus content-addressed digests (git/Merkle style) with replication; this is what the table's third row effectively is.

### Small experiment (about half a day, on our own data)

Goal: measure how much of the "recovery" comes from structure we can add cheaply.

1. Take a brain export (any installation's RVF/ruvector entries with `where`, `card`, `supersedes`, `kind`). Build a tree from existing fields: installation -> kind -> card/where-path prefix -> entry.
2. Delete 20% of entries at random, 5 seeds; also one run deleting whole `where` subtrees.
3. Recover for each deleted entry its kind, parent, and provenance (author/package) using three methods, built from survivors only:
   a. nearest neighbours on the existing HNSW embeddings with majority vote (our flat baseline);
   b. the same plus an explicit parent/children index (the "map");
   c. b plus per-entry digests.
4. Report exact-match rate on all fields, abstention rate, wrong-answer rate, with confidence intervals and the dataset size. Also report content recovery separately (expected 0% without a replica).
5. Decision rule: if b is within about 3 points of c, and both far above a, record "parent index is the whole story", add `parent` to the next schema revision only if an integrity feature is scheduled, and stop. If a is already above 90%, drop the idea.

## Sources

- Infographic as supplied by the owner (image, undated), saved at `/private/tmp/claude-501/-Users-mathewbeane-weftos/88b8ff08-5afc-4018-a326-11dcdb5d6d1e/scratchpad/clip2.png`.
- https://github.com/bar181/aisp-open-core (AISP v5.1, MIT, last commit 2026-06-09), https://github.com/bar181/bar181 (profile). Read 2026-10-12.
- https://github.com/ruvnet/ruvector at 31549f7 (2026-10-11): `crates/ruvector-kge/src/lib.rs`, `npm/packages/kge/docs/adr/`, `docs/research/gnn-v2/14-semantic-holography.md`. Read 2026-10-12.
- https://github.com/ruvnet/metaharness `docs/adrs/ADR-048-formal-proof-swarm-ruvnet-algo-stack.md` (AISP as swarm-contract notation), via GitHub code search 2026-10-12; not read in full.
- Local: `docs/adr/adr-114-weftos-uri-naming.md`, `agents/schemas/brain-metadata.schema.json`, `crates/weftos/src/init/digest.rs`, `crates/clawft-core/src/embeddings/hnsw_store.rs`, `crates/clawft-cow-memory/src/lib.rs`.
