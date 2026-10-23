//! Microsoft Teams channel adapter.
//!
//! Feature-gated behind the `teams` feature flag.
//! Uses Bot Framework and Graph API with Azure AD auth.

pub mod channel;
pub mod jwt;
#[cfg(test)]
pub(crate) mod jwt_tests;
pub mod types;

pub use channel::TeamsChannelAdapter;
