//! Principals, grants and the table that answers "may this principal do this" (ADR-126 section 3).

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::name::StoreName;
use crate::StoreError;

/// What a grant allows. Each level includes the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Get, head, list.
    Read,
    /// Read, plus put, copy, delete.
    Write,
    /// Write, plus managing the store's grants and deleting the store.
    Admin,
}

impl Access {
    /// Parse `read`, `write` or `admin`.
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "read" | "r" => Ok(Self::Read),
            "write" | "rw" | "w" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            o => Err(StoreError::BadName(format!("unknown access {o:?} (read, write, admin)"))),
        }
    }

    /// Lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

/// One operation on a store, and the access it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// GetObject.
    Get,
    /// HeadObject.
    Head,
    /// List objects.
    List,
    /// PutObject.
    Put,
    /// CopyObject (a rename is copy plus delete).
    Copy,
    /// DeleteObject.
    Delete,
}

impl Op {
    /// The access needed.
    pub fn needs(self) -> Access {
        match self {
            Self::Get | Self::Head | Self::List => Access::Read,
            Self::Put | Self::Copy | Self::Delete => Access::Write,
        }
    }

    /// The S3 action name, for logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Get => "GetObject",
            Self::Head => "HeadObject",
            Self::List => "ListObjects",
            Self::Put => "PutObject",
            Self::Copy => "CopyObject",
            Self::Delete => "DeleteObject",
        }
    }
}

/// What kind of thing holds a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    /// A paired node.
    Node,
    /// A person.
    Member,
    /// An agent instance.
    Agent,
    /// A cog instance.
    Cog,
}

/// Who a grant is for: `node:<id>`, `member:<id>`, `agent:<id>` or `cog:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub id: String,
}

impl Principal {
    /// Build one; the id must be a plain token.
    pub fn new(kind: PrincipalKind, id: &str) -> Result<Self, StoreError> {
        if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@')) {
            return Err(StoreError::BadName(format!("principal id {id:?}: letters, digits, '.', '_', '-' and '@' only")));
        }
        Ok(Self { kind, id: id.to_owned() })
    }

    /// Parse `kind:id`.
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        let (k, id) = s.split_once(':').ok_or_else(|| StoreError::BadName(format!("principal {s:?}: expected node:<id>, member:<id>, agent:<id> or cog:<id>")))?;
        let kind = match k {
            "node" => PrincipalKind::Node,
            "member" => PrincipalKind::Member,
            "agent" => PrincipalKind::Agent,
            "cog" => PrincipalKind::Cog,
            o => return Err(StoreError::BadName(format!("unknown principal kind {o:?}"))),
        };
        Self::new(kind, id)
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let k = match self.kind {
            PrincipalKind::Node => "node",
            PrincipalKind::Member => "member",
            PrincipalKind::Agent => "agent",
            PrincipalKind::Cog => "cog",
        };
        write!(f, "{k}:{}", self.id)
    }
}

/// One grant: (principal, store, access), switchable without losing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub principal: Principal,
    /// The store's `weftos://` name.
    pub store: String,
    pub access: Access,
    /// A disabled grant is kept and allows nothing.
    pub enabled: bool,
    /// Who made the grant (`default`, or an operator).
    pub by: String,
    pub ts_ms: u64,
}

/// All grants of this daemon's stores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GrantTable {
    grants: Vec<Grant>,
}

impl GrantTable {
    /// Add or replace the grant for (principal, store). A new grant starts enabled; replacing one keeps its enabled flag.
    pub fn grant(&mut self, principal: Principal, store: &StoreName, access: Access, by: &str, ts_ms: u64) -> Grant {
        let store = store.to_string();
        let mut g = Grant { principal, store, access, enabled: true, by: by.to_owned(), ts_ms };
        match self.grants.iter_mut().find(|x| x.principal == g.principal && x.store == g.store) {
            Some(x) => {
                // Changing the level of a switched-off grant does not switch it back on.
                g.enabled = x.enabled;
                *x = g.clone();
            }
            None => self.grants.push(g.clone()),
        }
        g
    }

    /// Remove a grant. True if there was one.
    pub fn revoke(&mut self, principal: &Principal, store: &StoreName) -> bool {
        let s = store.to_string();
        let n = self.grants.len();
        self.grants.retain(|x| !(x.principal == *principal && x.store == s));
        self.grants.len() != n
    }

