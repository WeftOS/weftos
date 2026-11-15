//! Errors. Variants carry numbers and static text only, never key material or secret values.

/// Why a vault operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    /// The keybag or blob bytes are not valid under the format.
    #[error("malformed vault data: {0}")]
    Malformed(&'static str),
    /// `format_version` is not 1.
    #[error("unsupported format version")]
    UnsupportedVersion,
    /// Wrapper kind not defined in v1 (escrow, node and unknown kinds).
    #[error("wrapper kind {0} is rejected in v1")]
    RejectedWrapperKind(u8),
    /// Device wrapper platform id not defined in v1.
    #[error("device platform {0} is rejected in v1")]
    RejectedPlatform(u8),
    /// Argon2id parameters outside the format-valid range.
    #[error("kdf parameters are outside the format-valid range")]
    KdfBounds,
    /// Argon2id parameters below the policy for new wrappers.
    #[error("kdf parameters are below the policy for new wrappers")]
    KdfPolicy,
    /// The keybag signature does not verify.
    #[error("keybag signature is invalid")]
    BadSignature,
    /// The keybag is signed by a key other than the pinned one.
    #[error("keybag signer does not match the pinned key")]
    SignerMismatch,
    /// The keybag sequence is lower than the highest this device has seen.
    #[error("keybag sequence {got} is older than the highest seen {seen}")]
    Rollback {
        /// Sequence in the presented keybag.
        got: u64,
        /// Highest sequence already stored.
        seen: u64,
    },
    /// The keybag's `current_vk_generation` is lower than the highest this device has seen: a
    /// pre-rotation keybag, possibly re-signed at a higher sequence by a compromised signer.
    #[error("keybag vk generation {got} is older than the highest seen {seen}")]
    GenerationRollback {
        /// Generation in the presented keybag.
        got: u32,
        /// Highest generation already stored.
        seen: u32,
    },
    /// Same sequence, different bytes.
    #[error("keybag fork at sequence {sequence}")]
    Fork {
        /// The sequence that has two different keybags.
        sequence: u64,
    },
    /// No wrapper opened with the supplied passcode or recovery key.
    #[error("unlock failed")]
    UnlockFailed,
    /// AEAD authentication failed (tampered, moved, wrong key or wrong slot).
    #[error("authentication failed")]
    AuthFailed,
    /// The unwrapped signing seed does not match the keybag's `sign_pub`.
    #[error("signing key does not match the keybag")]
    SigningKeyMismatch,
    /// The blob was sealed under a VK generation this vault has no key or history entry for.
    #[error("no key for vault key generation {0}")]
    GenerationMismatch(u32),
    /// A recovery code failed its checksum or format check.
    #[error("recovery code is invalid")]
    BadRecoveryCode,
    /// The vault carries the DRAFT flag and the caller marked the value as a real credential.
    #[error("vault is DRAFT and refuses real credentials")]
    DraftRefusesRealCredentials,
    /// Payload, name or slot outside the size limits.
    #[error("{0} is outside the format limits")]
    TooLarge(&'static str),
    /// The requested keybag change would break a keybag rule.
    #[error("invalid operation: {0}")]
    InvalidOperation(&'static str),
    /// A new passcode breaks the passcode rules: empty, or it contains an unassigned, private-use or
    /// control code point (whose NFKC form is not stable across Unicode versions).
    #[error("passcode rejected: {0}")]
    PasscodeRejected(&'static str),
    /// The keybag sequence counter cannot advance (it is at `u64::MAX`).
    #[error("keybag sequence counter exhausted")]
    SequenceExhausted,
    /// A pending keybag write was built on other keybag bytes than the vault holds now (another write
    /// was committed in between, or it belongs to another vault). Prepare it again.
    #[error("pending write is stale")]
    StaleWrite,
    /// The OS random source failed.
    #[error("random source failed")]
    Random,
}

/// Result alias for this crate.
pub type Result<T> = core::result::Result<T, VaultError>;
