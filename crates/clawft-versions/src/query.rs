//! Read-only queries over the version files.

use serde::Serialize;

use crate::{DepRef, VersionRecord, VersionsError, VersionsIndex, check_kind, check_object};

/// One version in a tree, with the versions derived from it (of this object
/// or of other objects: sub-packages and derivations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeNode {
    pub record: VersionRecord,
    pub children: Vec<TreeNode>,
}

/// What an object depends on and what depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyView {
    pub object: String,
    /// The latest version's dependencies.
    pub depends_on: Vec<DepRef>,
    /// Latest versions (any kind) that depend on this object.
    pub dependents: Vec<Dependent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dependent {
    pub kind: String,
    pub object: String,
    pub version: String,
    pub digest: String,
}

impl VersionsIndex {
    /// Versions of `object` in the order recorded. Refuses an object that was
    /// never recorded.
    pub fn history(&self, kind: &str, object: &str) -> Result<Vec<VersionRecord>, VersionsError> {
        check_kind(kind)?;
        check_object(object)?;
        let h: Vec<_> = self.list(kind)?.into_iter().filter(|r| r.object == object).collect();
        if h.is_empty() {
            return Err(VersionsError::NotFound { kind: kind.to_owned(), object: object.to_owned() });
        }
        Ok(h)
    }

    /// The version tree of `object`: its root versions (no parent, or a parent
    /// owned by another object), each with every version derived from it.
    pub fn tree(&self, kind: &str, object: &str) -> Result<Vec<TreeNode>, VersionsError> {
        let all = self.list(kind)?;
        let own: Vec<&VersionRecord> = all.iter().filter(|r| r.object == object).collect();
        if own.is_empty() {
            check_kind(kind)?;
            check_object(object)?;
            return Err(VersionsError::NotFound { kind: kind.to_owned(), object: object.to_owned() });
        }
        let owned_digests: Vec<&str> = own.iter().map(|r| r.digest.as_str()).collect();
        let roots = own.iter().filter(|r| r.parent.as_deref().is_none_or(|p| !owned_digests.contains(&p)));
        let mut seen = Vec::new();
        Ok(roots.map(|r| build(&all, r, &mut seen)).collect())
    }

    /// Forward and reverse dependencies of `object` (a package of `kind`).
    pub fn deps(&self, kind: &str, object: &str) -> Result<DependencyView, VersionsError> {
        let hist = self.history(kind, object)?;
        let latest = hist.last().expect("history is non-empty");
        let mut dependents = Vec::new();
        for k in self.kinds()? {
            let recs = self.list(&k)?;
            for r in &recs {
                let is_latest = recs.iter().rev().find(|x| x.object == r.object).is_some_and(|x| x.digest == r.digest);
                if is_latest && r.dependencies.iter().any(|d| dep_matches(d, kind, object)) {
                    dependents.push(Dependent { kind: k.clone(), object: r.object.clone(), version: r.version.clone(), digest: r.digest.clone() });
                }
            }
        }
        Ok(DependencyView { object: object.to_owned(), depends_on: latest.dependencies.clone(), dependents })
    }
}

/// A dependency names a package of kind `agents` as `agent`; other dependency
/// kinds match the index kind with the plural `s`.
fn dep_matches(d: &DepRef, kind: &str, object: &str) -> bool {
    d.name == object && (d.kind == kind || format!("{}s", d.kind) == kind)
}

fn build<'a>(all: &'a [VersionRecord], rec: &'a VersionRecord, seen: &mut Vec<&'a str>) -> TreeNode {
    seen.push(&rec.digest);
    let mut children = Vec::new();
    for c in all {
        if c.parent.as_deref() == Some(rec.digest.as_str()) && !seen.contains(&c.digest.as_str()) {
            children.push(build(all, c, seen));
        }
    }
    TreeNode { record: rec.clone(), children }
}
