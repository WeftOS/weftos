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
use crate::scan::{ident_is, name, punct_is};
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
}

/// `cfg(test)` or `cfg(all(.., test, ..))`: the only predicates that make an item test-only.
fn is_test_pred(v: &[TokenTree]) -> bool {
    if v.len() == 1 && ident_is(v.first(), "test") {
        return true;
    }
    if v.len() == 2 && ident_is(v.first(), "all") {
        if let Some(TokenTree::Group(g)) = v.get(1) {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            return split_commas(&inner)
                .iter()
                .any(|p| p.len() == 1 && ident_is(p.first(), "test"));
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
}

fn read_attr(v: &[TokenTree], under_test: bool, cond: bool, depth: usize, m: &mut ModAttrs) {
    if ident_is(v.first(), "cfg") && !cond {
        if let Some(TokenTree::Group(g)) = v.get(1) {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            if is_test_pred(&inner) {
                m.test = true;
            }
        }
        return;
    }
    if ident_is(v.first(), "path") && punct_is(v.get(1), '=') {
        if let Some(p) = lit(v.get(2)) {
            m.paths.push((p, !cond, under_test));
        }
        return;
    }
    if ident_is(v.first(), "cfg_attr") && depth < 16 {
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
    fn walk(&mut self, ts: TokenStream, dirs: &[PathBuf], test: bool) {
        let v: Vec<TokenTree> = ts.into_iter().collect();
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
                TokenTree::Ident(id) if *id == "unsafe" && ident_is(v.get(i + 1), "mod") => {
                    i += 1;
                    continue;
                }
                TokenTree::Ident(id) if *id == "mod" => {
                    if let Some(TokenTree::Ident(n)) = v.get(i + 1) {
                        let nm = name(&n.to_string());
                        let m = mod_attrs(&attrs);
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
                                self.walk(g.stream(), &child, t);
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
                    self.walk(g.stream(), dirs, test);
                    attrs.clear();
                    i += 1;
                    continue;
                }
                _ => {}
            }
            // a macro call `path!(...)`: include! adds a reference
            let t_here = test || mod_attrs(&attrs).test;
            if let Some((segs, abs, k)) = path_at(&v, i) {
                if punct_is(v.get(k), '!') {
                    if let Some(TokenTree::Group(g)) = v.get(k + 1) {
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
                        self.walk(g.stream(), dirs, test);
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
                       queue: &mut VecDeque<(PathBuf, Vec<PathBuf>)>| {
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
            if seen.insert((canon.clone(), dirs.clone())) {
                queue.push_back((canon, dirs));
            }
        }
    };
    for r in roots {
        enqueue(croot.join(r), Vec::new(), &mut out, &mut queue);
    }
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
        };
        w.walk(ts, &dirs, false);
        if let Some(why) = w.unknown.take() {
            out.unknown.insert(relf.clone(), why);
        }
        for r in w.refs {
            let extra = if r.splice { dirs.clone() } else { Vec::new() };
            for c in r.cands {
                enqueue(c, extra.clone(), &mut out, &mut queue);
            }
        }
    }
    out
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
        let r = built(&[
            ("src/lib.rs", "macro_rules! m { () => { mod s_tests; }; }\n"),
            ("src/s_tests.rs", T),
        ]);
        assert!(r.nontest.contains("src/s_tests.rs"));
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
