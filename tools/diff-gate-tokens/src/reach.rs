//! Which source files the non-test targets of a crate build (sensitive-crate exemption,
//! review round 3: R3-B1..B4).
//!
//! A file exempt by its name (`*_tests.rs`, `tests/`) counts as test code only if no non-test
//! root reaches it. Roots are the Cargo targets the gate reads with `cargo metadata` (lib, bin,
//! proc-macro, build script). From each root this follows, on rustc's tokens (so line layout,
//! comments and string escapes do not matter):
//!
//! - `mod name;`, to `<dir>/name.rs` and `<dir>/name/mod.rs`
//! - `#[path = "..."]` and `#[cfg_attr(pred, path = "...")]` on a module
//! - inline `mod name { ... }` (its children resolve one directory down)
//! - `include!("...")`, under any path or `use` alias that resolves to `include`
//!
//! A reference counts as test-only when its attributes, or an enclosing inline module's,
//! carry `#[cfg(test)]` or `#[cfg(all(.., test, ..))]`; a `cfg_attr` path is test-only when a
//! predicate on its chain is one of those. Everything else is followed, including declarations
//! inside macro bodies (they may expand anywhere). It over-approximates where rustc's rules
//! are subtle: children of every file are looked for both next to it and in `<dir>/<stem>/`.
//!
//! Paths are compared without case (a case-insensitive disk builds `Foo_Tests.rs` for
//! `foo_tests.rs`), and symlinks are followed inside the tree. A computed `include!`, a file
//! that does not tokenize or is not UTF-8, or too many files, make the declaring file
//! "unknown": the gate then exempts nothing in that crate.

use crate::resolve::{classify, Aliases, Path as RPath};
use crate::scan::{keyword_is, name, name_is, punct_is};
use crate::uses::collect_aliases;
use proc_macro2::{Delimiter, Group, Spacing, TokenStream, TokenTree};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};

const MAX_VISITS: usize = 50_000;

#[derive(Default)]
pub struct Reach {
    /// Tree-relative paths built by some non-test root (the symlink path and its target).
    pub nontest: BTreeSet<String>,
    /// Tree-relative file -> why the walk could not follow it.
    pub unknown: BTreeMap<String, String>,
}

struct Ref {
    cands: Vec<PathBuf>,
    /// The referenced file is spliced into the current module (`include!`): its children
    /// resolve in the current module's directories as well as its own.
    splice: bool,
}

struct Walk<'a> {
    file_dir: &'a Path,
    aliases: Aliases,
    refs: Vec<Ref>,
    unknown: Option<String>,
    /// `macro_rules!` definitions in this file: name and the macros its body invokes.
    defs: Vec<(String, Vec<Vec<String>>)>,
    /// Macros invoked in item position (module level), outside test code: every path the
    /// invocation may stand for (as written and through `use` aliases).
    item_calls: Vec<(String, Vec<Vec<String>>)>,
}

/// Item-position macros that cannot declare a module: std's `thread_local!` and
/// `compile_error!`, and `cfg_if!`, whose expansion is its own body (which the walk reads).
const ITEM_MACROS: &[&[&str]] = &[
    &["thread_local"],
    &["std", "thread_local"],
    &["compile_error"],
    &["std", "compile_error"],
    &["core", "compile_error"],
    &["cfg_if"],
    &["cfg_if", "cfg_if"],
];

/// std macros a `macro_rules!` body may invoke without making its expansion unknown: none of
/// them expands to an item.
const EXPR_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "cfg",
    "column",
    "compile_error",
    "concat",
    "dbg",
    "env",
    "eprint",
    "eprintln",
    "file",
    "format",
    "format_args",
    "line",
    "matches",
    "module_path",
    "option_env",
    "panic",
    "print",
    "println",
    "stringify",
    "todo",
    "unimplemented",
    "unreachable",
    "vec",
    "write",
    "writeln",
];

fn is_std_root(s: &str) -> bool {
    matches!(s, "std" | "core" | "alloc")
}

fn expr_macro_ok(segs: &[String]) -> bool {
    match segs {
        [n] => EXPR_MACROS.contains(&n.as_str()),
        [r, n] => is_std_root(r) && EXPR_MACROS.contains(&n.as_str()),
        _ => false,
    }
}

fn item_macro_ok(segs: &[String]) -> bool {
    ITEM_MACROS
        .iter()
        .any(|m| m.len() == segs.len() && m.iter().zip(segs).all(|(a, b)| *a == b))
}

