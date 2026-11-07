//! Store names and the buckets derived from them (ADR-126 section 2).
//!
//! Every store has one `weftos://` name. The bucket name is derived from it,
//! never chosen: lowercase, at most 63 characters, and `h12` is the first 12
//! hex digits of BLAKE3 over the slug.

use std::fmt;

use ulid::Ulid;

use crate::StoreError;

/// Longest S3 bucket name.
pub const MAX_BUCKET: usize = 63;

const MAX_ID: usize = 63 - 2;
const MAX_SLUG: usize = 128;

/// The owner of a store, and so its name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StoreName {
    /// `weftos://<mesh>/nodes/<node id>/store`
    Node { mesh: String, node: String },
    /// `weftos://<mesh>/projects/<ULID>/store`
    Project { mesh: String, project: Ulid },
    /// `weftos://<mesh>/projects/<ULID>/worktrees/<slug>/store`
    Worktree { mesh: String, project: Ulid, slug: String },
    /// `weftos://<mesh>/projects/<ULID>/releases/<channel>/store`
    Release { mesh: String, project: Ulid, channel: String },
}

fn ident_ok(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn h12(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex()[..12].to_owned()
}

impl StoreName {
    /// A node store. The node id must be lowercase `[a-z0-9-]`, so two ids
    /// can never map to one bucket.
    pub fn node(mesh: &str, node: &str) -> Result<Self, StoreError> {
        check_mesh(mesh)?;
        if node.is_empty() || node.len() > MAX_ID || !node.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err(StoreError::BadName(format!("node id {node:?}: lowercase letters, digits and '-' only, at most {MAX_ID} characters")));
        }
        Ok(Self::Node { mesh: mesh.into(), node: node.into() })
    }

    /// A project store.
    pub fn project(mesh: &str, project: &str) -> Result<Self, StoreError> {
        check_mesh(mesh)?;
        Ok(Self::Project { mesh: mesh.into(), project: parse_ulid(project)? })
    }

    /// A worktree store.
    pub fn worktree(mesh: &str, project: &str, slug: &str) -> Result<Self, StoreError> {
        check_mesh(mesh)?;
        check_slug("worktree slug", slug)?;
        Ok(Self::Worktree { mesh: mesh.into(), project: parse_ulid(project)?, slug: slug.into() })
    }

    /// A release store.
    pub fn release(mesh: &str, project: &str, channel: &str) -> Result<Self, StoreError> {
        check_mesh(mesh)?;
        check_slug("release channel", channel)?;
        Ok(Self::Release { mesh: mesh.into(), project: parse_ulid(project)?, channel: channel.into() })
    }

    /// The mesh this store belongs to.
    pub fn mesh(&self) -> &str {
        match self {
            Self::Node { mesh, .. } | Self::Project { mesh, .. } | Self::Worktree { mesh, .. } | Self::Release { mesh, .. } => mesh,
        }
    }

    /// The project store a worktree or release store belongs to.
    pub fn project_store(&self) -> Option<StoreName> {
        match self {
            Self::Worktree { mesh, project, .. } | Self::Release { mesh, project, .. } | Self::Project { mesh, project } => {
                Some(Self::Project { mesh: mesh.clone(), project: *project })
            }
            Self::Node { .. } => None,
        }
    }

    /// `node`, `project`, `worktree` or `release`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Node { .. } => "node",
            Self::Project { .. } => "project",
            Self::Worktree { .. } => "worktree",
            Self::Release { .. } => "release",
        }
    }

    /// The S3 bucket. Derived, lowercase, at most [`MAX_BUCKET`] characters.
    pub fn bucket(&self) -> String {
        let b = match self {
            Self::Node { node, .. } => format!("n-{node}"),
            Self::Project { project, .. } => format!("p-{}", project.to_string().to_lowercase()),
            Self::Worktree { project, slug, .. } => format!("w-{}-{}", project.to_string().to_lowercase(), h12(slug)),
            Self::Release { project, channel, .. } => format!("r-{}-{}", project.to_string().to_lowercase(), h12(channel)),
        };
        debug_assert!(b.len() <= MAX_BUCKET);
        b
    }

    /// The readable parts kept as bucket tags (slug, owner, kind).
    pub fn tags(&self) -> Vec<(String, String)> {
        let mut t = vec![("name".to_owned(), self.to_string()), ("kind".to_owned(), self.kind().to_owned())];
        match self {
            Self::Node { node, .. } => t.push(("owner".into(), format!("node:{node}"))),
            Self::Project { project, .. } => t.push(("owner".into(), format!("project:{project}"))),
            Self::Worktree { project, slug, .. } => {
                t.push(("owner".into(), format!("project:{project}")));
                t.push(("slug".into(), slug.clone()));
            }
            Self::Release { project, channel, .. } => {
                t.push(("owner".into(), format!("project:{project}")));
                t.push(("slug".into(), channel.clone()));
            }
        }
        t
    }

    /// Parse a `weftos://` store name.
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        let bad = || StoreError::BadName(format!("{s:?} is not a store name (weftos://<mesh>/nodes/<id>/store, .../projects/<ULID>/store, .../worktrees/<slug>/store, .../releases/<channel>/store)"));
        let rest = s.strip_prefix("weftos://").ok_or_else(bad)?;
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            [mesh, "nodes", id, "store"] => Self::node(mesh, id),
            [mesh, "projects", p, "store"] => Self::project(mesh, p),
            [mesh, "projects", p, "worktrees", slug, "store"] => Self::worktree(mesh, p, slug),
            [mesh, "projects", p, "releases", ch, "store"] => Self::release(mesh, p, ch),
            _ => Err(bad()),
        }
    }
}

