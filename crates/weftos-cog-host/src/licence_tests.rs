//! Tests of the start-time licence check: `check_start` against fake gates,
//! the supervisor with a fake gate, and `HostLicence` over the kernel stores
//! with records signed in the test.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use clawft_kernel::licence::{
    Approval, BindState, BindingRecord, CheckoutGrant, GrantArtifact, LicenceRef, sign_approval, sign_binding,
    sign_grant,
};
use clawft_kernel::mesh_swarm_revoke::sign_revocation;
use clawft_kernel::revocation::RevocationKind;
use ed25519_dalek::SigningKey;

use super::*;
use crate::supervise::Supervisor;

// ---- fakes -----------------------------------------------------------------

/// A gate with a fixed answer that records what it was asked.
struct FakeGate {
    answer: Mutex<Result<RunVerdict, RunRefusal>>,
    claims: bool,
    revoked: Mutex<Option<String>>,
    asked: Mutex<Vec<(String, String, String, String)>>,
}

impl FakeGate {
    fn new(answer: Result<RunVerdict, RunRefusal>) -> Self {
        Self { answer: Mutex::new(answer), claims: false, revoked: Mutex::new(None), asked: Mutex::new(Vec::new()) }
    }
    fn set(&self, answer: Result<RunVerdict, RunRefusal>) {
        *self.answer.lock().unwrap() = answer;
    }
    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl CognitumRunGate for FakeGate {
    fn check(&self, r: &RunRequest<'_>) -> Result<RunVerdict, RunRefusal> {
        self.asked.lock().unwrap().push((r.cog_id.into(), r.version.into(), r.sha256.into(), r.blake3.into()));
        self.answer.lock().unwrap().clone()
    }
    fn claims(&self, _: &str, _: &str) -> bool {
        self.claims
    }
    fn revoked(&self, b3: &str) -> bool {
        self.revoked.lock().unwrap().as_deref() == Some(b3)
    }
}

fn permit() -> RunVerdict {
    RunVerdict::Permit(RunPermit { grant_id: "g-1".into(), approval_id: "a-1".into(), blake3: "ignored".into() })
}

fn rec(source: Source) -> CogRecord {
    CogRecord {
        id: "fall-detect".into(),
        version: "1.2.0".into(),
        source,
        enabled: false,
        binary: "cog-fall-detect-arm".into(),
        args: vec![],
        signed: false,
    }
}

// ---- check_start -------------------------------------------------------------

#[test]
fn a_non_cognitum_cog_never_reaches_the_gate() {
    let gate = FakeGate::new(Err(RunRefusal::NoGrant));
    assert_eq!(check_start(&gate, &rec(Source::WeaveLogic), b"bytes"), Ok(None));
    assert_eq!(check_start(&gate, &rec(Source::Local), b"bytes"), Ok(None));
    assert_eq!(gate.asked(), 0);
}

#[test]
fn known_cognitum_bytes_relabelled_local_are_still_gated() {
    let mut gate = FakeGate::new(Err(RunRefusal::NoApproval));
    gate.claims = true;
    let e = check_start(&gate, &rec(Source::Local), b"bytes").unwrap_err();
    assert_eq!(e.code, "no_approval");
}

#[test]
fn the_gate_sees_hashes_of_the_bytes_and_the_record_identity() {
    let gate = FakeGate::new(Ok(permit()));
    let lic = check_start(&gate, &rec(Source::Cognitum), b"the binary").unwrap().unwrap();
    let (sha, b3) = hashes(b"the binary");
    assert_eq!(gate.asked.lock().unwrap()[0], ("fall-detect".into(), "1.2.0".into(), sha, b3.clone()));
    // The swept hash is the one computed here, not whatever the gate returned.
    assert_eq!(lic.blake3, b3);
    assert_eq!(lic.permit.grant_id, "g-1");
}

#[test]
fn not_seed_bound_lets_a_cognitum_cog_start() {
    let gate = FakeGate::new(Ok(RunVerdict::NotSeedBound));
    assert_eq!(check_start(&gate, &rec(Source::Cognitum), b"x"), Ok(None));
}

#[test]
fn every_refusal_keeps_the_run_gate_code() {
    let cases = [
        (RunRefusal::BindingInactive("x".into()), "binding_inactive"),
        (RunRefusal::NoGrant, "no_grant"),
        (RunRefusal::GrantLapsed, "grant_lapsed"),
        (RunRefusal::NotInGrant, "not_in_grant"),
        (RunRefusal::HashRevoked, "hash_revoked"),
        (RunRefusal::NoApproval, "no_approval"),
    ];
    for (r, code) in cases {
        let gate = FakeGate::new(Err(r));
        let e = check_start(&gate, &rec(Source::Cognitum), b"x").unwrap_err();
        assert_eq!(e.code, code);
        assert!(e.reason.starts_with(&format!("licence run gate: [{code}]")), "{}", e.reason);
        assert!(e.reason.contains("remedy:"), "{}", e.reason);
    }
}

// ---- supervisor with a fake gate ----------------------------------------------

#[cfg(unix)]
fn sleeper(root: &Path, source: Source) {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.join("fall-detect");
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("cog-fall-detect-arm");
    std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    crate::save_record(root, &rec(source)).unwrap();
}

#[test]
#[cfg(unix)]
fn a_refused_start_does_not_spawn_and_reports_the_code() {
    let root = tempfile::tempdir().unwrap();
    sleeper(root.path(), Source::Cognitum);
    let gate = Arc::new(FakeGate::new(Err(RunRefusal::NoGrant)));
    let mut s = Supervisor::new(root.path().to_path_buf());
    s.set_licence_gate(gate.clone());
    let e = s.start("fall-detect").unwrap_err();
    assert!(e.contains("[no_grant]"), "{e}");
    let st = &s.status()[0];
    assert!(!st.running && st.enabled);
    assert_eq!(st.licence_refusal.as_deref(), Some("no_grant"));

    // A grant and approval arrive: the next start (after backoff) runs it.
    gate.set(Ok(permit()));
    s.start("fall-detect").unwrap();
    let st = &s.status()[0];
    assert!(st.running && st.licence_refusal.is_none());
    assert_eq!(st.licence_grant.as_deref(), Some("g-1"));
    s.stop_all_running();
}

#[test]
#[cfg(unix)]
fn a_revoked_hash_stops_a_running_cog_and_a_lapse_does_not() {
    let root = tempfile::tempdir().unwrap();
    sleeper(root.path(), Source::Cognitum);
    let gate = Arc::new(FakeGate::new(Ok(permit())));
    let mut s = Supervisor::new(root.path().to_path_buf());
    s.set_licence_gate(gate.clone());
    s.start("fall-detect").unwrap();

    // A lapse leaves the running instance alone (soft stop).
    gate.set(Err(RunRefusal::GrantLapsed));
    s.tick();
    assert!(s.status()[0].running);

    // Revoking its hash stops it, and its restart is refused.
    let bytes = std::fs::read(root.path().join("fall-detect/cog-fall-detect-arm")).unwrap();
    *gate.revoked.lock().unwrap() = Some(hashes(&bytes).1);
    gate.set(Err(RunRefusal::HashRevoked));
    s.tick();
    let st = &s.status()[0];
    assert!(!st.running, "torn down");
    assert_eq!(st.licence_refusal.as_deref(), Some("hash_revoked"));
    assert!(s.start("fall-detect").unwrap_err().contains("[hash_revoked]"));
    s.stop_all_running();
}

/// A gate that swaps the cog's binary on disk while it is being asked, after the host hashed it.
#[cfg(unix)]
struct SwapGate {
    bin: PathBuf,
}

#[cfg(unix)]
impl CognitumRunGate for SwapGate {
    fn check(&self, _: &RunRequest<'_>) -> Result<RunVerdict, RunRefusal> {
        std::fs::write(&self.bin, "#!/bin/sh\necho swapped > ran.txt\nsleep 30\n").unwrap();
        Ok(permit())
    }
    fn claims(&self, _: &str, _: &str) -> bool {
        false
    }
    fn revoked(&self, _: &str) -> bool {
        false
    }
}

#[test]
#[cfg(unix)]
fn the_bytes_that_were_hashed_are_the_bytes_that_run() {
    let root = tempfile::tempdir().unwrap();
    sleeper(root.path(), Source::Cognitum);
    let bin = root.path().join("fall-detect/cog-fall-detect-arm");
    std::fs::write(&bin, "#!/bin/sh\necho original > ran.txt\nsleep 30\n").unwrap();
    let mut s = Supervisor::new(root.path().to_path_buf());
    s.set_licence_gate(Arc::new(SwapGate { bin: bin.clone() }));
    s.start("fall-detect").unwrap();
    // The path now holds the swapped bytes, but the instance runs the checked copy.
    assert!(std::fs::read_to_string(&bin).unwrap().contains("swapped"));
    let ran = root.path().join("fall-detect/ran.txt");
    for _ in 0..100 {
        if ran.exists() && !std::fs::read_to_string(&ran).unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(std::fs::read_to_string(&ran).unwrap().trim(), "original");
    // Where a file copy is used (not Linux memfd), it is 0700, content-addressed and cleared at host start.
    let run_dir = root.path().join(".run");
    if let Ok(rd) = std::fs::read_dir(&run_dir) {
        use std::os::unix::fs::PermissionsExt;
        let copies: Vec<_> = rd.flatten().collect();
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].metadata().unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777, 0o700);
        s.stop("fall-detect").unwrap();
        let _fresh = Supervisor::new(root.path().to_path_buf());
        assert!(!run_dir.exists());
    } else {
        s.stop("fall-detect").unwrap();
    }
}

