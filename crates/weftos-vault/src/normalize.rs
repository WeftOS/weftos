//! Text normalization (spec 3.3). The only module that touches `unicode-normalization`.

use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

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

/// Secret-name form: NFC, case-sensitive.
pub(crate) fn name(s: &str) -> String {
    s.nfc().collect()
}