/// A local macro named by `segs`: `name!`, `$crate::name!`, `crate::name!`, `self::name!`.
fn local_macro_name(segs: &[String]) -> Option<&str> {
    match segs {
        [n] => Some(n.as_str()),
        [r, n] if matches!(r.as_str(), "$crate" | "crate" | "self") => Some(n.as_str()),
        _ => None,
    }
}

/// What a `macro_rules!` body can produce, read on its tokens without expanding it. Returns
/// (whether it can declare a module or a module path, the macros it invokes).
fn macro_def_info(ts: TokenStream) -> (bool, Vec<Vec<String>>) {
    fn go(v: &[TokenTree], in_attr: bool, out: &mut (bool, Vec<Vec<String>>)) {
        let mut i = 0;
        while i < v.len() {
            match &v[i] {
                TokenTree::Ident(id) if *id == "mod" => out.0 = true,
                TokenTree::Ident(_) if in_attr && name_is(v.get(i), "path") => out.0 = true,
                TokenTree::Punct(p) if p.as_char() == '#' => {
                    let gi = if punct_is(v.get(i + 1), '!') {
                        i + 2
                    } else {
                        i + 1
                    };
                    if let Some(TokenTree::Group(g)) = v.get(gi) {
                        if g.delimiter() == Delimiter::Bracket {
                            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                            go(&inner, true, out);
                            i = gi + 1;
                            continue;
                        }
                    }
                }
                TokenTree::Group(g) => {
                    let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                    go(&inner, in_attr, out);
                }
                _ => {}
            }
            let start = if punct_is(v.get(i), '$') && keyword_is(v.get(i + 1), "crate") {
                Some(i + 1)
            } else if matches!(v.get(i), Some(TokenTree::Ident(_))) || colon2(v, i) {
                Some(i)
            } else {
                None
            };
            if let Some(st) = start {
                if let Some((mut segs, _, k)) = path_at(v, st) {
                    if st > i {
                        segs[0] = "$crate".into(); // `$crate` reads as the ident `crate`
                    }
                    if punct_is(v.get(k), '!') {
                        if segs.last().map(|l| l == "include").unwrap_or(false) {
                            out.0 = true;
                        }
                        out.1.push(segs);
                    }
                    i = k.max(i + 1);
                    continue;
                }
            }
            i += 1;
        }
    }
    let v: Vec<TokenTree> = ts.into_iter().collect();
    let mut out = (false, Vec::new());
    go(&v, false, &mut out);
    out
}

/// `cfg(test)` or `cfg(all(.., test, ..))`: the only predicates that make an item test-only.
fn is_test_pred(v: &[TokenTree]) -> bool {
    if v.len() == 1 && name_is(v.first(), "test") {
        return true;
    }
    if v.len() == 2 && name_is(v.first(), "all") {
        if let Some(TokenTree::Group(g)) = v.get(1) {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            return split_commas(&inner)
                .iter()
                .any(|p| p.len() == 1 && name_is(p.first(), "test"));
        }
    }
    false
}

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

fn lit(t: Option<&TokenTree>) -> Option<String> {
    match t {
        Some(TokenTree::Literal(l)) => {
            let ts: TokenStream = TokenTree::Literal(l.clone()).into();
            syn::parse2::<syn::LitStr>(ts).ok().map(|s| s.value())
        }
        _ => None,
    }
}

/// What a module's outer attributes say: test-only, and its `path` values with whether each
/// applies unconditionally and whether it applies only under test.
#[derive(Default)]
struct ModAttrs {
    test: bool,
    paths: Vec<(String, bool, bool)>, // (value, unconditional, test-only)
    /// A `path` value that is not a string literal (`#[path = $p]`, `concat!(..)`).
    computed_path: bool,
}

fn read_attr(v: &[TokenTree], under_test: bool, cond: bool, depth: usize, m: &mut ModAttrs) {
    if name_is(v.first(), "cfg") && !cond {
        if let Some(TokenTree::Group(g)) = v.get(1) {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            if is_test_pred(&inner) {
                m.test = true;
            }
        }
        return;
    }
    if name_is(v.first(), "path") && punct_is(v.get(1), '=') {
        match lit(v.get(2)) {
            Some(p) if v.len() == 3 => m.paths.push((p, !cond, under_test)),
            _ => m.computed_path = true,
        }
        return;
    }
    if name_is(v.first(), "cfg_attr") && depth < 16 {
        if let Some(TokenTree::Group(g)) = v.get(1) {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            let parts = split_commas(&inner);
            if let Some(pred) = parts.first() {
                let t = under_test || is_test_pred(pred);
                for p in parts.iter().skip(1) {
                    read_attr(p, t, true, depth + 1, m);
                }
            }
        }
    }
}

