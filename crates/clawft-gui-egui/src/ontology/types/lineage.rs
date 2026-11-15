//! `Lineage` — typed Object for derivation provenance on derived paths.
//!
//! ## Placement (WEFT-275 sign-off)
//!
//! Primary convention: **sibling path**
//!
//! ```text
//! <derived-path>/meta/lineage
//! ```
//!
//! Example:
//!
//! ```text
//! substrate/n-daemon/derived/transcript/n-6f3a9c/mic/meta/lineage
//! ```
//!
//! The value *at* that path is a flat lineage document (not nested under
//! a second `lineage` key). See
//! `docs/plans/wave-0-WEFT-275-lineage-metadata.md` and
//! `.planning/sensors/EXPLORER-MANAGEMENT-SURFACE.md` §6 Q3.
//!
//! ## Shape
//!
//! ```json
//! {
//!   "kind": "lineage",
//!   "source_paths": ["substrate/n-6f3a9c/sensor/mic"],
//!   "via_actor": "whisper",
//!   "target_path": "substrate/n-daemon/derived/transcript/n-6f3a9c/mic",
//!   "derivation": "transform",
//!   "ts": 1700000000000
//! }
//! ```
//!
//! Matching rules:
//! - `kind: "lineage"` → strong match (priority 13).
//! - non-empty `source_paths: [string, …]` at the root → match (13).
//! - Nested `lineage` on a parent payload is **not** matched (would steal
//!   transcript/sensor viewers). Select the sibling meta path instead.
//!
//! Priority: 13 — above HealthReport/ChainTail (12), below specialised
//! graph-shaped `{nodes,edges}` (14) so a pre-materialised `ui://graph`
//! still wins when both arrays are present without lineage keys.

use super::super::{ObjectType, ObjectTypeCapabilities, PropertyDecl, PropertyKind};
use serde_json::Value;

/// Priority returned by [`Lineage::matches`] when the shape qualifies.
pub const LINEAGE_PRIORITY: u32 = 13;

/// Typed Object for derivation provenance documents.
pub struct Lineage;

impl ObjectType for Lineage {
    fn name() -> &'static str {
        "lineage"
    }

    fn display_name() -> &'static str {
        "Lineage"
    }

    fn matches(value: &Value) -> u32 {
        let Some(obj) = value.as_object() else {
            return 0;
        };

        // Strong discriminator.
        if obj.get("kind").and_then(Value::as_str) == Some("lineage") {
            // Still require a usable source list when kind is set, so a
            // bare `{kind:"lineage"}` badge without edges doesn't claim
            // an empty graph. Exception: allow empty source_paths when
            // kind is set so publishers can stage a stub document.
            return LINEAGE_PRIORITY;
        }

        // Soft: top-level non-empty source_paths of strings.
        if has_usable_source_paths(obj.get("source_paths")) {
            return LINEAGE_PRIORITY;
        }

        0
    }

    fn properties() -> &'static [PropertyDecl] {
        &[
            PropertyDecl {
                name: "kind",
                kind: PropertyKind::String,
                doc: "Discriminator; expected \"lineage\".",
            },
            PropertyDecl {
                name: "source_paths",
                kind: PropertyKind::Array(&PropertyKind::String),
                doc: "Substrate paths this derivation reads from.",
            },
            PropertyDecl {
                name: "via_actor",
                kind: PropertyKind::String,
                doc: "Actor / pipeline stage that produced the derivation.",
            },
            PropertyDecl {
                name: "target_path",
                kind: PropertyKind::String,
                doc: "Derived path written by the sink (optional when path ends in /meta/lineage).",
            },
            PropertyDecl {
                name: "derivation",
                kind: PropertyKind::String,
                doc: "Free-form derivation tag (transform, clone, snapshot, …).",
            },
            PropertyDecl {
                name: "ts",
                kind: PropertyKind::U64,
                doc: "Publish timestamp (unix ms or ISO-8601 string).",
            },
        ]
    }

    fn capabilities() -> ObjectTypeCapabilities {
        ObjectTypeCapabilities {
            applicable_actions: &["lineage.copy_path", "lineage.open_target"],
            events_emitted: &[],
            default_viewer_priority_hint: Some(LINEAGE_PRIORITY),
        }
    }
}

/// True when `source_paths` is a non-empty array of strings.
pub fn has_usable_source_paths(v: Option<&Value>) -> bool {
    let Some(arr) = v.and_then(Value::as_array) else {
        return false;
    };
    !arr.is_empty() && arr.iter().all(|e| e.as_str().is_some())
}

