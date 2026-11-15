//! Name resolution inside one file, and the table of sensitive paths.
//!
//! Resolution over-approximates on purpose: every `use` and `extern crate` anywhere in the
//! file (any module, any block, inside macro bodies too) adds its bindings to one table, and
//! a path is checked under every expansion that table allows plus every glob import. A name
//! that could mean `std::process::Command` somewhere in the file is treated as meaning it
//! everywhere. That costs false positives, never misses.

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// One path, as segments. `absolute` is a leading `::`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Path {
    pub absolute: bool,
    pub segs: Vec<String>,
}

impl Path {
    pub fn new(absolute: bool, segs: Vec<String>) -> Self {
        Path { absolute, segs }
    }
    pub fn text(&self) -> String {
        let s = self.segs.join("::");
        if self.absolute { format!("::{s}") } else { s }
    }
}

/// Bindings collected from `use` and `extern crate` items.
#[derive(Default, Debug)]
pub struct Aliases {
    /// name -> (target, bound under a different name than the target's own)
    map: HashMap<String, BTreeSet<(Path, bool)>>,
    globs: BTreeSet<Path>,
}

const MAX_DEPTH: usize = 6;
const MAX_CANDIDATES: usize = 256;

impl Aliases {
    pub fn bind(&mut self, name: &str, target: Path) {
        if name == "_" {
            return;
        }
        let renamed = target.segs.last().map(|l| l != name).unwrap_or(true);
        self.map.entry(name.to_string()).or_default().insert((target, renamed));
    }
    pub fn glob(&mut self, prefix: Path) {
        self.globs.insert(prefix);
    }

    /// Every full path `p` may stand for, including `p` itself, with whether the candidate
    /// counts. `p` as written always counts. An expansion counts when some step in it could
    /// disguise what the line does: a glob, a rename (`as`), or a lower-case head (a module,
    /// crate or function alias: `p::Command`, `h::Client`, `get(url)`). Importing a type
    /// under its own name (`use hyper::StatusCode;` then `StatusCode::OK`) does not count:
    /// the import was the finding, and every later mention of the type would otherwise be one.
    /// The name-based checks (`Command::new`, socket types) still apply to such lines.
    pub fn expand(&self, p: &Path) -> Vec<(Path, bool)> {
        let mut seen: BTreeMap<Path, bool> = BTreeMap::new();
        let mut work = vec![(p.clone(), 0usize, true)];
        while let Some((cur, depth, counts)) = work.pop() {
            if seen.len() > MAX_CANDIDATES {
                break;
            }
            match seen.get(&cur) {
                Some(&c) if c || !counts => continue,
                _ => {}
            }
            seen.insert(cur.clone(), counts);
            if cur.absolute || depth >= MAX_DEPTH || cur.segs.is_empty() {
                continue;
            }
            let head = cur.segs[0].as_str();
            if is_local_root(head) {
                continue;
            }
            let lower = head.chars().next().map(|c| !c.is_ascii_uppercase()).unwrap_or(true);
            let disguised_so_far = counts && depth > 0;
            if let Some(ts) = self.map.get(head) {
                for (t, renamed) in ts {
                    let mut segs = t.segs.clone();
                    segs.extend_from_slice(&cur.segs[1..]);
                    work.push((Path::new(t.absolute, segs), depth + 1, disguised_so_far || *renamed || lower));
                }
            }
            for g in &self.globs {
                if g.segs.is_empty() || !glob_name_ok(g, head) {
                    continue;
                }
                let mut segs = g.segs.clone();
                segs.extend_from_slice(&cur.segs);
                work.push((Path::new(g.absolute, segs), depth + 1, true));
            }
        }
        seen.into_iter().collect()
    }
}

/// A glob import brings in a module's items, not every name in the file: a glob candidate
/// is only considered for a type-like name (upper-case first letter) or a function name the
/// table lists for that module (`libc::fork`). Otherwise `use std::process::*;` would make
/// every identifier in the file a process finding.
fn glob_name_ok(g: &Path, name: &str) -> bool {
    if name.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
        return true;
    }
    let segs: Vec<&str> = g.segs.iter().map(|s| norm_root(s.as_str())).collect();
    TABLE.iter().any(|e| !e.only.is_empty() && segs == e.prefix && e.only.iter().any(|o| name.starts_with(o)))
        || (segs.len() < 2 && matches!(name, "process" | "net" | "os" | "arch" | "include" | "unistd"))
}

