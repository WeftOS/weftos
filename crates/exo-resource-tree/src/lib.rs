//! Exo-Resource-Tree: hierarchical resource namespace for WeftOS.
//!
//! Provides a tree-structured resource namespace with:
//! - CRUD operations on typed resource nodes
//! - Merkle hash integrity with **incremental** path recompute
//!   ([`ResourceTree::recompute_path`], WEFT-145) — O(depth) per mutation
//! - Full-tree [`ResourceTree::recompute_all`] retained for bootstrap/checkpoints
//! - DAG-backed mutation log for audit trail
//! - Bootstrap from checkpoint or fresh namespace
//!
//! # K0 Scope
//! Tree CRUD, Merkle, mutation log, bootstrap.
//!
//! # K1 Scope (WEFT-130 scaffold + WEFT-554 Did / delegation foundation)
//! Permission engine ([`CapabilityChecker`]), DID principals ([`Did`]),
//! [`DelegationCert`] lifecycle (WEFT-131), local consent evaluate.
//!
//! ## Crate Ecosystem
//!
//! WeftOS is built from these crates:
//!
//! | Crate | Role |
//! |-------|------|
//! | [`weftos`](https://crates.io/crates/weftos) | Product facade -- re-exports kernel, core, types |
//! | [`clawft-kernel`](https://crates.io/crates/clawft-kernel) | Kernel: processes, services, governance, mesh, ExoChain |
//! | [`clawft-core`](https://crates.io/crates/clawft-core) | Agent framework: pipeline, context, tools, skills |
//! | [`clawft-types`](https://crates.io/crates/clawft-types) | Shared type definitions |
//! | [`clawft-platform`](https://crates.io/crates/clawft-platform) | Platform abstraction (native/WASM/browser) |
//! | [`clawft-plugin`](https://crates.io/crates/clawft-plugin) | Plugin SDK for tools, channels, and extensions |
//! | [`clawft-llm`](https://crates.io/crates/clawft-llm) | LLM provider abstraction (11 providers + local) |
//! | [`exo-resource-tree`](https://crates.io/crates/exo-resource-tree) | Hierarchical resource namespace with Merkle integrity |
//!
//! Source: <https://github.com/WeftOS/weftos>

pub mod boot;
pub mod consent;
pub mod delegation;
pub mod did;
pub mod error;
pub mod merkle;
pub mod model;
pub mod mutation;
pub mod permission;
pub mod scoring;
pub mod tree;

pub use boot::{bootstrap_fresh, from_checkpoint, to_checkpoint};
pub use consent::{
    ConsentDecision, ConsentEffect, ConsentPolicy, ConsentProof, RiskScore,
    create_consent_proof, evaluate as evaluate_consent,
};
pub use delegation::{DelegationCert, DelegationError, validate_chain};
pub use did::{Did, DidError};
pub use error::{TreeError, TreeResult};
pub use merkle::{
    DiffNodePayload, MerkleDiff, MerkleDiffEntry, MerkleDiffKind, MerkleInclusionProof,
    MerkleProofLevel, SiblingWitness,
};
pub use model::{Action, ResourceId, ResourceKind, ResourceNode, Role};
pub use mutation::{MutationEvent, MutationLog};
pub use permission::{
    AclPolicy, CapabilityChecker, Decision, Effect, EffectiveAclCache, Principal,
    check as check_permission, resource_in_scope,
};
pub use scoring::NodeScoring;
pub use tree::ResourceTree;
