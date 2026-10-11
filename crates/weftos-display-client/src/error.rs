//! Session errors. Every failure an app can get back from
//! [`DisplaySession`](crate::DisplaySession) is one of these; the
//! backends never panic on bad input.

use core::fmt;

/// Why a session call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayError {
    /// The target backend is not built or not reachable yet. Carries
    /// the reason so the console can show it verbatim.
    NotYet(&'static str),
    /// The grant was refused by the gate (D2 arbitration lands here).
    Denied(&'static str),
    /// The session was closed (explicitly or by the backend) and the
    /// call needs a live one.
    Closed,
    /// Applying the ops would exceed the per-session node cap.
    NodeBudget { max: usize, would_be: usize },
    /// Applying the ops would exceed the per-session op rate.
    OpRate { max_per_sec: u32, attempted: u32 },
    /// Applying the ops would exceed the per-session image budget.
    ImageBudget { max_bytes: usize, would_be: usize },
    /// The backend could not render or deliver the ops.
    Backend(String),
}

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotYet(why) => write!(f, "display backend not available yet: {why}"),
            Self::Denied(why) => write!(f, "display grant denied: {why}"),
            Self::Closed => write!(f, "display session is closed"),
            Self::NodeBudget { max, would_be } => {
                write!(f, "node budget exceeded: {would_be} nodes, session allows {max}")
            }
            Self::OpRate { max_per_sec, attempted } => write!(
                f,
                "op rate exceeded: {attempted} ops in the last second, session allows {max_per_sec}"
            ),
            Self::ImageBudget { max_bytes, would_be } => write!(
                f,
                "image budget exceeded: {would_be} bytes of bitmaps, session allows {max_bytes}"
            ),
            Self::Backend(msg) => write!(f, "display backend error: {msg}"),
        }
    }
}

impl std::error::Error for DisplayError {}
