//! Token walk: finds every sensitive construct in a token stream, at any depth (macro bodies,
//! attributes, nested modules and blocks), the way rustc's lexer sees it: whitespace and
//! comments between tokens do not matter, strings and comments are never code, and doc
//! comments arrive as `#[doc = "..."]` attributes whose code blocks are scanned as doctests.

use crate::doc::{self, DocLine};
use crate::resolve::{by_name, classify, Aliases, Path};
pub use crate::uses::collect_aliases;
use crate::uses::{parse_extern_crate, parse_use};
use proc_macro2::{Delimiter, Group, Spacing, Span, TokenStream, TokenTree};
use std::collections::BTreeSet;

/// One sensitive occurrence. Lines are 1-based; columns are 0-based character offsets.
#[derive(Clone, Debug)]
pub struct Occ {
    pub rule: &'static str,
    /// What it resolved to, e.g. `std::process::Command`; part of the context-change key.
    pub what: String,
    /// The line the finding is reported on.
    pub line: usize,
    /// Token spans (line, start column, end column) that make up the occurrence.
    pub spans: Vec<(usize, usize, usize)>,
    /// The construct's line range: an added line anywhere in it makes it a finding.
    pub lo: usize,
    pub hi: usize,
    /// For `unsafe { }` and `extern { }`: the block's lines. Added lines inside are findings.
    pub block: Option<(usize, usize)>,
    pub note: &'static str,
}

const MAX_DOC_DEPTH: usize = 3;

pub(crate) fn start(s: Span) -> (usize, usize) {
    let lc = s.start();
    (lc.line, lc.column)
}
pub(crate) fn end(s: Span) -> (usize, usize) {
    let lc = s.end();
    (lc.line, lc.column)
}
pub(crate) fn tspan(s: Span) -> (usize, usize, usize) {
    let (l, c) = start(s);
    let (l2, c2) = end(s);
    (l, c, if l2 == l { c2 } else { c + 1 })
}

/// `r#foo` and `foo` are the same name (for anything that is not a keyword).
pub fn name(s: &str) -> String {
    s.strip_prefix("r#").unwrap_or(s).to_string()
}

pub(crate) fn ident_is(t: Option<&TokenTree>, s: &str) -> bool {
    matches!(t, Some(TokenTree::Ident(i)) if *i == s)
}
pub(crate) fn punct_is(t: Option<&TokenTree>, c: char) -> bool {
    matches!(t, Some(TokenTree::Punct(p)) if p.as_char() == c)
}
fn colon2(v: &[TokenTree], k: usize) -> bool {
    matches!(v.get(k), Some(TokenTree::Punct(p)) if p.as_char() == ':' && p.spacing() == Spacing::Joint)
        && punct_is(v.get(k + 1), ':')
}
pub(crate) fn semi_after(v: &[TokenTree], i: usize) -> Option<usize> {
    (i..v.len()).find(|&k| punct_is(v.get(k), ';'))
}
pub(crate) fn slice(v: &[TokenTree], a: usize, b: usize) -> TokenStream {
    v[a..=b].iter().cloned().collect()
}
fn lit_str(t: Option<&TokenTree>) -> Option<(String, Span)> {
    match t {
        Some(TokenTree::Literal(l)) => {
            let ts: TokenStream = TokenTree::Literal(l.clone()).into();
            syn::parse2::<syn::LitStr>(ts)
                .ok()
                .map(|s| (s.value(), l.span()))
        }
        _ => None,
    }
}

/// A path read from tokens: `[::] seg (:: seg)*`, also `$crate::...`.
struct TokPath {
    path: Path,
    spans: Vec<(usize, usize, usize)>,
    next: usize,
}

