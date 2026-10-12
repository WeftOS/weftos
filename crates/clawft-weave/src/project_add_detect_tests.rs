use super::*;

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn none_set(_: &str) -> bool {
    false
}

#[test]
fn process_compose_ports_and_routes_are_reported_as_found() {
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "process-compose.yaml", "processes: {}\n");
    write(t.path(), "compose/ports.yaml", "project: shasta\nclaims:\n  - { port: 18110, use: process-compose-http }\n  - { port: 18120, use: web }\nroutes:\n  - { prefix: /shasta, port: 18120 }\n");
    let d = detect(t.path(), &none_set);
    assert_eq!(d.run, RunKind::ProcessCompose { file: "process-compose.yaml".into() });
    assert_eq!(d.pc_http, Some(18110));
    assert_eq!(d.claims.len(), 2);
    assert_eq!(d.repo_routes[0].prefix, "/shasta");
    assert_eq!(d.ports_project.as_deref(), Some("shasta"));
    assert!(d.notes.is_empty(), "{:?}", d.notes);
}

#[test]
fn nothing_recognisable_says_so_instead_of_guessing() {
    let t = tempfile::tempdir().unwrap();
    let d = detect(t.path(), &none_set);
    assert_eq!(d.run, RunKind::Unknown);
    assert!(d.notes.iter().any(|n| n.contains("nothing to start")), "{:?}", d.notes);
    assert!(d.notes.iter().any(|n| n.contains("no compose/ports.yaml")));
}

#[test]
fn package_scripts_alone_are_not_run_silently() {
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "package.json", r#"{"scripts":{"dev":"next dev","build":"next build"}}"#);
    let d = detect(t.path(), &none_set);
    assert_eq!(d.run, RunKind::PackageScripts { scripts: vec!["build".into(), "dev".into()] });
    assert!(d.notes.iter().any(|n| n.contains("only runs process-compose")));
}

#[test]
fn compose_without_a_pc_port_claim_is_flagged() {
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "process-compose.yaml", "processes: {}\n");
    let d = detect(t.path(), &none_set);
    assert!(d.notes.iter().any(|n| n.contains("process-compose-http")), "{:?}", d.notes);
}

#[test]
fn env_names_come_from_examples_and_compose_never_values() {
    let t = tempfile::tempdir().unwrap();
    write(t.path(), ".env.example", "# comment\nOPENROUTER_API_KEY=sk-super-secret\nexport DB_URL=\nlower=1\nPORT=3000\n");
    write(t.path(), "process-compose.yaml", "environment:\n  - \"TOKEN=${GATEWAY_TOKEN}\"\n  - \"X=${HAS_DEFAULT:-1}\"\n  - A=${PORT}\n");
    write(t.path(), ".env", "REAL_SECRET=nope\n");
    let d = detect(t.path(), &|n| n == "PORT");
    let names: Vec<&str> = d.env_vars.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["OPENROUTER_API_KEY", "DB_URL", "PORT", "GATEWAY_TOKEN"]);
    assert!(d.env_vars.iter().find(|e| e.name == "PORT").unwrap().set_here);
    assert!(!d.env_vars.iter().find(|e| e.name == "DB_URL").unwrap().set_here);
    let dump = serde_json::to_string(&d).unwrap();
    assert!(!dump.contains("sk-super-secret") && !dump.contains("REAL_SECRET"), "{dump}");
}

#[test]
fn env_list_is_bounded() {
    let t = tempfile::tempdir().unwrap();
    let many: String = (0..200).map(|i| format!("VAR_{i}=\n")).collect();
    write(t.path(), ".env.example", &many);
    assert_eq!(detect(t.path(), &none_set).env_vars.len(), MAX_ENV_NAMES);
}
