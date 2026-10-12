use super::*;

const ULID: &str = "01K6ZQ8N3T4V5W6X7Y8Z9A0B1C";
const MESH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn home() -> &'static Path {
    Path::new("/Users/m")
}

#[test]
fn the_four_spellings_are_told_apart() {
    let uri = format!("weftos://{MESH}/projects/{ULID}");
    assert!(matches!(parse_spec(&uri), Ok(SourceSpec::Mesh { ulid, .. }) if ulid == ULID));
    assert_eq!(parse_spec(ULID), Ok(SourceSpec::Ulid(ULID.into())));
    assert_eq!(parse_spec("https://github.com/org/demo.git"), Ok(SourceSpec::Git("https://github.com/org/demo.git".into())));
    assert_eq!(parse_spec("git@github.com:org/demo.git"), Ok(SourceSpec::Git("git@github.com:org/demo.git".into())));
    assert_eq!(parse_spec("0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d"), Err(SpecError::DashboardId));
}

#[test]
fn refusals_name_the_rule_and_never_echo_credentials() {
    assert!(matches!(parse_spec("weftos://not-a-mesh/projects/x"), Err(SpecError::Uri(_))));
    assert!(matches!(parse_spec(&format!("weftos://{MESH}/projects/{ULID}/repos/docs")), Err(SpecError::Uri(_))));
    assert!(matches!(parse_spec(&format!("weftos://{MESH}/cogs/abc")), Err(SpecError::Uri(_))));
    assert!(matches!(parse_spec("/etc/passwd"), Err(SpecError::Unrecognised(..))));
    assert!(matches!(parse_spec("-oProxyCommand=x"), Err(SpecError::Unrecognised(..))));
    let e = parse_spec("https://user:hunter2@github.com/org/demo.git").unwrap_err().to_string();
    assert!(!e.contains("hunter2"), "{e}");
}

#[test]
fn mesh_names_go_over_the_mesh_and_git_urls_to_git() {
    assert_eq!(choose_fetcher(&SourceSpec::Ulid(ULID.into())), FetchPlan::Mesh);
    assert_eq!(choose_fetcher(&SourceSpec::Git("https://h/o/r".into())), FetchPlan::Git);
}

#[test]
fn slug_comes_from_the_flag_the_path_or_the_repository_name_never_a_guess() {
    let git = parse_spec("git@github.com:org/My_Repo.git").unwrap();
    assert_eq!(choose_slug(&git, None, None).unwrap(), "my-repo");
    assert_eq!(choose_slug(&git, Some("x1"), None).unwrap(), "x1");
    assert_eq!(choose_slug(&git, None, Some(Path::new("/Users/m/code/Demo App"))).unwrap(), "demo-app");
    let ulid = SourceSpec::Ulid(ULID.into());
    assert_eq!(choose_slug(&ulid, None, None), Err(SpecError::NeedsSlug));
    assert_eq!(choose_slug(&ulid, Some("Bad Slug"), None), Err(SpecError::Slug("Bad Slug".into())));
    assert_eq!(choose_slug(&ulid, None, Some(Path::new("/Users/m/code/shasta"))).unwrap(), "shasta");
}

#[test]
fn plan_defaults_to_projects_slug_and_builds_the_request() {
    let p = plan_add("https://github.com/org/demo.git", None, None, Some("dev"), home()).unwrap();
    assert_eq!(p.target, Path::new("/Users/m/Projects/demo"));
    assert_eq!(p.fetch, FetchPlan::Git);
    assert_eq!(p.request.sources[0].branch.as_deref(), Some("dev"));
    assert_eq!(p.request.sources[0].dir, ".");
    let uri = format!("weftos://{MESH}/projects/{ULID}");
    let m = plan_add(&uri, Some("shasta"), Some("~/dev/Shasta"), None, home()).unwrap();
    assert_eq!(m.target, Path::new("/Users/m/dev/Shasta"));
    assert_eq!(m.request.project_ulid, ULID);
    assert!(m.request.sources.is_empty());
}

#[test]
fn plan_refuses_paths_outside_home_and_bad_branches() {
    assert!(matches!(plan_add("https://h/o/r", None, Some("/etc/x"), None, home()), Err(SpecError::Path(_))));
    assert!(matches!(plan_add("https://h/o/r", None, Some("~/../x"), None, home()), Err(SpecError::Path(_))));
    assert_eq!(plan_add("https://h/o/r", None, None, Some("--evil"), home()).unwrap_err(), SpecError::Branch);
    let uri = format!("weftos://{MESH}/projects/{ULID}");
    assert_eq!(plan_add(&uri, None, None, None, home()).unwrap_err(), SpecError::NeedsSlug);
    assert_eq!(plan_add(ULID, Some("x"), None, None, home()).unwrap_err(), SpecError::NeedsMesh);
}
