//! Population test for the verified project principal (ADR-103 A6, Phase 2
//! package I). `ProjectAttestation::from_verified` has to be `pub` (the
//! daemon verifies in another crate), so "only a `VerifiedProject` stamps a
//! principal" is held by construction path and by this test, which scans the
//! sources of every crate (`src`, `tests`, `benches`, `examples`, `build.rs`):
//!
//! * `from_verified` is called only from `verified_project.rs` (unit-test
//!   files may build attestations; benches, examples and build scripts may
//!   not);
//! * `GatePrincipal::with_project` is called only where the attestation is
//!   already in hand (`governance.rs`, `governance_project.rs`);
//! * no `.project_id =` assignment (whitespace and newlines tolerated) exists
//!   in kernel or weave code outside `governance.rs` and the certificate
//!   tests that set a `ProjectCert` field;
//! * no `impl` of `ProjectAttestation` outside `governance.rs`, no
//!   `From`/trait bridge into `VerifiedProject`;
//! * every `GovernanceRequest` construction site in production code is in
//!   the tables below, each struct-literal site is followed by `.attributed`
//!   in its own statement, and none reads a project from request parameters.
//!
//! "Production code" is a file's text with its `#[cfg(test)] mod ... { }`
//! blocks cut out (the code after them is still scanned), minus `*_tests.rs`
//! files and `tests/` directories; those are scanned as well, but only by the
//! rules that apply to any code (the first four).

use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Module declarations in `text`: `(file name or module name, gated)`.
/// `gated` means a `#[cfg(..test..)]` attribute precedes it. A
/// `#[path = "dir/x_tests.rs"] mod m;` gives `x_tests.rs`; a plain `mod m;`
/// gives `m`.
fn declarations(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let (mut gated, mut path): (bool, Option<String>) = (false, None);
    for l in text.lines().map(str::trim) {
        if l.starts_with("#[") {
            if l.starts_with("#[cfg(") && l.contains("test") {
                gated = true;
            }
            if l.starts_with("#[path") {
                path = l.split('"').nth(1).map(|p| p.rsplit('/').next().unwrap_or("").to_owned());
            }
            continue;
        }
        let item = l.strip_prefix("pub(crate) ").or_else(|| l.strip_prefix("pub ")).unwrap_or(l);
        if let Some(r) = item.strip_prefix("mod ") {
            let name = r.split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("");
            out.push((path.take().unwrap_or_else(|| name.to_owned()), gated));
        }
        if !l.is_empty() {
            gated = false;
            path = None;
        }
    }
    out
}