/// Rust keywords: never the start of a path we need to resolve.
pub fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "as" | "async" | "await" | "break" | "const" | "continue" | "dyn" | "else" | "enum" | "extern"
            | "false" | "fn" | "for" | "if" | "impl" | "in" | "let" | "loop" | "match" | "mod" | "move"
            | "mut" | "pub" | "ref" | "return" | "static" | "struct" | "trait" | "true" | "type"
            | "unsafe" | "use" | "where" | "while" | "yield" | "macro_rules"
    )
}

/// Roots that name the current crate or module: we cannot see other files, so these never
/// resolve further (name-based checks still apply to them).
pub fn is_local_root(s: &str) -> bool {
    matches!(s, "crate" | "self" | "super" | "Self" | "$crate")
}

/// `core` and `alloc` re-export the same items as `std` for everything we look at.
fn norm_root(s: &str) -> &str {
    match s {
        "core" | "alloc" => "std",
        other => other,
    }
}

/// A sensitive module or item. `prefix` matches the start of a resolved path; a path whose
/// next segment is in `exempt` is not sensitive (for example `std::process::exit`).
struct Entry {
    rule: &'static str,
    prefix: &'static [&'static str],
    exempt: &'static [&'static str],
    /// When set, only these next segments are sensitive (prefix match on the name).
    only: &'static [&'static str],
}

const ADDR_TYPES: &[&str] = &[
    "SocketAddrV4", "SocketAddrV6", "SocketAddr", "IpAddr", "Ipv4Addr", "Ipv6Addr", "AddrParseError",
];
const PROCESS_EXEMPT: &[&str] = &[
    "exit", "abort", "id", "ExitCode", "ExitStatus", "ExitStatusError", "Termination", "Stdio",
    "Output", "Child", "ChildStdin", "ChildStdout", "ChildStderr",
];
const LIBC_PROCESS: &[&str] = &["exec", "fork", "vfork", "system", "posix_spawn", "popen"];
const LIBC_NET: &[&str] = &["socket", "connect", "bind", "listen", "accept"];
const NET_CRATES: &[&str] = &[
    "reqwest", "ureq", "hyper", "isahc", "surf", "attohttpc", "curl", "socket2", "tungstenite", "quinn",
];

const TABLE: &[Entry] = &[
    Entry { rule: "process", prefix: &["std", "process"], exempt: PROCESS_EXEMPT, only: &[] },
    Entry { rule: "process", prefix: &["std", "os", "unix", "process"], exempt: &[], only: &[] },
    Entry { rule: "process", prefix: &["std", "os", "windows", "process"], exempt: &[], only: &[] },
    Entry { rule: "process", prefix: &["tokio", "process"], exempt: &[], only: &[] },
    Entry { rule: "process", prefix: &["async_std", "process"], exempt: &[], only: &[] },
    Entry { rule: "process", prefix: &["smol", "process"], exempt: &[], only: &[] },
    Entry { rule: "process", prefix: &["async_process"], exempt: &[], only: &[] },
    Entry { rule: "process", prefix: &["libc"], exempt: &[], only: LIBC_PROCESS },
    Entry { rule: "process", prefix: &["nix", "unistd"], exempt: &[], only: &["fork", "exec"] },
    Entry { rule: "net", prefix: &["std", "net"], exempt: ADDR_TYPES, only: &[] },
    Entry { rule: "net", prefix: &["tokio", "net"], exempt: &[], only: &[] },
    Entry { rule: "net", prefix: &["async_std", "net"], exempt: &[], only: &[] },
    Entry { rule: "net", prefix: &["mio", "net"], exempt: &[], only: &[] },
    Entry { rule: "net", prefix: &["libc"], exempt: &[], only: LIBC_NET },
    Entry { rule: "include", prefix: &["std", "include"], exempt: &[], only: &[] },
    Entry { rule: "ffi", prefix: &["std", "arch", "global_asm"], exempt: &[], only: &[] },
    Entry { rule: "ffi", prefix: &["std", "arch", "naked_asm"], exempt: &[], only: &[] },
    Entry { rule: "ffi", prefix: &["std", "arch", "asm"], exempt: &[], only: &[] },
];

