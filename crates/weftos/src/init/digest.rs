//! Bundle digests (ADR-121): the sha256 that pins one agent package, or one
//! team, so an install can be verified before anything is written.
//!
//! A package digest is the sha256 of one line per file under `<id>/`, sorted
//! by path: `file <path relative to the package> <sha256 of the bytes>\n`.
//! The source already leaves out `evals/` and dotfiles. A team digest covers
//! the files under `teams/<team>/` the same way, then one
//! `member <id> <package digest>\n` line per active member package, sorted by id,
//! so a team pin also pins the packages it installs.

use sha2::{Digest, Sha256};

use super::InitError;
use super::lock::sha256_hex;
use super::package::team_members;
use super::source::AgentSource;

/// What a digest names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subject {
    Agent,
    Team,
}

impl Subject {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent" => Some(Subject::Agent),
            "team" => Some(Subject::Team),
            _ => None,
        }
    }
}

fn file_lines(src: &AgentSource, prefix: &str) -> String {
    let mut out = String::new();
    for (rel, bytes) in src.under(prefix) {
        out.push_str(&format!("file {rel} {}\n", sha256_hex(bytes)));
    }
    out
}

fn finish(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Digest of the package `id`. Refuses an id the source does not hold; never
/// digests an empty bundle.
pub fn package_digest(src: &AgentSource, id: &str) -> Result<String, InitError> {
    if !src.package_ids().iter().any(|p| p == id) {
        return Err(InitError::Other(format!("unknown agent package '{id}'")));
    }
    Ok(finish(&file_lines(src, id)))
}

/// Digest of the team `name` and its active member packages.
pub fn team_digest(src: &AgentSource, name: &str) -> Result<String, InitError> {
    let members = team_members(src, name)?
        .ok_or_else(|| InitError::Other(format!("unknown team '{name}'")))?;
    let known = src.package_ids();
    let mut text = file_lines(src, &format!("teams/{name}"));
    let mut ids: Vec<&String> = members.active.iter().filter(|m| known.contains(m)).collect();
    ids.sort();
    for id in ids {
        text.push_str(&format!("member {id} {}\n", package_digest(src, id)?));
    }
    Ok(finish(&text))
}

/// Digest of an agent package or a team.
pub fn bundle_digest(src: &AgentSource, subject: Subject, name: &str) -> Result<String, InitError> {
    match subject {
        Subject::Agent => package_digest(src, name),
        Subject::Team => team_digest(src, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    fn src(files: &[(&str, &str)]) -> AgentSource {
        let files: BTreeMap<String, Cow<'static, [u8]>> = files
            .iter()
            .map(|(p, b)| (p.to_string(), Cow::Owned(b.as_bytes().to_vec())))
            .collect();
        AgentSource { files, origin: "test".into(), commit: "test".into() }
    }

    const BASE: &[(&str, &str)] = &[
        ("a/weftos-package.yaml", "name: a\n"),
        ("a/AGENT.md", "one"),
        ("b/weftos-package.yaml", "name: b\n"),
        ("teams/t/team.yaml", "schema: 1\nmembers:\n  - agent: a\n  - agent: b\n    active: false\n"),
    ];

    #[test]
    fn a_digest_is_stable_and_sensitive_to_content() {
        let d1 = package_digest(&src(BASE), "a").unwrap();
        assert_eq!(d1.len(), 64);
        assert_eq!(d1, package_digest(&src(BASE), "a").unwrap());
        let mut changed = BASE.to_vec();
        changed[1] = ("a/AGENT.md", "two");
        assert_ne!(d1, package_digest(&src(&changed), "a").unwrap());
        // Another package's change does not move this one.
        let mut other = BASE.to_vec();
        other[2] = ("b/weftos-package.yaml", "name: b2\n");
        assert_eq!(d1, package_digest(&src(&other), "a").unwrap());
    }

    #[test]
    fn a_team_digest_pins_its_active_members_only() {
        let t1 = team_digest(&src(BASE), "t").unwrap();
        let mut member = BASE.to_vec();
        member[1] = ("a/AGENT.md", "two");
        assert_ne!(t1, team_digest(&src(&member), "t").unwrap(), "active member change moves the team");
        let mut inactive = BASE.to_vec();
        inactive[2] = ("b/weftos-package.yaml", "name: b2\n");
        assert_eq!(t1, team_digest(&src(&inactive), "t").unwrap(), "inactive member is not installed or pinned");
        let mut file = BASE.to_vec();
        file[3] = ("teams/t/team.yaml", "schema: 1\nmembers:\n  - agent: a\n");
        assert_ne!(t1, team_digest(&src(&file), "t").unwrap());
    }

    #[test]
    fn unknown_names_refuse_instead_of_digesting_nothing() {
        assert!(package_digest(&src(BASE), "nope").is_err());
        assert!(team_digest(&src(BASE), "nope").is_err());
        assert!(bundle_digest(&src(BASE), Subject::Agent, "teams").is_err());
        assert_eq!(Subject::parse("cog"), None);
    }
}