fn mod_attrs(attrs: &[Group]) -> ModAttrs {
    let mut m = ModAttrs::default();
    for g in attrs {
        let v: Vec<TokenTree> = g.stream().into_iter().collect();
        read_attr(&v, false, false, 0, &mut m);
    }
    m
}

fn colon2(v: &[TokenTree], k: usize) -> bool {
    matches!(v.get(k), Some(TokenTree::Punct(p)) if p.as_char() == ':' && p.spacing() == Spacing::Joint)
        && punct_is(v.get(k + 1), ':')
}

/// A path at v[i] (`[::] a :: b ...`): its segments and the index after it.
fn path_at(v: &[TokenTree], i: usize) -> Option<(Vec<String>, bool, usize)> {
    let mut k = i;
    let mut abs = false;
    if colon2(v, k) {
        abs = true;
        k += 2;
    }
    let mut segs = Vec::new();
    while let Some(TokenTree::Ident(id)) = v.get(k) {
        segs.push(name(&id.to_string()));
        k += 1;
        if colon2(v, k) && matches!(v.get(k + 2), Some(TokenTree::Ident(_))) {
            k += 2;
        } else {
            break;
        }
    }
    (!segs.is_empty()).then_some((segs, abs, k))
}

/// For each token of one level: whether an item starts there (after `;`, after a `{ }` group,
/// at the level's start; attributes and `pub` / `pub(..)` keep the position).
fn item_starts(v: &[TokenTree]) -> Vec<bool> {
    let mut out = vec![false; v.len()];
    let mut at = true;
    let mut i = 0;
    while i < v.len() {
        out[i] = at;
        if punct_is(v.get(i), '#') {
            let gi = if punct_is(v.get(i + 1), '!') {
                i + 2
            } else {
                i + 1
            };
            if matches!(v.get(gi), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket)
            {
                for o in out.iter_mut().take(gi + 1).skip(i) {
                    *o = at;
                }
                i = gi + 1;
                continue;
            }
        }
        at = match &v[i] {
            TokenTree::Punct(p) => p.as_char() == ';',
            TokenTree::Group(g) => {
                g.delimiter() == Delimiter::Brace
                    || (g.delimiter() == Delimiter::Parenthesis
                        && at
                        && i > 0
                        && keyword_is(v.get(i - 1), "pub"))
            }
            TokenTree::Ident(id) => *id == "pub" && at,
            TokenTree::Literal(_) => false,
        };
        i += 1;
    }
    out
}

