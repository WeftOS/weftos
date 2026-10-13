//! `weftos-node-core`: the sans-IO core of the WeftOS phone parent node.
//!
//! Design: `docs/plans/phone-parent-node.md` (P0). No sockets, no timers, no platform
//! code; the native shell feeds bytes, time and events in and performs the returned
//! actions. The same crate compiles for the host (tests), `aarch64-linux-android`
//! (cargo-ndk) and later iOS.
//!
//! | Concern | Module |
//! |---------|--------|
//! | Node identity (shares `clawft-android-edge`'s file and node-id derivation) | [`identity`] |
//! | `glasses.v1` reliable-lane CBOR envelopes | [`proto`] |
//! | `glasses.v1` realtime audio frame header | [`audio`] |
//! | indicator state machine (3.7) | [`indicator`] |
//! | `led.set` and BES serial lines (3.6) | [`led`] |
//! | leaf-parent pairing (4.3) | [`pairing`] |
//! | lanes, framing, fake transport | [`transport`] |
//! | glasses (leaf) side: stream reassembly, commands, indicator | [`glasses`] |
//! | phone side of one glasses link: pairing, utterance assembly, relay turn | [`phone`] |
//! | UniFFI facades for Kotlin/Swift (phone, glasses, phone link) | [`ffi`], [`ffi_glasses`], [`ffi_phone`] |

pub mod audio;
pub mod error;
pub mod glasses;
pub mod identity;
pub mod indicator;
pub mod led;
pub mod pairing;
pub mod phone;
pub mod proto;
pub mod transport;

#[cfg(feature = "uniffi")]
pub mod ffi;
#[cfg(feature = "uniffi")]
pub mod ffi_glasses;
#[cfg(feature = "uniffi")]
pub mod ffi_phone;

pub use error::NodeError;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
