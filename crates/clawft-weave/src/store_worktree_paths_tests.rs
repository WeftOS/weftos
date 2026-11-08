use super::*;

#[test]
fn resolve_lenient_follows_existing_symlinks_and_keeps_the_missing_tail() {
    let d = tempfile::tempdir().unwrap();
    let real = d.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, d.path().join("alias")).unwrap();
    let got = resolve_lenient(&d.path().join("alias/new/deep")).unwrap();
    assert_eq!(got, real.canonicalize().unwrap().join("new/deep"));
}

#[test]
fn a_symlink_into_protected_state_is_still_protected() {
    let d = tempfile::tempdir().unwrap();
    let run = d.path().join("run");
    std::fs::create_dir(&run).unwrap();
    std::os::unix::fs::symlink(&run, d.path().join("sneaky")).unwrap();
    let g = Guard::new([run]);
    let e = g.check(&d.path().join("sneaky/x").to_string_lossy(), "dest", Use::WriteDir).unwrap_err();
    assert!(e.contains("daemon's own state"), "{e}");
    assert!(g.check(&d.path().join("elsewhere").to_string_lossy(), "dest", Use::WriteDir).is_ok());
}