#[test]
#[cfg(unix)]
fn a_dangling_or_unreadable_licence_dir_fails_closed_not_open() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(".licence");
    std::os::unix::fs::symlink(root.path().join("nowhere"), &dir).unwrap();
    let lic = HostLicence::open(dir);
    assert_eq!(lic.status()["state"], "broken");
    let e = check_start(&lic, &rec(Source::Cognitum), b"bytes").unwrap_err();
    assert_eq!(e.code, "binding_inactive");
    assert!(lic.import(&Records::default()).unwrap_err().contains("dangling"));
}

#[test]
#[cfg(unix)]
fn without_a_gate_nothing_changes() {
    let root = tempfile::tempdir().unwrap();
    sleeper(root.path(), Source::Cognitum);
    let mut s = Supervisor::new(root.path().to_path_buf());
    s.start("fall-detect").unwrap();
    assert!(s.status()[0].running && s.status()[0].licence_refusal.is_none());
    s.stop_all_running();
}

// ---- HostLicence over the kernel stores ----------------------------------------

const T0: u64 = 1_800_000_000;

#[cfg(unix)]
fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
#[cfg(unix)]
fn hexk(k: &SigningKey) -> String {
    k.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
#[cfg(unix)]
fn mesh() -> MeshId {
    MeshId::derive(&[9; 32], &[7; 32])
}
const BIN: &[u8] = b"cog binary bytes";

/// A licence dir with config + trust pinning operator key `sk(1)`, and a
/// private `cog.check_run` listener on that directory.
#[cfg(unix)]
fn fixture() -> (tempfile::TempDir, HostLicence, Arc<AtomicU64>, super::test_gate::Gate) {
    let dir = tempfile::tempdir().unwrap();
    let lic_dir = dir.path().join(".licence");
    std::fs::create_dir_all(&lic_dir).unwrap();
    std::fs::write(lic_dir.join(CONFIG_FILE), serde_json::json!({"mesh_id": mesh().to_hex()}).to_string()).unwrap();
    let trust = serde_json::json!({
        "schema": "weftos.workload-trust.v1",
        "operator_keys": [{"key_id": "operator-1", "public_key": hexk(&sk(1))}],
    });
    std::fs::write(lic_dir.join(TRUST_FILE), trust.to_string()).unwrap();
    let now = Arc::new(AtomicU64::new(T0));
    let tick = now.clone();
    let clock: clawft_kernel::licence::Clock = Arc::new(move || tick.load(Ordering::SeqCst));
    let gate = super::test_gate::Gate::start(lic_dir.clone(), clock.clone());
    let lic = HostLicence::with_clock(lic_dir, clock).with_daemon_socket(gate.socket());
    (dir, lic, now, gate)
}

#[cfg(unix)]
fn binding() -> SignedBinding {
    let r = BindingRecord {
        v: 2,
        device_id: "seed-test".into(),
        device_pubkey: hexk(&sk(20)),
        mesh_id: mesh().to_hex(),
        grant_pubkey: hexk(&sk(2)),
        steward_node_id: "node-steward".into(),
        steward_pubkey: hexk(&sk(21)),
        state: BindState::Bound,
        seq: 1,
        bound_at: T0,
    };
    sign_binding(&r, &sk(1)).unwrap()
}

#[cfg(unix)]
fn grant(seq: u64, issued: u64, ttl: u64) -> SignedGrant {
    let (sha256, blake3) = hashes(BIN);
    let g = CheckoutGrant {
        v: 1,
        grant_id: String::new(),
        mesh_id: mesh().to_hex(),
        seed_device_id: "seed-test".into(),
        grant_key_id: String::new(),
        source: "cognitum".into(),
        registry: "registry.example".into(),
        cog_id: "fall-detect".into(),
        version: "1.2.0".into(),
        artifacts: vec![GrantArtifact { arch: "arm".into(), size: BIN.len() as u64, sha256, blake3 }],
        manifest_sha256: weftos_cog_repo::sha256_hex(b"manifest"),
        licence: LicenceRef { ref_sha256: weftos_cog_repo::sha256_hex(b"licence"), expires: issued + 365 * 86400 },
        seq,
        issued_at: issued,
        expires_at: issued + ttl,
    };
    sign_grant(&g, &sk(2)).unwrap()
}

#[cfg(unix)]
fn approval(signer: &SigningKey) -> SignedApproval {
    let a = Approval {
        v: 1,
        mesh_id: mesh().to_hex(),
        cog_id: "fall-detect".into(),
        version: "1.2.0".into(),
        sha256: vec![hashes(BIN).0],
        approved_at: T0,
    };
    sign_approval(&a, signer).unwrap()
}

#[cfg(unix)]
fn code(lic: &HostLicence) -> Result<(), &'static str> {
    check_start(lic, &rec(Source::Cognitum), BIN).map(|_| ()).map_err(|e| e.code)
}

