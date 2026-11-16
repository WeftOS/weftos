//! Doc text is code.
//!
//! rustdoc compiles the code blocks of every documented item and `cargo test` runs them.
//! Which lines form a code block is decided by Markdown: fences on list-marker lines, block
//! quotes, indented blocks after a heading, info strings, block-comment stars, how outer and
//! inner docs are grouped. Four review rounds found escapes in each attempt to follow those
//! rules (G1, R3-F1, R4-D1, R4-D2), so this module does not look for code blocks at all. Every
//! line of doc text is lexed as Rust and the gate's token rules run over all of it, inside a
//! fence or not. Prose that names `Command::new` is a finding; an approval row clears it.
//!
//! Lexer state. A doctest always starts at the beginning of a line of the doc text, in code
//! state: a fence's content starts on the line after the fence, an indented block starts at a
//! line, and container prefixes (`> `, list indentation, ` * `, `# `) are whitespace or
//! punctuation to the lexer. The text around a doctest can leave a lexer in any state (a
//! quote in prose opens a string), so one lexing of the whole text is not enough. Each block
//! is lexed (1) as a whole and (2) once more from every line whose first non-blank character
//! the whole-block lexing did not see as the start of a token (the line starts inside a
//! string or a comment, or with one). Whatever state the prose leaves, every doctest's own
//! lexing is one of these views. Text that rustdoc may join with docs in another file (the
//! outer docs of `mod name;`, a file's leading inner docs) is checked at that edge too
//! ([`ends_open`], [`start_views`]).
//!
//! Lenient lexing. Backticks and non-ASCII characters that are neither letters nor digits
//! become spaces (columns are kept); then proc-macro2 lexes the text, and each error it
//! reports blanks the character it points at, until the text lexes. A view that still does
//! not lex within the repair budget, or a block past the size budget, falls back to a plain
//! text search for the rule tokens ([`text_rules`]): never a silent pass.

use proc_macro2::{Spacing, TokenStream, TokenTree};
use std::collections::BTreeSet;

pub struct DocLine {
    pub line: usize,
    /// Last source line of the literal this line came from (`line` for a `///` comment). A
    /// one-line `#[doc = "..."]` with `\n` escapes gives several doc lines on one source line.
    pub hi: usize,
    pub text: String,
}

/// One lexing of (part of) a doc block.
pub struct View {
    /// Source line of each line of the view's text.
    pub map: Vec<usize>,
    /// Last source line of the literal each line came from.
    pub hi: Vec<usize>,
    /// The lexed tokens, or None when the view fell back to a text search of `text`.
    pub ts: Option<TokenStream>,
    pub text: String,
    /// Characters the lenient lexer blanked, with their line in the view (1-based).
    pub repaired: Vec<(char, usize)>,
    /// The whole group lexed from its start (not a later line, not a start-state prefix).
    pub whole: bool,
}

/// Lexing budget per doc block, in characters lexed (repairs re-lex). Past it, text search.
const BLOCK_BUDGET: usize = 8_000_000;
/// Lexer errors repaired per view before it falls back to text search.
const MAX_REPAIRS: usize = 4_000;

