//! The registry fetcher over a local file registry (no network): reuses
//! cog-sources' sha256 and size checks and its https-only rule.
#![cfg(feature = "registry")]
mod common;
use common::*;
use weft_licence::providers::{CogFetcher, FetchError};
use weft_licence::registry::RegistryFetcher;
use cog_sources::FsReader;

const BIN: &[u8] = b"\x7fELF cognitum fall-detect armhf";

fn fixture(dir: &std::path::Path, sha: &str, size: Option<u64>) -> String {
    std::fs::create_dir_all(dir.join("cogs/arm")).unwrap();
    std::fs::write(dir.join("cogs/arm/cog-fall-detect-arm"), BIN).unwrap();
    let reg = serde_json::json!({
        "version": "1", "updated": "2026-10-02", "binary_base_url": dir.to_str().unwrap(),
        "cogs": [{"id": "fall-detect", "name": "Fall detect", "category": "health", "version": "2.1.0",
                  "description": "d", "size_kb": 1, "sha256": sha, "binary_size": size}]
    });
    let p = dir.join("app-registry.json");
    std::fs::write(&p, reg.to_string()).unwrap();
    p.to_str().unwrap().to_string()
}

fn fetcher(url: &str) -> RegistryFetcher {
    RegistryFetcher::new(url, true, Box::new(FsReader), 64 << 20)
}

#[test]
fn resolves_and_fetches_with_the_registry_hash_and_size_checks() {
    let d = tempfile::tempdir().unwrap();
    let url = fixture(d.path(), &weft_licence_wire::sha256_hex(BIN), Some(BIN.len() as u64));
    let f = fetcher(&url);
    let e = f.resolve("fall-detect", "latest").unwrap();
    assert_eq!((e.version.as_str(), e.artifacts.len(), e.artifacts[0].arch.as_str()), ("2.1.0", 1, "arm"));
    assert_eq!(e.artifacts[0].sha256, weft_licence_wire::sha256_hex(BIN));
    assert_eq!(f.fetch(&e, "arm").unwrap(), BIN);
    assert_eq!(f.resolve("fall-detect", "2.1.0").unwrap().version, "2.1.0");
    assert_eq!(f.resolve("fall-detect", "9.9.9"), Err(FetchError::VersionUnavailable("2.1.0".into())));
    assert_eq!(f.resolve("nope", "latest"), Err(FetchError::NotFound));
    // Only armhf exists in the Cognitum registry today (question C7).
    assert!(matches!(f.fetch(&e, "arm64"), Err(FetchError::ArchUnavailable(_))));
}

#[test]
fn a_hash_or_size_mismatch_is_a_verify_failure() {
    let d = tempfile::tempdir().unwrap();
    let url = fixture(d.path(), &"0".repeat(64), Some(BIN.len() as u64));
    let f = fetcher(&url);
    let e = f.resolve("fall-detect", "latest").unwrap();
    assert!(matches!(f.fetch(&e, "arm"), Err(FetchError::Verify(_))));
    let d2 = tempfile::tempdir().unwrap();
    let url2 = fixture(d2.path(), &weft_licence_wire::sha256_hex(BIN), Some(3));
    let f2 = fetcher(&url2);
    let e2 = f2.resolve("fall-detect", "latest").unwrap();
    assert!(matches!(f2.fetch(&e2, "arm"), Err(FetchError::Verify(_))));
}

#[test]
fn an_insecure_registry_location_is_refused_unless_the_lab_opt_in_is_set() {
    let f = RegistryFetcher::new("http://registry.example/app-registry.json", false, Box::new(FsReader), 64 << 20);
    assert!(matches!(f.resolve("fall-detect", "latest"), Err(FetchError::Failed(m)) if m.contains("https")));
}

#[test]
fn the_reader_is_bounded_by_the_artifact_limit() {
    let d = tempfile::tempdir().unwrap();
    let url = fixture(d.path(), &weft_licence_wire::sha256_hex(BIN), Some(BIN.len() as u64));
    let f = RegistryFetcher::new(&url, true, Box::new(FsReader), 8);
    let e = f.resolve("fall-detect", "latest");
    // The 8-byte clamp also covers the registry read: nothing larger is read.
    assert!(e.is_err());
}

#[test]
fn the_service_checks_out_through_the_registry_fetcher() {
    let d = tempfile::tempdir().unwrap();
    let url = fixture(d.path(), &weft_licence_wire::sha256_hex(BIN), Some(BIN.len() as u64));
    let h = Harness::new(&[]);
    let dir = tempfile::tempdir().unwrap();
    let cfg = base_config(dir.path());
    let init = weft_licence::keys::init(&cfg.state_dir).unwrap();
    let ops = weft_licence::state::OperatorKeys::load(&cfg.state_dir, &cfg.operator_pubkeys).unwrap();
    weft_licence::bind::apply(&cfg.state_dir, &cfg.device_id, &ops, &binding(1, weft_licence_wire::BindState::Bound, &init.grant_pubkey, &mesh()), None).unwrap();
    let svc = weft_licence::Service::open(
        cfg.clone(),
        std::sync::Arc::new(|| T0),
        Box::new(StubLicence::all(None)),
        Box::new(fetcher(&url)),
        Box::new(weft_licence::providers::StubDeviceSigner),
    )
    .unwrap();
    let sign = |body: &[u8]| weft_licence::request::Request {
        method: "POST".into(),
        target: "/licence/v1/checkout".into(),
        headers: weft_licence::request::sign_request(&steward(), NODE, "seed-test", "POST", "/licence/v1/checkout", body, T0 * 1000, "ab".repeat(16).as_str()),
        body: body.to_vec(),
    };
    let r = svc.handle(&sign(br#"{"request_id":"r","cog_id":"fall-detect","version":"latest","arch":"arm"}"#));
    assert_eq!(r.status, 200, "{:?}", r.json_body());
    let g = weft_licence_wire::verify_grant(&grant_of(&r), &init_key(&cfg), &mesh()).unwrap();
    assert_eq!(g.version, "2.1.0");
    let _ = h;
}

fn init_key(cfg: &weft_licence::Config) -> [u8; 32] {
    weft_licence::keys::load(&cfg.state_dir).unwrap().verifying_key().to_bytes()
}
