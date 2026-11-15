//! `use` trees and `extern crate` items: the bindings a file makes, parsed with syn from the
//! token slice between the keyword and its `;` (so they are found inside macro bodies too).

use crate::resolve::{is_local_root, Aliases, Path};
use crate::scan::{ident_is, name, semi_after, slice, tspan};
use proc_macro2::{Span, TokenStream, TokenTree};

/// One leaf of a `use` tree: what it imports, what name it binds, and its tokens.
pub struct Leaf {
    pub full: Path,
    pub bind: Option<String>,
    pub glob: bool,
    pub spans: Vec<(usize, usize, usize)>,
}

fn use_leaves(t: &syn::UseTree, pre: &mut Vec<(String, Span)>, abs: bool, out: &mut Vec<Leaf>) {
    let mk = |pre: &Vec<(String, Span)>, extra: &[(String, Span)]| -> (Path, Vec<(usize, usize, usize)>) {
        let segs = pre.iter().chain(extra.iter()).map(|(s, _)| s.clone()).collect();
        let spans = pre.iter().chain(extra.iter()).map(|(_, sp)| tspan(*sp)).collect();
        (Path::new(abs, segs), spans)
    };
    match t {
        syn::UseTree::Path(p) => {
            pre.push((name(&p.ident.to_string()), p.ident.span()));
            use_leaves(&p.tree, pre, abs, out);
            pre.pop();
        }
        syn::UseTree::Name(n) => {
            let s = name(&n.ident.to_string());
            if s == "self" {
                let (full, mut spans) = mk(pre, &[]);
                spans.push(tspan(n.ident.span()));
                out.push(Leaf { bind: full.segs.last().cloned(), full, glob: false, spans });
            } else {
                let (full, spans) = mk(pre, &[(s.clone(), n.ident.span())]);
                out.push(Leaf { full, bind: Some(s), glob: false, spans });
            }
        }
        syn::UseTree::Rename(r) => {
            let s = name(&r.ident.to_string());
            let (full, mut spans) = if s == "self" { mk(pre, &[]) } else { mk(pre, &[(s, r.ident.span())]) };
            spans.push(tspan(r.rename.span()));
            out.push(Leaf { full, bind: Some(name(&r.rename.to_string())), glob: false, spans });
        }
        syn::UseTree::Glob(g) => {
            let (mut full, mut spans) = mk(pre, &[]);
            full.segs.push("*".into());
            spans.push(tspan(g.star_token.span));
            out.push(Leaf { full, bind: None, glob: true, spans });
        }
        syn::UseTree::Group(g) => {
            for it in &g.items {
                use_leaves(it, pre, abs, out);
            }
        }
    }
}

/// `use ...;` starting at `i`: the leaves and the index of the `;`.
pub(crate) fn parse_use(v: &[TokenTree], i: usize) -> Option<(Vec<Leaf>, usize)> {
    let j = semi_after(v, i)?;
    let item: syn::ItemUse = syn::parse2(slice(v, i, j)).ok()?;
    let mut out = Vec::new();
    use_leaves(&item.tree, &mut Vec::new(), item.leading_colon.is_some(), &mut out);
    Some((out, j))
}

/// `extern crate NAME [as ALIAS];` starting at `i`: (name, alias, name span, index of `;`).
pub(crate) fn parse_extern_crate(v: &[TokenTree], i: usize) -> Option<(String, Option<String>, Span, usize)> {
    let j = semi_after(v, i)?;
    let item: syn::ItemExternCrate = syn::parse2(slice(v, i, j)).ok()?;
    let alias = item.rename.map(|(_, a)| name(&a.to_string()));
    Some((name(&item.ident.to_string()), alias, item.ident.span(), j))
}

/// Pass 1: every binding in the stream, at any depth.
pub fn collect_aliases(ts: TokenStream, a: &mut Aliases) {
    let v: Vec<TokenTree> = ts.into_iter().collect();
    let mut i = 0;
    while i < v.len() {
        if ident_is(v.get(i), "use") {
            if let Some((leaves, j)) = parse_use(&v, i) {
                for l in leaves {
                    if l.glob {
                        let mut p = l.full.clone();
                        p.segs.pop();
                        a.glob(p);
                    } else if let Some(b) = l.bind {
                        a.bind(&b, l.full);
                    }
                }
                i = j + 1;
                continue;
            }
        }
        if ident_is(v.get(i), "extern") && ident_is(v.get(i + 1), "crate") {
            if let Some((n, alias, _, j)) = parse_extern_crate(&v, i) {
                if let Some(al) = alias {
                    if !is_local_root(&n) {
                        a.bind(&al, Path::new(false, vec![n]));
                    }
                }
                i = j + 1;
                continue;
            }
        }
        if let TokenTree::Group(g) = &v[i] {
            collect_aliases(g.stream(), a);
        }
        i += 1;
    }
}

