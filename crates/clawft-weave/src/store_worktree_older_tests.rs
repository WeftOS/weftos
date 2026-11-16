//! WO-069 lane F1 (ADR-126 gate item 8): every worktree verb says, additively, when the bucket was
//! last written by an older daemon, and `WEFTOS_WORKTREE_REQUIRE_CURRENT_WRITERS` turns that into a
//! refusal for `claim` and `promote` only. An older daemon is simulated the way it shows in the
//! store: its rewrite of the handoff record drops the announcement, and its registry entry has no
//! writer check.

use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::{FsBackend, StoreService};

use super::*;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";
const SLUG: &str = "feat-rooms";
const DAY: u64 = 86_400_000;

fn rig() -> (tempfile::TempDir, StoreService) {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
        be.clone(),
        be,
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg)),
        Arc::new(ChainAudit(None)),
    )
    .unwrap();
    (d, svc)
}

fn call(svc: &StoreService, verb: &str, p: Value, now: u64) -> Result<Value, String> {
    run(svc, "local", &format!("store.worktree.{verb}"), p, now)
}

struct W {
    _d: tempfile::TempDir,
    svc: StoreService,
    src: tempfile::TempDir,
    wt: String,
    pr: String,
}

fn setup() -> W {
    let (_d, svc) = rig();
    let b = call(&svc, "buckets", json!({"project": PROJ, "slug": SLUG}), 1).unwrap();
    let (wt, pr) = (b["worktree"].as_str().unwrap().to_owned(), b["project"].as_str().unwrap().to_owned());
    call(&svc, "create", json!({"project": PROJ, "slug": SLUG, "branch": "feat/rooms", "base": "abc"}), 10).unwrap();
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join(".git")).unwrap();
    std::fs::create_dir_all(src.path().join("docs")).unwrap();
    std::fs::write(src.path().join("docs/plan.md"), "# Rooms plan\n").unwrap();
    std::fs::write(src.path().join("paths.z"), b"docs/plan.md\0").unwrap();
    let w = W { _d, svc, src, wt, pr };
    w.publish(20);
    w
}