#[test]
fn no_licence_dir_means_the_gate_does_not_apply() {
    let dir = tempfile::tempdir().unwrap();
    let lic = HostLicence::open(dir.path().join(".licence"));
    assert_eq!(lic.status()["state"], "unconfigured");
    assert_eq!(code(&lic), Ok(()));
    assert!(lic.import(&Records::default()).is_err(), "nothing to import into");
}

#[test]
fn a_licence_dir_without_config_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".licence")).unwrap();
    let lic = HostLicence::open(dir.path().join(".licence"));
    assert_eq!(lic.status()["state"], "broken");
    assert_eq!(code(&lic), Err("binding_inactive"));
    // Bytes the host cannot judge are not claimed, so a non-Cognitum record still runs.
    assert!(check_start(&lic, &rec(Source::Local), BIN).unwrap().is_none());
}

#[test]
#[cfg(unix)]
fn configured_but_never_bound_fails_closed_for_cognitum_cogs() {
    let (_d, lic, _, _gate) = fixture();
    assert_eq!(lic.status()["state"], "ready");
    // A configured directory means a binding is intended: nothing Cognitum-origin starts before it.
    let e = check_start(&lic, &rec(Source::Cognitum), BIN).unwrap_err();
    assert_eq!(e.code, "binding_inactive");
    assert!(e.reason.contains("no binding yet"), "{}", e.reason);
    // Cogs that are not Cognitum-origin (and not claimed) are untouched.
    assert!(check_start(&lic, &rec(Source::WeaveLogic), BIN).unwrap().is_none());
    // Importing the binding moves the refusal on to the grant.
    lic.import(&Records { binding: Some(binding()), ..Default::default() }).unwrap();
    assert_eq!(code(&lic), Err("no_grant"));
}