/// Whitespace and punctuation outside ASCII, and backticks, are never part of a Rust token;
/// a space keeps the columns. Letters and digits stay (identifiers may use them).
fn sanitize(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| {
            let never_token = c == '`'
                || (c.is_ascii_control() && c != '\t')
                || (!c.is_ascii() && !c.is_alphanumeric());
            if never_token {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Rows of the block: one per line of doc text. Markdown ends a line at LF, CR or CRLF.
fn rows(lines: &[DocLine]) -> Vec<(usize, usize, Vec<char>)> {
    let mut out = Vec::new();
    for l in lines {
        for part in l.text.split('\n') {
            let part = part.strip_suffix('\r').unwrap_or(part);
            for p in part.split('\r') {
                out.push((l.line, l.hi, sanitize(p)));
            }
        }
    }
    out
}

/// Result of lexing one text leniently.
struct Lexed {
    ts: TokenStream,
    chars: Vec<char>,
    /// The characters blanked to make the text lex, with their line.
    repaired: Vec<(char, usize)>,
}

fn char_index(chars: &[char], line: usize, col: usize) -> Option<usize> {
    let mut l = 1;
    let mut start = 0;
    if line > 1 {
        for (k, c) in chars.iter().enumerate() {
            if *c == '\n' {
                l += 1;
                if l == line {
                    start = k + 1;
                    break;
                }
            }
        }
        if l != line {
            return None;
        }
    }
    let idx = start + col;
    (idx < chars.len() && chars[start..idx].iter().all(|c| *c != '\n')).then_some(idx)
}

fn lex(mut chars: Vec<char>, budget: &mut usize) -> Option<Lexed> {
    let mut repaired = Vec::new();
    loop {
        let text: String = chars.iter().collect();
        let Some(left) = budget.checked_sub(text.len().max(1)) else {
            *budget = 0; // the caller falls back to a text search of the whole block
            return None;
        };
        *budget = left;
        match text.parse::<TokenStream>() {
            Ok(ts) => {
                return Some(Lexed {
                    ts,
                    chars,
                    repaired,
                })
            }
            Err(e) => {
                if repaired.len() >= MAX_REPAIRS {
                    return None;
                }
                let lc = e.span().start();
                let k = char_index(&chars, lc.line, lc.column)?;
                if chars[k].is_whitespace() {
                    return None; // no progress possible
                }
                repaired.push((chars[k], lc.line));
                chars[k] = ' ';
            }
        }
    }
}

/// (line, column) of the start of every token, at every depth.
fn token_starts(ts: &TokenStream, out: &mut BTreeSet<(usize, usize)>) {
    for t in ts.clone() {
        match &t {
            TokenTree::Group(g) => {
                let s = g.span_open().start();
                out.insert((s.line, s.column));
                token_starts(&g.stream(), out);
            }
            other => {
                let s = other.span().start();
                out.insert((s.line, s.column));
            }
        }
    }
}

fn join(rows: &[(usize, usize, Vec<char>)]) -> Vec<char> {
    let mut v = Vec::new();
    for (k, r) in rows.iter().enumerate() {
        if k > 0 {
            v.push('\n');
        }
        v.extend_from_slice(&r.2);
    }
    v
}

fn view_of(rows: &[(usize, usize, Vec<char>)], lexed: Option<Lexed>, raw: Vec<char>) -> View {
    let map = rows.iter().map(|r| r.0).collect();
    let hi = rows.iter().map(|r| r.1).collect();
    match lexed {
        Some(l) => View {
            map,
            hi,
            ts: Some(l.ts),
            text: l.chars.iter().collect(),
            repaired: l.repaired,
            whole: false,
        },
        None => View {
            map,
            hi,
            ts: None,
            text: raw.iter().collect(),
            repaired: Vec::new(),
            whole: false,
        },
    }
}

/// Every view of a doc block (see the module docs). `edge_start`: the block is a file's
/// leading inner docs, which rustdoc may join after the outer docs of `mod name;` in the
/// parent file, so it is also lexed as if it began inside a string or a block comment.
pub fn views(lines: &[DocLine], edge_start: bool) -> Vec<View> {
    let rows = rows(lines);
    if rows.is_empty() {
        return Vec::new();
    }
    let mut budget = BLOCK_BUDGET;
    let mut out = Vec::new();
    // rustdoc strips the leading ` * ` of a block doc comment, a block quote's `> ` and a hidden
    // line's `# ` before it compiles a line; left in, they can split a construct across lines
    // (`use std::{` then `> process::Command as C};`). So the block is also read without them.
    let stripped = strip_prefixes(&rows);
    if stripped != rows {
        out.extend(views_of(&stripped, edge_start, &mut budget));
    }
    out.extend(views_of(&rows, edge_start, &mut budget));
    out
}

/// Each row with its leading container markers blanked: any run of `>` and `*` (each with
/// the whitespace after it), then a hidden-line `#` followed by a space or the end of the line.
fn strip_prefixes(rows: &[(usize, usize, Vec<char>)]) -> Vec<(usize, usize, Vec<char>)> {
    rows.iter()
        .map(|(l, h, c)| {
            let mut c = c.clone();
            let mut k = 0;
            while k < c.len() && (c[k].is_whitespace() || c[k] == '>' || c[k] == '*') {
                c[k] = ' ';
                k += 1;
            }
            if k < c.len() && c[k] == '#' && c.get(k + 1).is_none_or(|n| n.is_whitespace()) {
                c[k] = ' ';
            }
            (*l, *h, c)
        })
        .collect()
}

fn views_of(rows: &[(usize, usize, Vec<char>)], edge_start: bool, budget: &mut usize) -> Vec<View> {
    let mut out = Vec::new();
    let whole = join(rows);
    let first = lex(whole.clone(), budget);
    let mut extra = Vec::new();
    match &first {
        Some(l) => {
            let mut starts = BTreeSet::new();
            token_starts(&l.ts, &mut starts);
            // the repaired text decides what is blank
            let text: String = l.chars.iter().collect();
            for (k, line) in text.split('\n').enumerate() {
                if let Some(c) = line.chars().position(|c| !c.is_whitespace()) {
                    if k > 0 && !starts.contains(&(k + 1, c)) {
                        extra.push(k);
                    }
                }
            }
        }
        None => {
            out.push(view_of(rows, None, whole));
            return out;
        }
    }
    let mut w = view_of(rows, first, whole);
    w.whole = true;
    out.push(w);
    for k in extra {
        let part = &rows[k..];
        let raw = join(part);
        let l = lex(raw.clone(), budget);
        out.push(view_of(part, l, raw));
        if *budget == 0 {
            // out of budget: one text search over the whole block covers the rest
            let whole = join(rows);
            out.push(view_of(rows, None, whole));
            return out;
        }
    }
    if edge_start {
        for v in start_views(rows, budget) {
            out.push(v);
        }
    }
    out
}

/// The block lexed as if the text before it (in another file) had left the lexer inside a
/// string, a raw string with any of the hash counts the block can close, or a block comment.
fn start_views(rows: &[(usize, usize, Vec<char>)], budget: &mut usize) -> Vec<View> {
    let whole = join(rows);
    let text: String = whole.iter().collect();
    let mut prefixes: Vec<String> = vec!["\"".into(), "b\"".into()];
    let mut hashes = BTreeSet::new();
    let ch: Vec<char> = text.chars().collect();
    for (k, c) in ch.iter().enumerate() {
        if *c == '"' {
            let n = ch[k + 1..].iter().take_while(|c| **c == '#').count();
            hashes.insert(n.min(64));
        }
    }
    for n in hashes {
        prefixes.push(format!("r{}\"", "#".repeat(n)));
    }
    let closes = text.matches("*/").count().min(4);
    for d in 1..=closes {
        prefixes.push("/*".repeat(d));
    }
    let mut out = Vec::new();
    for p in prefixes {
        let mut chars: Vec<char> = p.chars().collect();
        chars.extend_from_slice(&whole);
        let l = lex(chars.clone(), budget);
        out.push(view_of(rows, l, chars));
    }
    out
}

/// Text that rustdoc may join with docs in another file (the outer docs of `mod name;` are
/// followed by the module file's inner docs) must not end on a path separator (`use libc::`
/// here, `fork as f;` there). Returns why it does, if it does.
pub fn ends_open(lines: &[DocLine]) -> Option<&'static str> {
    let rows = rows(lines);
    if rows.is_empty() {
        return None;
    }
    let mut budget = BLOCK_BUDGET;
    let mut tails: Vec<Vec<char>> = vec![join(&rows)];
    // every line start too: a doctest can begin at any of them
    for k in 1..rows.len() {
        tails.push(join(&rows[k..]));
    }
    for t in tails {
        let Some(l) = lex(t, &mut budget) else {
            return Some("its doc text does not lex");
        };
        // A string, comment or group left open here would make the module file's docs start
        // inside it: `start_views` / `starts_open` judge that from the module file's side,
        // where it can be seen. Here only a trailing path separator is checked.
        let toks: Vec<TokenTree> = l.ts.into_iter().collect();
        let n = toks.len();
        if n >= 2 {
            if let (TokenTree::Punct(a), TokenTree::Punct(b)) = (&toks[n - 2], &toks[n - 1]) {
                if a.as_char() == ':' && a.spacing() == Spacing::Joint && b.as_char() == ':' {
                    return Some("its doc text ends on a path separator");
                }
            }
        }
    }
    None
}

/// A file's leading inner docs may continue the outer docs of `mod name;` in the parent: they
/// must not start on a path separator or close a group they did not open.
pub fn starts_open(views: &[View]) -> Option<&'static str> {
    views
        .iter()
        .filter(|v| v.whole)
        .find_map(|v| starts_open_one(v))
}

fn starts_open_one(first: &View) -> Option<&'static str> {
    let ts = first.ts.as_ref()?;
    let toks: Vec<TokenTree> = ts.clone().into_iter().collect();
    if let (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b))) = (toks.first(), toks.get(1)) {
        if a.as_char() == ':' && a.spacing() == Spacing::Joint && b.as_char() == ':' {
            return Some("its doc text starts on a path separator");
        }
    }
    // a closer with no opener before it in this text, in its first paragraph (`fork as f};`
    // closing the parent's `use libc::{`): the repair loop blanked it
    let para = first
        .text
        .split('\n')
        .position(|l| l.trim().is_empty())
        .unwrap_or(usize::MAX);
    if first
        .repaired
        .iter()
        .any(|(c, l)| matches!(c, '}' | ']') && *l <= para)
    {
        return Some("its doc text closes a group it did not open");
    }
    None
}