fn read_path(v: &[TokenTree], i: usize) -> Option<TokPath> {
    let mut k = i;
    let mut absolute = false;
    let mut segs = Vec::new();
    let mut spans = Vec::new();
    if colon2(v, k) {
        absolute = true;
        k += 2;
    } else if punct_is(v.get(k), '$') && ident_is(v.get(k + 1), "crate") {
        if let Some(TokenTree::Punct(p)) = v.get(k) {
            spans.push(tspan(p.span()));
        }
        segs.push("$crate".to_string());
        k += 2;
        if !colon2(v, k) {
            return Some(TokPath {
                path: Path::new(false, segs),
                spans,
                next: k,
            });
        }
        k += 2;
    }
    if !absolute && segs.is_empty() {
        if let Some(TokenTree::Ident(id)) = v.get(k) {
            if crate::resolve::is_keyword(&id.to_string()) {
                return None;
            }
        }
    }
    while let Some(TokenTree::Ident(id)) = v.get(k) {
        segs.push(name(&id.to_string()));
        spans.push(tspan(id.span()));
        k += 1;
        if colon2(v, k) && matches!(v.get(k + 2), Some(TokenTree::Ident(_))) {
            k += 2;
        } else {
            break;
        }
    }
    if segs.is_empty() {
        return None;
    }
    Some(TokPath {
        path: Path::new(absolute, segs),
        spans,
        next: k,
    })
}

pub struct Scanner<'a> {
    pub aliases: &'a Aliases,
    pub occs: Vec<Occ>,
    pub token_lines: BTreeSet<usize>,
    pub depth: usize,
    /// Lines of the attribute being scanned: a finding inside an attribute covers all of it.
    attr: Option<(usize, usize)>,
}

impl<'a> Scanner<'a> {
    pub fn new(aliases: &'a Aliases, depth: usize) -> Self {
        Scanner {
            aliases,
            occs: Vec::new(),
            token_lines: BTreeSet::new(),
            depth,
            attr: None,
        }
    }

    fn push(
        &mut self,
        rule: &'static str,
        what: String,
        spans: Vec<(usize, usize, usize)>,
        range: (usize, usize),
        note: &'static str,
    ) {
        let line = spans.last().map(|s| s.0).unwrap_or(range.0);
        let (a0, a1) = self.attr.unwrap_or((line, line));
        let lo = range.0.min(line).min(a0);
        let hi = range.1.max(line).max(a1);
        self.occs.push(Occ {
            rule,
            what,
            line,
            spans,
            lo,
            hi,
            block: None,
            note,
        });
    }

