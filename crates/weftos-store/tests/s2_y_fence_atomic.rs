//! WO-069 lane Y (card 1eb8c7e8): the claim fence holds across every interleaving of a fenced
//! finalisation with a claim or takeover, and on what was claimed, not only on the head. Design:
//! `reports/wo069-fence-atomicity-design-20261115.md`. Interleavings are driven deterministically by a
//! backend hook that fires just before the first conditional write of the handoff record, that is
//! after the caller has read and decided and before its write lands. Single-threaded.
use weftos_store::overlay::{ObjectBackend, Result as R, S2Error};
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-bbbbbbbbbbbb";
const NOW: u64 = 1_000_000_000;

fn who(n: &str) -> Actor {
    Actor { agent: n.into(), host: "h".into() }
}

/// Publish `files` at `head` (`uncommitted` names the files not in `head`).
fn publish(b: &MemBackend, head: &str, files: &[(&str, &str)], uncommitted: &[&str], now: u64) -> R<PublishReport> {
    let d = tempfile::tempdir().unwrap();
    for (p, c) in files {
        std::fs::write(d.path().join(p), c).unwrap();
    }
    let ps: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    let unc: Vec<String> = uncommitted.iter().map(|p| p.to_string()).collect();
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &unc,
        changes: &[],
        branch: "feat/y",
        base_commit: "base",
        head_commit: head,
        bundle: None,
        writer: who("writer"),
    };
    publish_tree(b, WT, &src, &Limits::default(), now)
}

/// Published at h1 (with an uncommitted `notes.md`) and handed off.
fn ready() -> MemBackend {
    let b = MemBackend::new();
    publish(&b, "h1", &[("a.md", "a"), ("notes.md", "v1")], &["notes.md"], NOW).unwrap();
    mark_handoff(&b, WT, &who("writer"), "ready", NOW).unwrap();
    b
}

/// Drop `field` from the stored handoff record, as a binary from before that field wrote it.
fn strip(b: &MemBackend, field: &str) {
    let mut v: serde_json::Value = serde_json::from_slice(&ObjectBackend::get(b, WT, HANDOFF_KEY).unwrap().unwrap()).unwrap();
    v.as_object_mut().unwrap().remove(field);
    ObjectBackend::put(b, WT, HANDOFF_KEY, &serde_json::to_vec(&v).unwrap()).unwrap();
}

fn state(b: &MemBackend) -> Handoff {
    read_handoff(b, WT).unwrap().unwrap()
}

// ---------------------------------------------------------------------------------------------------------------------
// A2 (item 3): a claim recorded before `claimed_head` existed cannot be merged; the claimer claims again
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_claim_without_a_claimed_head_is_refused_by_every_fenced_path_and_a_reclaim_merges() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    strip(&b, "claimed_head");
    assert_eq!(state(&b).claimed_head, None);
    for e in [
        mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW).unwrap_err(),
        mark_merged(&b, WT, &who("bob"), "m", NOW).unwrap_err(),
        fence_claim(&b, WT, c.epoch).unwrap_err(),
    ] {
        assert!(matches!(e, S2Error::Claimed(_)), "{e}");
        assert!(e.to_string().contains("claim again"), "the error names the next step: {e}");
    }
    assert_eq!(state(&b).state, State::Claimed, "nothing was written");
    // the holder re-claims: a new epoch, the head recorded, and that claim merges
    let again = claim_merge(&b, WT, &who("bob"), NOW + 1).unwrap();
    assert_eq!((again.epoch, again.claimed_head.as_deref()), (c.epoch + 1, Some("h1")));
    mark_merged_at(&b, WT, &who("bob"), Some(again.epoch), "m", NOW + 2).unwrap();
}

#[test]
fn an_unfenced_land_is_not_stopped_by_a_claim_without_a_claimed_head() {
    let b = ready();
    claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    strip(&b, "claimed_head");
    // promote without --epoch: git is the truth, as before (design note B6)
    assert!(mark_landed(&b, WT, &who("bob"), "landed", NOW).unwrap());
    assert_eq!(state(&b).state, State::Merged);
}
