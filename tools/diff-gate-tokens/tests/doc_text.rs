//! Review round 4: doc text is code (R4-D1, R4-D2) and raw identifiers name the same
//! attributes (R4-D3). `new_file` treats every line as added; `change` diffs base -> head.

use diff_gate_tokens::report::{findings, Finding};
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

/// R4-D3: a raw identifier names the same attribute (`r#doc`, `r#cfg_attr`, `r#path`,
/// `r#no_mangle`, `r#export_name`, `r#link_section`): rustdoc and rustc treat each like the
/// plain form. Every attribute form, at the top level and nested.
#[test]
fn r4_d3_raw_identifier_attribute_names() {
    let cmd = r#"std::process::Command::new(\"sh\").status().unwrap();"#;
    for attr in [
        format!("#[r#doc = \"```\\n{cmd}\\n```\"]"),
        format!("#![r#doc = \"```\\n{cmd}\\n```\"]"),
        format!("#[r#cfg_attr(all(), doc = \"```\\n{cmd}\\n```\")]"),
        format!("#[cfg_attr(all(), r#doc = \"```\\n{cmd}\\n```\")]"),
        format!("#[r#cfg_attr(all(), r#cfg_attr(unix, r#doc = \"```\\n{cmd}\\n```\"))]"),
    ] {
        let fs = new_file(&format!("{attr}\npub fn d() {{}}\n"));
        assert!(has(&fs, "process", 1), "{attr}: {fs:#?}");
    }
    for attr in [
        "#[r#doc = concat!(\"a\", \"b\")]",
        "#[r#cfg_attr(all(), doc = concat!(\"a\", \"b\"))]",
        "#[cfg_attr(all(), r#doc = include_str!(\"x.md\"))]",
        "#[r#path = \"gen.txt\"]",
        "#[cfg_attr(unix, r#path = \"gen.txt\")]",
    ] {
        let fs = new_file(&format!("{attr}\npub mod d;\n"));
        assert!(has(&fs, "include", 1), "{attr}: {fs:#?}");
    }
    for attr in [
        "#[r#no_mangle]",
        "#[r#export_name = \"x\"]",
        "#[r#link_section = \".x\"]",
    ] {
        let fs = new_file(&format!("{attr}\npub fn d() {{}}\n"));
        assert!(has(&fs, "ffi", 1), "{attr}: {fs:#?}");
    }
    // a raw identifier is never a keyword: r#unsafe and r#extern are plain names
    none(&new_file("fn r#unsafe() {}\nfn r#extern() {}\n"));
}

const CMD: &str = "std::process::Command::new(\"sh\").status().unwrap();";

/// R4-D1: Markdown forms that hide a code block from a fence parser. Doc text is lexed as code
/// whatever its Markdown, so each is a finding on the code line.
#[test]
fn r4_d1_markdown_forms() {
    for (doc, line) in [
        (format!("/// - ```\n///   {CMD}\n///   ```\n"), 2),
        (format!("/// 1. ```\n///    {CMD}\n///    ```\n"), 2),
        (format!("/// > ```\n/// > {CMD}\n/// > ```\n"), 2),
        (format!("/// ```{{.example}}\n/// {CMD}\n/// ```\n"), 2),
        (format!("/// # Example\n///     {CMD}\n"), 2),
        (
            format!("/// ```text\n/// output\n/// ```\n///     {CMD}\n"),
            4,
        ),
        (format!("/// > - > ```\n/// >   > {CMD}\n"), 2),
        (
            format!("/** ```\n * let _s = r\"\n * \"; {CMD}\n * ```\n */\n"),
            3,
        ),
        (format!("/**\n * ```\n * {CMD}\n * ```\n */\n"), 3),
        (format!("/// Words, then {CMD} in a sentence.\n"), 1),
    ] {
        let src = format!("{doc}pub fn d() {{}}\n");
        let fs = new_file(&src);
        assert!(has(&fs, "process", line), "{src}: {fs:#?}");
    }
}

/// R4-D2: rustdoc joins an item's outer docs with the inner docs of its body, and keeps crate
/// docs apart from the first item's. Neither grouping can hide the code line now.
#[test]
fn r4_d2_grouping() {
    for (src, line) in [
        (format!("/// ```\npub mod m {{\n    //! {CMD}\n    //! ```\n}}\n"), 3),
        (format!("//! ```\n//! assert!(true);\n/// ```\n/// {CMD}\n/// ```\npub fn d() {{}}\n"), 4),
        (format!("/// ```\npub fn f() {{\n    //! {CMD}\n}}\n"), 3),
        // a path split across the outer docs and the body's inner docs
        ("/// use std::\npub mod m {\n    //! process::Command as C;\n    //! fn f() { C::new(\"sh\"); }\n}\n".to_string(), 3),
    ] {
        let fs = new_file(&src);
        assert!(has(&fs, "process", line), "{src}: {fs:#?}");
    }
}

