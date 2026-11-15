//! Doctest extraction. rustdoc compiles and `cargo test` runs the Rust code blocks of every
//! documented item, so code inside `///` and `//!` comments is code. This follows rustdoc's
//! rules closely enough to over-approximate: fenced blocks whose info string is Rust (empty,
//! `rust`, `no_run`, `should_panic`, ...), indented blocks, and hidden `# ` lines.

pub struct DocLine {
    pub line: usize,
    pub text: String,
}

pub struct Code {
    pub text: String,
    /// Source line of each line of `text`.
    pub map: Vec<usize>,
}

const RUST_TAGS: &[&str] = &[
    "rust", "ignore", "should_panic", "no_run", "compile_fail", "test_harness", "standalone_crate",
    "allow_fail",
];

/// rustdoc: a block is Rust unless it carries an unknown tag and no Rust tag.
pub fn is_rust_info(info: &str) -> bool {
    let mut unknown = false;
    let mut rusty = false;
    for t in info.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()) {
        if RUST_TAGS.contains(&t) || t.starts_with("edition") || t.starts_with("ignore-") {
            rusty = true;
        } else {
            unknown = true;
        }
    }
    !unknown || rusty
}

fn indent(s: &str) -> usize {
    s.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

fn strip_cols(s: &str, n: usize) -> &str {
    let mut cols = 0;
    for (i, c) in s.char_indices() {
        if cols >= n || (c != ' ' && c != '\t') {
            return &s[i..];
        }
        cols += if c == '\t' { 4 } else { 1 };
    }
    ""
}

/// A hidden line (`# code`) is compiled but not shown; `##` stands for a literal `#`.
fn unhide(s: &str) -> String {
    let t = s.trim_start();
    if t == "#" {
        String::new()
    } else if let Some(rest) = t.strip_prefix("# ") {
        rest.to_string()
    } else if t.starts_with("##") {
        t[1..].to_string()
    } else {
        s.to_string()
    }
}

fn fence(t: &str) -> Option<(char, usize, &str)> {
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then(|| (c, n, &t[n..]))
}

pub fn code_blocks(lines: &[DocLine]) -> Vec<Code> {
    let common = lines.iter().filter(|l| !l.text.trim().is_empty()).map(|l| indent(&l.text)).min().unwrap_or(0);
    let body: Vec<(usize, &str)> = lines.iter().map(|l| (l.line, strip_cols(&l.text, common))).collect();
    let mut out = Vec::new();
    let mut k = 0;
    let mut prev_blank = true;
    while k < body.len() {
        let (_, text) = body[k];
        let t = text.trim_start();
        if let Some((c, n, info)) = fence(t) {
            let rust = is_rust_info(info.trim());
            let mut code = Code { text: String::new(), map: Vec::new() };
            k += 1;
            while k < body.len() {
                let (l, s) = body[k];
                if let Some((c2, n2, rest)) = fence(s.trim_start()) {
                    if c2 == c && n2 >= n && rest.trim().is_empty() {
                        break;
                    }
                }
                code.text.push_str(&unhide(s));
                code.text.push('\n');
                code.map.push(l);
                k += 1;
            }
            if rust && !code.map.is_empty() {
                out.push(code);
            }
            k += 1;
            prev_blank = false;
            continue;
        }
        if prev_blank && indent(text) >= 4 && !t.is_empty() {
            let mut code = Code { text: String::new(), map: Vec::new() };
            while k < body.len() && (indent(body[k].1) >= 4 || body[k].1.trim().is_empty()) {
                code.text.push_str(&unhide(strip_cols(body[k].1, 4)));
                code.text.push('\n');
                code.map.push(body[k].0);
                k += 1;
            }
            out.push(code);
            prev_blank = true;
            continue;
        }
        prev_blank = t.is_empty();
        k += 1;
    }
    out
}
