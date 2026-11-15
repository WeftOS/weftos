//! `host::build`: what the owner's one command wires from `cos.json`, with
//! every path in a tempdir (nothing touches the real home).

mod common;

use std::os::unix::fs::PermissionsExt;

use common::TEST_TOKEN;
use weftos_cos::CosConfig;
use weftos_cos::config::{BrainConfig, HarnessConfig};

const DIGEST: &str = "sha256:537fbd002578137017a15183931acc31d8e8800500ec4c6fcb72d472bb5942d6";

fn config(dir: &std::path::Path, token: bool) -> CosConfig {
    let app = dir.join("app.json");
    std::fs::write(&app, "{}").unwrap();
    let token_file = dir.join("board-token");
    if token {
        std::fs::write(&token_file, format!("{TEST_TOKEN}\n")).unwrap();
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    CosConfig {
        app_config: Some(app),
        staging_dir: dir.join("staging"),
        tool_log: Some(dir.join("tool-log.jsonl")),
        harness: HarnessConfig { token_file, project_id: Some(common::TEST_PROJECT.into()), ..Default::default() },
        // PLUMBING: these tests are about what the host binds, not recall quality, so the
        // brain opts in to the explicit lexical embedder (no model needed).
        brain: BrainConfig { path: Some(dir.join("cos.rvf")), installation: Some("weftos://local/installations/t".into()), package: Some(DIGEST.into()), lexical: true, ..Default::default() },
        ..Default::default()
    }
}

#[tokio::test]
async fn a_configured_host_binds_every_tool_and_the_brain() {
    let dir = tempfile::tempdir().unwrap();
    let built = weftos_cos::host::build(&config(dir.path(), true)).await.unwrap();
    assert!(built.unbound.is_empty(), "{:?}", built.unbound);
    assert!(built.brain.is_some());
    assert_eq!(built.quick_model, weftos_cos::model::LOCAL_DEFAULT_MODEL);
    assert_eq!(built.local_api_base.as_deref(), Some(weftos_cos::model::LOCAL_DEFAULT_BASE));
}

#[tokio::test]
async fn without_a_credential_the_board_tools_say_so_and_notes_still_reach_the_brain() {
    let dir = tempfile::tempdir().unwrap();
    let built = weftos_cos::host::build(&config(dir.path(), false)).await.unwrap();
    let unbound: Vec<&str> = built.unbound.iter().map(|(t, _)| t.as_str()).collect();
    for t in ["projects.activity", "board.read", "board.claim", "board.note", "card.current"] {
        assert!(unbound.contains(&t), "{t} should be reported unbound: {unbound:?}");
    }
    assert!(!unbound.contains(&"notes.record"), "the brain alone keeps notes.record");
    assert!(built.brain.is_some());
}

#[tokio::test]
async fn a_brain_that_cannot_bind_is_reported_not_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), true);
    cfg.brain.package = Some("sha256:short".into());
    let built = weftos_cos::host::build(&cfg).await.unwrap();
    assert!(built.brain.is_none());
    assert!(built.unbound.iter().any(|(t, r)| t == "brain" && r.contains("sha256")), "{:?}", built.unbound);
}

/// No embedding model, no API key, no opt-in: the host still builds (an offline boot
/// must not break) and reports the brain unbound with the reason. It never hashes.
#[tokio::test]
async fn without_an_embedding_model_the_brain_is_reported_unbound_and_the_host_still_builds() {
    if clawft_core::embeddings::select::lexical_mode_requested()
        || std::env::var("OPENAI_API_KEY").is_ok_and(|k| !k.is_empty())
        || clawft_core::embeddings::select::select_embedder().is_ok()
    {
        eprintln!("SKIP: this host has a model, an API key, or lexical mode on");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), true);
    cfg.brain.lexical = false;
    let built = weftos_cos::host::build(&cfg).await.unwrap();
    assert!(built.brain.is_none());
    assert!(
        built.unbound.iter().any(|(t, r)| t == "brain" && r.contains("semantic search unavailable")),
        "{:?}",
        built.unbound
    );
}
