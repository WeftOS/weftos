//! Size budget for the heartbeat `report`.
//!
//! The dashboard refuses a report over 64 KB (HTTP 413). The reporter trims
//! to [`REPORT_BUDGET_BYTES`] first, dropping the lowest-value sections
//! before the higher-value ones. `resources` and `exposures` (ADR-120) are
//! cut late, after `routes`: the library versions first, then the exposure
//! rows. cogbus `topics` (ADR-122) go just before `resources`, row by row
//! (the total survives). Identity (`host`, `weaver_version`),
//! `mesh_identity` and `health` are never trimmed.

use serde_json::{Value, json};

/// Serialized `report` budget, under the dashboard's 64 KB limit.
pub const REPORT_BUDGET_BYTES: usize = 60 * 1024;

/// Sections trimmed, lowest value first.
const TRIM_ORDER: [&str; 9] =
    ["workspaces", "instances", "observed", "services", "routes", "topics", "resources", "exposures", "pair_requests"];

/// Serialized byte length of `report` (what the dashboard measures).
pub fn report_len(report: &Value) -> usize {
    report.to_string().len()
}

/// Drop the last element/entry of a section. False if nothing is left to drop.
fn shrink_one(v: &mut Value) -> bool {
    match v {
        Value::Array(a) => a.pop().is_some(),
        Value::Object(o) => match o.keys().next_back().cloned() {
            Some(k) => o.remove(&k).is_some(),
            None => false,
        },
        _ => false,
    }
}

/// The part of `section` to shrink: for `instances`, the `recent` list then
/// `drift`; for `exposures`, the `items`; for `resources`, the `libs` then the
/// GPU list, so the counts survive. Any other section is shrunk whole.
fn target<'a>(section: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    let live = |v: Option<&Value>| match v {
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => false,
    };
    let pick = match key {
        "instances" if live(section.get("recent")) => "recent",
        "instances" => "drift",
        "exposures" | "topics" => "items",
        "resources" if live(section.get("libs")) => "libs",
        "resources" => "gpus",
        _ => return Some(section),
    };
    section.get_mut(pick)
}

/// Trim `report` until it fits `budget` bytes, section by section in
/// [`TRIM_ORDER`]. Sets `<section>_truncated` on every section it cut.
/// Returns true if the report fits afterwards.
pub fn fit(report: &mut Value, budget: usize) -> bool {
    for key in TRIM_ORDER {
        if report_len(report) <= budget {
            return true;
        }
        while report_len(report) > budget {
            let Some(section) = report.get_mut(key).and_then(|s| target(s, key)) else { break };
            if !shrink_one(section) {
                break;
            }
            // Set before measuring again, so the flag itself is in budget.
            report[format!("{key}_truncated")] = json!(true);
        }
    }
    report_len(report) <= budget
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(n: usize) -> Value {
        json!((0..n).map(|i| format!("{i:0>200}")).collect::<Vec<_>>())
    }

    #[test]
    fn under_budget_is_untouched() {
        let mut r = json!({ "host": "h", "workspaces": big(3) });
        let before = r.clone();
        assert!(fit(&mut r, 60 * 1024));
        assert_eq!(r, before);
    }

    #[test]
    fn workspaces_go_first_and_protected_sections_survive() {
        let mut r = json!({
            "host": "h", "weaver_version": "v", "mesh_identity": "id", "health": { "ok": true },
            "workspaces": big(100), "observed": { "a": "x", "b": "y" }, "services": big(2),
        });
        assert!(fit(&mut r, 8 * 1024));
        assert!(report_len(&r) <= 8 * 1024);
        assert_eq!(r["workspaces_truncated"], true);
        assert_eq!(r["observed"], json!({ "a": "x", "b": "y" }));
        assert_eq!(r["services"].as_array().unwrap().len(), 2);
        assert_eq!(r["mesh_identity"], "id");
        assert_eq!(r["health"], json!({ "ok": true }));
    }

    #[test]
    fn later_sections_are_cut_only_when_earlier_ones_are_exhausted() {
        let mut r = json!({
            "host": "h", "mesh_identity": "id",
            "workspaces": big(5), "observed": big(50), "services": big(50), "routes": big(50),
        });
        assert!(fit(&mut r, 2 * 1024));
        assert_eq!(r["workspaces"], json!([]));
        assert_eq!(r["observed_truncated"], true);
        assert!(r["observed"].as_array().unwrap().is_empty());
        assert_eq!(r["services_truncated"], true);
        assert_eq!(r["routes_truncated"], true);
        assert_eq!(r["mesh_identity"], "id");
        assert!(report_len(&r) <= 2 * 1024);
    }

    #[test]
    fn instances_shrink_their_lists_and_keep_the_counts() {
        let mut r = json!({
            "host": "h", "mesh_identity": "id",
            "instances": { "total": 40, "counts": { "idle": 40 }, "recent": big(40), "drift": big(2) },
            "observed": { "a": "x" },
        });
        assert!(fit(&mut r, 3 * 1024));
        assert_eq!(r["instances_truncated"], true);
        assert_eq!(r["instances"]["total"], 40);
        assert_eq!(r["instances"]["counts"], json!({ "idle": 40 }));
        assert!(r["instances"]["recent"].as_array().unwrap().len() < 40);
        assert_eq!(r["observed"], json!({ "a": "x" }), "observed is cut after instances");
    }

    #[test]
    fn resources_and_exposures_shrink_their_lists_and_keep_the_rest() {
        let libs: serde_json::Map<String, Value> = (0..40).map(|i| (format!("l{i:02}"), json!(format!("{i:0>200}")))).collect();
        let mut r = json!({
            "host": "h", "mesh_identity": "id",
            "resources": { "cpu": { "arch": "aarch64" }, "libs": libs, "gpus": [{ "kind": "metal" }] },
            "exposures": { "enabled": true, "items": big(2) },
            "pair_requests": [1],
        });
        assert!(fit(&mut r, 3 * 1024));
        assert_eq!(r["resources_truncated"], true);
        assert_eq!(r["resources"]["cpu"], json!({ "arch": "aarch64" }));
        assert_eq!(r["resources"]["gpus"], json!([{ "kind": "metal" }]), "gpus survive while libs can still shrink");
        assert_eq!(r["exposures"]["items"].as_array().unwrap().len(), 2, "exposures wait for resources");
        assert!(r.get("exposures_truncated").is_none());
        assert_eq!(r["pair_requests"], json!([1]), "pair_requests is cut last");
        assert!(report_len(&r) <= 3 * 1024);
    }

    #[test]
    fn exposure_items_go_before_pair_requests() {
        let mut r = json!({
            "host": "h", "mesh_identity": "id",
            "exposures": { "enabled": true, "items": big(30) },
            "pair_requests": big(2),
        });
        assert!(fit(&mut r, 1500));
        assert_eq!(r["exposures_truncated"], true);
        assert_eq!(r["exposures"]["enabled"], true);
        assert_eq!(r["pair_requests"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn identity_alone_over_budget_reports_failure_to_fit() {
        let mut r = json!({ "host": "h", "mesh_identity": "x".repeat(4096) });
        assert!(!fit(&mut r, 1024));
    }
}
