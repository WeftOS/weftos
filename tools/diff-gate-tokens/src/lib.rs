//! diff-gate-tokens: the Rust content checks of scripts/diff-gate.sh, done on tokens.
//!
//! The line gate's regexes see text; rustc sees tokens. This crate lexes each changed file
//! with proc-macro2 (the same token rules rustc uses), resolves `use` aliases and glob
//! imports within the file, scans doc-comment code blocks as the doctests they are, and
//! reports which sensitive constructs touch an added line. See docs/development/diff-gate.md.
//!
//! Fail closed: a file that does not tokenize, or that syn cannot parse, is itself a finding.

pub mod doc;
pub mod report;
pub mod resolve;
pub mod scan;
pub mod uses;

use proc_macro2::TokenStream;
use std::collections::BTreeSet;

pub use scan::Occ;

/// Wire protocol between the gate script and this binary. Bump on any output change.
pub const PROTOCOL: &str = "diff-gate-tokens protocol 1";

pub struct Scan {
    pub occs: Vec<Occ>,
    /// Lines on which some token starts (to skip blank and comment lines inside blocks).
    pub token_lines: BTreeSet<usize>,
    /// syn could not parse the file (the tokens were still scanned).
    pub parse_error: Option<(usize, String)>,
}

/// Blank a byte-order mark and a shebang line without moving any later token.
fn prepare(src: &str) -> String {
    let mut s = src.replacen('\u{feff}', " ", usize::from(src.starts_with('\u{feff}')));
    if let Some(rest) = s.strip_prefix("#!") {
        if !rest.trim_start().starts_with('[') {
            let end = s.find('\n').unwrap_or(s.len());
            let blank: String = s[..end].chars().map(|_| ' ').collect();
            s.replace_range(..end, &blank);
        }
    }
    s
}

/// Scan one file. Err is (line, message) when the file does not tokenize.
pub fn scan_source(src: &str) -> Result<Scan, (usize, String)> {
    let prepared = prepare(src);
    let ts: TokenStream = prepared.parse().map_err(|e: proc_macro2::LexError| (e.span().start().line.max(1), format!("does not tokenize: {e}")))?;
    let mut aliases = resolve::Aliases::default();
    scan::collect_aliases(ts.clone(), &mut aliases);
    let mut sc = scan::Scanner::new(&aliases, 0);
    sc.scan(ts, false);
    let parse_error = match syn::parse_file(src) {
        Ok(_) => None,
        Err(e) => Some((e.span().start().line.max(1), format!("syn cannot parse the file: {e}"))),
    };
    Ok(Scan { occs: sc.occs, token_lines: sc.token_lines, parse_error })
}
