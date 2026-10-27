//! Anti-rollback, key revocation and staleness tests for `weaver update`,
//! against the loopback mock release server (throwaway key, temp dirs).

use cog_repo::RevokedKeys;

use super::update_flow::{Opts, Outcome};
use super::update_install::HIGHEST_KEY;
use super::update_signature::{Trust, load_revocations, revocation_files};
use super::update_test_support::{Rel, test_key};
use super::update_tests::{Fake, OLD, World, no, run, version_of};

fn receipt(w: &World) -> serde_json::Value {
    let p = w.env.config_dir.join("weftos/weftos-receipt.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn set_receipt(w: &World, key: &str, value: &str) {
    let p = w.env.config_dir.join("weftos/weftos-receipt.json");
    let mut r = receipt(w);
    r[key] = value.into();
    std::fs::write(p, r.to_string()).unwrap();
}

/// An older, validly signed release served as "latest" (a rollback): --force
/// must not install it, and nothing is downloaded.
#[test]
fn force_never_installs_an_older_signed_release() {
    let w = World::receipt_install("0.8.0", "0.7.0", &Rel::default());
    let host = Fake::default();
    let ctx = w.ctx(w.weaver(), &host, &no);
    let (r, out) = run(&ctx, Opts { force: true, ..Opts::default() });
    assert_eq!(r.unwrap(), Outcome::DowngradeRefused { version: "0.7.0".into() }, "{out}");
    assert!(out.contains("--allow-downgrade"), "{out}");
    let (r, out) = run(&ctx, Opts::default());
    assert_eq!(r.unwrap(), Outcome::UpToDate, "{out}");
    assert!(out.contains("not downgrading"), "{out}");
    assert_eq!(w.versions(), OLD);
    assert_eq!(w.mock.asset_requests(), 0);
}

#[test]
fn allow_downgrade_installs_with_a_warning_and_keeps_the_mark() {
    let w = World::receipt_install("0.8.0", "0.7.0", &Rel::default());
    let host = Fake::default();
    let (r, out) = run(&w.ctx(w.weaver(), &host, &no), Opts { allow_downgrade: true, ..Opts::default() });
    assert!(matches!(r.unwrap(), Outcome::Installed { .. }), "{out}");
    assert!(out.contains("WARNING: --allow-downgrade: downgrading from v0.8.0 to v0.7.0"), "{out}");
    assert_eq!(version_of(&w.weaver()), "weaver 0.7.0");
    let rc = receipt(&w);
    assert_eq!(rc["version"], "0.7.0");
    assert_eq!(rc[HIGHEST_KEY], "0.8.0", "a downgrade never lowers the mark");
}

/// The receipt remembers the highest version installed; a release below it is
/// refused even when this build is older still.
#[test]
fn a_release_below_the_receipt_mark_is_refused() {
    let w = World::receipt_install("0.8.0", "0.9.0", &Rel::default());
    set_receipt(&w, HIGHEST_KEY, "0.9.5");
    let host = Fake::default();
    let ctx = w.ctx(w.weaver(), &host, &no);
    for opts in [Opts::default(), Opts { force: true, ..Opts::default() }, Opts { check: true, ..Opts::default() }] {
        let (r, out) = run(&ctx, opts);
        assert_eq!(r.unwrap(), Outcome::DowngradeRefused { version: "0.9.0".into() }, "{out}");
        assert!(out.contains("records v0.9.5"), "{out}");
    }
    assert_eq!(w.versions(), OLD);
    assert_eq!(w.mock.asset_requests(), 0);
    let (r, out) = run(&ctx, Opts { allow_downgrade: true, ..Opts::default() });
    assert!(matches!(r.unwrap(), Outcome::Installed { .. }), "{out}");
    assert!(out.contains("downgrading from v0.9.5 to v0.9.0"), "{out}");
    assert_eq!(receipt(&w)[HIGHEST_KEY], "0.9.5");
}

#[test]
fn an_update_raises_the_receipt_mark() {
    let w = World::receipt_install("0.8.0", "0.9.0", &Rel::default());
    let host = Fake::default();
    assert!(matches!(run(&w.ctx(w.weaver(), &host, &no), Opts::default()).0.unwrap(), Outcome::Installed { .. }));
    assert_eq!(receipt(&w)[HIGHEST_KEY], "0.9.0");
}

#[test]
fn a_revoked_release_key_refuses_everything_with_out_of_band_advice() {
    let w = World::receipt_install("0.8.0", "0.9.0", &Rel::default());
    let host = Fake::default();
    let mut ctx = w.ctx(w.weaver(), &host, &no);
    let key = test_key().verifying_key();
    ctx.trust = Trust::Pinned { key, revoked: RevokedKeys::from_keys([hex::encode(key.to_bytes())]) };
    for opts in [Opts::default(), Opts { check: true, ..Opts::default() }] {
        let msg = run(&ctx, opts).0.unwrap_err().to_string();
        assert!(msg.contains("revoked") && msg.contains("out of band"), "{msg}");
    }
    assert_eq!(w.versions(), OLD);
    assert_eq!(w.mock.asset_requests(), 0);
}

#[test]
fn a_stale_signed_release_gets_a_warning() {
    let w = World::receipt_install("0.8.0", "0.9.0", &Rel { published: Some("2020-01-01T00:00:00Z"), ..Rel::default() });
    let host = Fake::default();
    let (r, out) = run(&w.ctx(w.weaver(), &host, &no), Opts { check: true, ..Opts::default() });
    assert!(matches!(r.unwrap(), Outcome::Available { .. }), "{out}");
    assert!(out.contains("was signed") && out.contains("days ago (2020-01-01)"), "{out}");

    let w = World::receipt_install("0.8.0", "0.9.0", &Rel::default());
    let (_, out) = run(&w.ctx(w.weaver(), &host, &no), Opts { check: true, ..Opts::default() });
    assert!(!out.contains("days ago"), "{out}");
}

/// A home with a user-level revocation list and a project (`proj/.weftos/
/// project.toml`) whose runtime dir holds `project_list`; returns (home, cwd
/// inside the project). Temp dirs only.
fn home_and_project(w: &World, user_list: &str, project_list: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = w.root.join("rhome");
    std::fs::create_dir_all(home.join(".weftos/run")).unwrap();
    std::fs::write(home.join(".weftos/run/revoked_subjects.json"), user_list).unwrap();
    let proj = w.root.join("proj");
    std::fs::create_dir_all(proj.join(".weftos/runtime")).unwrap();
    std::fs::create_dir_all(proj.join("src/deep")).unwrap();
    std::fs::write(proj.join(".weftos/project.toml"), "").unwrap();
    std::fs::write(proj.join(".weftos/runtime/revoked_subjects.json"), project_list).unwrap();
    (home, proj.join("src/deep"))
}

fn revoke(key_hex: &str) -> String {
    format!(r#"[{{"kind":"signer_key","id":"{key_hex}","revoked_at":1,"reason":"leaked"}}]"#)
}

#[test]
fn a_user_level_revocation_is_honoured_from_inside_a_project_dir() {
    let w = World::receipt_install("0.8.0", "0.9.0", &Rel::default());
    let key = test_key().verifying_key();
    let (home, cwd) = home_and_project(&w, &revoke(&hex::encode(key.to_bytes())), "[]");
    let (user, project) = revocation_files(None, Some(&cwd), Some(&home)).unwrap();
    assert!(user.contains(&home.join(".weftos/run/revoked_subjects.json")), "{user:?}");
    assert_eq!(project, Some(w.root.join("proj/.weftos/runtime/revoked_subjects.json")));
    let (revoked, warnings) = load_revocations(None, Some(&cwd), Some(&home)).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");

    let host = Fake::default();
    let mut ctx = w.ctx(w.weaver(), &host, &no);
    ctx.trust = Trust::Pinned { key, revoked };
    assert!(run(&ctx, Opts::default()).0.unwrap_err().to_string().contains("revoked"));
    assert_eq!(w.versions(), OLD);

    // The project-level list is unioned in too.
    let (home, cwd) = home_and_project(&w, "[]", &revoke(&hex::encode(key.to_bytes())));
    assert!(load_revocations(None, Some(&cwd), Some(&home)).unwrap().0.contains(&hex::encode(key.to_bytes())));
}

#[test]
fn a_malformed_project_list_warns_and_does_not_block_but_a_malformed_user_list_does() {
    let w = World::receipt_install("0.8.0", "0.9.0", &Rel::default());
    let (home, cwd) = home_and_project(&w, "[]", "{not json");
    let (revoked, warnings) = load_revocations(None, Some(&cwd), Some(&home)).unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("ignoring the project revocation list"), "{warnings:?}");
    let host = Fake::default();
    let mut ctx = w.ctx(w.weaver(), &host, &no);
    ctx.trust = Trust::Pinned { key: test_key().verifying_key(), revoked };
    let (r, out) = run(&ctx, Opts::default());
    assert!(matches!(r.unwrap(), Outcome::Installed { .. }), "{out}");

    let (home, cwd) = home_and_project(&w, "{not json", "[]");
    assert!(load_revocations(None, Some(&cwd), Some(&home)).is_err());
    std::fs::write(home.join(".weftos/run/revoked_subjects.json"), "[]").unwrap();
    std::fs::create_dir_all(home.join(".clawft")).unwrap();
    std::fs::write(home.join(".clawft/revoked_subjects.json"), "oops").unwrap();
    assert!(load_revocations(None, Some(&cwd), Some(&home)).is_err(), "the legacy user list is user-level too");
}
