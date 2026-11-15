use super::*;

#[test]
fn plist_host_names_are_read_without_a_process() {
    let p = "<dict><key>System</key><dict><key>Network</key><dict><key>HostNames</key><dict>\n<key>LocalHostName</key>\n\t<string>mac-studio</string></dict></dict><key>System</key><dict><key>ComputerName</key><string>Mac Studio</string></dict></dict></dict>";
    assert_eq!(plist_string(p, "LocalHostName").as_deref(), Some("mac-studio"));
    assert_eq!(plist_string(p, "ComputerName").as_deref(), Some("Mac Studio"));
    assert_eq!(plist_string(p, "HostName"), None);
    assert_eq!(plist_string("", "LocalHostName"), None);
}

#[test]
fn this_machine_has_a_host_name_that_is_not_the_shared_fallback() {
    // On a Mac with no HOSTNAME exported this used to be "unknown".
    if let Some(h) = local_host() {
        assert!(!h.is_empty() && h != "unknown");
    }
}

#[test]
fn verbs_parse_and_required_args_are_enforced() {
    let p = |a: &[&str]| StoreArgs::try_parse_from(std::iter::once("store").chain(a.iter().copied()));
    assert!(p(&["ls"]).is_ok() && p(&["ls", "weftos://m/nodes/a/store", "--json"]).is_ok());
    assert!(p(&["grant", "n", "cog:c", "write"]).is_ok() && p(&["grant", "n", "cog:c", "--off"]).is_ok());
    assert!(p(&["grant", "n", "cog:c"]).is_err(), "needs access or --on/--off");
    assert!(p(&["grant", "n", "cog:c", "--on", "--off"]).is_err());
    assert!(p(&["promote", "n"]).is_err() && p(&["promote", "n", "--prefix", "assets/"]).is_ok());
    assert!(p(&["archive", "n", "--delete"]).is_ok() && p(&["revoke", "n", "cog:c"]).is_ok());
    assert!(p(&["put", "n", "k", "v"]).is_err() && p(&["put", "n", "k", "v", "--as", "cog:c"]).is_ok());
    assert!(p(&["create", "n", "--write", "a", "--write", "b"]).is_ok());
}

#[test]
fn worktree_args_become_params() {
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let p = worktree_params(&a(&["--project", "01ABC", "--paths-z", "/t/p.z", "--force", "--json"]), "ag", "h1").unwrap();
    assert_eq!((p["project"].as_str(), p["paths_z"].as_str(), p["force"].as_bool(), p["agent"].as_str()), (Some("01ABC"), Some("/t/p.z"), Some(true), Some("ag")));
    let d = worktree_params(&a(&["--project", "01ABC", "--slug", "x", "--dry-run", "--on-conflict", "keep_project", "--max-bytes", "9"]), "ag", "h1").unwrap();
    assert_eq!((d["dry_run"].as_bool(), d["on_conflict"].as_str(), d["max_bytes"].as_u64()), (Some(true), Some("keep_project"), Some(9)));
    assert!(worktree_params(&a(&["--max-bytes", "lots"]), "a", "h").is_err());
    assert!(worktree_params(&a(&["--project"]), "a", "h").is_err(), "value required");
    assert!(worktree_params(&a(&["stray"]), "a", "h").is_err());
    assert_eq!(wt_lines("buckets", &json!({"worktree": "w-1", "project": "p-1"})).unwrap(), "w-1 p-1\n");
    assert!(StoreArgs::try_parse_from(["store", "worktree", "publish", "--project", "X", "--changes-z", "/f"]).is_ok());
    let d = worktree_params(&a(&["--name", "n", "--root", "/r", "--read-only"]), "ag", "h1").unwrap();
    assert_eq!((d["read_only"].as_bool(), d["root"].as_str()), (Some(true), Some("/r")));
    assert!(StoreArgs::try_parse_from(["store", "drive", "sync-up", "--name", "n", "--root", "/r"]).is_ok());
    assert!(StoreArgs::try_parse_from(["store", "view", "open", "--name", "n", "--root", "/r", "--include", "docs=docs", "--read-only"]).is_ok());
    assert!(StoreArgs::try_parse_from(["store", "view", "close", "--name", "n", "--root", "/r", "--discard"]).is_ok());
    assert!(StoreArgs::try_parse_from(["store", "view"]).is_err(), "a verb is required");
}

#[test]
fn a_typed_refusal_surfaces_as_a_stable_error_kind_line() {
    let r = protocol::Response::error_with_kind("path_refused", "root is a symlink".to_string());
    assert_eq!(error_kind_line(&r).as_deref(), Some("error_kind: path_refused"));
    assert_eq!(error_kind_line(&protocol::Response::error("plain failure")), None, "untyped errors print no kind line");
    let bad = protocol::Response::error_with_kind("Path Refused\nError: x", "m".to_string());
    assert_eq!(error_kind_line(&bad), None, "a malformed kind is never echoed");
    assert_eq!(error_kind_line(&protocol::Response::success(json!({}))), None);
}

#[test]
fn ls_renders_stores_and_one_store() {
    let v = json!({"stores": [{"name": "weftos://m/nodes/a/store", "kind": "node", "objects": 2, "bytes": 10, "grants": [{}], "archived": false}]});
    let t = render_ls(&v);
    assert!(t.contains("weftos://m/nodes/a/store") && t.contains("node"), "{t}");
    assert!(render_ls(&json!({"stores": []})).contains("no stores"));
    let one = json!({"store": {"name": "n", "bucket": "n-a", "archived": false,
        "grants": [{"principal": {"kind": "cog", "id": "c1"}, "access": "write", "enabled": false}]},
        "objects": [{"key": "k", "size": 3}]});
    let t = render_ls(&one);
    assert!(t.contains("cog:c1") && t.contains("(disabled)") && t.contains("objects: 1"), "{t}");
}
