//! Two in-process nodes exchange signed facts and deltas over the mesh
//! runtime; the receiver assigns the tier and caps the provenance, and a
//! forged block is refused. Nothing here touches the operator's runtime
//! directory or chain.

use super::*;
use crate::cluster::ClusterConfig;
use crate::mesh_admit::PeerClass;
use crate::node_facts_advert::{FACTS_DOMAIN, verify_node_facts};
use clawft_types::placement::{Capability, CapabilityId};
use ed25519_dalek::Signer;

struct TestNode {
    id: String,
    ex: Arc<FactsExchange>,
    rt: Arc<MeshRuntime>,
}

fn node(n: u8) -> TestNode {
    let key = SigningKey::from_bytes(&[n; 32]);
    let rt = Arc::new(MeshRuntime::new(node_id_from_pubkey(
        &key.verifying_key().to_bytes(),
    )));
    let membership = Arc::new(ClusterMembership::new(ClusterConfig::default()));
    let ex = FactsExchange::new(key, membership, rt.clone(), FactsTrustPolicy::default());
    ex.start();
    TestNode {
        id: ex.node_id().to_string(),
        ex,
        rt,
    }
}

fn ctx(peer: &str, verified: bool) -> PeerCtx {
    if verified {
        PeerCtx {
            peer_id: peer.to_string(),
            node_verified: true,
            class: PeerClass::Node,
            remote_static: None,
            src_scope: None,
        }
    } else {
        PeerCtx::unauthenticated(peer)
    }
}

/// Connect `a` and `b` in process. `b_verifies_a` / `a_verifies_b` say
/// whether each side's admission marked the other verified.
fn link(a: &TestNode, b: &TestNode, a_verifies_b: bool, b_verifies_a: bool) {
    let (tx_ab, mut rx_ab) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (tx_ba, mut rx_ba) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (b_rt, a_id, back) = (b.rt.clone(), a.id.clone(), tx_ba.clone());
    tokio::spawn(async move {
        while let Some(bytes) = rx_ab.recv().await {
            let c = ctx(&a_id, b_verifies_a);
            let _ = b_rt.handle_incoming_peer(&bytes, back.clone(), Some(&c)).await;
        }
    });
    let (a_rt, b_id, back) = (a.rt.clone(), b.id.clone(), tx_ab.clone());
    tokio::spawn(async move {
        while let Some(bytes) = rx_ba.recv().await {
            let c = ctx(&b_id, a_verifies_b);
            let _ = a_rt.handle_incoming_peer(&bytes, back.clone(), Some(&c)).await;
        }
    });
    a.rt.add_peer(b.id.clone(), tx_ab);
    b.rt.add_peer(a.id.clone(), tx_ba);
}

fn cap(id: &str, p: Provenance) -> Capability {
    Capability::new(CapabilityId::new(id).unwrap(), p)
}

fn facts_of(n: &TestNode, caps: Vec<Capability>) -> NodeFacts {
    let mut f = NodeFacts::new(n.id.clone(), now(), 600, 0);
    f.capabilities = caps;
    f
}

