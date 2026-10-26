//! `mcu.call.v1`: the request body of a service call on `svc.mcu/<node>`
//! (ADR-122 section 12, Ventuno cog plan 5.1). Like `infer.v1` it carries no
//! envelope. The `mcu-bridge` cog forwards it to the MCU sketch through the
//! board's router and answers with the sketch's return value.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Check, fail};

/// Longest method name.
pub const MAX_METHOD: usize = 64;
/// Most parameters.
pub const MAX_PARAMS: usize = 8;

/// `mcu.call.v1`: call `method` of the WeftOS sketch with `params`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McuCall {
    /// A method the sketch provides, `[a-z][a-z0-9_.]*`, at most 64 chars.
    pub method: String,
    /// Positional parameters: numbers, booleans and strings, at most 8.
    #[serde(default)]
    pub params: Vec<Value>,
}

impl McuCall {
    /// Check the method name and the parameters.
    pub fn validate(&self) -> Check {
        let b = self.method.as_bytes();
        let ok = !b.is_empty()
            && b.len() <= MAX_METHOD
            && b[0].is_ascii_lowercase()
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.".contains(c));
        if !ok {
            return fail("method must be [a-z][a-z0-9_.]* of at most 64 chars");
        }
        if self.params.len() > MAX_PARAMS {
            return fail(format!("params holds more than {MAX_PARAMS} entries"));
        }
        if let Some(i) = self.params.iter().position(|p| !(p.is_number() || p.is_boolean() || p.is_string())) {
            return fail(format!("params[{i}] must be a number, boolean or string"));
        }
        Ok(())
    }
}
