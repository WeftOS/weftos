//! `weft://<node>/<user>/<project>[/<topic...>]` addresses (plan 1.6).
//!
//! `node` is 32 lowercase hex or `local`; `user` is 32 lowercase hex (a user
//! key id) or `_` (node level, no tenant); `project` is a 26 character
//! Crockford ULID or `_` (user level); `topic` is an optional dotted or
//! slashed topic. A project requires a user. `local` is only meaningful at
//! the service and must never appear between nodes ([`WeftAddr::require_wire`]).

use std::fmt;
use std::str::FromStr;

use crate::hexser;

/// URL scheme prefix every address must start with.
pub const SCHEME: &str = "weft://";
/// Upper bound on the whole address string.
pub const MAX_LEN: usize = 512;

/// Why an address string or [`WeftAddr`] failed parsing or validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddrError {
    #[error("address must start with weft://")]
    MissingScheme,
    #[error("address longer than {MAX_LEN} bytes")]
    TooLong,
    #[error("address contains a disallowed character {0:?}")]
    BadChar(char),
    #[error("address must be weft://<node>/<user>/<project>[/<topic>]")]
    Incomplete,
    #[error("node must be 32 lowercase hex characters or `local`")]
    BadNode,
    #[error("user must be 32 lowercase hex characters or `_`")]
    BadUser,
    #[error("project must be a 26 character uppercase Crockford ULID or `_`")]
    BadProject,
    #[error("a project requires a user")]
    ProjectWithoutUser,
    #[error("invalid topic: {0}")]
    BadTopic(&'static str),
    #[error("`local` node is not valid on the wire")]
    LocalOnWire,
}

/// The node part of an address: the sender's own node or a node id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Node {
    /// The sender's own registered node, resolved at the service.
    Local,
    /// A node id (32 lowercase hex).
    Id(String),
}

/// A parsed `weft://<node>/<user>/<project>[/<topic>]` address.
///
/// Use [`WeftAddr::new`] or `FromStr` to get a validated value; fields are
/// public, so call [`WeftAddr::validate`] after building one by hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WeftAddr {
    pub node: Node,
    /// `None` is the node-level `_`.
    pub user: Option<String>,
    /// `None` is the user-level `_`.
    pub project: Option<String>,
    /// Empty when absent.
    pub topic: String,
}

fn is_hex32(s: &str) -> bool {
    hexser::decode::<16>(s).is_some()
}

/// 26 chars of Crockford base32 (no I, L, O, U), uppercase; the first char is
/// at most `7` because a ULID is 128 bits.
fn is_ulid(s: &str) -> bool {
    const ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    s.len() == 26
        && s.chars().all(|c| ALPHABET.contains(c))
        && s.as_bytes()[0] <= b'7'
}

fn validate_topic(t: &str) -> Result<(), AddrError> {
    if t.is_empty() {
        return Ok(());
    }
    for seg in t.split('/') {
        if seg.is_empty() {
            return Err(AddrError::BadTopic("empty segment"));
        }
        if seg == "." || seg == ".." {
            return Err(AddrError::BadTopic("dot segment"));
        }
        if seg.starts_with('.') || seg.ends_with('.') || seg.contains("..") {
            return Err(AddrError::BadTopic("empty dotted component"));
        }
    }
    Ok(())
}

impl WeftAddr {
    /// Build a validated address.
    pub fn new(
        node: Node,
        user: Option<String>,
        project: Option<String>,
        topic: impl Into<String>,
    ) -> Result<Self, AddrError> {
        let a = Self { node, user, project, topic: topic.into() };
        a.validate()?;
        Ok(a)
    }

    /// Check the address against the format rules.
    ///
    /// A node id and a user must be 32 lowercase hex characters, a project
    /// must be a 26 character ULID and requires a user, the topic must have
    /// no empty or dot segments, and the rendered address must fit in
    /// [`MAX_LEN`] bytes.
    pub fn validate(&self) -> Result<(), AddrError> {
        if let Node::Id(n) = &self.node
            && !is_hex32(n)
        {
            return Err(AddrError::BadNode);
        }
        if let Some(u) = &self.user
            && !is_hex32(u)
        {
            return Err(AddrError::BadUser);
        }
        if let Some(p) = &self.project {
            if self.user.is_none() {
                return Err(AddrError::ProjectWithoutUser);
            }
            if !is_ulid(p) {
                return Err(AddrError::BadProject);
            }
        }
        validate_topic(&self.topic)?;
        if self.to_string().len() > MAX_LEN {
            return Err(AddrError::TooLong);
        }
        Ok(())
    }

    /// Reject `local`: it may not cross between nodes.
    pub fn require_wire(&self) -> Result<(), AddrError> {
        match self.node {
            Node::Local => Err(AddrError::LocalOnWire),
            Node::Id(_) => Ok(()),
        }
    }

    /// Replace `local` with the sender's registered node id.
    pub fn resolve_local(&self, node_id: &str) -> Result<Self, AddrError> {
        let mut a = self.clone();
        if a.node == Node::Local {
            a.node = Node::Id(node_id.to_string());
        }
        a.validate()?;
        Ok(a)
    }
}

impl FromStr for WeftAddr {
    type Err = AddrError;

    fn from_str(s: &str) -> Result<Self, AddrError> {
        let rest = s.strip_prefix(SCHEME).ok_or(AddrError::MissingScheme)?;
        if s.len() > MAX_LEN {
            return Err(AddrError::TooLong);
        }
        if let Some(c) = rest
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
        {
            return Err(AddrError::BadChar(c));
        }
        let mut it = rest.splitn(4, '/');
        let (node, user, project) = match (it.next(), it.next(), it.next()) {
            (Some(n), Some(u), Some(p)) => (n, u, p),
            _ => return Err(AddrError::Incomplete),
        };
        // `Some("")` is a trailing slash, which is not canonical.
        let topic = it.next();
        let node = if node == "local" {
            Node::Local
        } else if is_hex32(node) {
            Node::Id(node.to_string())
        } else {
            return Err(AddrError::BadNode);
        };
        let user = match user {
            "_" => None,
            u if is_hex32(u) => Some(u.to_string()),
            _ => return Err(AddrError::BadUser),
        };
        let project = match project {
            "_" => None,
            p if is_ulid(p) => Some(p.to_string()),
            _ => return Err(AddrError::BadProject),
        };
        if topic == Some("") {
            return Err(AddrError::BadTopic("empty segment"));
        }
        WeftAddr::new(node, user, project, topic.unwrap_or(""))
    }
}

impl fmt::Display for WeftAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let node = match &self.node {
            Node::Local => "local",
            Node::Id(n) => n,
        };
        write!(
            f,
            "{SCHEME}{node}/{}/{}",
            self.user.as_deref().unwrap_or("_"),
            self.project.as_deref().unwrap_or("_")
        )?;
        if !self.topic.is_empty() {
            write!(f, "/{}", self.topic)?;
        }
        Ok(())
    }
}