#[test]
#[cfg(unix)]
fn grant_and_approval_permit_then_withdrawal_and_revocation_refuse() {
    let (_d, lic, now, _gate) = fixture();
    now.store(T0, Ordering::SeqCst);
    let out = lic.import(&Records { binding: Some(binding()), ..Default::default() }).unwrap();
    assert_eq!(out[0].outcome, "applied", "{out:?}");
    assert_eq!(code(&lic), Err("no_grant"));

    lic.import(&Records { grants: vec![grant(1, T0, 3600)], ..Default::default() }).unwrap();
    assert_eq!(code(&lic), Err("no_approval"));
    // The grant key cannot sign an approval; only a pinned operator key can.
    let out = lic.import(&Records { approvals: vec![approval(&sk(2))], ..Default::default() }).unwrap();
    assert_eq!(out[0].outcome, "refused", "{out:?}");
    assert_eq!(code(&lic), Err("no_approval"));

    lic.import(&Records { approvals: vec![approval(&sk(1))], ..Default::default() }).unwrap();
    let ok = check_start(&lic, &rec(Source::Cognitum), BIN).unwrap().expect("licensed");
    assert_eq!(ok.blake3, hashes(BIN).1);
    // Other bytes under the same id and version are not in the grant.
    assert_eq!(check_start(&lic, &rec(Source::Cognitum), b"other").unwrap_err().code, "not_in_grant");
    // Relabelled local, the granted bytes are still recognised and checked.
    assert!(lic.claims(&hashes(BIN).0, &hashes(BIN).1));

    // A withdrawal (a renewal with expires_at <= issued_at) lapses it too.
    now.store(T0 + 10, Ordering::SeqCst);
    lic.import(&Records { grants: vec![grant(2, T0 + 5, 0)], ..Default::default() }).unwrap();
    assert_eq!(code(&lic), Err("grant_lapsed"));

    // A fresh renewal permits again; an operator hash revocation then refuses and is swept.
    lic.import(&Records { grants: vec![grant(3, T0 + 6, 3600)], ..Default::default() }).unwrap();
    assert_eq!(code(&lic), Ok(()));
    let b3 = hashes(BIN).1;
    let notice = sign_revocation(RevocationKind::ArtifactHash, &b3, "test", T0, &sk(1)).unwrap();
    let out = lic.import(&Records { revocations: vec![notice], ..Default::default() }).unwrap();
    assert_eq!(out[0].outcome, "applied", "{out:?}");
    assert_eq!(code(&lic), Err("hash_revoked"));
    assert!(lic.revoked(&b3));
}