/// Plain text search for the rule tokens: the fallback when a view does not lex. Over-wide on
/// purpose (any mention is a finding).
pub fn text_rules(line: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    let mut add = |r: &'static str| {
        if !out.contains(&r) {
            out.push(r);
        }
    };
    let words = line
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '#'))
        .map(|w| w.strip_prefix("r#").unwrap_or(w));
    for w in words {
        match w {
            "unsafe" => add("unsafe"),
            "extern" | "no_mangle" | "export_name" | "link_section" | "asm" | "global_asm"
            | "naked_asm" => add("ffi"),
            "Command" | "CommandExt" | "posix_spawn" | "posix_spawnp" | "fork" | "vfork"
            | "popen" | "execv" | "execvp" | "execve" => add("process"),
            "TcpStream" | "TcpListener" | "TcpSocket" | "UdpSocket" | "reqwest" | "ureq"
            | "hyper" | "isahc" | "surf" | "attohttpc" | "curl" | "socket2" | "tungstenite"
            | "quinn" => add("net"),
            "include" | "include_str" | "include_bytes" | "paste" | "concat_idents" => {
                add("include")
            }
            _ => {}
        }
    }
    if compact.contains("process::") {
        add("process");
    }
    if compact.contains("net::") {
        add("net");
    }
    if compact.contains("path=") || compact.contains("doc=") {
        add("include");
    }
    out
}
