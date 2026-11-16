//! Token walk: finds every sensitive construct in a token stream, at any depth (macro bodies,
//! attributes, nested modules and blocks), the way rustc's lexer sees it: whitespace and
//! comments between tokens do not matter, strings and comments are never code, and doc
//! comments arrive as `#[doc = "..."]` attributes whose text is lexed as code (`doc`).

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

/// `r#foo` and `foo` are the same name. rustc compares symbols, not spellings: `#[r#doc]`,
/// `#[r#cfg_attr(..)]`, `#[r#path = ..]`, `#[r#cfg(r#test)]` and `cfg(r#all(r#test))` act
/// exactly like the plain forms (checked with rustc 1.95; review round 4, R4-D3 / R4-B2).
/// Every comparison of an identifier with a name (attribute names, macro names, path
/// segments, cfg predicates) goes through this function or [`name_is`].
pub fn name(s: &str) -> String {
    s.strip_prefix("r#").unwrap_or(s).to_string()
}

/// The identifier at `t` names `s`, written plainly or as a raw identifier.
pub(crate) fn name_is(t: Option<&TokenTree>, s: &str) -> bool {
    matches!(t, Some(TokenTree::Ident(i)) if name(&i.to_string()) == s)
}

/// The token at `t` is the keyword `s`. A raw identifier is never a keyword (`r#mod` is an
/// identifier named `mod`), so this compares the spelling exactly. Only for keywords.
pub(crate) fn keyword_is(t: Option<&TokenTree>, s: &str) -> bool {
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
    } else if punct_is(v.get(k), '$') && keyword_is(v.get(k + 1), "crate") {
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
    /// Scanning doc text: a bare crate name (`hyper`, `curl`) on its own is prose, not a path
    /// (code names a crate with `use`, `extern crate` or a path: those still count).
    pub doc_text: bool,
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
            doc_text: false,
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
        self.scan_level(ts, in_attr, Vec::new(), false);
    }

    /// A whole file. Its leading inner docs may continue the outer docs of `mod name;` in the
    /// parent file (rustdoc joins the two), so they are also judged at that edge.
    pub fn scan_file(&mut self, ts: TokenStream) {
        self.scan_level(ts, false, Vec::new(), true);
    }

    /// One token level. Doc text is grouped the way rustdoc groups it, closely enough that a
    /// group never splits one rustdoc text where it matters: consecutive doc attributes (other
    /// attributes between them do not split), an item's outer docs joined with the inner docs
    /// at the start of its `{ }` body (`carry`), and inner docs kept apart from the outer docs
    /// of the item after them (crate docs, then the first item's).
    fn scan_level(&mut self, ts: TokenStream, in_attr: bool, carry: Vec<DocLine>, file: bool) {
        let v: Vec<TokenTree> = ts.into_iter().collect();
        let mut docs: Vec<DocLine> = carry;
        let mut docs_inner = false;
        // leading inner docs of a file: rustdoc may join them after another file's docs
        let mut leading = file;
        let mut edge_start = false;
        let mut carry_to: Option<(usize, Vec<DocLine>)> = None;
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
                        let here = (start(v[i].span()).0, end(g.span()).0);
                        let inner = gi == i + 2;
                        for d in doc_values(g) {
                            match d {
                                DocVal::Lines(lines) => {
                                    if docs_inner && !inner && !docs.is_empty() {
                                        self.doc_group(std::mem::take(&mut docs), edge_start, false);
                                        edge_start = false;
                                    }
                                    if inner && leading {
                                        edge_start = true;
                                    }
                                    docs_inner = inner;
                                    docs.extend(lines);
                                }
                                // rustdoc expands the value and runs its code blocks, which
                                // this scan cannot see: fail closed (R3-F1)
                                DocVal::Computed(sp) => self.push(
                                    "include",
                                    "doc attribute with a computed value (rustdoc runs its code blocks as doctests)".into(),
                                    vec![sp],
                                    here,
                                    "",
                                ),
                            }
                        }
                        let outer = self.attr;
                        self.attr =
                            Some(outer.map_or(here, |(a, b)| (a.min(here.0), b.max(here.1))));
                        self.scan(g.stream(), true);
                        self.attr = outer;
                        i = gi + 1;
                        continue;
                    }
                }
            }
            leading = false;
            if !docs.is_empty() {
                let docs_now = std::mem::take(&mut docs);
                if docs_inner {
                    self.doc_group(docs_now, edge_start, false);
                } else if mod_file_decl(&v, i) {
                    // `mod name;`: rustdoc joins these docs with the module file's inner docs
                    self.doc_group(docs_now, false, true);
                } else if let Some(gk) = body_with_inner_docs(&v, i) {
                    carry_to = Some((gk, docs_now));
                } else {
                    self.doc_group(docs_now, false, false);
                }
                edge_start = false;
                docs_inner = false;
            }
            match &v[i] {
                TokenTree::Group(g) => {
                    match carry_to.take() {
                        Some((gk, c)) if gk == i => self.scan_level(g.stream(), in_attr, c, false),
                        other => {
                            carry_to = other;
                            self.scan(g.stream(), in_attr);
                        }
                    }
                    i += 1;
                }
                TokenTree::Ident(id) => {
                    let raw = id.to_string();
                    let sp = vec![tspan(id.span())];
                    let l = sp[0].0;
                    // keywords match the exact spelling; attribute names match through name()
                    let nm = name(&raw);
                    let key = match nm.as_str() {
                        "no_mangle" | "export_name" | "link_section" | "path" => nm.as_str(),
                        _ => raw.as_str(),
                    };
                    match key {
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
                            if keyword_is(v.get(i + 1), "crate") {
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
                            self.push("ffi", nm.clone(), sp, (l, l), "");
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
                    if colon2(&v, i)
                        || (punct_is(v.get(i), '$') && keyword_is(v.get(i + 1), "crate"))
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
            self.doc_group(docs, edge_start, false);
        }
        if let Some((_, c)) = carry_to {
            self.doc_group(c, false, false);
        }
    }

    fn path_at(&mut self, v: &[TokenTree], i: &mut usize, in_attr: bool) {
        let Some(tp) = read_path(v, *i) else {
            *i += 1;
            return;
        };
        let bare = self.doc_text && tp.path.segs.len() == 1 && !tp.path.absolute;
        let lo = tp.spans.first().map(|s| s.0).unwrap_or(0);
        let hi = tp.spans.last().map(|s| s.0).unwrap_or(lo);
        let mcall = punct_is(v.get(tp.next), '!')
            && matches!(v.get(tp.next + 1), Some(TokenTree::Group(_)));
        for (r, what) in self.path_rules(&tp, mcall) {
            if bare && r == "net" && crate::resolve::by_name(&tp.path.segs).is_empty() {
                continue;
            }
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

    /// One group of doc text (see `doc`): every line is lexed as Rust and the token rules
    /// run over all of it. Each occurrence covers the whole group, so an added line anywhere
    /// in the group re-judges all of it (an edit to a fence or an info string can turn
    /// unchanged lines into run code). The lexing runs on its own thread: proc-macro2 keeps a
    /// per-thread source map that the lenient lexer's many re-lexes would otherwise grow.
    fn doc_group(&mut self, lines: Vec<DocLine>, edge_start: bool, edge_end: bool) {
        if self.depth >= MAX_DOC_DEPTH || lines.is_empty() {
            return;
        }
        let lo = lines.iter().map(|l| l.line).min().unwrap_or(0);
        let hi = lines.iter().map(|l| l.line.max(l.hi)).max().unwrap_or(lo);
        let depth = self.depth;
        let res = std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(64 << 20)
                .spawn_scoped(s, || {
                    doc_occs(&lines, depth, edge_start, edge_end, (lo, hi))
                })
                .map(|h| h.join())
        });
        match res {
            Ok(Ok(occs)) => self.occs.extend(occs),
            _ => self.occs.push(Occ {
                rule: "include",
                what: "doc text that could not be scanned".into(),
                line: lo,
                spans: Vec::new(),
                lo,
                hi,
                block: None,
                note: "doc text",
            }),
        }
    }
}

/// `[pub [(..)]] [unsafe] mod name ;` at `v[i]`.
fn mod_file_decl(v: &[TokenTree], i: usize) -> bool {
    let mut k = i;
    if keyword_is(v.get(k), "pub") {
        k += 1;
        if matches!(v.get(k), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
        {
            k += 1;
        }
    }
    if keyword_is(v.get(k), "unsafe") {
        k += 1;
    }
    keyword_is(v.get(k), "mod")
        && matches!(v.get(k + 1), Some(TokenTree::Ident(_)))
        && punct_is(v.get(k + 2), ';')
}

/// The item at `v[i]` has a `{ }` body (the first brace group before `;`) that starts with
/// inner doc attributes: its index.
fn body_with_inner_docs(v: &[TokenTree], i: usize) -> Option<usize> {
    for (k, t) in v.iter().enumerate().skip(i) {
        match t {
            TokenTree::Punct(p) if p.as_char() == ';' => return None,
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                let b: Vec<TokenTree> = g.stream().into_iter().collect();
                let inner_doc = punct_is(b.first(), '#')
                    && punct_is(b.get(1), '!')
                    && matches!(b.get(2), Some(TokenTree::Group(a)) if a.delimiter() == Delimiter::Bracket
                        && doc_values(a).iter().any(|d| matches!(d, DocVal::Lines(_))));
                return inner_doc.then_some(k);
            }
            _ => {}
        }
    }
    None
}

/// The occurrences in one doc group: every view's tokens through the rules, a text search for
/// a view that did not lex, and the edge checks. Each covers the group's lines.
fn doc_occs(
    lines: &[DocLine],
    depth: usize,
    edge_start: bool,
    edge_end: bool,
    (lo, hi): (usize, usize),
) -> Vec<Occ> {
    let views = doc::views(lines, edge_start);
    let mut out: Vec<Occ> = Vec::new();
    let mut seen: BTreeSet<(&'static str, String, usize)> = BTreeSet::new();
    let mut push = |rule: &'static str, what: String, line: usize, out: &mut Vec<Occ>| {
        if seen.insert((rule, what.clone(), line)) {
            out.push(Occ {
                rule,
                what,
                line,
                spans: Vec::new(),
                lo,
                hi,
                block: None,
                note: "doc text",
            });
        }
    };
    for v in &views {
        let map = |l: usize| v.map.get(l.saturating_sub(1)).copied().unwrap_or(lo);
        match &v.ts {
            Some(ts) => {
                let mut a = Aliases::default();
                collect_aliases(ts.clone(), &mut a);
                let mut sub = Scanner::new(&a, depth + 1);
                sub.doc_text = true;
                sub.scan(ts.clone(), false);
                for o in sub.occs {
                    push(o.rule, o.what, map(o.line), &mut out);
                }
            }
            None => {
                for (k, line) in v.text.split('\n').enumerate() {
                    for r in doc::text_rules(line) {
                        push(r, "doc text (text search)".into(), map(k + 1), &mut out);
                    }
                }
            }
        }
    }
    if edge_end {
        if let Some(why) = doc::ends_open(lines) {
            let last = lines.last().map(|l| l.line).unwrap_or(hi);
            push(
                "include",
                format!(
                    "docs on `mod name;`: {why} (rustdoc joins them with the module file's docs)"
                ),
                last,
                &mut out,
            );
        }
    }
    if edge_start {
        if let Some(why) = doc::starts_open(&views) {
            push(
                "include",
                format!(
                    "leading inner docs: {why} (rustdoc may join them after the parent's docs)"
                ),
                lo,
                &mut out,
            );
        }
    }
    out
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

/// What one attribute sets as documentation.
enum DocVal {
    /// A string literal: its lines with their source line numbers.
    Lines(Vec<DocLine>),
    /// Anything else (`concat!(...)`, `$x`, `include_str!(...)`, a macro call): the value is
    /// only known after expansion. The span is the value's first token.
    Computed((usize, usize, usize)),
}

/// Split a token list on its top-level commas.
fn split_commas(v: &[TokenTree]) -> Vec<Vec<TokenTree>> {
    let mut out = vec![Vec::new()];
    for t in v {
        if punct_is(Some(t), ',') {
            out.push(Vec::new());
        } else {
            out.last_mut().unwrap().push(t.clone());
        }
    }
    out.retain(|p| !p.is_empty());
    out
}

/// `doc = <value>` in an attribute, also inside `cfg_attr(pred, ...)` at any depth: rustdoc
/// applies a `cfg_attr` doc when its predicate holds (`all()`, `doc`, ...), so it is scanned
/// like any doc comment (R3-F1).
fn doc_values(g: &Group) -> Vec<DocVal> {
    let v: Vec<TokenTree> = g.stream().into_iter().collect();
    let mut out = Vec::new();
    doc_values_in(&v, 0, &mut out);
    out
}

fn doc_values_in(v: &[TokenTree], depth: usize, out: &mut Vec<DocVal>) {
    if name_is(v.first(), "doc") && punct_is(v.get(1), '=') {
        if v.len() == 3 {
            if let Some((val, sp)) = lit_str(v.get(2)) {
                let first = start(sp).0;
                let last = end(sp).0.max(first);
                out.push(DocVal::Lines(
                    val.split('\n')
                        .enumerate()
                        .map(|(k, t)| DocLine {
                            // a one-line literal with \n escapes stays on its line
                            line: (first + k).min(last),
                            hi: last,
                            text: t.to_string(),
                        })
                        .collect(),
                ));
                return;
            }
        }
        let sp = v.get(2).or(v.first()).map(|t| tspan(t.span())).unwrap();
        out.push(DocVal::Computed(sp));
        return;
    }
    if name_is(v.first(), "cfg_attr") && depth < 16 {
        if let Some(TokenTree::Group(g)) = v.get(1) {
            if g.delimiter() == Delimiter::Parenthesis {
                let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                for part in split_commas(&inner).iter().skip(1) {
                    doc_values_in(part, depth + 1, out);
                }
            }
        }
    }
}
