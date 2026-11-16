//! Which occurrences are findings for a diff, and their wire format.
//!
//! An occurrence is a finding when its construct overlaps an added line. Two more cases
//! need the base file:
//! - an added line inside an existing `unsafe { }` or `extern { }` block is new unsafe or
//!   FFI code even though the keyword's line did not change (a new block is one finding, on
//!   its keyword);
//! - an unchanged line whose meaning changed, because an import elsewhere was edited or
//!   deleted (`use foo::Cmd` became `use std::process::Command as Cmd`). Occurrences are
//!   keyed by (rule, what they resolve to, line text); a key that occurs more often in the
//!   head than in the base, beyond what added lines explain, reports all its occurrences.

use crate::{scan_source, Occ};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: String,
    pub line: usize,
    /// The line with tabs as spaces and leading whitespace removed (the gate's matching text).
    pub text: String,
    /// One span per occurrence, 1-based inclusive byte offsets into `text`. Empty for a
    /// whole-file finding such as a parse error.
    pub groups: Vec<(usize, usize)>,
    pub note: String,
}

struct Lines {
    raw: Vec<String>,
}

impl Lines {
    fn new(src: &str) -> Self {
        Lines {
            raw: src
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
                .collect(),
        }
    }
    fn raw(&self, line: usize) -> &str {
        self.raw
            .get(line.wrapping_sub(1))
            .map(|s| s.as_str())
            .unwrap_or("")
    }
    fn lead(&self, line: usize) -> usize {
        self.raw(line)
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count()
    }
    /// The gate's text for a line: tabs as spaces, leading blanks trimmed.
    fn text(&self, line: usize) -> String {
        self.raw(line)[self.lead(line)..].replace('\t', " ")
    }
    fn byte(&self, line: usize, col: usize) -> usize {
        let r = self.raw(line);
        r.char_indices().nth(col).map(|(b, _)| b).unwrap_or(r.len())
    }
    /// The occurrence's tokens on its report line as one 1-based inclusive span of `text`.
    fn group(&self, o: &Occ) -> (usize, usize) {
        let lead = self.lead(o.line);
        let tlen = self.text(o.line).len().max(1);
        let on: Vec<_> = o.spans.iter().filter(|s| s.0 == o.line).collect();
        if on.is_empty() {
            return (1, tlen);
        }
        let b0 = on.iter().map(|s| self.byte(o.line, s.1)).min().unwrap();
        let b1 = on.iter().map(|s| self.byte(o.line, s.2)).max().unwrap();
        let s = (b0 + 1).saturating_sub(lead).max(1);
        let e = b1.saturating_sub(lead).max(s);
        (s, e.min(tlen))
    }
}

fn hit(added: &BTreeSet<usize>, lo: usize, hi: usize) -> bool {
    lo <= hi && added.range(lo..=hi).next().is_some()
}

fn note_for(o: &Occ) -> String {
    match o.note {
        "" => String::new(),
        n => n.to_string(),
    }
}

