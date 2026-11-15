//! diff-gate-tokens --manifest FILE [--json]
//! diff-gate-tokens --protocol
//!
//! Each manifest line is: repo path <TAB> head file <TAB> base file (empty for a new file)
//! <TAB> added line numbers, comma-separated. Findings go to stdout in the gate's TSV format
//! (or JSON with --json). Exit 0 when every file was read; 2 on a usage or read error.

use diff_gate_tokens::{report, PROTOCOL};
use std::collections::BTreeSet;
use std::io::Write;
use std::process::ExitCode;

fn read(path: &str) -> Result<Option<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    Ok(String::from_utf8(bytes).ok())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut manifest = None;
    let mut as_json = false;
    let mut k = 0;
    while k < args.len() {
        match args[k].as_str() {
            "--protocol" => {
                println!("{PROTOCOL}");
                return Ok(());
            }
            "--json" => as_json = true,
            "--manifest" => {
                k += 1;
                manifest = Some(args.get(k).ok_or("--manifest needs a file")?.clone());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        k += 1;
    }
    let manifest = manifest.ok_or("usage: diff-gate-tokens --manifest FILE [--json] | --protocol")?;
    let text = std::fs::read_to_string(&manifest).map_err(|e| format!("cannot read {manifest}: {e}"))?;
    let mut all: Vec<(String, Vec<report::Finding>)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 4 {
            return Err(format!("manifest line {} has {} fields, wanted 4", n + 1, f.len()));
        }
        let mut added = BTreeSet::new();
        for a in f[3].split(',').filter(|a| !a.is_empty()) {
            added.insert(a.parse::<usize>().map_err(|_| format!("manifest line {}: bad line number {a}", n + 1))?);
        }
        let fs = match read(f[1])? {
            None => vec![report::Finding {
                rule: "rust-parse".into(),
                line: 1,
                text: "file is not valid UTF-8".into(),
                groups: vec![],
                note: String::new(),
            }],
            Some(head) => {
                // An unreadable or non-UTF-8 base gives no base: every head occurrence on an
                // unchanged line then counts as a meaning change, which fails closed.
                let base = if f[2].is_empty() { None } else { Some(read(f[2])?.unwrap_or_default()) };
                report::findings(&head, base.as_deref(), &added)
            }
        };
        all.push((f[0].to_string(), fs));
    }
    let out = if as_json { report::json(&all) } else { all.iter().map(|(p, fs)| report::tsv(p, fs)).collect() };
    std::io::stdout().write_all(out.as_bytes()).map_err(|e| format!("write failed: {e}"))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("diff-gate-tokens: {e}");
            ExitCode::from(2)
        }
    }
}
