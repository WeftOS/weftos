//! Visual encoding table — ADR-067 D6 / design §3.
//!
//! G1: state / role / edge → style. G5: classification badges + cluster hue.

use super::model::{NodeRole, NodeStateTag};
use eframe::egui::Color32;

/// Node paint style derived from state + role.
#[derive(Debug, Clone, Copy)]
pub struct NodeStyle {
    pub fill: Color32,
    pub outline: Color32,
    pub outline_width: f32,
    pub opacity: f32,
    /// When true, draw dashed outline (Speculative).
    pub dashed: bool,
    /// When true, draw diagonal strike (Pruned).
    pub tombstone: bool,
}

/// Edge paint style.
#[derive(Debug, Clone, Copy)]
pub struct EdgeStyle {
    pub color: Color32,
    pub width: f32,
    pub dashed: bool,
    pub zigzag: bool,
}

/// State → fill / opacity (primary visual signal).
pub fn style_for_state(state: NodeStateTag) -> NodeStyle {
    match state {
        NodeStateTag::Speculative => NodeStyle {
            fill: crate::theming::palette::CG_SPECULATIVE_FILL,
            outline: crate::theming::palette::CG_SPECULATIVE_OUTLINE,
            outline_width: 1.5,
            opacity: 0.4,
            dashed: true,
            tombstone: false,
        },
        NodeStateTag::Frontier => NodeStyle {
            fill: crate::theming::palette::CG_FRONTIER_FILL,
            outline: crate::theming::palette::CG_FRONTIER_OUTLINE,
            outline_width: 2.5,
            opacity: 1.0,
            dashed: false,
            tombstone: false,
        },
        NodeStateTag::Committed => NodeStyle {
            fill: crate::theming::palette::CG_COMMITTED_FILL,
            outline: crate::theming::palette::CG_COMMITTED_OUTLINE,
            outline_width: 1.0,
            opacity: 1.0,
            dashed: false,
            tombstone: false,
        },
        NodeStateTag::Stale => NodeStyle {
            fill: crate::theming::palette::CG_STALE_FILL,
            outline: crate::theming::palette::CG_STALE_OUTLINE,
            outline_width: 1.0,
            opacity: 0.7,
            dashed: false,
            tombstone: false,
        },
        NodeStateTag::Pruned => NodeStyle {
            fill: crate::theming::palette::CG_PRUNED_FILL,
            outline: crate::theming::palette::CG_PRUNED_OUTLINE,
            outline_width: 1.0,
            opacity: 0.3,
            dashed: false,
            tombstone: true,
        },
    }
}

/// Role → shape key (paint maps to geometry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeShape {
    Circle,
    RoundedSquare,
    Diamond,
    Ring,
}

pub fn shape_for_role(role: NodeRole) -> NodeShape {
    match role {
        NodeRole::User => NodeShape::Circle,
        NodeRole::Assistant => NodeShape::RoundedSquare,
        NodeRole::Tool => NodeShape::Diamond,
        NodeRole::Speaker => NodeShape::Ring,
    }
}

/// Edge type → line style.
pub fn style_for_edge_kind(kind: &str) -> EdgeStyle {
    match kind {
        "Contradicts" => EdgeStyle {
            color: crate::theming::palette::CG_EDGE_CONTRADICTS,
            width: 1.5,
            dashed: false,
            zigzag: true,
        },
        "Enables" | "EvidenceFor" => EdgeStyle {
            color: crate::theming::palette::CG_EDGE_SUPPORTS,
            width: 1.2,
            dashed: true,
            zigzag: false,
        },
        "Speaker" | "Continuer" | "EmotionCause" | "GoalMotivation" => EdgeStyle {
            color: crate::theming::palette::CG_EDGE_SPEAKER,
            width: 1.0,
            dashed: true,
            zigzag: false,
        },
        // Follows and default causal
        _ => EdgeStyle {
            color: crate::theming::palette::CG_EDGE_DEFAULT,
            width: 1.2,
            dashed: false,
            zigzag: false,
        },
    }
}

/// G5: stable hue for a topic tag (hash → color).
pub fn topic_hue(topic: &str) -> Color32 {
    let mut h: u32 = 2166136261;
    for b in topic.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(16777619);
    }
    let r = 80 + (h & 0x7f) as u8;
    let g = 80 + ((h >> 8) & 0x7f) as u8;
    let b = 80 + ((h >> 16) & 0x7f) as u8;
    crate::theming::palette::data_rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_covers_all_states() {
        for s in [
            NodeStateTag::Speculative,
            NodeStateTag::Frontier,
            NodeStateTag::Committed,
            NodeStateTag::Stale,
            NodeStateTag::Pruned,
        ] {
            let st = style_for_state(s);
            assert!(st.opacity > 0.0 && st.opacity <= 1.0);
        }
        assert!(style_for_state(NodeStateTag::Speculative).dashed);
        assert!(style_for_state(NodeStateTag::Pruned).tombstone);
        assert!(style_for_state(NodeStateTag::Frontier).outline_width >= 2.0);
    }

    #[test]
    fn shapes_and_edges() {
        assert_eq!(shape_for_role(NodeRole::User), NodeShape::Circle);
        assert_eq!(shape_for_role(NodeRole::Assistant), NodeShape::RoundedSquare);
        assert!(style_for_edge_kind("Contradicts").zigzag);
        assert!(style_for_edge_kind("Speaker").dashed);
        assert!(!style_for_edge_kind("Follows").dashed);
    }
}