/// Extract source path strings from a lineage document.
pub fn source_paths(value: &Value) -> Vec<String> {
    value
        .as_object()
        .and_then(|o| o.get("source_paths"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the derived target path: explicit `target_path`, else strip
/// a trailing `/meta/lineage` from the Explorer selection path.
pub fn resolve_target_path(value: &Value, selected_path: &str) -> Option<String> {
    if let Some(t) = value
        .as_object()
        .and_then(|o| o.get("target_path"))
        .and_then(Value::as_str)
        && !t.is_empty()
    {
        return Some(t.to_owned());
    }
    strip_meta_lineage_suffix(selected_path)
}

/// Strip a trailing `/meta/lineage` (with optional trailing slash).
pub fn strip_meta_lineage_suffix(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    const SUFFIX: &str = "/meta/lineage";
    if let Some(base) = trimmed.strip_suffix(SUFFIX)
        && !base.is_empty()
    {
        return Some(base.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_kind_discriminator() {
        let v = json!({
            "kind": "lineage",
            "source_paths": ["substrate/n-1/sensor/mic"],
        });
        assert_eq!(Lineage::matches(&v), LINEAGE_PRIORITY);
    }

    #[test]
    fn matches_kind_alone_as_stub() {
        // Publishers may stage a stub before wiring sources.
        let v = json!({ "kind": "lineage" });
        assert_eq!(Lineage::matches(&v), LINEAGE_PRIORITY);
    }

    #[test]
    fn matches_source_paths_without_kind() {
        let v = json!({
            "source_paths": [
                "substrate/n-6f3a9c/sensor/mic",
                "substrate/n-6f3a9c/sensor/tof"
            ],
            "via_actor": "whisper",
        });
        assert_eq!(Lineage::matches(&v), LINEAGE_PRIORITY);
    }

    #[test]
    fn rejects_empty_source_paths_without_kind() {
        let v = json!({ "source_paths": [], "via_actor": "whisper" });
        assert_eq!(Lineage::matches(&v), 0);
    }

    #[test]
    fn rejects_non_string_source_paths() {
        let v = json!({ "source_paths": [1, 2, 3] });
        assert_eq!(Lineage::matches(&v), 0);
    }

    #[test]
    fn rejects_nested_lineage_on_parent_payload() {
        // Inline opt-in must not steal the parent transcript shape.
        let v = json!({
            "text": "hello",
            "lineage": {
                "source_paths": ["substrate/n-1/sensor/mic"],
                "via_actor": "whisper"
            }
        });
        assert_eq!(Lineage::matches(&v), 0);
    }

    #[test]
    fn rejects_unrelated_object() {
        let v = json!({ "rssi": -47, "free_heap": 1000 });
        assert_eq!(Lineage::matches(&v), 0);
    }

    #[test]
    fn rejects_array_and_null() {
        assert_eq!(Lineage::matches(&json!([])), 0);
        assert_eq!(Lineage::matches(&Value::Null), 0);
    }

    #[test]
    fn smoke_derived_path_with_lineage_attached() {
        // AC smoke: derived transcript path + sibling meta/lineage document.
        let derived = "substrate/n-daemon/derived/transcript/n-6f3a9c/mic";
        let meta_path = format!("{derived}/meta/lineage");
        let doc = json!({
            "kind": "lineage",
            "source_paths": ["substrate/n-6f3a9c/sensor/mic"],
            "via_actor": "whisper",
            "target_path": derived,
            "derivation": "transform",
            "ts": 1_700_000_000_000_u64,
        });
        assert_eq!(Lineage::matches(&doc), LINEAGE_PRIORITY);
        assert_eq!(
            resolve_target_path(&doc, &meta_path).as_deref(),
            Some(derived)
        );
        assert_eq!(
            source_paths(&doc),
            vec!["substrate/n-6f3a9c/sensor/mic".to_string()]
        );
        assert_eq!(
            strip_meta_lineage_suffix(&meta_path).as_deref(),
            Some(derived)
        );
    }

    #[test]
    fn strip_meta_lineage_when_target_omitted() {
        let meta = "substrate/n-x/derived/out/meta/lineage";
        let doc = json!({
            "kind": "lineage",
            "source_paths": ["substrate/n-x/sensor/mic"],
        });
        assert_eq!(
            resolve_target_path(&doc, meta).as_deref(),
            Some("substrate/n-x/derived/out")
        );
    }

    #[test]
    fn identity_and_properties() {
        assert_eq!(Lineage::name(), "lineage");
        assert_eq!(Lineage::display_name(), "Lineage");
        let names: Vec<&str> = Lineage::properties().iter().map(|p| p.name).collect();
        assert!(names.contains(&"source_paths"));
        assert!(names.contains(&"via_actor"));
        assert!(names.contains(&"target_path"));
    }

    #[test]
    fn capabilities_surface() {
        let caps = Lineage::capabilities();
        assert!(caps.applicable_actions.contains(&"lineage.copy_path"));
        assert_eq!(caps.default_viewer_priority_hint, Some(LINEAGE_PRIORITY));
    }
}