    /// Enable or disable one grant. True if there was one.
    pub fn set_enabled(&mut self, principal: &Principal, store: &StoreName, enabled: bool) -> bool {
        let s = store.to_string();
        self.grants.iter_mut().find(|x| x.principal == *principal && x.store == s).map(|x| x.enabled = enabled).is_some()
    }

    /// Enable or disable every grant of a principal (a rule hit). The number changed.
    pub fn set_principal_enabled(&mut self, principal: &Principal, enabled: bool) -> usize {
        let mut n = 0;
        for g in self.grants.iter_mut().filter(|g| g.principal == *principal && g.enabled != enabled) {
            g.enabled = enabled;
            n += 1;
        }
        n
    }

    /// The access this principal holds on the store now (enabled grants only).
    pub fn effective(&self, principal: &Principal, store: &StoreName) -> Option<Access> {
        let s = store.to_string();
        self.grants.iter().find(|g| g.enabled && g.principal == *principal && g.store == s).map(|g| g.access)
    }

    /// Every grant of a store.
    pub fn for_store(&self, store: &StoreName) -> Vec<Grant> {
        let s = store.to_string();
        self.grants.iter().filter(|g| g.store == s).cloned().collect()
    }

    /// Every grant of a principal.
    pub fn for_principal(&self, p: &Principal) -> Vec<Grant> {
        self.grants.iter().filter(|g| g.principal == *p).cloned().collect()
    }

    /// Drop every grant of a store (the store is gone).
    pub fn drop_store(&mut self, store: &StoreName) {
        let s = store.to_string();
        self.grants.retain(|g| g.store != s);
    }

    /// All grants.
    pub fn all(&self) -> &[Grant] {
        &self.grants
    }

    /// Read from `path`; a missing file is an empty table, a broken one is an error (never read as empty).
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| StoreError::State(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(StoreError::State(format!("{}: {e}", path.display()))),
        }
    }

    /// Write atomically (temp file, then rename).
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        crate::fs::write_atomic(path, &serde_json::to_vec_pretty(self).map_err(|e| StoreError::State(e.to_string()))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> StoreName {
        StoreName::node("m", "n1").unwrap()
    }
    fn cog() -> Principal {
        Principal::parse("cog:c1").unwrap()
    }

    #[test]
    fn access_levels_include_the_lower_ones() {
        assert!(Access::Admin >= Op::Put.needs() && Access::Write >= Op::Delete.needs() && Access::Write >= Op::List.needs());
        assert!(Access::Read < Op::Put.needs());
    }

    #[test]
    fn grant_revoke_and_disable() {
        let mut t = GrantTable::default();
        t.grant(cog(), &st(), Access::Write, "op", 1);
        assert_eq!(t.effective(&cog(), &st()), Some(Access::Write));
        assert!(t.set_enabled(&cog(), &st(), false));
        assert_eq!(t.effective(&cog(), &st()), None, "disabled allows nothing");
        assert_eq!(t.for_store(&st()).len(), 1, "but the grant is kept");
        t.set_enabled(&cog(), &st(), true);
        t.grant(cog(), &st(), Access::Read, "op", 2);
        assert_eq!(t.all().len(), 1, "a re-grant replaces");
        assert_eq!(t.effective(&cog(), &st()), Some(Access::Read));
        assert!(t.revoke(&cog(), &st()));
        assert!(!t.revoke(&cog(), &st()));
    }

    #[test]
    fn principals_parse_and_refuse_junk() {
        assert_eq!(Principal::parse("agent:a-1").unwrap().to_string(), "agent:a-1");
        assert!(Principal::parse("a-1").is_err());
        assert!(Principal::parse("robot:x").is_err());
        assert!(Principal::parse("cog:a/b").is_err());
        assert!(Principal::parse("cog:").is_err());
    }

    #[test]
    fn table_persists_and_a_broken_file_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("grants.json");
        assert!(GrantTable::load(&p).unwrap().all().is_empty());
        let mut t = GrantTable::default();
        t.grant(cog(), &st(), Access::Admin, "op", 5);
        t.save(&p).unwrap();
        assert_eq!(GrantTable::load(&p).unwrap().effective(&cog(), &st()), Some(Access::Admin));
        std::fs::write(&p, b"{not json").unwrap();
        assert!(GrantTable::load(&p).is_err());
    }
}
