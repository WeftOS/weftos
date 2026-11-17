//! # exo-core — K6 substrate primitives (ADR-043 / WEFT-619)
//!
//! In-tree implementation of the **exo-core contract** used by SPARC docs
//! (`Blake3Hash`, domain-separated hashing, `HybridLogicalClock`, `EventId`).
//!
//! This crate intentionally does **not** vendor third-party exo-core source
//! (FSL / license clearance deferred). API names match the K6 migration path
//! so `exo-dag` and future `ChainManager` cutover can depend on it.
//!
//! ## Target support
//!
//! Pure Rust + `blake3` — builds for native and `wasm32` (no OS clocks required
//! for deterministic hashing; wall-clock HLC ticks use `std::time` when available).

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 32-byte BLAKE3 digest used as a content-addressed identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Blake3Hash(pub [u8; 32]);

impl Blake3Hash {
    /// Zero / empty digest (genesis / unset).
    pub const ZERO: Self = Self([0u8; 32]);

    /// Construct from a raw 32-byte array.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex encoding (64 chars).
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for b in &self.0 {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    /// Parse lowercase or uppercase hex (exactly 64 ASCII hex digits).
    ///
    /// # Errors
    ///
    /// [`ExoCoreError::InvalidHexLength`] if the input is not 64 bytes long,
    /// [`ExoCoreError::InvalidHexChar`] for any byte that is not `0-9a-fA-F`
    /// (including a leading `+`, which `u8::from_str_radix` would accept, and
    /// non-ASCII text).
    pub fn from_hex(s: &str) -> Result<Self, ExoCoreError> {
        fn nibble(b: u8) -> Result<u8, ExoCoreError> {
            match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                b'A'..=b'F' => Ok(b - b'A' + 10),
                _ => Err(ExoCoreError::InvalidHexChar),
            }
        }
        let raw = s.as_bytes();
        if raw.len() != 64 {
            return Err(ExoCoreError::InvalidHexLength(raw.len()));
        }
        let mut bytes = [0u8; 32];
        for (out, pair) in bytes.iter_mut().zip(raw.chunks_exact(2)) {
            *out = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for Blake3Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blake3Hash({})", self.to_hex())
    }
}

impl fmt::Display for Blake3Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl From<[u8; 32]> for Blake3Hash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Blake3Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Content-addressed event id (BLAKE3 of the event body).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId(pub Blake3Hash);

impl EventId {
    /// Wrap an existing hash.
    pub const fn new(hash: Blake3Hash) -> Self {
        Self(hash)
    }

    /// Derive an event id from canonical payload bytes.
    pub fn from_payload(payload: &[u8]) -> Self {
        Self(blake3_hash(payload))
    }

    /// Hex form for logging / wire.
    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Hybrid Logical Clock: physical milliseconds + logical tick.
///
/// Monotone under concurrent updates when used via [`HybridLogicalClock`].
/// Matches the SPARC `HLC` / `HybridLogicalClock` contract for K6 DAG events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hlc {
    /// Wall-clock milliseconds since Unix epoch (best-effort).
    pub physical_ms: u64,
    /// Logical counter for same-ms causality.
    pub logical: u32,
}

impl Hlc {
    /// Epoch zero timestamp.
    pub const ZERO: Self = Self {
        physical_ms: 0,
        logical: 0,
    };

    /// Construct from parts.
    pub const fn new(physical_ms: u64, logical: u32) -> Self {
        Self {
            physical_ms,
            logical,
        }
    }

    /// Canonical 12-byte encoding: `physical_ms` BE + `logical` BE.
    pub fn to_bytes(self) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..8].copy_from_slice(&self.physical_ms.to_be_bytes());
        out[8..12].copy_from_slice(&self.logical.to_be_bytes());
        out
    }

    /// Decode from [`Hlc::to_bytes`].
    pub fn from_bytes(bytes: [u8; 12]) -> Self {
        let mut phys = [0u8; 8];
        let mut log = [0u8; 4];
        phys.copy_from_slice(&bytes[0..8]);
        log.copy_from_slice(&bytes[8..12]);
        Self {
            physical_ms: u64::from_be_bytes(phys),
            logical: u32::from_be_bytes(log),
        }
    }
}

