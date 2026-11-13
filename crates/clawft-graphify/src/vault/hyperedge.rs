//! Vault-domain hyperedge SUGGEST step.
//!
//! Proposes multi-document [`HyperedgeProposal`]s from a vault link graph
//! (tags, path co-location, link motifs). Proposals start in
//! [`ProposalLifecycle::Suggested`] and are later ratified / rejected by the
//! pipeline (WEFT-378).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::analyze::VaultNode;

// ---------------------------------------------------------------------------
// Lifecycle + kind
// ---------------------------------------------------------------------------

/// Lifecycle of a vault hyperedge proposal (SUGGEST → ratify gate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalLifecycle {
    /// Emitted by SUGGEST; awaiting human / agent decision.
    Suggested,
    /// Explicitly approved (7R Ratify / human gate).
    Ratified,
    /// Explicitly declined.
    Rejected,
}

/// How the hyperedge was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HyperedgeKind {
    /// ≥2 vault docs share one or more tags.
    SharedTags,
    /// Docs co-located under the same parent directory.
    PathCluster,
    /// Closed neighborhood of a link hub (node + neighbors, size ≥3).
    LinkMotif,
    /// Orphan documents grouped by keyword / tag affinity.
    OrphanBridge,
}

impl HyperedgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SharedTags => "shared_tags",
            Self::PathCluster => "path_cluster",
            Self::LinkMotif => "link_motif",
            Self::OrphanBridge => "orphan_bridge",
        }
    }
}

// ---------------------------------------------------------------------------
// Proposal
// ---------------------------------------------------------------------------

/// A suggested vault-domain hyperedge (multi-document group).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HyperedgeProposal {
    /// Deterministic id: BLAKE3(kind || sorted members).
    pub id: String,
    pub label: String,
    pub kind: HyperedgeKind,
    /// Vault-relative paths of member documents (sorted).
    pub members: Vec<String>,
    /// Raw suggestion score (higher = stronger signal).
    pub score: f64,
    /// Score normalized into `[0, 1]` for ratify thresholds.
    pub confidence: f64,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_tags: Vec<String>,
    pub status: ProposalLifecycle,
}

impl HyperedgeProposal {
    /// Build a proposal with a deterministic id from kind + members.
    pub fn new(
        kind: HyperedgeKind,
        mut members: Vec<String>,
        label: impl Into<String>,
        score: f64,
        reason: impl Into<String>,
        shared_tags: Vec<String>,
    ) -> Self {
        members.sort();
        members.dedup();
        let id = proposal_id(kind, &members);
        let confidence = (score / 15.0).clamp(0.0, 1.0);
        Self {
            id,
            label: label.into(),
            kind,
            members,
            score,
            confidence,
            reason: reason.into(),
            shared_tags,
            status: ProposalLifecycle::Suggested,
        }
    }
}

/// Deterministic proposal id (hex-encoded BLAKE3).
pub fn proposal_id(kind: HyperedgeKind, members: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b"\0");
    for m in members {
        hasher.update(m.as_bytes());
        hasher.update(b"\0");
    }
    hex_encode(hasher.finalize().as_bytes())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Knobs for the vault hyperedge SUGGEST step.
#[derive(Debug, Clone)]
pub struct HyperedgeSuggestConfig {
    /// Minimum members in a hyperedge (must be ≥2; default 3 for true hyperedges).
    pub min_members: usize,
    /// Drop proposals below this raw score.
    pub min_score: f64,
    /// Cap on returned proposals (highest score first).
    pub max_proposals: usize,
    /// Emit shared-tag groups.
    pub enable_shared_tags: bool,
    /// Emit path co-location groups.
    pub enable_path_cluster: bool,
    /// Emit link-motif neighborhoods.
    pub enable_link_motif: bool,
    /// Emit orphan-bridge groups.
    pub enable_orphan_bridge: bool,
}

impl Default for HyperedgeSuggestConfig {
    fn default() -> Self {
        Self {
            min_members: 3,
            min_score: 3.0,
            max_proposals: 50,
            enable_shared_tags: true,
            enable_path_cluster: true,
            enable_link_motif: true,
            enable_orphan_bridge: true,
        }
    }
}

// ---------------------------------------------------------------------------
// SUGGEST
// ---------------------------------------------------------------------------