impl W {
    fn publish(&self, now: u64) -> Value {
        let root = self.src.path().to_string_lossy().into_owned();
        call(
            &self.svc,
            "publish",
            json!({"project": PROJ, "slug": SLUG, "root": root, "branch": "feat/rooms", "base": "abc", "head": "def",
                   "paths_z": self.src.path().join("paths.z").to_string_lossy(), "agent": "alice", "host": "m1"}),
            now,
        )
        .unwrap()
    }
    fn verb(&self, verb: &str, extra: Value, now: u64) -> Result<Value, String> {
        let mut p = json!({"project": PROJ, "slug": SLUG, "agent": "bob", "host": "m2"});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call(&self.svc, verb, p, now)
    }
    fn objects(&self) -> Arc<dyn S1> {
        self.svc.operator_objects()
    }
    /// What an older daemon's claim, handoff or merge does to the record: it drops the fields it does
    /// not know.
    fn older_daemon_rewrites_the_record(&self) {
        self.drop_from_record(&["publishing", "claimed_head", "claimed_tree"]);
    }
    fn drop_from_record(&self, keys: &[&str]) {
        let raw = self.objects().get(&self.wt, ".wt/handoff.json").unwrap();
        let mut v: Value = serde_json::from_slice(&raw).unwrap();
        for k in keys {
            v.as_object_mut().unwrap().remove(*k);
        }
        self.objects().put(&self.wt, ".wt/handoff.json", &serde_json::to_vec_pretty(&v).unwrap(), false).unwrap();
    }
    /// ... and its registry entry, which has no writer check.
    fn older_daemon_rewrites_the_entry(&self) {
        let key = format!("worktrees/{SLUG}.json");
        let mut v: Value = serde_json::from_slice(&self.objects().get(&self.pr, &key).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("writer_check");
        self.objects().put(&self.pr, &key, &serde_json::to_vec_pretty(&v).unwrap(), false).unwrap();
    }
    fn older_daemon(&self) {
        self.older_daemon_rewrites_the_record();
        self.older_daemon_rewrites_the_entry();
    }
}

fn require(on: bool) {
    super::publish_env::test_require::FORCE.with(|f| f.set(Some(on)));
}

#[test]
fn a_clean_bucket_changes_no_reply() {
    require(false);
    let w = setup();
    let st = w.src.path().join("follow.json");
    let replies = [
        w.verb("browse", json!({"slug": ""}), 29).unwrap(),
        w.verb("browse", json!({}), 30).unwrap(),
        w.verb("browse", json!({"path": "docs/"}), 31).unwrap(),
        w.verb("browse", json!({"path": "docs/plan.md"}), 32).unwrap(),
        w.verb("leftovers", json!({}), 20 * DAY).unwrap(),
        w.verb("follow", json!({"state": st.to_string_lossy()}), 33).unwrap(),
        w.publish(34),
        w.verb("claim", json!({}), 40).unwrap(),
        w.verb("promote", json!({"epoch": 1}), 50).unwrap(),
    ];
    for r in &replies {
        assert!(!r.to_string().contains("older_writer"), "{r}");
    }
    assert!(replies[4]["leftovers"].as_array().is_some_and(|l| !l.is_empty()), "the leftovers reply had a worktree to carry the field");
}

#[test]
fn the_registry_verbs_show_an_entry_an_older_daemon_wrote() {
    require(false);
    let w = setup();
    w.older_daemon();
    let kind = |v: &Value| v["older_writer"]["kind"].as_str().map(str::to_owned);
    assert_eq!(kind(&w.verb("browse", json!({"slug": ""}), 30).unwrap()["worktrees"][0]).as_deref(), Some("entry_from_older_build"));
    assert_eq!(kind(&w.verb("browse", json!({"path": "docs/"}), 31).unwrap()).as_deref(), Some("entry_from_older_build"));
    assert_eq!(kind(&w.verb("browse", json!({"path": "docs/plan.md"}), 32).unwrap()).as_deref(), Some("entry_from_older_build"));
    let l = w.verb("leftovers", json!({}), 20 * DAY).unwrap();
    assert_eq!(kind(&l["leftovers"][0]["worktree"]).as_deref(), Some("entry_from_older_build"));
    let st = w.src.path().join("follow.json");
    let f = w.verb("follow", json!({"state": st.to_string_lossy()}), 33).unwrap();
    assert_eq!(kind(&f["changes"][0]["worktree"]).as_deref(), Some("entry_from_older_build"));
    let ow = &l["leftovers"][0]["worktree"]["older_writer"];
    assert!(ow["what"].is_string() && ow["evidence"].is_string());
}

#[test]
fn publish_says_what_it_found_and_heals_the_bucket() {
    require(true); // a publish is never refused, whatever the switch says
    let w = setup();
    w.older_daemon();
    let r = w.publish(60);
    assert_eq!(r["older_writer"]["kind"], "handoff_rewritten", "{r}");
    // healed: the record carries an announcement again and the entry a clean check
    let again = w.publish(61);
    assert!(again.get("older_writer").is_none(), "{again}");
    assert!(w.verb("browse", json!({"slug": ""}), 62).unwrap()["worktrees"][0].get("older_writer").is_none());
}

#[test]
fn claim_warns_by_default_and_refuses_when_required_without_writing() {
    let w = setup();
    w.older_daemon_rewrites_the_record();
    require(true);
    let before = w.objects().get(&w.wt, ".wt/handoff.json").unwrap();
    let e = w.verb("claim", json!({}), 40).unwrap_err();
    assert!(e.contains("older daemon") && e.contains("gate item 8"), "{e}");
    assert_eq!(w.objects().get(&w.wt, ".wt/handoff.json").unwrap(), before, "a refused claim writes nothing");
    require(false);
    let c = w.verb("claim", json!({}), 41).unwrap();
    assert_eq!(c["state"], "claimed");
    assert_eq!(c["older_writer"]["kind"], "handoff_rewritten", "{c}");
}

#[test]
fn promote_warns_by_default_and_refuses_when_required_without_landing() {
    let w = setup();
    let epoch = w.verb("claim", json!({}), 40).unwrap()["epoch"].as_u64().unwrap();
    // the announcement is gone, the claim's own head fields are intact: the fence passes, the check says why
    w.drop_from_record(&["publishing"]);
    require(true);
    let e = w.verb("promote", json!({"epoch": epoch}), 50).unwrap_err();
    assert!(e.contains("older daemon") && e.contains("gate item 8"), "{e}");
    assert!(w.verb("promote", json!({}), 51).unwrap_err().contains("gate item 8"), "unfenced: the record alone says it");
    let h: Value = serde_json::from_slice(&w.objects().get(&w.wt, ".wt/handoff.json").unwrap()).unwrap();
    assert_eq!(h["state"], "claimed", "nothing landed");
    // a dry run lands nothing and is not refused
    assert!(w.verb("promote", json!({"dry_run": true}), 52).is_ok());
    require(false);
    let p = w.verb("promote", json!({"epoch": epoch}), 53).unwrap();
    assert_eq!(p["older_writer"]["kind"], "handoff_rewritten", "the fenced land read the manifest too: {p}");
}

#[test]
fn an_unfenced_promote_sees_only_the_record() {
    require(false);
    let w = setup();
    w.older_daemon_rewrites_the_record();
    let p = w.verb("promote", json!({}), 54).unwrap();
    assert_eq!(p["older_writer"]["kind"], "handoff_no_announcement", "{p}");
}
