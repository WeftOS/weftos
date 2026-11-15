//! WEFT-355 / KG-015: EA-Agent entity alignment for multi-repo dedup.
//!
//! # Design note
//!
//! Entity alignment identifies when two entities in **different** knowledge
//! graphs (typically two repositories or two extraction runs) denote the same
//! real-world concept. This is distinct from single-graph dedup
//! ([`crate::model::KnowledgeGraph::dedup`] / KG-008): alignment produces a
//! **cross-graph mapping** and optional merge proposals that a human or agent
//! can accept or reject.
//!
//! ## EA-Agent multi-step pipeline (Paper 11, arXiv 2604.11686)
//!
//! Inspired by *EA-Agent: A Structured Multi-Step Reasoning Agent for Entity
//! Alignment* (ACL 2026):
//!
//! 1. **Candidate generation** — cheap filters (normalized label similarity +
//!    local or external embedding cosine) shortlist partners in graph B for
//!    each entity in graph A.
//! 2. **Triple / neighborhood selection** — structural evidence is the Jaccard
//!    overlap of neighbor labels (and optional shared relation types),
//!    analogous to the paper's attribute/relation triple selectors that strip
//!    redundant context before scoring.
//! 3. **Score fusion** — weighted blend of name, embedding, and structural
//!    similarities → confidence in `[0, 1]`.
//! 4. **Propose merges** — each surviving pair becomes a [`MergeProposal`] with
//!    status [`ProposalStatus::Pending`] (or auto-accepted above a threshold).
//! 5. **Optional agent refine** — [`AlignmentRefiner`] can adjust confidence /
//!    rationale (LLM dispatch scaffold; default is a pure heuristic refiner).
//! 6. **Manual override** — accept / reject individual proposals, then
//!    [`EaAgent::apply_accepted`] merges B→A for accepted pairs.
//!
//! ## Similarity signals
//!
//! | Signal | Default weight | Implementation |
//! |--------|----------------|----------------|
//! | Name / label | 0.45 | Normalized Levenshtein on snake_case labels |
//! | Embedding | 0.25 | Cosine over provided vectors, else local char-trigram bag |
//! | Structural | 0.30 | Jaccard of neighbor labels |
//!
//! External embeddings are optional via [`EmbeddingLookup`]. When none are
//! supplied, a deterministic local char-trigram embedder keeps the pipeline
//! offline-testable without `clawft-llm` or kernel HNSW.
//!
//! ## Wiring
//!
//! - Public API: [`EaAgent`], [`align_entities`], [`propose_merges`].
//! - Re-exported from the crate root for pipeline / service callers.
//! - LLM agent-core delegation (full chat-agent path) remains a follow-up;
//!   implement [`AlignmentRefiner`] when that lands.
//!
//! ## Non-goals (deferred)
//!
//! - Blocking 1-1 matching (Hungarian / mutual nearest neighbors).
//! - MCP `graphify_align` tool (add when multi-graph workflows hit the MCP
//!   surface).
//! - Live LLM triple filtering (trait-ready; no default network call).

mod agent;
mod score;

pub use agent::{
    AlignmentRefiner, ApplyResult, EaAgent, HeuristicRefiner, MergeProposal, PreferredSide,
    ProposalStatus, RefineDecision, align_entities, align_entities_with_config, propose_merges,
};
pub use score::{
    AlignmentConfig, AlignmentEvidence, AlignmentMethod, EmbeddingLookup, EntityAlignment,
    MapEmbeddingLookup, ScoreBreakdown, cosine_similarity, label_similarity, local_label_embedding,
    normalize_label,
};