/// Propose vault-domain hyperedges from an analyzed vault graph.
///
/// Detection strategies (motif-based, offline, no LLM):
/// 1. **Shared tags** — all docs carrying a given tag form a candidate set.
/// 2. **Path cluster** — co-located under the same parent directory.
/// 3. **Link motif** — closed neighborhood of a hub (node + neighbors).
/// 4. **Orphan bridge** — orphans sharing ≥1 tag or keyword token.
pub fn suggest_hyperedges(
    nodes: &HashMap<String, VaultNode>,
    config: &HyperedgeSuggestConfig,
) -> Vec<HyperedgeProposal> {
    let min_m = config.min_members.max(2);
    let mut by_id: HashMap<String, HyperedgeProposal> = HashMap::new();

    if config.enable_shared_tags {
        for p in suggest_shared_tags(nodes, min_m) {
            upsert(&mut by_id, p);
        }
    }
    if config.enable_path_cluster {
        for p in suggest_path_clusters(nodes, min_m) {
            upsert(&mut by_id, p);
        }
    }
    if config.enable_link_motif {
        for p in suggest_link_motifs(nodes, min_m) {
            upsert(&mut by_id, p);
        }
    }
    if config.enable_orphan_bridge {
        for p in suggest_orphan_bridges(nodes, min_m) {
            upsert(&mut by_id, p);
        }
    }

    let mut out: Vec<HyperedgeProposal> = by_id
        .into_values()
        .filter(|p| p.score >= config.min_score && p.members.len() >= min_m)
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    out.truncate(config.max_proposals);
    out
}

fn upsert(map: &mut HashMap<String, HyperedgeProposal>, p: HyperedgeProposal) {
    match map.get(&p.id) {
        Some(existing) if existing.score >= p.score => {}
        _ => {
            map.insert(p.id.clone(), p);
        }
    }
}

fn suggest_shared_tags(nodes: &HashMap<String, VaultNode>, min_m: usize) -> Vec<HyperedgeProposal> {
    let mut tag_to_docs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (key, node) in nodes {
        for tag in &node.tags {
            let t = tag.to_lowercase();
            tag_to_docs.entry(t).or_default().insert(key.clone());
        }
    }

    let mut proposals = Vec::new();
    for (tag, docs) in tag_to_docs {
        if docs.len() < min_m {
            continue;
        }
        let members: Vec<String> = docs.into_iter().collect();
        let n = members.len();
        // Score: base + size + bonus for multi-tag cohesion later.
        let score = 4.0 + (n as f64) * 1.5;
        proposals.push(HyperedgeProposal::new(
            HyperedgeKind::SharedTags,
            members,
            format!("tag:{tag}"),
            score,
            format!("{n} documents share tag '{tag}'"),
            vec![tag],
        ));
    }
    proposals
}

fn suggest_path_clusters(
    nodes: &HashMap<String, VaultNode>,
    min_m: usize,
) -> Vec<HyperedgeProposal> {
    let mut dir_to_docs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for key in nodes.keys() {
        let parent = Path::new(key)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ".".into());
        dir_to_docs.entry(parent).or_default().insert(key.clone());
    }

    let mut proposals = Vec::new();
    for (dir, docs) in dir_to_docs {
        if docs.len() < min_m {
            continue;
        }
        // Collect tags that appear in ≥2 members for label richness.
        let mut tag_counts: HashMap<String, usize> = HashMap::new();
        for d in &docs {
            if let Some(n) = nodes.get(d) {
                for t in &n.tags {
                    *tag_counts.entry(t.to_lowercase()).or_default() += 1;
                }
            }
        }
        let shared_tags: Vec<String> = tag_counts
            .into_iter()
            .filter(|(_, c)| *c >= 2)
            .map(|(t, _)| t)
            .collect();

        let members: Vec<String> = docs.into_iter().collect();
        let n = members.len();
        let score = 3.0 + n as f64 + shared_tags.len() as f64 * 0.5;
        let label = if dir == "." {
            "path:root".into()
        } else {
            format!("path:{dir}")
        };
        proposals.push(HyperedgeProposal::new(
            HyperedgeKind::PathCluster,
            members,
            label,
            score,
            format!("{n} documents co-located under '{dir}'"),
            shared_tags,
        ));
    }
    proposals
}

fn suggest_link_motifs(nodes: &HashMap<String, VaultNode>, min_m: usize) -> Vec<HyperedgeProposal> {
    let mut proposals = Vec::new();
    for (hub, node) in nodes {
        let mut group: BTreeSet<String> = BTreeSet::new();
        group.insert(hub.clone());
        for n in node.outgoing.iter().chain(node.incoming.iter()) {
            if nodes.contains_key(n) {
                group.insert(n.clone());
            }
        }
        if group.len() < min_m {
            continue;
        }
        let members: Vec<String> = group.into_iter().collect();
        let n = members.len();
        let degree = node.outgoing.len() + node.incoming.len();
        let score = 3.5 + degree as f64 * 0.8 + n as f64 * 0.5;
        let hub_label = Path::new(hub)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(hub);
        proposals.push(HyperedgeProposal::new(
            HyperedgeKind::LinkMotif,
            members,
            format!("motif:{hub_label}"),
            score,
            format!("link neighborhood of '{hub}' (degree {degree}, {n} members)"),
            vec![],
        ));
    }
    proposals
}