/// Thread-safe hybrid logical clock generator.
///
/// `tick()` advances past both wall time and last observed HLC so concurrent
/// producers observe a total order suitable for DAG event stamping. Every
/// stamp returned by one clock is unique and strictly greater than all earlier
/// stamps from that clock, even if the wall clock steps backwards or the
/// logical counter overflows (it then carries into `physical_ms`).
#[derive(Debug)]
pub struct HybridLogicalClock {
    /// Last stamp issued. Physical and logical parts must change together,
    /// so they live under one lock rather than in two independent atomics.
    last: Mutex<Hlc>,
}

impl Default for HybridLogicalClock {
    fn default() -> Self {
        Self::new()
    }
}

impl HybridLogicalClock {
    /// Create a clock starting at epoch zero.
    pub fn new() -> Self {
        Self {
            last: Mutex::new(Hlc::ZERO),
        }
    }

    /// Wall-clock ms; `0` if the system clock is before the Unix epoch
    /// (the stored state then keeps the stamp monotone).
    fn wall_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }

    /// Produce the next HLC tick (local observation only).
    pub fn tick(&self) -> Hlc {
        self.observe(None)
    }

    /// Merge a remote HLC (if any) then advance.
    pub fn observe(&self, remote: Option<Hlc>) -> Hlc {
        self.observe_at(Self::wall_ms(), remote)
    }

    /// [`observe`](Self::observe) with an explicit wall-clock reading.
    fn observe_at(&self, wall: u64, remote: Option<Hlc>) -> Hlc {
        // The guarded value is plain `Copy` data written in one assignment, so
        // it is valid even if another thread panicked while holding the lock.
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let remote = remote.unwrap_or(Hlc::ZERO);
        let phys = wall.max(last.physical_ms).max(remote.physical_ms);

        // Highest logical counter already seen at `phys`, if any.
        let mut seen: Option<u32> = None;
        if phys == last.physical_ms {
            seen = Some(last.logical);
        }
        if phys == remote.physical_ms {
            seen = Some(seen.map_or(remote.logical, |l| l.max(remote.logical)));
        }

        let next = match seen {
            None => Hlc::new(phys, 0),
            Some(l) => match l.checked_add(1) {
                Some(l) => Hlc::new(phys, l),
                // Logical overflow: carry into the next millisecond.
                None => Hlc::new(phys.saturating_add(1), 0),
            },
        };
        *last = next;
        next
    }

    /// Last stamped value without advancing (may be ZERO before first tick).
    pub fn peek(&self) -> Hlc {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Alias used in SPARC sketches (`HLC`).
pub type HLC = Hlc;

/// BLAKE3 hash of `data`.
pub fn blake3_hash(data: &[u8]) -> Blake3Hash {
    Blake3Hash(*blake3::hash(data).as_bytes())
}

/// Domain-separated BLAKE3: `hash(domain || 0x00 || data)`.
///
/// Domain strings should be stable protocol constants (e.g.
/// `WEFTOS-RESOURCE-PATH-v1`).
pub fn blake3_hash_with_domain(domain: &[u8], data: &[u8]) -> Blake3Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&[0u8]);
    hasher.update(data);
    Blake3Hash(*hasher.finalize().as_bytes())
}

