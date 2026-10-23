//! P3-K1 review fixes: admitted-vs-key-bound, verdict cache, timeouts,
//! signed fields, close cleanup. Nested in `tests` to reuse its helpers.

use super::*;
use crate::mesh_limits::Limits;
use crate::mesh_serve::serve_listener_with;

fn verified_with_key(pk: [u8; 32], genesis: [u8; 32]) -> VerifiedHello {
    VerifiedHello {
        node_id: node_id_from_pubkey(&pk),
        pubkey: pk,
        platform: "linux".into(),
        capabilities: vec![],
        genesis_hash: genesis,
        ts: now(),
    }
}

fn gate_with(mode: MeshAdmissionMode, v: Verdict) -> (CryptoGate, Arc<Counting>) {
    let (g, src, _) = gate(mode, v);
    (g, src)
}

// ── 1: admitted, not just key-bound ──────────────────────────────

#[tokio::test]
async fn only_enforce_marks_a_peer_admitted() {
    for (mode, expect) in [
        (MeshAdmissionMode::Enforce, true),
        (MeshAdmissionMode::Observe, false),
        (MeshAdmissionMode::Off, false),
    ] {
        let (g, _) = gate_with(mode, Verdict::Permit);
        let Admission::Admit(grant) = g.admit(&verified(1, GENESIS, vec![]), &NOISE_CTX).await
        else {
            panic!("admitted")
        };
        assert_eq!(grant.admitted, expect, "{mode:?}");
        assert_eq!(grant.trust_scope, expect, "{mode:?}");
    }
    let Admission::Admit(grant) = AllowAll.admit(&verified(1, GENESIS, vec![]), &NOISE_CTX).await
    else {
        panic!("admitted")
    };
    assert!(!grant.admitted && !grant.trust_scope);
}

#[tokio::test]
async fn observe_and_allow_all_never_report_node_verified_to_delivery() {
    let k = key(1);
    for (g, expect) in [
        (crypto(MeshAdmissionMode::Enforce) as Arc<dyn AdmissionGate>, true),
        (crypto(MeshAdmissionMode::Observe), false),
        (Arc::new(AllowAll), false),
    ] {
        let srv = server(g, true).await;
        let mut c = Client::connect(&srv.addr, true).await;
        c.send(&c.hello(&k, &GENESIS, vec![]).to_bytes()).await;
        c.publish(&id_of(&k), "t.v").await;
        assert!(wait_for(&srv.rec, "t.v").await);
        let flag = srv.rec.verified.lock().unwrap()[0].1;
        assert_eq!(flag, expect);
    }
}

#[tokio::test]
async fn key_bound_but_not_admitted_peer_still_cannot_spoof_source_node() {
    let k = key(1);
    let srv = server(crypto(MeshAdmissionMode::Observe), true).await;
    let mut c = Client::connect(&srv.addr, true).await;
    c.send(&c.hello(&k, &GENESIS, vec![]).to_bytes()).await;
    c.publish("spoofed", "t.spoof").await;
    c.publish(&id_of(&k), "t.after").await;
    assert!(wait_for(&srv.rec, "t.after").await);
    assert!(!srv.rec.got.lock().unwrap().iter().any(|t| t == "t.spoof"));
}

// ── 3: verdict cache ─────────────────────────────────────────────

#[tokio::test]
async fn cache_key_includes_class_and_platform() {
    let (g, src) = gate_with(MeshAdmissionMode::Enforce, Verdict::Permit);
    let v = verified(1, GENESIS, vec![]);
    let leaf = AdmitContext { class: PeerClass::Leaf, channel: ChannelKind::Noise };
    g.admit(&v, &NOISE_CTX).await;
    g.admit(&v, &leaf).await;
    assert_eq!(src.0.load(Ordering::SeqCst), 2, "class is part of the key");
    let mut other = v.clone();
    other.platform = "esp32".into();
    g.admit(&other, &NOISE_CTX).await;
    assert_eq!(src.0.load(Ordering::SeqCst), 3, "platform is part of the key");
}

