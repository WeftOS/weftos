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

/// `Default_Ignorable_Code_Point` from Unicode `DerivedCoreProperties.txt` (the property includes its
/// reserved ranges, so it is stable across versions). Covers ZWSP, ZWNJ, ZWJ, the bidi controls
/// (U+061C, U+200E, U+200F, U+202A..U+202E, U+2066..U+2069), the soft hyphen, word joiner, BOM,
/// Hangul fillers, variation selectors and tag characters.
const DEFAULT_IGNORABLE: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
];

/// Invisible when typed: default-ignorable, or a line or paragraph separator (Zl U+2028, Zp U+2029).
fn is_invisible(c: char) -> bool {
    let u = c as u32;
    u == 0x2028
        || u == 0x2029
        || DEFAULT_IGNORABLE
            .iter()
            .any(|(a, b)| (*a..=*b).contains(&u))
}

/// Rules for a passcode being SET (create, change, rotation re-seal; review L4 and R2-L3). Unlock does
/// not apply them, so an existing wrapper always stays openable.
///
/// - Not empty, and not whitespace only.
/// - Every code point is assigned and public in the Unicode version of the pinned
///   `unicode-normalization` (`=0.1.26`, Unicode 18.0) and is not a control character. NFKC of an
///   unassigned code point can change when a later Unicode version assigns it a decomposition, which
///   would derive a different KEK after a dependency bump and lock the user out. Private-use code points
///   and controls are refused too: they are invisible or device specific and are a typing hazard.
/// - No invisible code point: default-ignorable (zero-width characters, bidi controls, soft hyphen,
///   variation selectors, tag characters) or a line or paragraph separator. They are NFKC-stable, but a
///   user cannot see them and often cannot type them again on another device.
pub(crate) fn check_new_passcode(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(VaultError::PasscodeRejected("empty"));
    }
    if s.chars().all(char::is_whitespace) {
        return Err(VaultError::PasscodeRejected("whitespace only"));
    }
    if s.chars().any(|c| c.is_control() || !is_public_assigned(c)) {
        return Err(VaultError::PasscodeRejected(
            "unassigned, private-use or control code point",
        ));
    }
    if s.chars().any(is_invisible) {
        return Err(VaultError::PasscodeRejected("invisible code point"));
    }
    Ok(())
}

/// Secret-name form: NFC, case-sensitive.
pub(crate) fn name(s: &str) -> String {
    s.nfc().collect()
}