    /// Rules for a path: resolved through the alias table, plus the name-based checks.
    fn path_rules(&mut self, tp: &TokPath, macro_call: bool) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = Vec::new();
        for (c, counts) in self.aliases.expand(&tp.path) {
            if !counts {
                continue;
            }
            for r in classify(&c) {
                if macro_call && r == "include" {
                    continue; // checked against its argument instead
                }
                if !out.iter().any(|(x, _)| *x == r) {
                    out.push((r, c.text()));
                }
            }
        }
        for r in by_name(&tp.path.segs) {
            if !out.iter().any(|(x, _)| *x == r) {
                out.push((r, tp.path.text()));
            }
        }
        out
    }

    fn is_include_macro(&self, tp: &TokPath) -> bool {
        if tp.path.segs.len() == 1 && tp.path.segs[0] == "include" {
            return true;
        }
        self.aliases
            .expand(&tp.path)
            .iter()
            .any(|(c, _)| classify(c).contains(&"include"))
    }

    /// The first `{ }` after a keyword, before `;` or `,` at the same level.
    fn block_after(v: &[TokenTree], i: usize) -> Option<(usize, usize)> {
        for t in &v[i + 1..] {
            match t {
                TokenTree::Punct(p) if p.as_char() == ';' || p.as_char() == ',' => return None,
                TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                    return Some((start(g.span()).0, end(g.span()).0));
                }
                _ => {}
            }
        }
        None
    }

    pub fn scan(&mut self, ts: TokenStream, in_attr: bool) {
        let v: Vec<TokenTree> = ts.into_iter().collect();
        let mut docs: Vec<DocLine> = Vec::new();
        let mut i = 0;
        while i < v.len() {
            if let TokenTree::Group(g) = &v[i] {
                self.token_lines.insert(start(g.span_open()).0);
            } else {
                self.token_lines.insert(start(v[i].span()).0);
            }
            // attribute: # [!] [ ... ]
            if punct_is(v.get(i), '#') {
                let gi = if punct_is(v.get(i + 1), '!') {
                    i + 2
                } else {
                    i + 1
                };
                if let Some(TokenTree::Group(g)) = v.get(gi) {
                    if g.delimiter() == Delimiter::Bracket {
                        if let Some(lines) = doc_attr(g) {
                            docs.extend(lines);
                        }
                        let outer = self.attr;
                        let here = (start(v[i].span()).0, end(g.span()).0);
                        self.attr =
                            Some(outer.map_or(here, |(a, b)| (a.min(here.0), b.max(here.1))));
                        self.scan(g.stream(), true);
                        self.attr = outer;
                        i = gi + 1;
                        continue;
                    }
                }
            }
            if !docs.is_empty() {
                self.doctests(std::mem::take(&mut docs));
            }
            match &v[i] {
                TokenTree::Group(g) => {
                    self.scan(g.stream(), in_attr);
                    i += 1;
                }
                TokenTree::Ident(id) => {
                    let raw = id.to_string();
                    let sp = vec![tspan(id.span())];
                    let l = sp[0].0;
                    match raw.as_str() {
                        "use" => {
                            if let Some((leaves, j)) = parse_use(&v, i) {
                                let hi = start(v[j].span()).0;
                                for leaf in leaves {
                                    let tp = TokPath {
                                        path: leaf.full.clone(),
                                        spans: leaf.spans.clone(),
                                        next: 0,
                                    };
                                    for (r, what) in self.path_rules(&tp, false) {
                                        self.push(r, what, leaf.spans.clone(), (l, hi), "");
                                    }
                                }
                                i = j + 1;
                                continue;
                            }
                        }
                        "extern" => {
                            if ident_is(v.get(i + 1), "crate") {
                                if let Some((n, _, nsp, j)) = parse_extern_crate(&v, i) {
                                    let tp = TokPath {
                                        path: Path::new(false, vec![n]),
                                        spans: vec![tspan(nsp)],
                                        next: 0,
                                    };
                                    let hi = start(v[j].span()).0;
                                    for (r, what) in self.path_rules(&tp, false) {
                                        self.push(r, what, tp.spans.clone(), (l, hi), "");
                                    }
                                    i = j + 1;
                                    continue;
                                }
                            } else {
                                self.push("ffi", "extern".into(), sp, (l, l), "");
                                // a foreign block `extern "C" { ... }`, not an `extern fn` body
                                let k = if matches!(v.get(i + 1), Some(TokenTree::Literal(_))) {
                                    i + 2
                                } else {
                                    i + 1
                                };
                                if let Some(TokenTree::Group(g)) = v.get(k) {
                                    if g.delimiter() == Delimiter::Brace {
                                        self.occs.last_mut().unwrap().block =
                                            Some((start(g.span()).0, end(g.span()).0));
                                    }
                                }
                                i += 1;
                                continue;
                            }
                        }
                        "unsafe" => {
                            self.push("unsafe", "unsafe".into(), sp, (l, l), "");
                            if let Some(b) = Self::block_after(&v, i) {
                                self.occs.last_mut().unwrap().block = Some(b);
                            }
                            i += 1;
                            continue;
                        }
                        "no_mangle" | "export_name" | "link_section" => {
                            self.push("ffi", raw.clone(), sp, (l, l), "");
                            i += 1;
                            continue;
                        }
                        "path" if in_attr && punct_is(v.get(i + 1), '=') => {
                            match lit_str(v.get(i + 2)) {
                                Some((val, _)) if val.to_ascii_lowercase().ends_with(".rs") => {}
                                _ => {
                                    let mut sp = sp;
                                    if let Some(t) = v.get(i + 2) {
                                        sp.push(tspan(t.span()));
                                    }
                                    self.push(
                                        "include",
                                        "#[path] to a non-.rs file".into(),
                                        sp,
                                        (l, l),
                                        "",
                                    );
                                }
                            }
                            i += 1;
                            continue;
                        }
                        _ => {}
                    }
                    self.path_at(&v, &mut i, in_attr);
                }
                TokenTree::Punct(_) => {
                    if colon2(&v, i) || (punct_is(v.get(i), '$') && ident_is(v.get(i + 1), "crate"))
                    {
                        self.path_at(&v, &mut i, in_attr);
                    } else {
                        i += 1;
                    }
                }
                TokenTree::Literal(_) => i += 1,
            }
        }
        if !docs.is_empty() {
            self.doctests(docs);
        }
    }

    fn path_at(&mut self, v: &[TokenTree], i: &mut usize, in_attr: bool) {
        let Some(tp) = read_path(v, *i) else {
            *i += 1;
            return;
        };
        let lo = tp.spans.first().map(|s| s.0).unwrap_or(0);
        let hi = tp.spans.last().map(|s| s.0).unwrap_or(lo);
        let mcall = punct_is(v.get(tp.next), '!')
            && matches!(v.get(tp.next + 1), Some(TokenTree::Group(_)));
        for (r, what) in self.path_rules(&tp, mcall) {
            self.push(r, what, tp.spans.clone(), (lo, hi), "");
        }
        if mcall {
            let last = tp.path.segs.last().map(|s| s.as_str()).unwrap_or("");
            let g = match v.get(tp.next + 1) {
                Some(TokenTree::Group(g)) => g.clone(),
                _ => unreachable!(),
            };
            let ghi = end(g.span()).0;
            if matches!(last, "global_asm" | "asm" | "naked_asm")
                && !self
                    .occs
                    .iter()
                    .any(|o| o.rule == "ffi" && o.line == hi && o.lo == lo)
            {
                self.push(
                    "ffi",
                    format!("{last}! (assembly)"),
                    tp.spans.clone(),
                    (lo, ghi),
                    "",
                );
            }
            if self.is_include_macro(&tp) && !include_arg_ok(&g) {
                self.push(
                    "include",
                    "include! of a non-.rs or computed path".into(),
                    tp.spans.clone(),
                    (lo, ghi),
                    "",
                );
            }
            if in_attr && matches!(last, "include_str" | "include_bytes") {
                self.push(
                    "include",
                    format!("{last}! in an attribute (a doc include becomes doctests)"),
                    tp.spans.clone(),
                    (lo, ghi),
                    "",
                );
            }
            if matches!(last, "paste" | "concat_idents") {
                self.push(
                    "include",
                    format!("{last}! can build any identifier or path"),
                    tp.spans.clone(),
                    (lo, ghi),
                    "",
                );
            }
        }
        *i = tp.next;
    }

    /// Code blocks in a run of doc attributes are compiled and run by `cargo test`.
    fn doctests(&mut self, lines: Vec<DocLine>) {
        if self.depth >= MAX_DOC_DEPTH {
            return;
        }
        for code in doc::code_blocks(&lines) {
            let Ok(ts) = code.text.parse::<TokenStream>() else {
                continue;
            };
            let mut a = Aliases::default();
            collect_aliases(ts.clone(), &mut a);
            let mut sub = Scanner::new(&a, self.depth + 1);
            sub.scan(ts, false);
            let map = |l: usize| code.map.get(l.saturating_sub(1)).copied().unwrap_or(0);
            for mut o in sub.occs {
                let l = map(o.line);
                o.line = l;
                o.lo = map(o.lo).min(l);
                o.hi = map(o.hi).max(l);
                o.spans.clear();
                o.block = o.block.map(|(a, b)| (map(a), map(b)));
                o.note = "doctest";
                self.occs.push(o);
            }
        }
    }
}

/// `include!("x.rs")`: one string literal ending in .rs (an optional trailing comma).
fn include_arg_ok(g: &Group) -> bool {
    let v: Vec<TokenTree> = g.stream().into_iter().collect();
    let n = if punct_is(v.last(), ',') {
        v.len() - 1
    } else {
        v.len()
    };
    n == 1 && matches!(lit_str(v.first()), Some((s, _)) if s.to_ascii_lowercase().ends_with(".rs"))
}

/// `doc = "..."` inside an attribute: its lines with their source line numbers.
fn doc_attr(g: &Group) -> Option<Vec<DocLine>> {
    let v: Vec<TokenTree> = g.stream().into_iter().collect();
    if v.len() != 3 || !ident_is(v.first(), "doc") || !punct_is(v.get(1), '=') {
        return None;
    }
    let (val, sp) = lit_str(v.get(2))?;
    let first = start(sp).0;
    Some(
        val.split('\n')
            .enumerate()
            .map(|(k, t)| DocLine {
                line: first + k,
                text: t.to_string(),
            })
            .collect(),
    )
}