struct Src {
    /// Path under `crates/`, `/`-separated.
    path: String,
    /// Comments removed; test modules removed unless `is_test_file`.
    prod: String,
    is_test_file: bool,
    /// Named like a unit-test file but no non-test file of its crate
    /// declares it through `#[cfg(test)] mod` / `#[path]`.
    undeclared_test_file: bool,
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn strip_comments(text: &str) -> String {
    text.lines()
        .map(|l| {
            // Line comments only; a `//` inside a string literal in these
            // files would only hide code from the scan, never add some.
            match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cut every `#[cfg(test)] mod name { ... }` block (and `mod name;`).
fn strip_test_modules(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..i]);
        let after = &rest[i + "#[cfg(test)]".len()..];
        // Skip further attributes, then look at the item.
        let item = after.trim_start();
        let item = {
            let mut it = item;
            while it.starts_with("#[") {
                it = it[it.find(']').map_or(it.len(), |j| j + 1)..].trim_start();
            }
            it
        };
        let is_mod = item.starts_with("mod ") || item.starts_with("pub mod ") || item.starts_with("pub(crate) mod ");
        if !is_mod {
            // A lone cfg(test) item (fn, use, impl): keep scanning after the attribute.
            rest = after;
            continue;
        }
        let off = text.len() - item.len();
        let semi = item.find(';');
        let brace = item.find('{');
        match (semi, brace) {
            (Some(s), b) if b.is_none_or(|b| s < b) => rest = &text[off + s + 1..],
            (_, Some(b)) => {
                let mut depth = 0usize;
                let mut end = item.len();
                for (k, c) in item[b..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = b + k + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                rest = &text[off + end..];
            }
            _ => rest = "",
        }
    }
    out.push_str(rest);
    out
}

fn sources() -> Vec<Src> {
    let root = crates_dir();
    let mut v = Vec::new();
    for krate in std::fs::read_dir(&root).unwrap().flatten() {
        if krate.file_name() == "archive" || !krate.path().is_dir() {
            continue;
        }
        let mut files = Vec::new();
        for sub in ["src", "tests", "benches", "examples"] {
            collect(&krate.path().join(sub), &mut files);
        }
        let build = krate.path().join("build.rs");
        if build.is_file() {
            files.push(build);
        }
        // (rel, text, named_test, integration)
        let mut loaded: Vec<(String, String, bool, bool)> = Vec::new();
        for p in files {
            let rel = p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let parts: Vec<&str> = rel.split('/').collect();
            let integration = parts.get(1) == Some(&"tests");
            // A unit-test file: by name, or inside a `tests/` directory of `src`.
            let in_src_tests_dir = parts.get(1) == Some(&"src") && parts[2..parts.len() - 1].contains(&"tests");
            let named_test = name.ends_with("_tests.rs") || name == "tests.rs" || name.starts_with("tests_") || in_src_tests_dir;
            let text = strip_comments(&std::fs::read_to_string(&p).unwrap());
            loaded.push((rel, text, named_test, integration));
        }
        // Declared names: `#[cfg(test)]`-gated declarations in non-test files,
        // then, transitively, every declaration inside a declared test file.
        let key = |rel: &str| -> Vec<String> {
            let name = rel.rsplit('/').next().unwrap().to_owned();
            let stem = name.trim_end_matches(".rs").to_owned();
            let dir = rel.rsplit('/').nth(1).unwrap_or("").to_owned();
            if name == "mod.rs" { vec![dir] } else { vec![stem, name] }
        };
        let mut declared: Vec<String> = loaded
            .iter()
            .filter(|(_, _, named, integ)| !named && !integ)
            .flat_map(|(_, t, _, _)| declarations(t))
            .filter(|(_, gated)| *gated)
            .map(|(n, _)| n)
            .collect();
        loop {
            let before = declared.len();
            for (rel, text, named, integ) in &loaded {
                if *named && !*integ && key(rel).iter().any(|k| declared.contains(k)) {
                    for (n, _) in declarations(text) {
                        if !declared.contains(&n) {
                            declared.push(n);
                        }
                    }
                }
            }
            if declared.len() == before {
                break;
            }
        }
        for (rel, text, named_test, integration) in loaded {
            let declared_ok = key(&rel).iter().any(|k| declared.contains(k));
            let undeclared_test_file = named_test && !integration && !declared_ok;
            // Exempt only integration tests and declared unit-test files.
            let is_test_file = integration || (named_test && declared_ok);
            let prod = if is_test_file { text } else { strip_test_modules(&text) };
            v.push(Src { path: rel, prod, is_test_file, undeclared_test_file });
        }
    }
    v
}

const SELF_FILE: &str = "clawft-weave/tests/project_principal_population.rs";

fn kernel_or_weave(p: &str) -> bool {
    p.starts_with("clawft-kernel/") || p.starts_with("clawft-weave/")
}

fn files_with(srcs: &[Src], needle: &str) -> Vec<String> {
    let mut v: Vec<String> = srcs
        .iter()
        .filter(|s| s.path != SELF_FILE && !s.is_test_file && s.prod.contains(needle))
        .map(|s| s.path.clone())
        .collect();
    v.sort();
    v
}

/// Whether `text` contains `.project_id` followed by optional whitespace and
/// a lone `=` (an assignment, not `==` or `=>`).
fn assigns_project_id(text: &str) -> Option<usize> {
    let pat = ".project_id";
    let mut from = 0;
    while let Some(i) = text[from..].find(pat) {
        let at = from + i;
        let after = text[at + pat.len()..].trim_start();
        if after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>") {
            return Some(text[..at].lines().count());
        }
        from = at + pat.len();
    }
    None
}

#[test]
fn every_exempt_test_file_is_declared_by_a_cfg_test_mod_or_path() {
    let bad: Vec<String> = sources().into_iter().filter(|s| s.undeclared_test_file).map(|s| s.path).collect();
    assert!(bad.is_empty(), "test-named files not declared via #[cfg(test)] mod / #[path]: {bad:?}");
    let d = declarations("#[cfg(test)]\n#[path = \"a/b_tests.rs\"]\nmod tests;\n#[cfg(all(test, unix))]\nmod x {}\nmod plain;");
    assert_eq!(d, [("b_tests.rs".to_owned(), true), ("x".to_owned(), true), ("plain".to_owned(), false)]);
}

#[test]
fn from_verified_is_called_only_by_verified_project_attest() {
    // The Wasmtime project guest cannot link clawft-weave. It mints exactly one
    // attestation, as `BoundKernel`, after `cert_check`/`ack_check` verified its
    // certificate against the pinned user key and its own project key.
    let hits = files_with(&sources(), "ProjectAttestation::from_verified(");
    assert_eq!(
        hits,
        ["clawft-wasm/src/project_kernel/supervisor.rs", "clawft-weave/src/verified_project.rs"],
        "attestations are minted only by VerifiedProject::attest and the certificate-verifying Wasmtime guest"
    );
}

#[test]
fn verified_project_constructors_are_called_only_from_their_verification_sites() {
    // The three laundering-prone constructors (a `String` or a `TokenInfo`
    // becomes a trusted project id): each has exactly the call sites that
    // sit right after the check that justifies it.
    let srcs = sources();
    assert_eq!(
        files_with(&srcs, "VerifiedProject::from_verified_forward("),
        ["clawft-weave/src/project_forward.rs"],
        "a forward-header principal is minted only by ForwardVerifier::verify (signature, window, replay, project binding)"
    );
    assert_eq!(
        files_with(&srcs, "VerifiedProject::from_token("),
        ["clawft-weave/src/caller_principal.rs", "clawft-weave/src/shared_rpc.rs"],
        "a token principal is minted only from a TokenInfo the authority returned"
    );
    assert_eq!(
        files_with(&srcs, "VerifiedProject::from_bound("),
        ["clawft-weave/src/caller_principal.rs"],
        "the bound-kernel principal is minted only from the handshake state"
    );
}

#[test]
fn with_project_is_called_only_with_an_attestation_in_hand() {
    let hits = files_with(&sources(), ".with_project(");
    assert_eq!(hits, ["clawft-kernel/src/governance.rs", "clawft-kernel/src/governance_project.rs"]);
}

#[test]
fn no_project_id_assignment_outside_governance() {
    // `ProjectCert` / `ProjectAnchorStmt` field edits in the identity tests
    // are another type's `project_id`.
    let other_type = [
        "clawft-kernel/src/project_identity_tests.rs",
        "clawft-weave/src/project_forward_tests.rs",
        // Builds a tampered `ProjectAnchorStmt` (P2 D) to test refusal.
        "clawft-weave/src/anchor_rpc_tests.rs",
    ];
    for s in sources() {
        if !kernel_or_weave(&s.path)
            || s.path == "clawft-kernel/src/governance.rs"
            || s.path == SELF_FILE
            || other_type.contains(&s.path.as_str())
        {
            continue;
        }
        if let Some(line) = assigns_project_id(&s.prod) {
            panic!("{}: `.project_id =` outside governance.rs (near line {line})", s.path);
        }
    }
    assert!(assigns_project_id("p.project_id\n   = x").is_some());
    assert!(assigns_project_id("a.project_id == b").is_none());
}

#[test]
fn the_attestation_path_has_no_impls_outside_its_modules() {
    for s in sources() {
        if s.path == SELF_FILE {
            continue;
        }
        for l in s.prod.lines().map(str::trim).filter(|l| l.starts_with("impl")) {
            if l.contains("ProjectAttestation") {
                assert_eq!(s.path, "clawft-kernel/src/governance.rs", "{}: {l}", s.path);
            }
            assert!(
                !(l.contains("VerifiedProject") && (l.contains(" for VerifiedProject") || l.contains("From<"))),
                "{}: bridge into VerifiedProject: {l}",
                s.path
            );
        }
    }
}

/// Production `GovernanceRequest::new` sites (attributed inside `new`).
const NEW_SITES: &[&str] = &[
    "clawft-kernel/src/http_api.rs",
    "clawft-kernel/src/profile_store.rs",
    "clawft-kernel/src/hnsw_service.rs",
    "clawft-kernel/src/causal.rs",
    "clawft-kernel/src/wasm_runner/runner.rs",
    // cogbus (ADR-122 B3): agent = the token-authenticated cog instance;
    // project and instance principal from `new`'s `attributed()` (the
    // kernel attestation); context is topic, cog id and schema only.
    "clawft-weave/src/cogbus_governance.rs",
];
/// Production `GovernanceRequest { .. }` literal sites.
const LITERAL_SITES: &[&str] = &["clawft-kernel/src/gate.rs", "clawft-kernel/src/workload_governance/gate.rs"];

/// Start offsets of every `GovernanceRequest {` struct literal in `text`.
fn literals(text: &str) -> Vec<usize> {
    let mut v = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("GovernanceRequest {") {
        let at = from + i;
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        let head = text[line_start..at].trim();
        // Not the type's definition or an `impl GovernanceRequest {`.
        if !(head.ends_with("struct") || head.ends_with("pub struct") || head.starts_with("impl") || head.contains("impl ")) {
            v.push(at);
        }
        from = at + 1;
    }
    v
}

/// The statement tail after the literal starting at `at`: from the closing
/// brace of the literal to the next `;`.
fn tail_after_literal(text: &str, at: usize) -> &str {
    let open = at + text[at..].find('{').unwrap();
    let mut depth = 0usize;
    let mut end = text.len();
    for (k, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + k + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let rest = &text[end..];
    &rest[..rest.find(';').unwrap_or(rest.len())]
}

#[test]
fn every_governance_request_site_is_enumerated_and_attributed_from_the_attestation() {
    let srcs = sources();
    let (mut seen_new, mut seen_lit) = (Vec::new(), Vec::new());
    for s in srcs.iter().filter(|s| kernel_or_weave(&s.path) && !s.is_test_file) {
        if s.path != "clawft-kernel/src/governance.rs" && s.prod.contains("GovernanceRequest::new(") {
            seen_new.push(s.path.clone());
        }
        let lits = literals(&s.prod);
        if !lits.is_empty() {
            seen_lit.push(s.path.clone());
        }
        // Per site, not per file: each literal's own statement is attributed.
        for at in lits {
            assert!(
                tail_after_literal(&s.prod, at).contains(".attributed"),
                "{}: a GovernanceRequest literal is not attributed in its own statement",
                s.path
            );
        }
    }
    let (mut want_new, mut want_lit) = (NEW_SITES.to_vec(), LITERAL_SITES.to_vec());
    seen_new.sort();
    seen_lit.sort();
    want_new.sort();
    want_lit.sort();
    assert_eq!(seen_new, want_new, "GovernanceRequest::new sites changed: update NEW_SITES (and review them)");
    assert_eq!(seen_lit, want_lit, "GovernanceRequest literal sites changed: update LITERAL_SITES (and review them)");

    // `new` attributes the request itself.
    let gov = srcs.iter().find(|s| s.path == "clawft-kernel/src/governance.rs").unwrap();
    let new_at = gov.prod.find("pub fn new(agent_id: impl Into<String>, action: impl Into<String>)").unwrap();
    assert!(gov.prod[new_at..new_at + 600].contains(".attributed()"), "GovernanceRequest::new must call attributed()");
}

#[test]
fn no_governance_request_site_reads_a_project_from_request_params() {
    let sites: Vec<&str> = NEW_SITES.iter().chain(LITERAL_SITES).copied().collect();
    for s in sources().iter().filter(|s| sites.contains(&s.path.as_str())) {
        for (n, l) in s.prod.lines().enumerate() {
            for bad in ["\"project_id\"", "\"project\"", "params.project", ".project_id"] {
                // The workload gate may read the attestation's own id.
                let attestation_read =
                    s.path.ends_with("workload_governance/gate.rs") && l.contains("project_id") && !l.contains('"');
                assert!(!l.contains(bad) || attestation_read, "{}:{}: governance site names a project: {l}", s.path, n + 1);
            }
        }
    }
}

#[test]
fn reserved_context_keys_are_the_only_project_keys_and_are_kernel_owned() {
    let srcs = sources();
    let t = srcs.iter().find(|s| s.path == "clawft-kernel/src/governance_project.rs").unwrap();
    assert!(t.prod.contains(r#"RESERVED_CONTEXT_KEYS: &[&str] = &["project_id", "instance_id"]"#));
}

#[test]
fn the_scanner_cuts_test_modules_and_keeps_code_after_them() {
    let text = "fn a() {}\n#[cfg(test)]\nmod t { fn x() { let _ = 1; } }\nfn after() { GovernanceRequest::new(1) }\n#[cfg(test)]\nfn lone() {}\nfn last() {}";
    let cut = strip_test_modules(text);
    assert!(cut.contains("fn a()") && cut.contains("fn after()") && cut.contains("fn last()"));
    assert!(!cut.contains("fn x()"));
}
