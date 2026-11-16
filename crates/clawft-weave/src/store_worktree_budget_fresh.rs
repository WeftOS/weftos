//! WO-069 lane J2: the call budget of the `fresh` path of `browse`, `search` and `overlaps`. `fresh`
//! skips the read cache for the call and refreshes what it holds, so it costs a cold load: the
//! uncached budget plus the one entry HEAD that takes the etag before the body. The default
//! (cached) rows in the parent module are unchanged. A child of `store_worktree_budget`.

use super::*;

#[test]
fn fresh_costs_a_cold_load_and_leaves_the_default_rows_alone() {
    let r = rig_w();
    r.run("browse", json!({"project": PROJ, "slug": "wt0"}));
    pin(r.run("browse", json!({"project": PROJ, "slug": "wt0"})).1, [0, 0, 0, 0, 0, 0, 0, 0], "browse ls, warm (default, unchanged)");
    let (v, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0", "fresh": true}));
    // entry HEAD + GET, meta GET, manifest HEAD + GET (uncached [3, 1])
    pin(c, [3, 2, 0, 0, 0, 0, 0, 0], "browse ls, fresh");
    assert_eq!(v["cache"]["source"], "store");
    let (_, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0", "path": "docs/d0.md", "fresh": true}));
    // the cold load plus the object HEAD + GET (uncached [4, 2])
    pin(c, [4, 3, 0, 0, 0, 0, 0, 0], "browse get, fresh");
    pin(r.run("browse", json!({"project": PROJ, "slug": "wt0"})).1, [0, 0, 0, 0, 0, 0, 0, 0], "the fresh reads refreshed the slot: warm again");
    // the worktree list is a LIST either way; `fresh` changes nothing there
    r.run("browse", json!({"project": PROJ}));
    pin(r.run("browse", json!({"project": PROJ, "fresh": true})).1, [0, 0, 1, 0, 0, 0, 0, 0], "browse registry, fresh = the default");

    r.run("search", json!({"project": PROJ, "query": "doc"}));
    pin(r.run("search", json!({"project": PROJ, "query": "doc"})).1, [0, 0, 1, 0, 0, 0, 0, 0], "search, warm (default, unchanged)");
    let (v, c) = r.run("search", json!({"project": PROJ, "query": "doc", "fresh": true}));
    // the LIST alone for the registry (entries unchanged), per worktree meta GET + manifest HEAD + GET (uncached [3W, W, 1])
    pin(c, [2 * W, W, 1, 0, 0, 0, 0, 0], "search, fresh");
    assert_eq!(v["cache"]["source"], "store");

    r.run("overlaps", json!({"project": PROJ, "a": "wt1", "b": "wt2"}));
    pin(r.run("overlaps", json!({"project": PROJ, "a": "wt1", "b": "wt2"})).1, [0, 0, 0, 0, 0, 0, 0, 0], "overlaps, warm (default, unchanged)");
    let (v, c) = r.run("overlaps", json!({"project": PROJ, "a": "wt1", "b": "wt2", "fresh": true}));
    // two cold loads (uncached [8, 4])
    pin(c, [6, 4, 0, 0, 0, 0, 0, 0], "overlaps, fresh");
    assert_eq!(v["cache"]["source"], "store");
}

/// With the cache off every reply says `store` with a zero window, and `fresh` changes no call.
#[test]
fn cache_off_replies_say_store_and_fresh_is_a_no_op() {
    let r = rig_w();
    wtcache::set_cfg(&r.svc, None);
    for fresh in [false, true] {
        let (v, c) = r.run("browse", json!({"project": PROJ, "slug": "wt0", "fresh": fresh}));
        pin(c, [3, 1, 0, 0, 0, 0, 0, 0], "browse ls, off");
        assert_eq!(v["cache"], json!({"age_ms": 0, "ttl_ms": 0, "source": "store"}));
        pin(r.run("search", json!({"project": PROJ, "query": "doc", "fresh": fresh})).1, [3 * W, W, 1, 0, 0, 0, 0, 0], "search, off");
    }
}
