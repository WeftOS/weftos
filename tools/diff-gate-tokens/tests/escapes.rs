//! Ports of the gate's Rust-rule fixtures (scripts/tests/diff-gate-test.sh), the regex
//! escapes found by the adversarial review rounds, and the cases strings and comments must
//! not trigger. `new_file` treats every line as added; `change` diffs base -> head by line.

use diff_gate_tokens::report::{findings, tsv, Finding};
use std::collections::BTreeSet;

fn new_file(src: &str) -> Vec<Finding> {
    let n = src.split('\n').count();
    findings(src, None, &(1..=n).collect())
}

/// Added lines: head lines with no equal line at the same position in a simple LCS diff.
fn change(base: &str, head: &str) -> Vec<Finding> {
    let b: Vec<&str> = base.split('\n').collect();
    let h: Vec<&str> = head.split('\n').collect();
    let mut dp = vec![vec![0usize; h.len() + 1]; b.len() + 1];
    for i in (0..b.len()).rev() {
        for j in (0..h.len()).rev() {
            dp[i][j] = if b[i] == h[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut added = BTreeSet::new();
    while j < h.len() {
        if i < b.len() && b[i] == h[j] {
            i += 1;
            j += 1;
        } else if i < b.len() && dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            added.insert(j + 1);
            j += 1;
        }
    }
    findings(head, Some(base), &added)
}

fn rules(fs: &[Finding]) -> Vec<String> {
    let mut v: Vec<String> = fs.iter().map(|f| f.rule.clone()).collect();
    v.sort();
    v.dedup();
    v
}

fn has(fs: &[Finding], rule: &str, line: usize) -> bool {
    fs.iter().any(|f| f.rule == rule && f.line == line)
}

fn none(fs: &[Finding]) {
    assert!(fs.is_empty(), "expected no findings, got {fs:#?}");
}

// ── Ported fixtures ──────────────────────────────────────────────────

#[test]
fn comments_strings_and_lint_attrs_pass() {
    none(&new_file("// unsafe { x }\nfn a() {}\n"));
    none(&new_file("const S: &str = \"unsafe\";\n"));
    none(&new_file("#![forbid(unsafe_code)]\n"));
}

#[test]
fn each_rule_added_fails_and_on_base_passes() {
    assert_eq!(rules(&new_file("fn f() { unsafe { g() } }\n")), ["unsafe"]);
    let base = "pub fn a() {}\nfn f() { unsafe { g() } }\n";
    none(&change(base, &format!("{base}fn z() {{}}\n")));
    assert_eq!(rules(&new_file("use std::net::TcpStream;\n")), ["net"]);
    assert_eq!(
        rules(&new_file("fn g() { let _ = reqwest::Client::new(); }\n")),
        ["net"]
    );
    none(&new_file("use std::net::{IpAddr, SocketAddr};\n"));
    let base = "use std::net::UdpSocket;\n";
    none(&change(base, &format!("{base}fn z() {{}}\n")));
    assert_eq!(
        rules(&new_file(
            "fn g() { std::process::Command::new(\"ls\"); }\n"
        )),
        ["process"]
    );
    assert_eq!(
        rules(&new_file("use tokio::process::Child;\n")),
        ["process"]
    );
    let base = "fn g() { Command::new(\"ls\"); }\n";
    none(&change(base, &format!("{base}fn z() {{}}\n")));
    assert_eq!(
        rules(&new_file("extern \"C\" { fn abs(x: i32) -> i32; }\n")),
        ["ffi"]
    );
    assert_eq!(rules(&new_file("#[no_mangle] pub fn q() {}\n")), ["ffi"]);
}

#[test]
fn lexer_fixtures() {
    assert!(has(
        &new_file("fn f(p: *mut u8) {\n    *p = unsafe { std::ptr::read(p) };\n}\n"),
        "unsafe",
        2
    ));
    assert!(has(
        &new_file("/**/ fn f(p: *const u8) { let _s = unsafe { *p }; }\n"),
        "unsafe",
        1
    ));
    assert!(has(
        &new_file("fn f() { let _q = '\"'; unsafe { g() } }\n"),
        "unsafe",
        1
    ));
    assert!(has(
        &new_file("fn f() { let _r = r\"\\\"; std::process::Command::new(\"sh\"); }\n"),
        "process",
        1
    ));
    assert!(has(
        &new_file("const S: &str = \"a\nb\"; fn f() { unsafe { g() } }\n"),
        "unsafe",
        2
    ));
    assert!(has(
        &new_file("fn f<'a>(x: &'a u8) -> &'a u8 { unsafe { g(x) } }\n"),
        "unsafe",
        1
    ));
    none(&new_file("fn f() { let _c = 'u'; let _r = r#\"unsafe \"Command::new\"\"#; /* a /* unsafe */ b */ }\n"));
    assert!(has(
        &new_file("fn f() { let _s = \"a\u{0}b\"; unsafe { g() } }\n"),
        "unsafe",
        1
    ));
}

#[test]
fn alias_ffi_and_net_fixtures() {
    assert_eq!(
        rules(&new_file(
            "use std::{process::Command as K}; fn f() { K::new(\"sh\"); }\n"
        )),
        ["process"]
    );
    assert_eq!(
        rules(&new_file(
            "fn f() { let _ = tokio::net::TcpSocket::new_v4(); }\n"
        )),
        ["net"]
    );
    assert_eq!(rules(&new_file("extern \"system\" fn f() {}\n")), ["ffi"]);
    assert_eq!(
        rules(&new_file("extern { fn abs(x: i32) -> i32; }\n")),
        ["ffi"]
    );
    none(&new_file("extern crate alloc;\n"));
}

#[test]
fn include_fixtures() {
    assert_eq!(
        rules(&new_file("include!(\"../evil.inc\");\n")),
        ["include"]
    );
    assert_eq!(
        rules(&new_file("#[path = \"evil.txt\"] mod m;\n")),
        ["include"]
    );
    assert_eq!(
        rules(&new_file(
            "include!(concat!(env!(\"OUT_DIR\"), \"/x.rs\"));\n"
        )),
        ["include"]
    );
    none(&new_file("include!(\"gen.rs\");\n"));
    assert!(has(
        &new_file("use core::include as inc; inc!(\"x.txt\");\n"),
        "include",
        1
    ));
}

// ── Escapes from the review rounds ───────────────────────────────────

#[test]
fn escape_spaces_around_path_separators() {
    assert!(has(
        &new_file("fn f() { std :: process :: Command :: new(\"sh\"); }\n"),
        "process",
        1
    ));
    assert!(has(
        &new_file("fn f() { let _ = std\n    ::process\n    ::Command::args; }\n"),
        "process",
        3
    ));
}

#[test]
fn escape_glob_plus_alias() {
    let fs = new_file("use std::process::*;\nuse Command as C;\nfn f() { C::args; }\n");
    assert!(has(&fs, "process", 1));
    assert!(has(&fs, "process", 2));
    assert!(has(&fs, "process", 3));
}

#[test]
fn escape_multi_line_brace_import() {
    let src =
        "use std::{\n    process::{\n        Command as K,\n    },\n};\nfn f() { K::args; }\n";
    let fs = new_file(src);
    assert!(has(&fs, "process", 3), "{fs:#?}");
    assert!(has(&fs, "process", 6));
    // Only the use line's prefix changed: still a finding on the leaf.
    let base = src.replace("std::{", "mock::{");
    let fs = change(&base, src);
    assert!(has(&fs, "process", 3), "{fs:#?}");
    assert!(
        has(&fs, "process", 6),
        "usage line changed meaning: {fs:#?}"
    );
}

#[test]
fn escape_absolute_crate_alias() {
    let fs = new_file("use ::reqwest as r;\nfn f() { let _ = r::get(\"http://x\"); }\n");
    assert!(has(&fs, "net", 1));
    assert!(has(&fs, "net", 2));
}

#[test]
fn escape_multi_line_cfg_attr_path() {
    let src = "#[cfg_attr(\n    unix,\n    path = \"x.txt\"\n)]\nmod m;\n";
    assert!(has(&new_file(src), "include", 3));
    let base = src.replace("unix", "windows");
    assert!(
        has(&change(&base, src), "include", 3),
        "an edit on another line of the attribute"
    );
}

#[test]
fn escape_doctest_with_command() {
    let src = "/// Runs it.\n///\n/// ```\n/// std::process::Command::new(\"sh\").status().unwrap();\n/// ```\npub fn f() {}\n";
    let fs = new_file(src);
    assert!(has(&fs, "process", 4), "{fs:#?}");
    assert_eq!(fs[0].note, "doctest");
}

#[test]
fn escape_doc_include_str() {
    assert!(has(
        &new_file("#![doc = include_str!(\"README.md\")]\n"),
        "include",
        1
    ));
    assert!(has(
        &new_file("#[cfg_attr(doc, doc = include_str!(\"x.md\"))]\npub fn f() {}\n"),
        "include",
        1
    ));
    none(&new_file("const B: &[u8] = include_bytes!(\"x.bin\");\n"));
}

#[test]
fn escape_global_asm() {
    assert!(has(
        &new_file("core::arch::global_asm!(\".globl x\");\n"),
        "ffi",
        1
    ));
    assert!(has(
        &new_file("use std::arch::global_asm;\nglobal_asm!(\"nop\");\n"),
        "ffi",
        2
    ));
}

// ── Escapes this lane found ──────────────────────────────────────────

#[test]
fn raw_identifiers_are_the_same_name() {
    assert!(has(
        &new_file("fn f() { std::r#process::Command::args; }\n"),
        "process",
        1
    ));
    none(&new_file("fn r#unsafe() {}\n"));
}

#[test]
fn added_line_inside_an_existing_unsafe_block() {
    let base = "fn f(p: *mut u8) {\n    unsafe {\n        ok();\n    }\n}\n";
    let head = "fn f(p: *mut u8) {\n    unsafe {\n        ok();\n        p.write(9);\n    }\n}\n";
    let fs = change(base, head);
    assert!(has(&fs, "unsafe", 4), "{fs:#?}");
    let head2 =
        "fn f(p: *mut u8) {\n    unsafe {\n        ok();\n        // just a comment\n    }\n}\n";
    none(&change(base, head2));
}

#[test]
fn added_declaration_inside_an_existing_extern_block() {
    let base = "extern \"C\" {\n    fn abs(x: i32) -> i32;\n}\n";
    let head =
        "extern \"C\" {\n    fn abs(x: i32) -> i32;\n    fn system(c: *const u8) -> i32;\n}\n";
    assert!(has(&change(base, head), "ffi", 3));
}

#[test]
fn import_edit_changes_an_unchanged_line() {
    let base = "use mock as p;\nfn f() { p::Command::args; }\n";
    let head = "use std::process as p;\nfn f() { p::Command::args; }\n";
    let fs = change(base, head);
    assert!(has(&fs, "process", 1));
    assert!(has(&fs, "process", 2), "{fs:#?}");
}

#[test]
fn extern_crate_aliases() {
    assert!(has(
        &new_file("extern crate std as s;\nfn f() { s::process::Command::args; }\n"),
        "process",
        2
    ));
    assert_eq!(rules(&new_file("extern crate reqwest;\n")), ["net"]);
}

#[test]
fn ident_pasting_and_macro_bodies() {
    assert!(has(
        &new_file("paste::paste! { fn f() { [<Com mand>]::new(); } }\n"),
        "include",
        1
    ));
    assert!(has(
        &new_file("macro_rules! m { () => { std::process::Command::args } }\n"),
        "process",
        1
    ));
}

#[test]
fn doctest_forms() {
    let hidden =
        "/// ```\n/// # use std::process::Command as C;\n/// C::args;\n/// ```\npub fn f() {}\n";
    assert!(has(&new_file(hidden), "process", 3));
    let indented =
        "/// Example:\n///\n///     unsafe { core::ptr::null::<u8>().read() };\npub fn f() {}\n";
    assert!(has(&new_file(indented), "unsafe", 3));
    let text = "/// ```text\n/// std::process::Command::new(\"sh\")\n/// ```\npub fn f() {}\n";
    none(&new_file(text));
    let block = "/** ```\nunsafe { x() }\n``` */\npub fn f() {}\n";
    assert!(has(&new_file(block), "unsafe", 2), "{:#?}", new_file(block));
}

#[test]
fn globs_of_ancestor_modules() {
    assert!(has(&new_file("use std::*;\n"), "process", 1));
    let fs = new_file("use libc::*;\nfn f() { execvp(a, b); }\n");
    assert!(has(&fs, "process", 1) && has(&fs, "process", 2), "{fs:#?}");
}

#[test]
fn unsafe_attribute_and_link_attrs() {
    let fs = new_file("#[unsafe(no_mangle)] pub fn q() {}\n");
    assert_eq!(rules(&fs), ["ffi", "unsafe"]);
    assert_eq!(
        rules(&new_file("#[export_name = \"x\"] pub fn q() {}\n")),
        ["ffi"]
    );
    assert_eq!(
        rules(&new_file("#[link_section = \".x\"] static S: u8 = 0;\n")),
        ["ffi"]
    );
}

#[test]
fn not_flagged() {
    none(&new_file("// std::process::Command::new(\"sh\")\nfn f() { let _ = \"std::process::Command::new\"; let _ = b\"TcpStream\"; }\n"));
    none(&new_file("fn f() { std::process::exit(1) }\n"));
    none(&new_file(
        "use std::process::ExitCode;\nfn main() -> ExitCode { ExitCode::SUCCESS }\n",
    ));
    none(&new_file("#!/usr/bin/env run-cargo-script\nfn main() {}\n"));
    none(&new_file(
        "use clap::Command as Cli;\nfn f() { Cli::about; }\n",
    ));
}

#[test]
fn parse_errors_fail_closed() {
    assert_eq!(rules(&new_file("fn f() {\n")), ["rust-parse"]);
    let fs = new_file("fn f() { let = ; unsafe { g() } }\n");
    assert_eq!(rules(&fs), ["rust-parse", "unsafe"]);
}

#[test]
fn crlf_and_tsv_spans() {
    let fs = new_file("fn a() {}\r\n\tuse std::net::TcpStream;\r\n");
    assert!(has(&fs, "net", 2));
    let out = tsv("src/lib.rs", &fs);
    assert_eq!(
        out,
        "net\tsrc/lib.rs\t2\tuse std::net::TcpStream;\t\t5-23\tuse std::net::TcpStream;\n"
    );
}

// ── Noise controls measured on real history (2026-11-15) ─────────────

#[test]
fn types_imported_under_their_own_name_are_not_findings_on_use() {
    let base = "use hyper::StatusCode;\nuse std::process::Command;\n";
    let head = format!("{base}fn f() {{ StatusCode::OK; }}\nfn r(c: &mut Command) {{}}\n");
    none(&change(base, &head));
    let head = format!("{base}fn g() {{ Command::new(\"sh\"); }}\n");
    assert!(
        has(&change(base, &head), "process", 3),
        "Command::new is still found by name"
    );
}

#[test]
fn a_new_unsafe_block_is_one_finding() {
    let fs = new_file("fn f() {\n    unsafe {\n        a();\n        b();\n    }\n}\n");
    assert_eq!(fs.len(), 1, "{fs:#?}");
    assert!(has(&fs, "unsafe", 2));
}

#[test]
fn an_extern_fn_body_is_not_a_foreign_block() {
    let base = "extern \"C\" fn cb() {\n    a();\n}\n";
    let head = "extern \"C\" fn cb() {\n    a();\n    b();\n}\n";
    none(&change(base, head));
}