#[test]
#[cfg(unix)]
fn an_expired_grant_lapses() {
    let (_d, lic, now, _gate) = fixture();
    lic.import(&Records { binding: Some(binding()), grants: vec![grant(1, T0, 3600)], approvals: vec![approval(&sk(1))], revocations: vec![] })
        .unwrap();
    assert_eq!(code(&lic), Ok(()));
    now.store(T0 + 7200, Ordering::SeqCst);
    assert_eq!(code(&lic), Err("grant_lapsed"));
}

#[test]
#[cfg(unix)]
fn records_from_untrusted_signers_are_refused() {
    let (_d, lic, _, _gate) = fixture();
    let forged = sign_binding(&serde_json::from_str(&binding().payload).unwrap(), &sk(3)).unwrap();
    let notice = sign_revocation(RevocationKind::ArtifactHash, &hashes(BIN).1, "x", T0, &sk(3)).unwrap();
    let out = lic
        .import(&Records { binding: Some(forged), revocations: vec![notice], ..Default::default() })
        .unwrap();
    assert!(out.iter().all(|l| l.outcome == "refused"), "{out:?}");
    // Bound by a real binding, a grant signed by another key is refused.
    lic.import(&Records { binding: Some(binding()), ..Default::default() }).unwrap();
    let bad = {
        let g: CheckoutGrant = serde_json::from_str(&grant(1, T0, 60).payload).unwrap();
        sign_grant(&g, &sk(4)).unwrap()
    };
    assert_eq!(lic.import(&Records { grants: vec![bad], ..Default::default() }).unwrap()[0].outcome, "refused");
    assert_eq!(code(&lic), Err("no_grant"));
}

#[test]
#[cfg(unix)]
fn state_survives_a_restart_and_a_deleted_store_fails_closed() {
    let (d, lic, _, gate) = fixture();
    lic.import(&Records { binding: Some(binding()), grants: vec![grant(1, T0, 3600)], approvals: vec![approval(&sk(1))], revocations: vec![] })
        .unwrap();
    assert_eq!(code(&lic), Ok(()));
    let dir = lic.dir().to_path_buf();
    drop(lic);
    // The listener still owns the original clock. The grant covers T0 + 1.
    let lic = HostLicence::with_clock(dir.clone(), Arc::new(|| T0 + 1)).with_daemon_socket(gate.socket());
    assert_eq!(code(&lic), Ok(()), "reopened from disk");

    // Another process deletes the grant store: the bound marker keeps the gate on.
    std::fs::remove_file(dir.join(GRANTS_FILE)).unwrap();
    lic.refresh();
    assert_eq!(code(&lic), Err("binding_inactive"));
    drop(d);
}

#[test]
#[cfg(unix)]
fn a_corrupt_store_or_revocation_list_fails_closed() {
    let (_d, lic, _, _gate) = fixture();
    lic.import(&Records { binding: Some(binding()), grants: vec![grant(1, T0, 3600)], approvals: vec![approval(&sk(1))], revocations: vec![] })
        .unwrap();
    std::fs::write(lic.dir().join(SUBJECTS_FILE), "{not json").unwrap();
    lic.refresh();
    assert_eq!(code(&lic), Err("binding_inactive"));
    std::fs::remove_file(lic.dir().join(SUBJECTS_FILE)).unwrap();
    std::fs::write(lic.dir().join(GRANTS_FILE), "{not json").unwrap();
    lic.refresh();
    assert_eq!(code(&lic), Err("binding_inactive"));
}

#[test]
#[cfg(unix)]
fn import_limits_are_enforced() {
    let (_d, lic, _, _gate) = fixture();
    let many = Records { approvals: vec![approval(&sk(1)); MAX_IMPORT_RECORDS + 1], ..Default::default() };
    assert!(lic.import(&many).unwrap_err().contains("at most"));
    assert!(serde_json::from_str::<Records>(r#"{"bogus": 1}"#).is_err(), "unknown fields are refused");
}
