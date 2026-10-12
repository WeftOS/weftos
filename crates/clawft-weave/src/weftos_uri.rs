//! `weftos://` names (ADR-114; strictness rules from rvm ADR-157).
//!
//! ```text
//! weftos://<authority>/<kind>/<id>[/<segment>...][?rev=sha256:<64 hex>[&view=abstract|overview|content]]
//! ```
//!
//! A name identifies a thing on a mesh, never a location: the authority is
//! the **mesh**, its `MeshId` as 64 lowercase hex ([`Authority`]) and nothing
//! else. Friendly mesh names are dashboard display labels; they never appear
//! in a name that is stored, chained, granted or sent (owner decision,
//! 2026-10-09). One thing has one name; relationships are data, not path.
//!
//! Parsing is segment by segment and refuses everything the grammar does not
//! name: any `%` (no percent-encoding), non-ASCII, a fragment, userinfo, a
//! port, empty, trailing, `.` and `..` segments, uppercase in the scheme, the
//! structural tokens or the rev hex, unknown or repeated query keys, and a
//! query out of order. Parse then [`Display`] reproduces the input exactly;
//! nothing is normalised.
//!
//! Kinds (v1): companies, projects, goals, tickets, installations, members,
//! nodes, hosts, services, cogs, teams, agents, instances, memory, sensors. Only
//! `projects/<ULID>` (the project's root repository) and
//! `projects/<ULID>/repos/<dir>` (a sibling repository) have a resolver today
//! ([`WeftosUri::project_repo`]); nodes are
//! `nodes/<node id>[/services/<name>]`.

use std::fmt;

/// The scheme, lowercase only.
pub const SCHEME: &str = "weftos://";
const MAX_SEGMENT: usize = 128;
/// Segments after the id (ADR-114 §2).
const MAX_SEGMENTS: usize = 32;
/// The whole name, in bytes (ADR-114 §2).
const MAX_TOTAL: usize = 2048;

/// Whose name space the name lives in: a mesh, by its `MeshId` (64 lowercase
/// hex on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Authority(pub [u8; 32]);

impl Authority {
    fn parse(s: &str) -> Result<Self, UriError> {
        if !hex64(s) {
            return Err(UriError::Authority);
        }
        let mut id = [0u8; 32];
        hex::decode_to_slice(s, &mut id).map_err(|_| UriError::Authority)?;
        Ok(Self(id))
    }
}

impl fmt::Display for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// What kind of thing a name points at (v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Companies,
    Projects,
    Goals,
    Tickets,
    Installations,
    Members,
    Nodes,
    Hosts,
    Services,
    Cogs,
    Teams,
    Agents,
    Instances,
    Memory,
    Sensors,
}

impl Kind {
    /// Every kind with its path token.
    pub const ALL: [(Kind, &'static str); 15] = [
        (Kind::Companies, "companies"),
        (Kind::Projects, "projects"),
        (Kind::Goals, "goals"),
        (Kind::Tickets, "tickets"),
        (Kind::Installations, "installations"),
        (Kind::Members, "members"),
        (Kind::Nodes, "nodes"),
        (Kind::Hosts, "hosts"),
        (Kind::Services, "services"),
        (Kind::Cogs, "cogs"),
        (Kind::Teams, "teams"),
        (Kind::Agents, "agents"),
        (Kind::Instances, "instances"),
        (Kind::Memory, "memory"),
        (Kind::Sensors, "sensors"),
    ];

    /// The path token.
    pub fn as_str(self) -> &'static str {
        Self::ALL.iter().find(|(k, _)| *k == self).map(|(_, s)| *s).unwrap_or("")
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|(_, t)| *t == s).map(|(k, _)| *k)
    }
}

/// How much of a thing a reader wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Abstract,
    Overview,
    Content,
}

impl View {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abstract => "abstract",
            Self::Overview => "overview",
            Self::Content => "content",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "abstract" => Some(Self::Abstract),
            "overview" => Some(Self::Overview),
            "content" => Some(Self::Content),
            _ => None,
        }
    }
}