fn check_mesh(m: &str) -> Result<(), StoreError> {
    if ident_ok(m, 64) { Ok(()) } else { Err(StoreError::BadName(format!("mesh {m:?}: letters, digits, '.', '_' and '-' only"))) }
}

fn check_slug(what: &str, s: &str) -> Result<(), StoreError> {
    if ident_ok(s, MAX_SLUG) { Ok(()) } else { Err(StoreError::BadName(format!("{what} {s:?}: letters, digits, '.', '_' and '-' only, at most {MAX_SLUG} characters"))) }
}

fn parse_ulid(p: &str) -> Result<Ulid, StoreError> {
    Ulid::from_string(p).map_err(|e| StoreError::BadName(format!("project id {p:?} is not a ULID: {e}")))
}

impl fmt::Display for StoreName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Node { mesh, node } => write!(f, "weftos://{mesh}/nodes/{node}/store"),
            Self::Project { mesh, project } => write!(f, "weftos://{mesh}/projects/{project}/store"),
            Self::Worktree { mesh, project, slug } => write!(f, "weftos://{mesh}/projects/{project}/worktrees/{slug}/store"),
            Self::Release { mesh, project, channel } => write!(f, "weftos://{mesh}/projects/{project}/releases/{channel}/store"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";

    #[test]
    fn names_round_trip_and_map_to_the_documented_buckets() {
        let n = StoreName::node("weft", "pg-1").unwrap();
        assert_eq!(n.to_string(), "weftos://weft/nodes/pg-1/store");
        assert_eq!(n.bucket(), "n-pg-1");
        let p = StoreName::project("weft", P).unwrap();
        assert_eq!(p.bucket(), format!("p-{}", P.to_lowercase()));
        let w = StoreName::worktree("weft", P, "feat-x").unwrap();
        assert!(w.bucket().starts_with(&format!("w-{}-", P.to_lowercase())), "{}", w.bucket());
        assert_eq!(w.bucket().len(), 2 + 26 + 1 + 12);
        let r = StoreName::release("weft", P, "stable").unwrap();
        assert!(r.bucket().starts_with("r-"));
        for name in [n, p, w, r] {
            assert_eq!(StoreName::parse(&name.to_string()).unwrap(), name);
            assert!(name.bucket().len() <= MAX_BUCKET && name.bucket() == name.bucket().to_lowercase());
        }
    }

    #[test]
    fn h12_is_the_first_12_hex_of_blake3_of_the_slug() {
        let w = StoreName::worktree("m", P, "feat-x").unwrap();
        let expect = &blake3::hash(b"feat-x").to_hex()[..12];
        assert!(w.bucket().ends_with(expect));
        let other = StoreName::worktree("m", P, "feat-y").unwrap();
        assert_ne!(w.bucket(), other.bucket());
    }

    #[test]
    fn lowercase_project_id_is_the_same_store() {
        let a = StoreName::project("m", P).unwrap();
        let b = StoreName::project("m", &P.to_lowercase()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.bucket(), b.bucket());
    }

    #[test]
    fn bad_names_are_refused() {
        assert!(StoreName::node("m", "Pg-1").is_err(), "uppercase would collide");
        assert!(StoreName::node("m", "a/b").is_err());
        assert!(StoreName::node("m", &"a".repeat(62)).is_err());
        assert!(StoreName::project("m", "not-a-ulid").is_err());
        assert!(StoreName::worktree("m", P, "../x").is_err());
        assert!(StoreName::worktree("m", P, "").is_err());
        assert!(StoreName::node("a/b", "x").is_err());
        for bad in ["http://x/nodes/a/store", "weftos://m/nodes/a", "weftos://m/nodes/a/store/extra", "weftos://m/projects/x/store"] {
            assert!(StoreName::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn project_store_of_a_worktree_is_its_project() {
        let w = StoreName::worktree("m", P, "s").unwrap();
        assert_eq!(w.project_store().unwrap(), StoreName::project("m", P).unwrap());
        assert!(StoreName::node("m", "n").unwrap().project_store().is_none());
    }
}