/// Errors from exo-core parsing helpers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExoCoreError {
    /// Hex string was not 64 characters.
    #[error("blake3 hex length {0}, expected 64")]
    InvalidHexLength(usize),
    /// Non-hex character in digest string.
    #[error("invalid hex character in blake3 digest")]
    InvalidHexChar,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_hash_stable() {
        let a = blake3_hash(b"weftos-k6");
        let b = blake3_hash(b"weftos-k6");
        assert_eq!(a, b);
        assert_ne!(a, Blake3Hash::ZERO);
        assert_eq!(a.to_hex().len(), 64);
        assert_eq!(Blake3Hash::from_hex(&a.to_hex()).unwrap(), a);
    }

    #[test]
    fn domain_separation_differs() {
        let plain = blake3_hash(b"path");
        let dom = blake3_hash_with_domain(b"WEFTOS-RESOURCE-PATH-v1", b"path");
        assert_ne!(plain, dom);
        let dom2 = blake3_hash_with_domain(b"WEFTOS-RESOURCE-PATH-v1", b"path");
        assert_eq!(dom, dom2);
    }

    #[test]
    fn hlc_bytes_roundtrip() {
        let h = Hlc::new(1_700_000_000_000, 42);
        assert_eq!(Hlc::from_bytes(h.to_bytes()), h);
    }

    #[test]
    fn hybrid_clock_monotone() {
        let clock = HybridLogicalClock::new();
        let mut prev = Hlc::ZERO;
        for _ in 0..64 {
            let t = clock.tick();
            assert!(t > prev, "expected {t:?} > {prev:?}");
            prev = t;
        }
    }

    #[test]
    fn hybrid_clock_observes_remote() {
        let clock = HybridLogicalClock::new();
        let local = clock.tick();
        let remote = Hlc::new(local.physical_ms + 10_000, 0);
        let merged = clock.observe(Some(remote));
        assert!(merged.physical_ms >= remote.physical_ms);
        assert!(merged > remote || merged.logical > remote.logical);
    }

    #[test]
    fn hybrid_clock_unique_and_monotone_under_threads() {
        use std::collections::HashSet;
        use std::sync::Arc;
        const THREADS: usize = 16;
        const TICKS: usize = 5_000;
        let clock = Arc::new(HybridLogicalClock::new());
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let clock = Arc::clone(&clock);
                std::thread::spawn(move || (0..TICKS).map(|_| clock.tick()).collect::<Vec<_>>())
            })
            .collect();
        let mut all = HashSet::new();
        for h in handles {
            let stamps = h.join().expect("tick thread panicked");
            assert!(stamps.windows(2).all(|w| w[0] < w[1]), "per-thread order");
            for s in stamps {
                assert!(all.insert(s), "duplicate stamp {s:?}");
            }
        }
        assert_eq!(all.len(), THREADS * TICKS);
    }

    #[test]
    fn hybrid_clock_survives_wall_clock_going_backwards() {
        let clock = HybridLogicalClock::new();
        let a = clock.observe_at(10_000, None);
        let b = clock.observe_at(5_000, None);
        let c = clock.observe_at(5_000, None);
        assert!(a < b && b < c, "{a:?} {b:?} {c:?}");
        assert_eq!(b.physical_ms, 10_000);
    }

    #[test]
    fn hybrid_clock_logical_overflow_carries() {
        let clock = HybridLogicalClock::new();
        let far = Hlc::new(u64::MAX / 2, u32::MAX);
        let a = clock.observe(Some(far));
        let b = clock.tick();
        let c = clock.tick();
        assert!(far < a && a < b && b < c, "{far:?} {a:?} {b:?} {c:?}");
    }

    #[test]
    fn hybrid_clock_merges_equal_physical_logicals() {
        let clock = HybridLogicalClock::new();
        let a = clock.observe_at(100, Some(Hlc::new(100, 7)));
        assert_eq!(a, Hlc::new(100, 8));
        let b = clock.observe_at(100, Some(Hlc::new(100, 3)));
        assert_eq!(b, Hlc::new(100, 9));
    }

    #[test]
    fn from_hex_rejects_malformed_input() {
        let ok = "ab".repeat(32);
        assert!(Blake3Hash::from_hex(&ok).is_ok());
        assert!(Blake3Hash::from_hex(&ok.to_uppercase()).is_ok());
        // `+` is accepted by u8::from_str_radix; it must not be here.
        let plus = format!("+f{}", "00".repeat(31));
        assert_eq!(
            Blake3Hash::from_hex(&plus),
            Err(ExoCoreError::InvalidHexChar)
        );
        // 64 bytes long, but multi-byte chars straddle pair boundaries.
        let non_ascii = format!("é{}", "0".repeat(62));
        assert_eq!(non_ascii.len(), 64);
        assert_eq!(
            Blake3Hash::from_hex(&non_ascii),
            Err(ExoCoreError::InvalidHexChar)
        );
        assert_eq!(
            Blake3Hash::from_hex("abc"),
            Err(ExoCoreError::InvalidHexLength(3))
        );
        assert_eq!(
            Blake3Hash::from_hex(""),
            Err(ExoCoreError::InvalidHexLength(0))
        );
        assert_eq!(
            Blake3Hash::from_hex(&"g".repeat(64)),
            Err(ExoCoreError::InvalidHexChar)
        );
    }

    #[test]
    fn event_id_from_payload() {
        let id = EventId::from_payload(b"event-body");
        assert_eq!(id.0, blake3_hash(b"event-body"));
    }

    #[test]
    fn serde_blake3_roundtrip() {
        let h = blake3_hash(b"serde");
        let json = serde_json::to_string(&h).unwrap();
        let back: Blake3Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }
}
