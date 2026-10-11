//! Grants and arbitration seams (ADR-117 §5). D2 fills these in with
//! ADR-108 per-resource grants and the firmware system strip. D1 only
//! fixes the types the session carries, so lane B code written now
//! does not change shape when D2 lands.

use crate::error::DisplayError;

/// What an app presents to open a session. In D1 the content is not
/// checked by anything except the [`GrantGate`] given to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayGrant {
    /// Who is asking, as the console names itself (`"cos-console"`).
    pub subject: String,
    /// The display resource, as an ADR-114 name when known
    /// (`weftos://<mesh>/nodes/<leaf>/services/display`). The sim
    /// backends accept any string.
    pub resource: String,
    /// Opaque proof for the leaf (an ADR-108 grant chain in D2). Empty
    /// for development sessions.
    pub proof: Vec<u8>,
}

impl DisplayGrant {
    /// A development grant for the simulator backends.
    pub fn dev(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            resource: "weftos://local/sim/display".to_string(),
            proof: Vec::new(),
        }
    }
}

/// The admission seam. The session asks the gate once at `open` and
/// again on every `apply`, which is where D2's revocation ("ends the
/// session on the next op") plugs in.
pub trait GrantGate: Send {
    /// May this grant open a session on this display now?
    fn admit(&mut self, grant: &DisplayGrant) -> Result<(), DisplayError>;

    /// Is the grant still valid for another op? Default: yes.
    fn still_valid(&mut self, _grant: &DisplayGrant) -> Result<(), DisplayError> {
        Ok(())
    }
}

/// D1 gate: everything is admitted and nothing is revoked.
#[derive(Debug, Default, Clone, Copy)]
pub struct AdmitAll;

impl GrantGate for AdmitAll {
    fn admit(&mut self, _grant: &DisplayGrant) -> Result<(), DisplayError> {
        Ok(())
    }
}

/// Who holds the screen and who is queued behind them. The leaf will
/// report this in its `display` service record; D1 sim backends report
/// the one open session as the holder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Arbitration {
    pub holder: Option<String>,
    pub queued: Vec<String>,
}
