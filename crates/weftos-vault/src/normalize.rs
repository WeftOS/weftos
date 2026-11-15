//! Text normalization (spec 3.3). The only module that touches `unicode-normalization`.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_public_assigned;
use zeroize::Zeroizing;

use crate::error::{Result, VaultError};

/// Passcode form: NFKC, no trimming, no case folding.
///
/// The output buffer is sized exactly before it is filled, so no reallocation leaves an unwiped copy
/// of the passcode on the heap (review L3). The normalizer's own small internal buffer is outside our
/// control.
pub(crate) fn passcode(s: &str) -> Zeroizing<String> {
    let len: usize = s.nfkc().map(char::len_utf8).sum();
    let mut out = Zeroizing::new(String::with_capacity(len));
    out.extend(s.nfkc());
    out
}

/// Rules for a passcode being SET (create, change, rotation re-seal; review L4). Unlock does not apply
/// them, so an existing wrapper always stays openable.
///
/// - Not empty.
/// - Every code point is assigned and public in the Unicode version of the pinned
///   `unicode-normalization` (`=0.1.26`, Unicode 18.0) and is not a control character. NFKC of an
///   unassigned code point can change when a later Unicode version assigns it a decomposition, which
///   would derive a different KEK after a dependency bump and lock the user out. Private-use code points
///   and controls are refused too: they are invisible or device specific and are a typing hazard.
pub(crate) fn check_new_passcode(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(VaultError::PasscodeRejected("empty"));
    }
    if s.chars().any(|c| c.is_control() || !is_public_assigned(c)) {
        return Err(VaultError::PasscodeRejected(
            "unassigned, private-use or control code point",
        ));
    }
    Ok(())
}

/// Secret-name form: NFC, case-sensitive.
pub(crate) fn name(s: &str) -> String {
    s.nfc().collect()
}