#[tokio::test]
async fn unavailable_is_never_cached_and_denials_are_cached_briefly() {
    let (g, src) = gate_with(MeshAdmissionMode::Enforce, Verdict::Unavailable("not ready".into()));
    let v = verified(1, GENESIS, vec![]);
    for _ in 0..2 {
        let r = g.admit(&v, &NOISE_CTX).await;
        assert!(matches!(r, Admission::Refuse(Refusal { code: "verdict_unavailable", .. })));
    }
    assert_eq!((src.0.load(Ordering::SeqCst), g.cache_len()), (2, 0));

    let (g, src) = gate_with(MeshAdmissionMode::Enforce, Verdict::Deny("no".into()));
    g.admit(&v, &NOISE_CTX).await;
    g.admit(&v, &NOISE_CTX).await;
    assert_eq!(src.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cache_is_bounded() {
    let (g, _) = gate_with(MeshAdmissionMode::Enforce, Verdict::Permit);
    for i in 0..1200u32 {
        let mut pk = [0u8; 32];
        pk[..4].copy_from_slice(&i.to_be_bytes());
        g.admit(&verified_with_key(pk, GENESIS), &NOISE_CTX).await;
    }
    assert!(g.cache_len() <= 1024, "cache grew to {}", g.cache_len());
}

#[tokio::test]
async fn enforce_verdict_can_override_the_declared_class() {
    let declared_node = verified(1, GENESIS, vec![]);
    let (e, _) = gate_with(MeshAdmissionMode::Enforce, Verdict::PermitAs(PeerClass::Leaf));
    let Admission::Admit(g) = e.admit(&declared_node, &NOISE_CTX).await else { panic!() };
    assert_eq!((g.class, g.limits, g.trust_scope), (PeerClass::Leaf, PeerLimits::Leaf, false));
    let (o, _) = gate_with(MeshAdmissionMode::Observe, Verdict::PermitAs(PeerClass::Leaf));
    let Admission::Admit(g) = o.admit(&declared_node, &NOISE_CTX).await else { panic!() };
    assert_eq!((g.class, g.limits), (PeerClass::Node, PeerLimits::None));
}

#[cfg(feature = "exochain")]
#[tokio::test]
async fn gate_verdict_source_states() {
    let req = VerdictRequest {
        node_id: "n".into(),
        pubkey: [0; 32],
        class: PeerClass::Node,
        platform: "x".into(),
    };
    let s = GateVerdictSource::late();
    assert!(matches!(s.verdict(&req).await, Verdict::Unavailable(_)));
    s.bind_closed();
    assert!(matches!(s.verdict(&req).await, Verdict::Unavailable(_)));
    assert!(!s.is_open());
    let o = GateVerdictSource::late();
    o.bind_open();
    assert_eq!(o.verdict(&req).await, Verdict::Permit);
    assert!(o.is_open());
}

// ── 4: timeouts and caps ─────────────────────────────────────────

async fn server_with(gate: Arc<dyn AdmissionGate>, limits: Limits) -> Server {
    let rec = Arc::new(Recorder::default());
    let mut rt = MeshRuntime::new("node-b".into());
    rt.set_local_delivery(rec.clone());
    let rt = Arc::new(rt);
    let listener = crate::mesh_tcp::TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(serve_listener_with(
        Arc::clone(&rt), listener, None, "tcp", "x", gate, limits,
    ));
    Server { rt, addr, rec, task, noise_pub: vec![] }
}

fn short(first: u64, per_ip: usize) -> Limits {
    Limits {
        first_frame: Duration::from_millis(first),
        idle: Duration::from_secs(30),
        per_ip,
        ..Limits::default()
    }
}

fn every_mode() -> Vec<(&'static str, Arc<dyn AdmissionGate>)> {
    vec![
        ("allow-all", Arc::new(AllowAll) as Arc<dyn AdmissionGate>),
        ("observe", crypto(MeshAdmissionMode::Observe)),
        ("enforce", crypto(MeshAdmissionMode::Enforce)),
    ]
}

#[tokio::test]
async fn silent_connection_is_dropped_under_every_mode() {
    for (name, g) in every_mode() {
        let srv = server_with(g, short(300, 64)).await;
        let mut c = Client::connect(&srv.addr, false).await;
        assert!(c.closed().await, "{name}: a peer silent before its first frame must be dropped");
    }
}

#[tokio::test]
async fn lenient_gates_keep_no_idle_limit_once_a_frame_arrived() {
    for g in [Arc::new(AllowAll) as Arc<dyn AdmissionGate>, crypto(MeshAdmissionMode::Observe)] {
        let srv = server_with(g, short(300, 64)).await;
        let mut c = Client::connect(&srv.addr, false).await;
        c.publish("legacy-leaf", "t.hello").await;
        assert!(wait_for(&srv.rec, "t.hello").await);
        let r = tokio::time::timeout(Duration::from_millis(900), c.ch.recv_encrypted()).await;
        assert!(r.is_err(), "a quiet legacy peer is not cut off after its first frame");
    }
}

#[tokio::test]
async fn per_ip_cap_refuses_the_extra_connection_in_every_mode() {
    for (name, g) in every_mode() {
        let srv = server_with(g, short(30_000, 2)).await;
        let _a = Client::connect(&srv.addr, false).await;
        let _b = Client::connect(&srv.addr, false).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut third = Client::connect(&srv.addr, false).await;
        assert!(third.closed().await, "{name}: third connection from one IP must be dropped");
    }
}

// ── responder join on handshake ──────────────────────────────────

#[tokio::test]
async fn responder_registers_an_admitted_peer_without_any_app_traffic() {
    let srv = server(crypto(MeshAdmissionMode::Enforce), true).await;
    let mut events = srv.rt.subscribe_peer_events();
    let k = key(1);
    let mut c = Client::connect(&srv.addr, true).await;
    // Hello only: no envelope follows.
    c.send(&c.hello(&k, &GENESIS, vec![]).to_bytes()).await;
    let ev = tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .expect("join event")
        .unwrap();
    assert!(matches!(&ev, crate::mesh_discovery::MeshPeerEvent::Joined { node_id, .. } if *node_id == id_of(&k)), "{ev:?}");
    assert_eq!(srv.rt.peer_ids(), vec![id_of(&k)]);
    let _ = c.ch.close().await;
}

#[tokio::test]
async fn under_observe_no_one_joins_at_the_handshake_and_the_first_envelope_joins_as_a_member() {
    use crate::mesh_discovery::MeshPeerEvent;
    let srv = server(crypto(MeshAdmissionMode::Observe), true).await;
    let mut events = srv.rt.subscribe_peer_events();
    let k = key(1);
    let mut c = Client::connect(&srv.addr, true).await;
    c.send(&c.hello(&k, &GENESIS, vec![]).to_bytes()).await;
    // A valid hello under observe is key possession, not membership: no
    // early join (compare responder_registers_an_admitted_peer_...).
    let early = tokio::time::timeout(Duration::from_millis(400), events.recv()).await;
    assert!(early.is_err(), "no join before the first envelope: {early:?}");
    assert!(srv.rt.peer_ids().is_empty());
    c.publish(&id_of(&k), "t.first").await;
    assert!(wait_for(&srv.rec, "t.first").await);
    let ev = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
    assert!(
        // `verified: true` here means "counts as a member": observe does
        // not exclude anyone (the runtime is not enforcing).
        matches!(&ev, MeshPeerEvent::Joined { node_id, verified: true, .. } if *node_id == id_of(&k)),
        "{ev:?}"
    );
    let _ = c.ch.close().await;
}

#[tokio::test]
async fn under_enforce_the_join_event_is_verified() {
    use crate::mesh_discovery::MeshPeerEvent;
    let srv = server(crypto(MeshAdmissionMode::Enforce), true).await;
    let mut events = srv.rt.subscribe_peer_events();
    let k = key(2);
    let mut c = Client::connect(&srv.addr, true).await;
    c.send(&c.hello(&k, &GENESIS, vec![]).to_bytes()).await;
    let ev = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
    assert!(matches!(&ev, MeshPeerEvent::Joined { verified: true, .. }), "{ev:?}");
    let _ = c.ch.close().await;
}

// ── 7: signed fields ─────────────────────────────────────────────

#[test]
fn capabilities_platform_and_id_are_signed() {
    let (h, s) = ([1u8; 32], [2u8; 32]);
    let good = AdmitHello::sign(&key(1), &h, &s, &GENESIS, now(), "linux", caps(&[CAP_LEAF]));
    assert!(good.verify(Some(&binding(&h, &s)), now()).is_ok());
    let mut t = good.clone();
    t.capabilities = vec![];
    assert_eq!(t.verify(Some(&binding(&h, &s)), now()), Err(HelloFailure::BadSignature));
    let mut t = good.clone();
    t.platform = "other".into();
    assert_eq!(t.verify(Some(&binding(&h, &s)), now()), Err(HelloFailure::BadSignature));
    let mut t = good;
    t.capabilities.push("extra".into());
    assert_eq!(t.verify(Some(&binding(&h, &s)), now()), Err(HelloFailure::BadSignature));
}

#[test]
fn skew_is_checked_before_the_signature() {
    let (h, s) = ([1u8; 32], [2u8; 32]);
    let mut hello = hello_for(&key(1), &h, &s, &GENESIS, now() - MAX_SKEW_SECS - 10);
    hello.sig = hello_for(&key(9), &h, &s, &GENESIS, now()).sig;
    assert_eq!(hello.verify(Some(&binding(&h, &s)), now()), Err(HelloFailure::ClockSkew));
}

// ── 9: close cleanup ─────────────────────────────────────────────

#[tokio::test]
async fn route_is_removed_when_the_connection_closes() {
    let srv = server(crypto(MeshAdmissionMode::Enforce), true).await;
    let k = key(1);
    let mut c = Client::connect(&srv.addr, true).await;
    c.send(&c.hello(&k, &GENESIS, vec![]).to_bytes()).await;
    c.publish(&id_of(&k), "t.up").await;
    assert!(wait_for(&srv.rec, "t.up").await);
    assert_eq!(srv.rt.peer_ids(), vec![id_of(&k)]);
    let _ = c.ch.close().await;
    drop(c);
    for _ in 0..150 {
        if srv.rt.peer_ids().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("dead route left behind: {:?}", srv.rt.peer_ids());
}