/// Sensitive rules for one resolved path. A trailing `*` segment is a glob import: it is
/// sensitive when it imports a sensitive module or any ancestor of one.
pub fn classify(p: &Path) -> Vec<&'static str> {
    let mut segs: Vec<&str> = p.segs.iter().map(|s| s.as_str()).collect();
    if segs.is_empty() || is_local_root(segs[0]) {
        return Vec::new();
    }
    segs[0] = norm_root(segs[0]);
    let glob = segs.last() == Some(&"*");
    let body = if glob { &segs[..segs.len() - 1] } else { &segs[..] };
    let mut out = Vec::new();
    for e in TABLE {
        if hit(e, body, glob) && !out.contains(&e.rule) {
            out.push(e.rule);
        }
    }
    if let Some(first) = body.first() {
        if NET_CRATES.contains(first) && !out.contains(&"net") {
            out.push("net");
        }
    }
    out
}

fn hit(e: &Entry, body: &[&str], glob: bool) -> bool {
    let n = e.prefix.len();
    if body.len() >= n && body[..n] == *e.prefix {
        match body.get(n) {
            None => e.only.is_empty() || glob,
            Some(next) => {
                if !e.only.is_empty() {
                    e.only.iter().any(|o| next.starts_with(o))
                } else {
                    !e.exempt.contains(next)
                }
            }
        }
    } else {
        // `use std::*;` or `use std::os::*;` brings the sensitive module into scope.
        glob && body.len() < n && e.prefix[..body.len()] == *body
    }
}

/// Name-based checks that need no resolution: kept so that a file whose imports we cannot
/// see (another module, `use super::*`) is still checked the way the line gate checks it.
pub fn by_name(segs: &[String]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (i, s) in segs.iter().enumerate() {
        let s = s.as_str();
        if matches!(s, "TcpStream" | "TcpListener" | "TcpSocket" | "UdpSocket") && !out.contains(&"net") {
            out.push("net");
        }
        if s == "CommandExt" && !out.contains(&"process") {
            out.push("process");
        }
        if s == "Command" && segs.get(i + 1).map(|n| n == "new").unwrap_or(false) && !out.contains(&"process") {
            out.push("process");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(s: &str) -> Path {
        Path::new(false, s.split("::").map(String::from).collect())
    }
    #[test]
    fn table() {
        assert_eq!(classify(&p("std::process::Command")), vec!["process"]);
        assert!(classify(&p("std::process::exit")).is_empty());
        assert_eq!(classify(&p("std::process")), vec!["process"]);
        assert_eq!(classify(&p("std::*")), vec!["process", "net", "include", "ffi"]);
        assert!(classify(&p("std::net::SocketAddr")).is_empty());
        assert_eq!(classify(&p("core::net::TcpStream")), vec!["net"]);
        assert_eq!(classify(&p("libc::execvp")), vec!["process"]);
        assert_eq!(classify(&p("libc::*")), vec!["process", "net"]);
        assert!(classify(&p("libc::c_int")).is_empty());
        assert_eq!(classify(&p("reqwest")), vec!["net"]);
        assert!(classify(&p("crate::process::Command")).is_empty());
    }
    #[test]
    fn expansion() {
        let mut a = Aliases::default();
        a.bind("p", p("std::process"));
        a.bind("q", p("p"));
        let c = a.expand(&p("q::Command"));
        assert!(c.contains(&(p("std::process::Command"), true)));
        let mut g = Aliases::default();
        g.glob(p("std::process"));
        assert!(g.expand(&p("Command")).contains(&(p("std::process::Command"), true)));
        let mut t = Aliases::default();
        t.bind("StatusCode", p("hyper::StatusCode"));
        assert!(t.expand(&p("StatusCode::OK")).contains(&(p("hyper::StatusCode::OK"), false)));
    }
}
