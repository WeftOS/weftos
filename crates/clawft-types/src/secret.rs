//! Secret string wrapper that prevents accidental exposure.
//!
//! [`SecretString`] wraps sensitive values (API keys, passwords, tokens) and
//! ensures they never appear in logs, Debug output, or serialized JSON.
//!
//! Redaction on `Serialize` is the default so that `weft config show`, API
//! responses and tracing output are safe. Code that must move a secret
//! through serde on purpose (handing a typed channel config to a channel
//! factory, writing a typed config back to disk) opts in explicitly with
//! [`with_secrets_exposed`] or [`to_value_exposing_secrets`]. Without that,
//! such a round trip silently turns every credential into `""`.

use std::cell::Cell;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

thread_local! {
    /// Set while a [`with_secrets_exposed`] closure runs on this thread.
    static EXPOSE_ON_SERIALIZE: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with [`SecretString`] serializing its real value on this thread.
///
/// Use only where persisting or transferring the secret is the intent, and
/// never for output that reaches a user, a log or a network response. The
/// previous mode is restored when `f` returns or panics, and calls nest.
pub fn with_secrets_exposed<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            EXPOSE_ON_SERIALIZE.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(EXPOSE_ON_SERIALIZE.with(|c| c.replace(true)));
    f()
}

/// Serialize `value` to JSON with secrets included.
///
/// See [`with_secrets_exposed`] for when this is appropriate.
///
/// # Errors
///
/// Returns the underlying `serde_json` error if `value` fails to serialize.
pub fn to_value_exposing_secrets<T: Serialize>(value: &T) -> serde_json::Result<serde_json::Value> {
    with_secrets_exposed(|| serde_json::to_value(value))
}

/// A string value that should not appear in logs, Debug output, or serialized JSON.
///
/// - `Debug` prints `[REDACTED]` (or `""` if empty)
/// - `Serialize` emits an empty string unless inside [`with_secrets_exposed`]
/// - `Deserialize` accepts a plain string (backward compatible with old configs)
/// - `Display` prints `[REDACTED]` (or empty if the value is empty)
/// - [`expose()`](SecretString::expose) returns the inner value for actual use
#[derive(Clone, Default)]
pub struct SecretString(String);

impl SecretString {
    /// Create a new `SecretString` wrapping the given value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Get the actual secret value. Use sparingly and only where needed
    /// (e.g., HTTP Authorization headers, API calls).
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Returns `true` if the wrapped value is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            write!(f, "\"\"")
        } else {
            write!(f, "\"[REDACTED]\"")
        }
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            write!(f, "")
        } else {
            write!(f, "[REDACTED]")
        }
    }
}

impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Redact unless the caller explicitly opted in for persistence or
        // in-process transfer.
        if EXPOSE_ON_SERIALIZE.with(Cell::get) {
            serializer.serialize_str(&self.0)
        } else {
            serializer.serialize_str("")
        }
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(SecretString(s))
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        SecretString(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        SecretString(s.to_string())
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_non_empty() {
        let s = SecretString::new("my-secret-key");
        assert_eq!(format!("{:?}", s), "\"[REDACTED]\"");
    }

    #[test]
    fn debug_shows_empty_for_empty() {
        let s = SecretString::default();
        assert_eq!(format!("{:?}", s), "\"\"");
    }

    #[test]
    fn display_redacts_non_empty() {
        let s = SecretString::new("secret");
        assert_eq!(format!("{}", s), "[REDACTED]");
    }

    #[test]
    fn display_empty_for_empty() {
        let s = SecretString::default();
        assert_eq!(format!("{}", s), "");
    }

    #[test]
    fn expose_returns_actual_value() {
        let s = SecretString::new("actual-secret");
        assert_eq!(s.expose(), "actual-secret");
    }

    #[test]
    fn is_empty_works() {
        assert!(SecretString::default().is_empty());
        assert!(!SecretString::new("x").is_empty());
    }

    #[test]
    fn serialize_emits_empty_string() {
        let s = SecretString::new("my-api-key");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"\"");
        assert!(!json.contains("my-api-key"));
    }

    #[test]
    fn exposed_serialization_round_trips_and_resets() {
        let s = SecretString::new("dummy-secret");
        let json = with_secrets_exposed(|| serde_json::to_string(&s).unwrap());
        assert_eq!(json, "\"dummy-secret\"");
        let back: SecretString = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // Mode is restored afterwards.
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"\"");
    }

    #[test]
    fn exposed_mode_nests_and_survives_panic() {
        let s = SecretString::new("dummy-secret");
        with_secrets_exposed(|| {
            with_secrets_exposed(|| {});
            assert_eq!(serde_json::to_string(&s).unwrap(), "\"dummy-secret\"");
        });
        let r = std::panic::catch_unwind(|| with_secrets_exposed(|| panic!("boom")));
        assert!(r.is_err());
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"\"");
    }

    #[test]
    fn typed_config_round_trip_keeps_credentials_only_when_asked() {
        let cfg = crate::config::channels::TelegramConfig {
            token: SecretString::new("dummy-token"),
            ..Default::default()
        };
        // Default: redacted, no leak.
        let v = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["token"], "");
        // Explicit persistence: credential survives load -> save -> load.
        let v = to_value_exposing_secrets(&cfg).unwrap();
        let back: crate::config::channels::TelegramConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back.token.expose(), "dummy-token");
    }

    #[test]
    fn deserialize_accepts_plain_string() {
        let s: SecretString = serde_json::from_str("\"my-api-key\"").unwrap();
        assert_eq!(s.expose(), "my-api-key");
    }

    #[test]
    fn from_string() {
        let s: SecretString = "test".into();
        assert_eq!(s.expose(), "test");
    }

    #[test]
    fn from_owned_string() {
        let s: SecretString = String::from("test").into();
        assert_eq!(s.expose(), "test");
    }

    #[test]
    fn equality() {
        let a = SecretString::new("same");
        let b = SecretString::new("same");
        let c = SecretString::new("different");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
