//! # weftos-cos-console
//!
//! The chief-of-staff console screen (ADR-118 C2) as scene objects: a
//! green-screen vector interface on the 800×480 edge pad, drawn with
//! `weftos-leaf-scene` ops through a `weftos-display-client` session.
//!
//! ```text
//!  split                              guide
//!  ┌──────┬── status 500×64 ──────┐   ┌──────┬─ guide head 500×184 ──┐
//!  │visual├───────────────────────┤   │guide ├───────────────────────┤
//!  │300×  │  chat log 500×392     │   │side  │ status line 500×24    │
//!  │ 456  │  newest at the bottom │   │300×  ├───────────────────────┤
//!  │      │                       │   │ 456  │  chat log 500×248     │
//!  └──────┴───────────────────────┘   └──────┴───────────────────────┘
//!  page: one 800×456 region (title, headings, paragraphs, bullets, rules)
//! ```
//!
//! - [`Console`] is the host API from the lane contract §2.
//! - [`host`] joins the console to the chief-of-staff agent over its
//!   HTTP/SSE API (contract §3) and carries push-to-talk to it; the
//!   `cos-console-host` binary runs it in a Mac window.
//! - [`frame`] opens simulator sessions and renders a leaf snapshot to
//!   PNG, for tests and the `cos-console-demo` binary.
//! - Node ranges: chrome 1–99, avatar 100–199, status 200–299,
//!   page 300–499, guide 500–999, chat 1000–4095 (recycled).

pub mod avatar;
mod avatar_shader;
pub mod chat_log;
pub mod chat_view;
pub mod chrome;
pub mod console;
pub mod frame;
pub mod guide;
pub mod guide_checklist;
pub mod guide_steps;
pub mod guide_survey;
pub mod host;
pub mod page;
pub mod pool;
pub mod prim;
pub mod status;
pub mod text;
pub mod theme;

pub use console::{Console, ConsoleOptions, Layout};
pub use guide::{Checklist, ChecklistItem, Choice, GuideAnswer, StepDir, Steps, Survey};
pub use page::{PageBlock, PageDoc};
pub use status::{AgentState, StatusLine};
pub use weftos_display_client::{DisplayError, DisplaySession, LeafSim, ObjectEvent, SensorValue};