/// Findings for one file. `base` is None for a new file.
pub fn findings(head: &str, base: Option<&str>, added: &BTreeSet<usize>) -> Vec<Finding> {
    let hl = Lines::new(head);
    let scan = match scan_source(head) {
        Ok(s) => s,
        Err((line, msg)) => {
            return vec![Finding {
                rule: "rust-parse".into(),
                line,
                text: msg,
                groups: vec![],
                note: String::new(),
            }]
        }
    };
    let mut out: BTreeMap<(String, usize), Finding> = BTreeMap::new();
    let mut add = |rule: &str, line: usize, group: (usize, usize), note: String, text: String| {
        let f = out
            .entry((rule.to_string(), line))
            .or_insert_with(|| Finding {
                rule: rule.into(),
                line,
                text,
                groups: vec![],
                note: String::new(),
            });
        if !f.groups.contains(&group) {
            f.groups.push(group);
        }
        if f.note.is_empty() {
            f.note = note;
        }
    };
    let mut parse_finding = None;
    if let Some((line, msg)) = &scan.parse_error {
        parse_finding = Some(Finding {
            rule: "rust-parse".into(),
            line: *line,
            text: msg.clone(),
            groups: vec![],
            note: String::new(),
        });
    }

    let mut reported = vec![false; scan.occs.len()];
    for (k, o) in scan.occs.iter().enumerate() {
        if hit(added, o.lo, o.hi) {
            reported[k] = true;
            add(o.rule, o.line, hl.group(o), note_for(o), hl.text(o.line));
        }
        // Lines inside a block whose keyword line is itself new are part of that finding.
        if let Some((b0, b1)) = o.block.filter(|_| !reported[k]) {
            for &l in added.range(b0..=b1.max(b0)) {
                if l != o.line && scan.token_lines.contains(&l) {
                    let t = hl.text(l);
                    let g = (1, t.len().max(1));
                    add(
                        o.rule,
                        l,
                        g,
                        format!("inside the {} block opened at line {}", o.what, o.line),
                        t,
                    );
                }
            }
        }
    }

    // Context change: compare keys with the base.
    if let Some(b) = base {
        let bl = Lines::new(b);
        let mut base_count: HashMap<(String, String, String), usize> = HashMap::new();
        if let Ok(bs) = scan_source(b) {
            for o in &bs.occs {
                *base_count
                    .entry((o.rule.into(), o.what.clone(), bl.text(o.line)))
                    .or_default() += 1;
            }
        }
        let mut by_key: HashMap<(String, String, String), Vec<usize>> = HashMap::new();
        for (k, o) in scan.occs.iter().enumerate() {
            by_key
                .entry((o.rule.into(), o.what.clone(), hl.text(o.line)))
                .or_default()
                .push(k);
        }
        for (key, idx) in by_key {
            let via_added = idx.iter().filter(|&&k| reported[k]).count();
            let base_n = base_count.get(&key).copied().unwrap_or(0);
            if idx.len() > base_n && idx.len() - base_n > via_added {
                for &k in idx.iter().filter(|&&k| !reported[k]) {
                    let o = &scan.occs[k];
                    add(
                        o.rule,
                        o.line,
                        hl.group(o),
                        "meaning changed by an edit elsewhere in the file".into(),
                        hl.text(o.line),
                    );
                }
            }
        }
    }

    let mut v: Vec<Finding> = out.into_values().collect();
    if let Some(p) = parse_finding {
        v.push(p);
    }
    v.sort_by(|a, b| (a.line, &a.rule).cmp(&(b.line, &b.rule)));
    v
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn display(t: &str) -> String {
    if t.len() <= 160 {
        return t.to_string();
    }
    let mut cut = 157;
    while !t.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &t[..cut])
}

/// The gate's finding format. Content findings: rule, path, line, display text, "", spans
/// ("s-e|s-e"), matching text. Whole-file findings: rule, path, line, text.
pub fn tsv(path: &str, fs: &[Finding]) -> String {
    let mut s = String::new();
    for f in fs {
        if f.groups.is_empty() {
            s.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                f.rule,
                clean(path),
                f.line,
                clean(&f.text)
            ));
        } else {
            let mut d = display(&clean(&f.text));
            if !f.note.is_empty() {
                d = format!("{d}  [{}]", f.note);
            }
            let g: Vec<String> = f.groups.iter().map(|(a, b)| format!("{a}-{b}")).collect();
            s.push_str(&format!(
                "{}\t{}\t{}\t{}\t\t{}\t{}\n",
                f.rule,
                clean(path),
                f.line,
                d,
                g.join("|"),
                clean(&f.text)
            ));
        }
    }
    s
}

fn js(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// JSON for people and other tools: {"findings":[{path, rule, line, text, spans, note}]}.
pub fn json(all: &[(String, Vec<Finding>)]) -> String {
    let mut items = Vec::new();
    for (path, fs) in all {
        for f in fs {
            let spans: Vec<String> = f.groups.iter().map(|(a, b)| format!("[{a},{b}]")).collect();
            items.push(format!(
                "{{\"path\":{},\"rule\":{},\"line\":{},\"text\":{},\"spans\":[{}],\"note\":{}}}",
                js(path),
                js(&f.rule),
                f.line,
                js(&f.text),
                spans.join(","),
                js(&f.note)
            ));
        }
    }
    format!("{{\"findings\":[{}]}}\n", items.join(","))
}
