//! The stderr warning `weaver store worktree` prints when a reply says a worktree bucket was last
//! written by an older daemon (WO-069 lane F1, ADR-126 gate item 8). The reply itself is untouched
//! (`--json` passes the `older_writer` field through); this only adds one line per affected worktree
//! per command, on stderr, so scripts and pipes reading stdout see no change and the exit status is
//! never affected.

use serde_json::Value;

/// One warning line per worktree whose reply carries `older_writer`. `slug` is the one the caller
/// named (publish, claim, promote, a single-worktree browse); list verbs carry their own slugs.
pub fn older_writer_warnings(slug: Option<&str>, v: &Value) -> Vec<String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut add = |slug: &str, w: &Value| {
        if let (Some(what), Some(ev)) = (w["what"].as_str(), w["evidence"].as_str())
            && !out.iter().any(|(s, _)| s == slug)
        {
            out.push((slug.to_string(), line(slug, what, ev)));
        }
    };
    if v["older_writer"].is_object() {
        add(v["slug"].as_str().or(slug).unwrap_or("?"), &v["older_writer"]);
    }
    for list in ["worktrees", "leftovers", "changes"] {
        for x in v[list].as_array().into_iter().flatten() {
            let w = if x["worktree"].is_object() { &x["worktree"] } else { x };
            if w["older_writer"].is_object() {
                add(w["slug"].as_str().unwrap_or("?"), &w["older_writer"]);
            }
        }
    }
    out.into_iter().map(|(_, l)| l).collect()
}

fn line(slug: &str, what: &str, evidence: &str) -> String {
    format!(
        "warning: worktree {slug} was last written by an older daemon ({what}). Upgrade that writer before a client project uses shared worktrees (ADR-126 section 4a, gate item 8). Evidence: {evidence}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ow() -> Value {
        json!({"kind": "handoff_rewritten", "what": "the record was rewritten", "evidence": "no publishing"})
    }

    #[test]
    fn a_clean_reply_prints_nothing() {
        assert!(older_writer_warnings(Some("a"), &json!({"state": "claimed", "epoch": 2})).is_empty());
        assert!(older_writer_warnings(None, &json!({"worktrees": [{"slug": "a"}], "unreadable": []})).is_empty());
        assert!(older_writer_warnings(None, &json!({"changes": [], "trust": {}})).is_empty());
    }

    #[test]
    fn one_line_per_affected_worktree_naming_the_slug_the_gate_and_the_remedy() {
        let w = older_writer_warnings(Some("feat-a"), &json!({"state": "claimed", "older_writer": ow()}));
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("warning: worktree feat-a ") && w[0].contains("gate item 8") && w[0].contains("Upgrade that writer"), "{w:?}");
        assert!(!w[0].contains('\n'));
        // a listing names each one once, whatever shape the verb replies with
        let list = json!({"worktrees": [
            {"slug": "a", "older_writer": ow()}, {"slug": "b"}, {"slug": "c", "older_writer": ow()}, {"slug": "a", "older_writer": ow()}]});
        assert_eq!(older_writer_warnings(None, &list).len(), 2);
        let f = json!({"changes": [{"change": "new", "worktree": {"slug": "x", "older_writer": ow()}}, {"change": "gone", "slug": "y"}]});
        assert_eq!(older_writer_warnings(None, &f).len(), 1);
        let l = json!({"leftovers": [{"why": "orphan", "worktree": {"slug": "z", "older_writer": ow()}}]});
        assert!(older_writer_warnings(None, &l)[0].contains("worktree z "));
        // a folder or file read names its own slug
        let ls = json!({"slug": "feat-b", "path": "", "older_writer": ow()});
        assert!(older_writer_warnings(Some("ignored"), &ls)[0].contains("worktree feat-b "));
    }
}