fn now() -> u64 {
    FactsExchange::now()
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..400 {
        if ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

fn held(on: &TestNode, of: &TestNode) -> Option<crate::node_facts::CachedNodeFacts> {
    on.ex.membership().facts().get(&of.id, now())
}

fn sample(a: &TestNode) -> NodeFacts {
    facts_of(
        a,
        vec![
            cap("cpu.arch.aarch64", Provenance::Probed),
            cap("accel.tpu.coral", Provenance::Probed).exclusive(),
            cap("perf.cog.cycle_ms", Provenance::Measured).with_attr("value", 790.0),
        ],
    )
}

#[tokio::test]
async fn verified_peer_facts_are_paired_and_never_measured() {
    let (a, b) = (node(1), node(2));
    link(&a, &b, true, true);
    let seq = a.ex.publish(sample(&a), now()).await.unwrap();

    wait_for("b to cache a's facts", || held(&b, &a).is_some()).await;
    let got = held(&b, &a).unwrap();
    assert_eq!(got.trust_tier(), TrustTier::Paired);
    assert_eq!(got.facts.seq, seq);
    assert!(
        got.capabilities().iter().all(|c| c.provenance <= Provenance::Probed),
        "a remote node's measured claim must not be held as measured"
    );
    let perf = got.facts.find("perf.cog.cycle_ms").next().unwrap();
    assert_eq!(perf.provenance, Provenance::Probed);
    // The envelope is kept as received, so it still verifies and still says measured.
    let original = verify_node_facts(&got.signed, now()).unwrap();
    assert_eq!(
        original.find("perf.cog.cycle_ms").next().unwrap().provenance,
        Provenance::Measured
    );
    // The sender's own cache holds its facts as the local node.
    assert_eq!(held(&a, &a).unwrap().trust_tier(), TrustTier::Pinned);
}

#[tokio::test]
async fn unverified_or_observe_mode_peer_gets_the_lowest_tier_and_claimed_data() {
    let (a, b) = (node(3), node(4));
    link(&a, &b, false, false); // observe mode: no peer is marked verified
    a.ex.publish(sample(&a), now()).await.unwrap();

    wait_for("b to cache a's facts", || held(&b, &a).is_some()).await;
    let got = held(&b, &a).unwrap();
    assert_eq!(got.trust_tier(), TrustTier::Discovered);
    assert!(got.capabilities().iter().all(|c| c.provenance == Provenance::Claimed));
}

#[tokio::test]
async fn deltas_carry_live_state_and_a_changed_shape_rebases() {
    let (a, b) = (node(5), node(6));
    link(&a, &b, true, true);
    let base = sample(&a);
    let s1 = a.ex.publish(base.clone(), now()).await.unwrap();
    wait_for("base", || held(&b, &a).is_some()).await;

    // Nothing changed: nothing sent.
    assert_eq!(a.ex.update_live(base.clone(), now()).await.unwrap(), Published::Nothing);

    // The Coral is now busy: a delta, not a new base.
    let mut busy = base.clone();
    busy.capabilities[1].state = CapabilityState::Busy;
    let p = a.ex.update_live(busy.clone(), now()).await.unwrap();
    assert_eq!(p, Published::Delta { seq: 1, changes: 1 });
    wait_for("delta", || {
        held(&b, &a).is_some_and(|c| c.delta_seq == 1)
    })
    .await;
    let got = held(&b, &a).unwrap();
    assert_eq!(got.facts.seq, s1, "a delta does not change the base");
    assert_eq!(got.facts.capabilities[1].state, CapabilityState::Busy);
    assert_eq!(got.load().busy, 1);

    // Free again, with free memory reported: another delta.
    let mut free = base.clone();
    free.capabilities.push(cap("mem.system", Provenance::Probed).with_attr("free", 100i64));
    // A new capability is a shape change: a new base, higher seq.
    let p = a.ex.update_live(free, now()).await.unwrap();
    let Published::Base(s2) = p else { panic!("expected a new base, got {p:?}") };
    assert!(s2 > s1);
    wait_for("rebase", || held(&b, &a).is_some_and(|c| c.facts.seq == s2)).await;
    let got = held(&b, &a).unwrap();
    assert_eq!(got.delta_seq, 0);
    assert_eq!(got.facts.capabilities[1].state, CapabilityState::Available);
}

#[tokio::test]
async fn a_peer_that_joins_later_is_sent_the_current_facts_and_delta() {
    let (a, b) = (node(7), node(8));
    let base = sample(&a);
    a.ex.publish(base.clone(), now()).await.unwrap();
    let mut busy = base;
    busy.capabilities[1].state = CapabilityState::Busy;
    a.ex.update_live(busy, now()).await.unwrap();

    link(&a, &b, true, true); // join after the fact
    wait_for("announce on join", || {
        held(&b, &a).is_some_and(|c| c.delta_seq == 1)
    })
    .await;
    assert_eq!(held(&b, &a).unwrap().load().busy, 1);
}

#[tokio::test]
async fn forged_facts_are_rejected() {
    let (a, b, c) = (node(9), node(10), node(11));
    let good = sign_node_facts(&sample(&a), &SigningKey::from_bytes(&[9; 32])).unwrap();
    let mut replies = Vec::new();
    let from_a = ctx(&a.id, true);

    // Payload altered after signing.
    let mut tampered = good.clone();
    tampered.payload = tampered.payload.replace("790", "1");
    let r = b.ex.ingest(&from_a, FactsWire::Facts { signed: tampered }, now(), &mut replies);
    assert!(matches!(r, Err(IngestError::Cache(CacheError::Verify(NodeFactsAdvertError::BadSignature)))), "{r:?}");

    // Signature bit flipped.
    let mut flipped = good.clone();
    flipped.signature[0] ^= 1;
    let r = b.ex.ingest(&from_a, FactsWire::Facts { signed: flipped }, now(), &mut replies);
    assert!(r.is_err());

    // C re-signs A's facts with its own key: a valid signature, but the key
    // does not own A's node id.
    let ckey = SigningKey::from_bytes(&[11; 32]);
    let mut spoof = good.clone();
    spoof.public_key = ckey.verifying_key().to_bytes().to_vec();
    let mut msg = FACTS_DOMAIN.to_vec();
    msg.extend_from_slice(spoof.payload.as_bytes());
    spoof.signature = ckey.sign(&msg).to_bytes().to_vec();
    let r = b.ex.ingest(&from_a, FactsWire::Facts { signed: spoof }, now(), &mut replies);
    assert!(matches!(r, Err(IngestError::Cache(CacheError::Verify(NodeFactsAdvertError::NodeMismatch { .. })))), "{r:?}");

    assert!(held(&b, &a).is_none(), "nothing forged may be cached");

    // Genuine facts, but relayed by a connection that is not their subject.
    let from_c = ctx(&c.id, true);
    let r = b.ex.ingest(&from_c, FactsWire::Facts { signed: good.clone() }, now(), &mut replies);
    assert!(matches!(r, Err(IngestError::NotSender { .. })), "{r:?}");

    // Nobody can replace this node's own facts from the wire.
    let own = sign_node_facts(&facts_of(&b, vec![]), &SigningKey::from_bytes(&[10; 32])).unwrap();
    let r = b.ex.ingest(&ctx(&b.id, true), FactsWire::Facts { signed: own }, now(), &mut replies);
    assert_eq!(r, Err(IngestError::LocalNode));

    // And the genuine block from its owner is accepted.
    assert!(b.ex.ingest(&from_a, FactsWire::Facts { signed: good }, now(), &mut replies).is_ok());
}

fn signed_for(n: u8, seq: u64) -> (String, SignedNodeFacts) {
    let k = SigningKey::from_bytes(&[n; 32]);
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    let mut f = NodeFacts::new(id.clone(), now(), 600, seq);
    f.capabilities = vec![cap("cpu.arch.aarch64", Provenance::Probed)];
    (id, sign_node_facts(&f, &k).unwrap())
}

#[tokio::test]
async fn the_tier_follows_the_current_connection_and_old_facts_cannot_roll_back() {
    let (a, b) = (node(12), node(13));
    let ka = SigningKey::from_bytes(&[12; 32]);
    let mut replies = Vec::new();
    let mut f1 = sample(&a);
    f1.seq = 10;
    let s1 = sign_node_facts(&f1, &ka).unwrap();
    let mut f2 = sample(&a);
    f2.seq = 11;
    let s2 = sign_node_facts(&f2, &ka).unwrap();

    let out = b.ex.ingest(&ctx(&a.id, true), FactsWire::Facts { signed: s1.clone() }, now(), &mut replies).unwrap();
    assert!(matches!(out, IngestOutcome::Accepted { tier: TrustTier::Paired, .. }));
    // Newer facts over a connection that is no longer verified: Discovered.
    // (Admission is re-derived on every frame; it is not remembered.)
    let out = b.ex.ingest(&ctx(&a.id, false), FactsWire::Facts { signed: s2 }, now(), &mut replies).unwrap();
    assert!(matches!(out, IngestOutcome::Accepted { tier: TrustTier::Discovered, outcome: InsertOutcome::Replaced, .. }), "{out:?}");
    assert_eq!(held(&b, &a).unwrap().trust_tier(), TrustTier::Discovered);
    // Replaying the older block is stale.
    let r = b.ex.ingest(&ctx(&a.id, true), FactsWire::Facts { signed: s1 }, now(), &mut replies);
    assert!(matches!(r, Err(IngestError::Cache(CacheError::Stale { .. }))), "{r:?}");
}

#[tokio::test]
async fn an_unchanged_block_over_an_unverified_connection_also_lowers_the_tier() {
    let (a, b) = (node(20), node(21));
    let s = sign_node_facts(&sample(&a), &SigningKey::from_bytes(&[20; 32])).unwrap();
    let mut r = Vec::new();
    b.ex.ingest(&ctx(&a.id, true), FactsWire::Facts { signed: s.clone() }, now(), &mut r).unwrap();
    assert_eq!(held(&b, &a).unwrap().trust_tier(), TrustTier::Paired);
    let out = b.ex.ingest(&ctx(&a.id, false), FactsWire::Facts { signed: s }, now(), &mut r).unwrap();
    assert!(matches!(out, IngestOutcome::Accepted { outcome: InsertOutcome::Unchanged, tier: TrustTier::Discovered, .. }), "{out:?}");
}

#[tokio::test]
async fn admitted_then_revoked_goes_back_to_discovered() {
    let (a, b) = (node(14), node(15));
    link(&a, &b, true, true);
    a.ex.publish(sample(&a), now()).await.unwrap();
    wait_for("paired", || held(&b, &a).is_some_and(|c| c.trust_tier() == TrustTier::Paired)).await;
    // `weaver mesh peer revoke` closes the connection: the runtime emits Left.
    b.rt.disconnect_peer(&a.id);
    wait_for("demoted", || held(&b, &a).is_some_and(|c| c.trust_tier() == TrustTier::Discovered)).await;
    assert!(held(&b, &a).is_some(), "the signed facts stay; only the tier drops");
}

#[tokio::test]
async fn an_operator_set_tier_survives_mesh_frames_and_disconnects() {
    let (a, b) = (node(16), node(17));
    let ka = SigningKey::from_bytes(&[16; 32]);
    let mut r = Vec::new();
    let mut f = sample(&a);
    f.seq = 1;
    b.ex.ingest(&ctx(&a.id, true), FactsWire::Facts { signed: sign_node_facts(&f, &ka).unwrap() }, now(), &mut r).unwrap();
    assert!(b.ex.membership().facts().set_trust_tier(&a.id, TrustTier::Pinned));
    f.seq = 2;
    let out = b.ex.ingest(&ctx(&a.id, false), FactsWire::Facts { signed: sign_node_facts(&f, &ka).unwrap() }, now(), &mut r).unwrap();
    assert!(matches!(out, IngestOutcome::Accepted { tier: TrustTier::Pinned, .. }), "{out:?}");
    assert!(!b.ex.on_peer_gone(&a.id), "an operator tier is not demoted");
    assert_eq!(held(&b, &a).unwrap().trust_tier(), TrustTier::Pinned);
}

#[tokio::test]
async fn one_unverified_connection_cannot_mint_more_than_its_quota() {
    let b = node(30);
    let mut r = Vec::new();
    for n in 100..100 + MAX_DISCOVERED_PER_CONN as u8 {
        let (id, s) = signed_for(n, 1);
        b.ex.ingest_from(&ctx(&id, false), 7, FactsWire::Facts { signed: s }, now(), &mut r).unwrap();
    }
    let (id, s) = signed_for(200, 1);
    let e = b.ex.ingest_from(&ctx(&id, false), 7, FactsWire::Facts { signed: s.clone() }, now(), &mut r);
    assert!(matches!(e, Err(IngestError::Quota { conn: 7, .. })), "{e:?}");
    // Another connection has its own quota; a verified one is not limited by it.
    assert!(b.ex.ingest_from(&ctx(&id, false), 8, FactsWire::Facts { signed: s }, now(), &mut r).is_ok());
    let (vid, vs) = signed_for(201, 1);
    assert!(b.ex.ingest_from(&ctx(&vid, true), 7, FactsWire::Facts { signed: vs }, now(), &mut r).is_ok());
    // An id it already holds can still be refreshed.
    let (id0, s0) = signed_for(100, 2);
    assert!(b.ex.ingest_from(&ctx(&id0, false), 7, FactsWire::Facts { signed: s0 }, now(), &mut r).is_ok());
}

#[tokio::test]
async fn a_flooding_connection_is_cut_off_before_anything_is_verified() {
    let b = node(31);
    let (id, s) = signed_for(110, 1);
    let ctxu = ctx(&id, false);
    // Garbage frames drain the budget (the parse fails, but each is charged first).
    let junk = serde_json::json!({"op": "nonsense"});
    for _ in 0..(2.0 * FRAMES_PER_SEC) as usize {
        assert!(b.ex.on_peer_control(&ctxu, 55, &junk).is_empty());
    }
    // Now a valid, signed block on the same connection is dropped unread.
    let wire = serde_json::to_value(FactsWire::Facts { signed: s.clone() }).unwrap();
    assert!(b.ex.on_peer_control(&ctxu, 55, &wire).is_empty());
    assert!(held_by_id(&b, &id).is_none(), "over-budget frame was not verified or cached");
    // Another connection is unaffected.
    b.ex.on_peer_control(&ctxu, 56, &wire);
    assert!(held_by_id(&b, &id).is_some());
}

fn held_by_id(on: &TestNode, id: &str) -> Option<crate::node_facts::CachedNodeFacts> {
    on.ex.membership().facts().get(id, now())
}

#[test]
fn a_full_cache_evicts_discovered_before_refusing_and_never_pushes_out_paired() {
    use crate::node_facts::cache::MAX_CACHED_NODES;
    let cache = crate::node_facts::NodeFactsCache::new();
    let mk = |n: u32| {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        b[31] = 1;
        let k = SigningKey::from_bytes(&b);
        let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
        let mut f = NodeFacts::new(id, now(), 600, 1);
        f.capabilities = vec![cap("cpu.arch.aarch64", Provenance::Probed)];
        sign_node_facts(&f, &k).unwrap()
    };
    // Discovered entries are capped as a class: past the cap a newcomer
    // evicts instead of failing, and a Paired one is simply added.
    use crate::node_facts::cache::MAX_DISCOVERED;
    for n in 0..(MAX_DISCOVERED as u32 + 100) {
        cache.insert_remote(mk(n), TrustTier::Discovered, Provenance::Claimed, 1 + (n as u64 % 200), now() + (n as u64 % 5)).unwrap();
    }
    assert_eq!(cache.len(), MAX_DISCOVERED);
    cache.insert_remote(mk(9_000), TrustTier::Paired, Provenance::Probed, 2, now() + 10).unwrap();
    assert_eq!(cache.len(), MAX_DISCOVERED + 1);
    // Full of Paired: Discovered cannot displace them.
    let full = crate::node_facts::NodeFactsCache::new();
    for n in 0..MAX_CACHED_NODES as u32 {
        full.insert_remote(mk(n), TrustTier::Paired, Provenance::Probed, 1, now()).unwrap();
    }
    let r = full.insert_remote(mk(9_001), TrustTier::Discovered, Provenance::Claimed, 2, now());
    assert_eq!(r, Err(CacheError::Full));
    assert_eq!(full.len(), MAX_CACHED_NODES);
}

#[tokio::test]
async fn a_delta_without_its_base_asks_for_the_facts() {
    let (a, b) = (node(14), node(15));
    let base = sample(&a);
    a.ex.publish(base.clone(), now()).await.unwrap();
    let mut busy = base;
    busy.capabilities[1].state = CapabilityState::Busy;
    a.ex.update_live(busy, now()).await.unwrap();
    let delta = a.ex.current().into_iter().find(|w| matches!(w, FactsWire::Delta { .. })).unwrap();

    let mut replies = Vec::new();
    let r = b.ex.ingest(&ctx(&a.id, true), delta, now(), &mut replies);
    assert!(matches!(r, Err(IngestError::Cache(CacheError::UnknownNode(_)))), "{r:?}");
    assert_eq!(replies, vec![FactsWire::Request]);

    // A answers the request with its base and latest delta.
    let mut answers = Vec::new();
    a.ex.ingest(&ctx(&b.id, true), FactsWire::Request, now(), &mut answers).unwrap();
    assert_eq!(answers.len(), 2);
    for w in answers {
        b.ex.ingest(&ctx(&a.id, true), w, now(), &mut Vec::new()).unwrap();
    }
    assert_eq!(held(&b, &a).unwrap().load().busy, 1);
}

#[test]
fn a_reconnecting_attacker_cannot_evict_a_real_discovered_peer() {
    use crate::node_facts::cache::MAX_DISCOVERED;
    let cache = crate::node_facts::NodeFactsCache::new();
    let mk = |n: u32| {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        b[31] = 7;
        let k = SigningKey::from_bytes(&b);
        let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
        let mut f = NodeFacts::new(id.clone(), now(), 600, 1);
        f.capabilities = vec![cap("cpu.arch.aarch64", Provenance::Probed)];
        (id, sign_node_facts(&f, &k).unwrap())
    };
    // A real, unverified leaf (connection 1) arrives first, one entry.
    let (real, s) = mk(0);
    cache.insert_remote(s, TrustTier::Discovered, Provenance::Claimed, 1, now()).unwrap();
    // The attacker reconnects over and over, four fresh ids per connection,
    // far past the global cap.
    let mut n = 1;
    for conn in 100..100 + (3 * MAX_DISCOVERED as u64 / 4) {
        for _ in 0..4 {
            let (_, s) = mk(n);
            n += 1;
            cache.insert_remote(s, TrustTier::Discovered, Provenance::Claimed, conn, now() + 1).unwrap();
        }
    }
    assert!(cache.get(&real, now()).is_some(), "the real peer survives the churn");
    let held = cache.list(now()).into_iter().filter(|c| c.trust_tier() == TrustTier::Discovered).count();
    assert!(held <= MAX_DISCOVERED, "{held}");
    // A verified peer is never displaced either.
    let (paired, s) = mk(999_999);
    cache.insert_remote(s, TrustTier::Paired, Provenance::Probed, 2, now() + 2).unwrap();
    for conn in 5_000..5_100u64 {
        for _ in 0..4 {
            let (_, s) = mk(n);
            n += 1;
            cache.insert_remote(s, TrustTier::Discovered, Provenance::Claimed, conn, now() + 3).unwrap();
        }
    }
    assert!(cache.get(&paired, now()).is_some());
    assert!(cache.get(&real, now()).is_some());
}

#[tokio::test]
async fn the_byte_budget_is_charged_on_the_whole_frame() {
    let b = node(40);
    // A frame whose signed payload is tiny but which carries a large extra
    // field: charged by total size, so a few of them exhaust the byte budget.
    let big = serde_json::json!({"op": "request", "pad": "x".repeat(200_000)});
    let c = ctx("node-x", false);
    let mut dropped = 0;
    for _ in 0..8 {
        // `allow` is what the sink calls first: count refusals through it.
        if !b.ex.allow(77, super::json_size(&big)) {
            dropped += 1;
        }
    }
    assert!(dropped >= 3, "4 x 256 KiB burst, then refill: {dropped} dropped");
    let _ = c;
}

fn busy(base: &NodeFacts) -> NodeFacts {
    let mut b = base.clone();
    b.capabilities[1].state = CapabilityState::Busy;
    b
}

fn feed(rx: &TestNode, from: &TestNode, wires: Vec<FactsWire>, t: u64) -> Vec<Result<IngestOutcome, IngestError>> {
    let c = ctx(&from.id, false);
    wires
        .into_iter()
        .map(|w| rx.ex.ingest(&c, w, t, &mut Vec::new()))
        .collect()
}

#[tokio::test]
async fn a_re_announce_replays_the_delta_as_a_benign_duplicate() {
    let (a, b) = (node(21), node(22));
    let base = sample(&a);
    a.ex.publish(base.clone(), now()).await.unwrap();
    a.ex.update_live(busy(&base), now()).await.unwrap();

    // The first announce applies; every later send_current is a replay.
    let first = feed(&b, &a, a.ex.current(), now());
    assert!(first.iter().all(Result::is_ok), "{first:?}");
    for _ in 0..2 {
        let again = feed(&b, &a, a.ex.current(), now());
        assert!(matches!(again[0], Ok(IngestOutcome::Accepted { outcome: InsertOutcome::Unchanged, .. })), "{again:?}");
        assert!(
            matches!(again[1], Err(IngestError::Cache(CacheError::DuplicateDelta { .. }))),
            "{again:?}"
        );
    }
    assert_eq!(held(&b, &a).unwrap().load().busy, 1);
}

#[tokio::test]
async fn a_restarted_peer_starts_over_at_delta_seq_1_without_being_refused() {
    // Same key, fresh sender state: base seq follows the clock, delta seq
    // is back at 1, and the content differs from the old incarnation's.
    let (a1, b) = (node(23), node(24));
    let base = sample(&a1);
    let t0 = now();
    a1.ex.publish(base.clone(), t0).await.unwrap();
    a1.ex.update_live(busy(&base), t0).await.unwrap();
    assert!(feed(&b, &a1, a1.ex.current(), t0).iter().all(Result::is_ok));

    let a2 = node(23);
    let t1 = t0 + 5;
    a2.ex.publish(base.clone(), t1).await.unwrap();
    let mut other = base.clone();
    other.capabilities[0].state = CapabilityState::Busy;
    a2.ex.update_live(other, t1).await.unwrap();
    let new = a2.ex.current();

    // The delta alone (its base lost in transit) asks for the base; no fault.
    let lost = feed(&b, &a2, vec![new[1].clone()], t1);
    assert!(matches!(lost[0], Err(IngestError::Cache(CacheError::BaseBehind { .. }))), "{lost:?}");
    // Base then delta: the new incarnation is accepted.
    let ok = feed(&b, &a2, new, t1);
    assert!(ok.iter().all(Result::is_ok), "{ok:?}");
    let got = held(&b, &a1).unwrap();
    assert_eq!((got.delta_seq, got.capabilities()[0].state), (1, CapabilityState::Busy));
}
