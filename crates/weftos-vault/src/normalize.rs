//! Text normalization (spec 3.3). The only module that touches `unicode-normalization`.

use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

/// Passcode form: NFKC, no trimming, no case folding.
pub(crate) fn passcode(s: &str) -> Zeroizing<String> {
    Zeroizing::new(s.nfkc().collect())
}

/// Secret-name form: NFC, case-sensitive.
pub(crate) fn name(s: &str) -> String {
    s.nfc().collect()
}