/// Text before a doctest can leave a whole-block lexer inside a string or comment; every line
/// that starts in such a state is lexed again from code state.
#[test]
fn doc_text_lexer_state_cannot_hide_code() {
    for (doc, line) in [
        // a prose quote opens a string; the code then puts `//` inside a string literal
        ("/// He said \"hi\n/// ```\n/// let s = \"//\"; use std::process::Command as C;\n/// C::new(\"sh\");\n/// ```\n/// bye\"\n".to_string(), 3),
        // a block-comment opener inside a string in the code, closer later
        (format!("/// Note \"x\n/// ```\n/// let a = \"/*\"; {CMD} let b = \"*/\";\n/// ```\n"), 3),
        // a multi-line raw string in the code whose second line closes it
        ("/// ```\n/// let _s = r\"\n/// \"; let t = \"//\"; use std::process::Command as C; \"\n/// \"; C::new(\"sh\");\n/// ```\n".to_string(), 3),
        // a prose apostrophe and backticks around inline code
        (format!("/// Don't run `this`: users' code.\n/// ```\n/// {CMD}\n/// ```\n"), 3),
        // non-ASCII punctuation (em dash, curly quotes) in prose
        (format!("/// It \u{2014} \u{201c}runs\u{201d}:\n/// ```\n/// {CMD}\n/// ```\n"), 3),
    ] {
        let src = format!("{doc}pub fn d() {{}}\n");
        let fs = new_file(&src);
        assert!(has(&fs, "process", line), "{src}: {fs:#?}");
    }
    // a doc string with CR line endings: Markdown ends a line at CR too
    let src =
        "#[doc = \"x\\r```\\rstd::process::Command::new(\\\"sh\\\");\\r```\"]\npub fn d() {}\n";
    assert!(has(&new_file(src), "process", 1), "{:#?}", new_file(src));
}

/// Any added line in a doc group re-judges the whole group: an edit to a fence's info string
/// turns unchanged lines into run code.
#[test]
fn doc_group_is_rejudged_on_any_change() {
    let base = format!("/// ```text\n/// {CMD}\n/// ```\npub fn d() {{}}\n");
    let head = format!("/// ```\n/// {CMD}\n/// ```\npub fn d() {{}}\n");
    assert!(has(&change(&base, &head), "process", 2));
    // a line added to another item's docs does not re-judge this group
    let head = format!("{base}/// More.\npub fn e() {{}}\n");
    none(&change(&base, &head));
    // nor does a code line elsewhere
    none(&change(&base, &format!("{base}fn z() {{}}\n")));
}

/// Docs that rustdoc joins across files: the outer docs of `mod name;` and the module file's
/// leading inner docs. Each edge must be closed.
#[test]
fn doc_text_joined_across_files() {
    // the parent's side: a trailing path separator (`use libc::` there, `fork as f;` here)
    let fs = new_file("/// use libc::\npub mod m;\n");
    assert!(has(&fs, "include", 1), "{fs:#?}");
    // a string, comment or group left open there is judged from the module file's side,
    // below; on the parent's side it is not a finding (prose URLs and globs open those)
    none(&new_file(
        "/// See https://example.com (or /sys/class/*).\npub mod m;\n",
    ));
    for src in [
        "//! ::fork as f;\n//! f();\npub fn x() {}\n",
        "//! fork as f};\npub fn x() {}\n",
        "//! fork as f,\n//!     execv as e};\npub fn x() {}\n",
    ] {
        let fs = new_file(src);
        assert!(has(&fs, "include", 1), "{src}: {fs:#?}");
    }
    // the module file's docs read as continuing a string, raw string or block comment the
    // parent's docs opened
    for src in [
        "//! \"; use libc::fork as f; \"\n//! f();\npub fn x() {}\n",
        "//! \"#; use libc::fork as f; r#\"\n//! \"#; f();\npub fn x() {}\n",
        "//! x */ use libc::fork as f; \"\n//! \"; f();\npub fn x() {}\n",
    ] {
        let fs = new_file(src);
        assert!(
            rules(&fs).contains(&"process".to_string()),
            "{src}: {fs:#?}"
        );
    }
    // controls: ordinary module docs; a closer after the first paragraph is prose
    none(&new_file("/// The store (see `Drive`).\npub mod store;\n"));
    none(&new_file(
        "//! Crate docs: what it does, and why.\npub fn x() {}\n",
    ));
    none(&new_file(
        "//! A record.\n//!\n//! `{\"a\":1}` then `}`.\npub fn x() {}\n",
    ));
}

#[test]
fn doc_text_fallback_search() {
    assert_eq!(
        diff_gate_tokens::doc::text_rules("calls Command::new"),
        ["process"]
    );
    assert_eq!(
        diff_gate_tokens::doc::text_rules("an unsafe block"),
        ["unsafe"]
    );
    assert!(diff_gate_tokens::doc::text_rules("plain words").is_empty());
}

/// Controls: prose without rule tokens is not a finding.
#[test]
fn doc_text_controls() {
    none(&new_file("/// Returns the length (in bytes) of `s`.\n///\n/// ```\n/// assert_eq!(f(\"ab\"), 2);\n/// ```\npub fn f(s: &str) -> usize { s.len() }\n"));
    none(&new_file(
        "//! A crate.\n//!\n//! - one\n//! - two)\n//!\n//! > quoted 'text'\npub fn x() {}\n",
    ));
}

/// rustdoc strips a block comment's ` * `, a block quote's `> ` and a hidden line's `# ` before
/// it compiles a line. Left in, they would split a construct across lines; the block is also
/// read with them blanked.
#[test]
fn container_prefixes_cannot_split_a_construct() {
    for (src, line) in [
        ("/// > ```\n/// > use std::{\n/// > process::Command as C};\n/// > C::new(\"sh\");\n/// > ```\npub fn d() {}\n", 3),
        ("/**\n * ```\n * use std::{\n * process::Command as C};\n * C::new(\"sh\");\n * ```\n */\npub fn d() {}\n", 4),
        ("/// ```\n/// # use std::{\n/// # process::Command as C};\n/// C::new(\"sh\");\n/// ```\npub fn d() {}\n", 3),
        ("/// > > ```\n/// > > use std::{\n/// > > process::Command as C};\n/// > > ```\npub fn d() {}\n", 3),
    ] {
        let fs = new_file(src);
        assert!(has(&fs, "process", line), "{src}: {fs:#?}");
    }
}
