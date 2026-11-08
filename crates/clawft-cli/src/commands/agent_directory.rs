//! Agent directory join: lives in `weftos::agent_directory` so the daemon
//! (`agent.directory`) and this CLI share one implementation. Re-exported
//! here so existing `super::agent_directory::...` paths keep working.

pub use weftos::agent_directory::*;