impl Walk<'_> {
    fn is_include(&self, segs: &[String], abs: bool) -> bool {
        if segs.len() == 1 && segs[0] == "include" {
            return true;
        }
        let p = RPath::new(abs, segs.to_vec());
        self.aliases
            .expand(&p)
            .iter()
            .any(|(c, _)| classify(c).contains(&"include"))
    }

    /// Walk one token level. `dirs`: where child modules of the current module live.
    /// `items`: this level is a module body (file, inline module, or the body of a macro
    /// invoked there), where a macro invocation may expand to a module declaration.
    fn walk(&mut self, ts: TokenStream, dirs: &[PathBuf], test: bool, items: bool) {
        let v: Vec<TokenTree> = ts.into_iter().collect();
        let starts = item_starts(&v);
        let mut attrs: Vec<Group> = Vec::new();
        let mut i = 0;
        while i < v.len() {
            if punct_is(v.get(i), '#') {
                let inner = punct_is(v.get(i + 1), '!');
                let gi = if inner { i + 2 } else { i + 1 };
                if let Some(TokenTree::Group(g)) = v.get(gi) {
                    if g.delimiter() == Delimiter::Bracket {
                        if !inner {
                            attrs.push(g.clone());
                        }
                        i = gi + 1;
                        continue;
                    }
                }
            }
            match &v[i] {
                // visibility and `unsafe mod` keep the pending attributes
                TokenTree::Ident(id) if *id == "pub" => {
                    i += 1;
                    if matches!(v.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
                    {
                        i += 1;
                    }
                    continue;
                }
                TokenTree::Ident(id) if *id == "unsafe" && keyword_is(v.get(i + 1), "mod") => {
                    i += 1;
                    continue;
                }
                TokenTree::Ident(id) if *id == "mod" => {
                    let m = mod_attrs(&attrs);
                    if !(test || m.test) {
                        if !matches!(v.get(i + 1), Some(TokenTree::Ident(_))) {
                            self.unknown =
                                Some("a mod declaration whose name is not written out".into());
                        } else if m.computed_path {
                            self.unknown = Some(
                                "a #[path] attribute whose value is not a string literal".into(),
                            );
                        }
                    }
                    if let Some(TokenTree::Ident(n)) = v.get(i + 1) {
                        let nm = name(&n.to_string());
                        let t = test || m.test;
                        match v.get(i + 2) {
                            Some(TokenTree::Punct(p)) if p.as_char() == ';' => {
                                let mut cands = Vec::new();
                                let mut default = true;
                                for (p, uncond, ptest) in &m.paths {
                                    if *uncond {
                                        default = false;
                                    }
                                    if !(t || *ptest) {
                                        cands.push(self.file_dir.join(p));
                                        cands.extend(dirs.iter().map(|d| d.join(p)));
                                    }
                                }
                                if default && !t {
                                    for d in dirs {
                                        cands.push(d.join(format!("{nm}.rs")));
                                        cands.push(d.join(&nm).join("mod.rs"));
                                    }
                                }
                                self.refs.push(Ref {
                                    cands,
                                    splice: false,
                                });
                                attrs.clear();
                                i += 3;
                                continue;
                            }
                            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => {
                                let mut child: Vec<PathBuf> =
                                    dirs.iter().map(|d| d.join(&nm)).collect();
                                for (p, _, _) in &m.paths {
                                    child.push(self.file_dir.join(p));
                                    child.extend(dirs.iter().map(|d| d.join(p)));
                                }
                                self.walk(g.stream(), &child, t, true);
                                attrs.clear();
                                i += 3;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    attrs.clear();
                    i += 1;
                    continue;
                }
                TokenTree::Group(g) => {
                    self.walk(g.stream(), dirs, test, false);
                    attrs.clear();
                    i += 1;
                    continue;
                }
                _ => {}
            }
            // a macro call `path!(...)`: include! adds a reference
            let t_here = test || mod_attrs(&attrs).test;
            if let Some((segs, abs, k)) = path_at(&v, i) {
                // macro_rules! name { body }: the walk does not expand macros, so a body that
                // can declare a module, a module path or an include makes this file unknown
                if segs.len() == 1 && segs[0] == "macro_rules" && punct_is(v.get(k), '!') {
                    if let (Some(TokenTree::Ident(n)), Some(TokenTree::Group(g))) =
                        (v.get(k + 1), v.get(k + 2))
                    {
                        let nm = name(&n.to_string());
                        let (creates, calls) = macro_def_info(g.stream());
                        if creates && !t_here {
                            self.unknown = Some(format!(
                                "macro_rules! {nm} can declare a module, a #[path] or an include! (macros are not expanded)"
                            ));
                        }
                        if !t_here {
                            self.defs.push((nm, calls));
                        }
                        attrs.clear();
                        i = k + 3;
                        continue;
                    }
                }
                if punct_is(v.get(k), '!') {
                    if let Some(TokenTree::Group(g)) = v.get(k + 1) {
                        if items && starts[i] && !t_here && !self.is_include(&segs, abs) {
                            let p = RPath::new(abs, segs.clone());
                            let mut cands: Vec<Vec<String>> = vec![segs.clone()];
                            for (c, _) in self.aliases.expand(&p) {
                                if !cands.contains(&c.segs) {
                                    cands.push(c.segs);
                                }
                            }
                            self.item_calls.push((segs.join("::"), cands));
                        }
                        if self.is_include(&segs, abs) {
                            let args: Vec<TokenTree> = g.stream().into_iter().collect();
                            let n = if punct_is(args.last(), ',') {
                                args.len() - 1
                            } else {
                                args.len()
                            };
                            match lit(args.first()) {
                                Some(p) if n == 1 && !t_here => {
                                    self.refs.push(Ref {
                                        cands: vec![self.file_dir.join(p)],
                                        splice: true,
                                    });
                                }
                                Some(_) if n == 1 => {}
                                _ if !t_here => {
                                    self.unknown = Some("include! of a computed path".into());
                                }
                                _ => {}
                            }
                        }
                        self.walk(g.stream(), dirs, test, items);
                        attrs.clear();
                        i = k + 2;
                        continue;
                    }
                }
                attrs.clear();
                i = k;
                continue;
            }
            attrs.clear();
            i += 1;
        }
    }
}

/// Lexical normalization: `a/./b/../c` -> `a/c`. A path that climbs above `/` stays as is.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Existing files that `p` names: the exact path, or else every path whose components match
/// without case (R3-B3).
fn resolve_ci(p: &Path) -> Vec<PathBuf> {
    if p.symlink_metadata().is_ok() {
        return vec![p.to_path_buf()];
    }
    let mut cur: Vec<PathBuf> = vec![PathBuf::new()];
    for c in p.components() {
        let mut next = Vec::new();
        match c {
            Component::Normal(s) => {
                let want = s.to_string_lossy().to_lowercase();
                for base in &cur {
                    let dir = if base.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        base.clone()
                    };
                    let Ok(rd) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().to_lowercase() == want {
                            next.push(base.join(e.file_name()));
                        }
                    }
                }
            }
            other => next = cur.iter().map(|b| b.join(other.as_os_str())).collect(),
        }
        if next.is_empty() {
            return Vec::new();
        }
        cur = next;
    }
    cur
}

fn rel(root: &Path, p: &Path) -> Option<String> {
    p.strip_prefix(root)
        .ok()
        .map(|r| r.to_string_lossy().replace('\\', "/"))
}

fn child_dirs(f: &Path) -> Vec<PathBuf> {
    let dir = f.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut out = vec![dir.clone()];
    if let Some(stem) = f.file_stem() {
        if f.file_name().map(|n| n != "mod.rs").unwrap_or(true) {
            out.push(dir.join(stem));
        }
    }
    out
}

/// Walk from `roots` (paths relative to `root`) and return what non-test code builds.
pub fn reach(root: &Path, roots: &[String]) -> Reach {
    let mut out = Reach::default();
    let Ok(croot) = root.canonicalize() else {
        out.unknown
            .insert(".".into(), "tree root cannot be resolved".into());
        return out;
    };
    let mut queue: VecDeque<(PathBuf, Vec<PathBuf>)> = VecDeque::new();
    let mut seen: BTreeSet<(PathBuf, Vec<PathBuf>)> = BTreeSet::new();
    let mut visits = 0;
    let mut enqueue = |p: PathBuf,
                       extra: Vec<PathBuf>,
                       out: &mut Reach,
                       queue: &mut VecDeque<(PathBuf, Vec<PathBuf>)>|
     -> bool {
        let mut any = false;
        for found in resolve_ci(&normalize(&p)) {
            let Ok(canon) = found.canonicalize() else {
                continue; // a dangling symlink builds nothing
            };
            if !canon.starts_with(&croot) || !canon.is_file() {
                continue; // outside the tree the gate extracted: not followed
            }
            if let Some(r) = rel(&croot, &normalize(&found)) {
                out.nontest.insert(r); // the path as written (a symlink's own path)
            }
            if let Some(r) = rel(&croot, &canon) {
                out.nontest.insert(r);
            }
            let mut dirs = child_dirs(&canon);
            for d in &extra {
                if !dirs.contains(d) {
                    dirs.push(d.clone());
                }
            }
            any = true;
            if seen.insert((canon.clone(), dirs.clone())) {
                queue.push_back((canon, dirs));
            }
        }
        any
    };
    // A root is a target Cargo reported: one the walk cannot open is a failure, not a skip
    // (R4-A2). Its own path is the unknown file.
    for r in roots {
        if !enqueue(croot.join(r), Vec::new(), &mut out, &mut queue) {
            out.unknown
                .insert(r.clone(), "a Cargo target root the walk cannot open".into());
        }
    }
    let mut defs: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    let mut calls: Vec<(String, String, Vec<Vec<String>>)> = Vec::new();
    while let Some((f, dirs)) = queue.pop_front() {
        visits += 1;
        let relf = rel(&croot, &f).unwrap_or_default();
        if visits > MAX_VISITS {
            out.unknown.insert(relf, "too many module files".into());
            break;
        }
        let Ok(bytes) = std::fs::read(&f) else {
            out.unknown.insert(relf, "cannot be read".into());
            continue;
        };
        let Ok(src) = String::from_utf8(bytes) else {
            out.unknown.insert(relf, "not valid UTF-8".into());
            continue;
        };
        let Ok(ts) = crate::prepare(&src).parse::<TokenStream>() else {
            out.unknown.insert(relf, "does not tokenize".into());
            continue;
        };
        let mut aliases = Aliases::default();
        collect_aliases(ts.clone(), &mut aliases);
        let dir = f.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut w = Walk {
            file_dir: &dir,
            aliases,
            refs: Vec::new(),
            unknown: None,
            defs: Vec::new(),
            item_calls: Vec::new(),
        };
        w.walk(ts, &dirs, false, true);
        if let Some(why) = w.unknown.take() {
            out.unknown.insert(relf.clone(), why);
        }
        for (n, c) in w.defs.drain(..) {
            defs.entry(n).or_default().extend(c);
        }
        for (shown, c) in w.item_calls.drain(..) {
            calls.push((relf.clone(), shown, c));
        }
        for r in w.refs {
            let extra = if r.splice { dirs.clone() } else { Vec::new() };
            for c in r.cands {
                enqueue(c, extra.clone(), &mut out, &mut queue);
            }
        }
    }
    // A macro invoked in item position may expand to a module declaration (R4-B1). It is
    // followed only when it is std's thread_local! / compile_error!, cfg_if!, or a
    // macro_rules! of this crate whose body invokes nothing but such macros and std's
    // expression macros (a body that declares modules is already unknown above).
    let mut memo: BTreeMap<String, bool> = BTreeMap::new();
    for (f, shown, cands) in calls {
        let ok = cands.iter().all(|c| {
            item_macro_ok(c)
                || local_macro_name(c).is_some_and(|n| clean_def(n, &defs, &mut memo, 0))
        });
        if !ok {
            out.unknown.entry(f).or_insert(format!(
                "{shown}! in item position may declare a module (macros are not expanded)"
            ));
        }
    }
    out
}

/// A local `macro_rules!` whose expansion cannot declare a module: defined in the walked
/// files, and invoking only std macros or other such local macros. A cycle is not clean.
fn clean_def(
    n: &str,
    defs: &BTreeMap<String, Vec<Vec<String>>>,
    memo: &mut BTreeMap<String, bool>,
    depth: usize,
) -> bool {
    if let Some(&v) = memo.get(n) {
        return v;
    }
    let Some(calls) = defs.get(n) else {
        return false;
    };
    if depth > 32 {
        return false;
    }
    memo.insert(n.to_string(), false); // a cycle reads as not clean
    let ok = calls.iter().all(|c| {
        expr_macro_ok(c)
            || item_macro_ok(c)
            || local_macro_name(c).is_some_and(|m| clean_def(m, defs, memo, depth + 1))
    });
    memo.insert(n.to_string(), ok);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(files: &[(&str, &str)]) -> tempdir::Dir {
        let d = tempdir::Dir::new();
        for (p, c) in files {
            let f = d.0.join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, c).unwrap();
        }
        d
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let p = std::env::temp_dir().join(format!(
                    "dgt-reach-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::SeqCst)
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn built(files: &[(&str, &str)]) -> Reach {
        let d = tree(files);
        reach(&d.0, &["src/lib.rs".to_string()])
    }

    const T: &str = "fn t() {}\n";

    #[test]
    fn test_cfg_keeps_exempt() {
        let r = built(&[
            ("src/lib.rs", "#[cfg(test)]\nmod s_tests;\n"),
            ("src/s_tests.rs", T),
        ]);
        assert!(!r.nontest.contains("src/s_tests.rs"));
        let r = built(&[
            (
                "src/lib.rs",
                "#[cfg(all(unix, test))] pub(crate) mod s_tests;\n",
            ),
            ("src/s_tests.rs", T),
        ]);
        assert!(!r.nontest.contains("src/s_tests.rs"));
        let r = built(&[
            ("src/lib.rs", "#[cfg(test)]\nmod tests { mod s_tests; }\n"),
            ("src/tests/s_tests.rs", T),
        ]);
        assert!(!r.nontest.contains("src/tests/s_tests.rs"));
    }

    #[test]
    fn split_and_multiline_forms() {
        let r = built(&[
            ("src/lib.rs", "pub(crate) mod\n    s_tests;\n"),
            ("src/s_tests.rs", T),
        ]);
        assert!(r.nontest.contains("src/s_tests.rs"));
        let r = built(&[
            ("src/lib.rs", "include!(\n    \"s_tests.rs\"\n);\n"),
            ("src/s_tests.rs", T),
        ]);
        assert!(r.nontest.contains("src/s_tests.rs"));
        // a module declared inside a macro_rules! body: the walk does not expand macros
        let r = built(&[
            ("src/lib.rs", "macro_rules! m { () => { mod s_tests; }; }\n"),
            ("src/s_tests.rs", T),
        ]);
        assert!(r.unknown.contains_key("src/lib.rs"));
    }

    /// R4-B1: a macro can fill in a module's name or path, and a macro the walk cannot see
    /// can expand to a module declaration. Each makes the declaring file unknown.
    #[test]
    fn macros_fail_closed() {
        for lib in [
            "macro_rules! decl { ($n:ident) => { pub mod $n; }; }\ndecl!(s_tests);\n",
            "macro_rules! decl { ($($n:ident),*) => { $(pub mod $n;)* }; }\ndecl!(a, s_tests);\n",
            "macro_rules! at { ($p:literal) => { #[path = $p] pub mod h; }; }\nat!(\"s_tests.rs\");\n",
            "macro_rules! inc { ($p:literal) => { include!($p); }; }\n",
            "macro_rules! at { ($p:literal) => { #[cfg_attr(unix, path = $p)] pub mod h; }; }\n",
            // invoked in item position, defined elsewhere (another crate, a proc-macro)
            "decl!(s_tests);\n",
            "some_crate::decl! { s_tests }\n",
            "pub mod inner { decl!(s_tests); }\n",
            "#[cfg(unix)]\npub(crate) decl!(s_tests);\n",
            "pub fn a() {}\ndecl!(s_tests);\n",
            // a local macro that hands its input to a macro the walk cannot see
            "macro_rules! wrap { ($n:ident) => { other::decl!($n); }; }\nwrap!(s_tests);\n",
            // an allowlisted name bound to something else
            "use evil::mk as thread_local;\nthread_local! { s_tests }\n",
            // the module name or path written by a macro in a #[path] value
            "#[path = concat!(\"s_tests\", \".rs\")]\nmod h;\n",
        ] {
            let r = built(&[("src/lib.rs", lib), ("src/s_tests.rs", T), ("src/a.rs", T)]);
            assert!(r.unknown.contains_key("src/lib.rs"), "{lib}: {:?}", r.unknown);
        }
        // controls: std's item macros, cfg_if (its body is read), a local macro that only uses
        // std macros, statement-position macros, and macros in test code
        for lib in [
            "thread_local! { static X: u8 = 0; }\ncompile_error!(\"x\");\n",
            "std::thread_local! { static X: u8 = 0; }\n",
            "cfg_if::cfg_if! { if #[cfg(unix)] { pub fn a() {} } }\n",
            "macro_rules! k { ($e:expr) => { pub const K: &str = stringify!($e); }; }\nk!(1);\n",
            "macro_rules! k { () => { pub fn k() { $crate::j!(); } }; }\nmacro_rules! j { () => { println!(\"x\") }; }\nk!();\n",
            "pub fn f() { println!(\"x\"); tracing::info!(\"y\"); }\n",
            "pub const V: &str = env!(\"CARGO_PKG_VERSION\");\nstatic S: &str = include_str!(\"x.txt\");\n",
            "#[doc = include_str!(\"x.md\")]\npub struct D;\n",
            "#[cfg(test)]\nmod t { decl!(s_tests); }\n",
            "#[cfg(test)]\nmacro_rules! decl { ($n:ident) => { pub mod $n; }; }\n",
        ] {
            let r = built(&[("src/lib.rs", lib), ("src/s_tests.rs", T)]);
            assert!(r.unknown.is_empty(), "{lib}: {:?}", r.unknown);
            assert!(!r.nontest.contains("src/s_tests.rs"), "{lib}");
        }
    }

    /// R4-A2: a root the walk cannot open is unknown, not skipped.
    #[test]
    fn unopenable_root_is_unknown() {
        let d = tree(&[("src/lib%.rs", "mod s_tests;\n"), ("src/s_tests.rs", T)]);
        let r = reach(&d.0, &["src/lib%25.rs".to_string()]);
        assert!(r.unknown.contains_key("src/lib%25.rs"), "{:?}", r.unknown);
        let r = reach(&d.0, &["src/lib%.rs".to_string()]);
        assert!(r.unknown.is_empty());
        assert!(r.nontest.contains("src/s_tests.rs"));
        for name in ["src/a b.rs", "src/\u{e9}t\u{e9}.rs"] {
            let d = tree(&[(name, "mod s_tests;\n"), ("src/s_tests.rs", T)]);
            let r = reach(&d.0, &[name.to_string()]);
            assert!(r.nontest.contains("src/s_tests.rs"), "{name}");
        }
    }

    #[test]
    fn escapes_case_and_aliases() {
        let r = built(&[
            ("src/lib.rs", "#[path = \"s_tests\\x2Ers\"]\nmod h;\n"),
            ("src/s_tests.rs", T),
        ]);
        assert!(r.nontest.contains("src/s_tests.rs"));
        let r = built(&[
            ("src/lib.rs", "#[path = \"S_Tests.rs\"]\nmod h;\n"),
            ("src/s_tests.rs", T),
        ]);
        assert!(r.nontest.contains("src/s_tests.rs"));
        let r = built(&[
            (
                "src/lib.rs",
                "use std::include as inc;\ninc!(\"s_tests.rs\");\n",
            ),
            ("src/s_tests.rs", T),
        ]);
        assert!(r.nontest.contains("src/s_tests.rs"));
        let r = built(&[
            (
                "src/lib.rs",
                "#[cfg_attr(not(test), path = \"s_tests.rs\")]\nmod h;\n",
            ),
            ("src/s_tests.rs", T),
        ]);
        assert!(r.nontest.contains("src/s_tests.rs"));
        let r = built(&[
            (
                "src/lib.rs",
                "#[cfg_attr(test, path = \"s_tests.rs\")]\nmod h;\n",
            ),
            ("src/s_tests.rs", T),
            ("src/h.rs", T),
        ]);
        assert!(!r.nontest.contains("src/s_tests.rs"));
        assert!(r.nontest.contains("src/h.rs"));
    }

    #[test]
    fn nested_and_computed() {
        let r = built(&[
            ("src/lib.rs", "pub mod a;\n"),
            ("src/a.rs", "mod b;\n"),
            ("src/a/b.rs", "#[path = \"../x_tests.rs\"] mod c;\n"),
            ("src/x_tests.rs", T),
        ]);
        assert!(r.nontest.contains("src/a/b.rs"));
        assert!(r.nontest.contains("src/x_tests.rs"));
        let r = built(&[(
            "src/lib.rs",
            "include!(concat!(env!(\"OUT_DIR\"), \"/g.rs\"));\n",
        )]);
        assert!(r.unknown.contains_key("src/lib.rs"));
        let r = built(&[(
            "src/lib.rs",
            "#[cfg(test)]\nmod t { include!(concat!(\"a\", \"b\")); }\n",
        )]);
        assert!(r.unknown.is_empty());
    }

    /// R4-B2: raw identifiers name the same attributes and cfg predicates (rustc 1.95):
    /// `#[r#path]`, `#[r#cfg_attr(.., path = ..)]` are followed; `cfg(r#test)` is test-only.
    #[test]
    fn raw_identifier_attribute_names() {
        for lib in [
            "#[r#path = \"s_tests.rs\"]\nmod h;\n",
            "#[r#cfg_attr(not(test), path = \"s_tests.rs\")]\nmod h;\n",
            "#[cfg_attr(not(test), r#path = \"s_tests.rs\")]\nmod h;\n",
            "#[r#cfg_attr(all(), r#cfg_attr(unix, r#path = \"s_tests.rs\"))]\nmod h;\n",
        ] {
            let r = built(&[("src/lib.rs", lib), ("src/s_tests.rs", T), ("src/h.rs", T)]);
            assert!(r.nontest.contains("src/s_tests.rs"), "{lib}");
        }
        for lib in [
            "#[cfg(r#test)]\nmod s_tests;\n",
            "#[r#cfg(test)]\nmod s_tests;\n",
            "#[r#cfg(r#all(unix, r#test))]\nmod s_tests;\n",
            "#[r#cfg_attr(r#test, path = \"s_tests.rs\")]\nmod h;\n",
        ] {
            let r = built(&[("src/lib.rs", lib), ("src/s_tests.rs", T), ("src/h.rs", T)]);
            assert!(!r.nontest.contains("src/s_tests.rs"), "{lib}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_module_file() {
        let d = tree(&[("src/lib.rs", "pub mod real;\n"), ("src/s_tests.rs", T)]);
        std::os::unix::fs::symlink("s_tests.rs", d.0.join("src/real.rs")).unwrap();
        let r = reach(&d.0, &["src/lib.rs".to_string()]);
        assert!(r.nontest.contains("src/s_tests.rs"));
        assert!(r.nontest.contains("src/real.rs"));
    }
}