/// Why a name was refused. The texts name the rule, never the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UriError {
    #[error("not a weftos:// name")]
    Scheme,
    #[error("name has characters outside its grammar")]
    Characters,
    #[error("name has a fragment, userinfo or port")]
    Structure,
    #[error("authority is not a mesh id (64 lowercase hex)")]
    Authority,
    #[error("unknown kind")]
    Kind,
    #[error("a path segment is empty, '.', '..', too long or has a character outside [A-Za-z0-9._~-]; or too many segments or bytes")]
    Segment,
    #[error("query must be rev=sha256:<64 lowercase hex>, then view=abstract|overview|content")]
    Query,
}

/// A parsed name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeftosUri {
    pub authority: Authority,
    pub kind: Kind,
    pub id: String,
    /// Segments after the id, in order.
    pub path: Vec<String>,
    /// `rev=sha256:<hex>`: the 64 lowercase hex.
    pub rev: Option<String>,
    pub view: Option<View>,
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.len() <= MAX_SEGMENT
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
}

fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl WeftosUri {
    /// Parse strictly; see the module docs for what is refused.
    pub fn parse(input: &str) -> Result<Self, UriError> {
        if input.len() > MAX_TOTAL {
            return Err(UriError::Segment);
        }
        if !input.is_ascii() || input.contains('%') || input.bytes().any(|b| b.is_ascii_control() || b == b' ') {
            return Err(UriError::Characters);
        }
        let rest = input.strip_prefix(SCHEME).ok_or(UriError::Scheme)?;
        if rest.contains('#') {
            return Err(UriError::Structure);
        }
        let (path_part, query) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };
        let (authority, path) = path_part.split_once('/').ok_or(UriError::Segment)?;
        if authority.contains('@') || authority.contains(':') {
            return Err(UriError::Structure);
        }
        let authority = Authority::parse(authority)?;
        let mut segs = path.split('/');
        let kind = Kind::parse(segs.next().unwrap_or("")).ok_or(UriError::Kind)?;
        let id = segs.next().filter(|s| segment_ok(s)).ok_or(UriError::Segment)?.to_owned();
        let mut rest_segs = Vec::new();
        for s in segs {
            if !segment_ok(s) || rest_segs.len() >= MAX_SEGMENTS {
                return Err(UriError::Segment);
            }
            rest_segs.push(s.to_owned());
        }
        let (rev, view) = match query {
            None => (None, None),
            Some(q) => Self::parse_query(q)?,
        };
        Ok(Self { authority, kind, id, path: rest_segs, rev, view })
    }

    fn parse_query(q: &str) -> Result<(Option<String>, Option<View>), UriError> {
        let mut parts = q.split('&');
        let first = parts.next().ok_or(UriError::Query)?;
        let rev = first.strip_prefix("rev=sha256:").filter(|h| hex64(h)).ok_or(UriError::Query)?.to_owned();
        let view = match parts.next() {
            None => None,
            Some(v) => Some(v.strip_prefix("view=").and_then(View::parse).ok_or(UriError::Query)?),
        };
        if parts.next().is_some() {
            return Err(UriError::Query);
        }
        Ok((Some(rev), view))
    }

    /// `(project ULID, repository dir)` for a project repository name:
    /// `projects/<ULID>` is the root repository (`.`), `projects/<ULID>/repos/<dir>`
    /// a sibling. Anything else is `None`.
    pub fn project_repo(&self) -> Option<(&str, &str)> {
        if self.kind != Kind::Projects || clawft_types::project::validate_id(&self.id).is_err() {
            return None;
        }
        match self.path.as_slice() {
            [] => Some((&self.id, ".")),
            [repos, dir] if repos == "repos" && crate::project_install::dir_ok(dir) && dir != "." => Some((&self.id, dir)),
            _ => None,
        }
    }

    /// The name of a project repository on `mesh`.
    pub fn for_project_repo(mesh: Authority, ulid: &str, dir: &str) -> Self {
        let path = if dir == "." { Vec::new() } else { vec!["repos".to_owned(), dir.to_owned()] };
        Self { authority: mesh, kind: Kind::Projects, id: ulid.to_owned(), path, rev: None, view: None }
    }
}

impl fmt::Display for WeftosUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}{}/{}/{}", self.authority, self.kind.as_str(), self.id)?;
        for s in &self.path {
            write!(f, "/{s}")?;
        }
        if let Some(r) = &self.rev {
            write!(f, "?rev=sha256:{r}")?;
            if let Some(v) = self.view {
                write!(f, "&view={}", v.as_str())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "weftos_uri_tests.rs"]
mod tests;
