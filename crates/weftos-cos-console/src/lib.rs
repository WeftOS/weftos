//! # weftos-cos-console
//!
//! The chief-of-staff console screen (ADR-118 C2) as scene objects: a
//! green-screen vector interface on the 800×480 edge pad, drawn with
//! `weftos-leaf-scene` ops through a display session.
//!
//! ```text
//!  0,0   ┌──────────── system strip (firmware) ───────────────┐ 800×24
//!  0,24  ├─────────────┬──────── status 500×64 ────────────────┤
//!        │   visual    ├────────────────────────────────────────┤
//!        │   300×456   │        chat log 500×392                │
//!        │  (avatar)   │        newest at the bottom            │
//!        └─────────────┴────────────────────────────────────────┘ 480
//! ```
//!
//! - [`Console`] is the host API from the lane contract §2.
//! - [`DisplaySession`] is the small session trait the console draws
//!   through; [`SimSession`] (feature `sim`) runs it against
//!   `weftos-leaf-sim` for tests and the `cos-console-demo` binary.
//! - Node ranges: chrome 1–99, avatar 100–199, status 200–299,
//!   page 300–999, chat 1000–4095 (recycled).

pub mod avatar;
pub mod chat_log;
pub mod chat_view;
pub mod chrome;
pub mod console;
pub mod page;
pub mod pool;
pub mod prim;
pub mod session;
pub mod status;
pub mod text;
pub mod theme;

pub use console::{Console, ConsoleOptions};
pub use page::PageDoc;
pub use session::{DisplaySession, ObjectEvent};
#[cfg(feature = "sim")]
pub use session::SimSession;
pub use status::{AgentState, StatusLine};