fn suggest_orphan_bridges(
    nodes: &HashMap<String, VaultNode>,
    min_m: usize,
) -> Vec<HyperedgeProposal> {
    let orphans: Vec<&String> = nodes
        .iter()
        .filter(|(_, n)| n.outgoing.is_empty() && n.incoming.is_empty())
        .map(|(k, _)| k)
        .collect();
    if orphans.len() < min_m {
        return Vec::new();
    }

    // Group orphans by any shared tag.
    let mut tag_to_orphans: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for key in &orphans {
        let node = &nodes[*key];
        if node.tags.is_empty() {
            // Fall back to first significant title token.
            if let Some(title) = &node.title {
                for word in title.split_whitespace() {
                    let w = word.to_lowercase();
                    if w.len() >= 4 {
                        tag_to_orphans
                            .entry(format!("kw:{w}"))
                            .or_default()
                            .insert((*key).clone());
                        break;
                    }
                }
            }
        } else {
            for tag in &node.tags {
                tag_to_orphans
                    .entry(tag.to_lowercase())
                    .or_default()
                    .insert((*key).clone());
            }
        }
    }

    let mut proposals = Vec::new();
    for (tag, docs) in tag_to_orphans {
        if docs.len() < min_m {
            continue;
        }
        let members: Vec<String> = docs.into_iter().collect();
        let n = members.len();
        let score = 3.0 + n as f64 * 1.2;
        let display = tag.strip_prefix("kw:").unwrap_or(&tag);
        proposals.push(HyperedgeProposal::new(
            HyperedgeKind::OrphanBridge,
            members,
            format!("orphans:{display}"),
            score,
            format!("{n} orphan documents share affinity '{display}'"),
            if tag.starts_with("kw:") {
                vec![]
            } else {
                vec![tag]
            },
        ));
    }
    proposals
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn node(path: &str, tags: &[&str], out: &[&str], inc: &[&str]) -> VaultNode {
        VaultNode {
            path: PathBuf::from(path),
            title: Some(
                Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path)
                    .to_string(),
            ),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            outgoing: out.iter().map(|s| s.to_string()).collect(),
            incoming: inc.iter().map(|s| s.to_string()).collect(),
            word_count: 50,
        }
    }

    #[test]
    fn shared_tags_form_hyperedge() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "docs/a.md".into(),
            node("docs/a.md", &["rust", "api"], &[], &[]),
        );
        nodes.insert("docs/b.md".into(), node("docs/b.md", &["rust"], &[], &[]));
        nodes.insert(
            "docs/c.md".into(),
            node("docs/c.md", &["rust", "cli"], &[], &[]),
        );
        nodes.insert(
            "other/d.md".into(),
            node("other/d.md", &["design"], &[], &[]),
        );

        let cfg = HyperedgeSuggestConfig {
            enable_path_cluster: false,
            enable_link_motif: false,
            enable_orphan_bridge: false,
            min_members: 3,
            ..Default::default()
        };
        let props = suggest_hyperedges(&nodes, &cfg);
        assert!(!props.is_empty());
        let top = props
            .iter()
            .find(|p| p.kind == HyperedgeKind::SharedTags)
            .expect("shared_tags proposal");
        assert_eq!(top.members.len(), 3);
        assert!(top.shared_tags.iter().any(|t| t == "rust"));
        assert_eq!(top.status, ProposalLifecycle::Suggested);
        // Id is deterministic.
        let again = suggest_hyperedges(&nodes, &cfg);
        assert_eq!(top.id, again[0].id);
    }

    #[test]
    fn path_cluster_groups_siblings() {
        let mut nodes = HashMap::new();
        for name in ["x.md", "y.md", "z.md"] {
            let key = format!("notes/{name}");
            nodes.insert(key.clone(), node(&key, &[], &[], &[]));
        }
        let cfg = HyperedgeSuggestConfig {
            enable_shared_tags: false,
            enable_link_motif: false,
            enable_orphan_bridge: false,
            min_members: 3,
            min_score: 1.0,
            ..Default::default()
        };
        let props = suggest_hyperedges(&nodes, &cfg);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].kind, HyperedgeKind::PathCluster);
        assert_eq!(props[0].members.len(), 3);
    }

    #[test]
    fn link_motif_closed_neighborhood() {
        let mut nodes = HashMap::new();
        nodes.insert("hub.md".into(), node("hub.md", &[], &["a.md", "b.md"], &[]));
        nodes.insert("a.md".into(), node("a.md", &[], &[], &["hub.md"]));
        nodes.insert("b.md".into(), node("b.md", &[], &[], &["hub.md"]));
        let cfg = HyperedgeSuggestConfig {
            enable_shared_tags: false,
            enable_path_cluster: false,
            enable_orphan_bridge: false,
            min_members: 3,
            min_score: 1.0,
            ..Default::default()
        };
        let props = suggest_hyperedges(&nodes, &cfg);
        assert!(
            props
                .iter()
                .any(|p| p.kind == HyperedgeKind::LinkMotif && p.members.len() == 3)
        );
    }

    #[test]
    fn proposal_id_stable() {
        let m = vec!["a.md".into(), "b.md".into()];
        let a = proposal_id(HyperedgeKind::SharedTags, &m);
        let b = proposal_id(HyperedgeKind::SharedTags, &m);
        assert_eq!(a, b);
        assert_ne!(a, proposal_id(HyperedgeKind::PathCluster, &m));
    }
}
