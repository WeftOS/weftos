//! Reading-shaped cog schemas: SenML packs, CAN frames, transcripts and
//! telemetry lists.

use serde::{Deserialize, Serialize};

use super::{Check, Envelope, fail, in_range, is_lower_hex};

/// One SenML record (RFC 8428 subset).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SenmlRecord {
    /// Name, 1..=64 chars.
    pub n: String,
    /// Unit, if any (the readings vocabulary owns the unit per name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub u: Option<String>,
    /// Numeric value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<f64>,
    /// String value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vs: Option<String>,
    /// Boolean value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vb: Option<bool>,
}

/// `senml.v1`: the cog envelope around a SenML pack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SenmlMsg {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// 1..=64 records.
    pub pack: Vec<SenmlRecord>,
}

impl SenmlMsg {
    /// Check every record carries exactly one value.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        if self.pack.is_empty() || self.pack.len() > 64 {
            return fail("pack must hold 1..=64 records");
        }
        for (i, r) in self.pack.iter().enumerate() {
            if r.n.is_empty() || r.n.chars().count() > 64 {
                return fail(format!("pack[{i}].n must be 1..=64 chars"));
            }
            let values = [r.v.is_some(), r.vs.is_some(), r.vb.is_some()];
            if values.iter().filter(|b| **b).count() != 1 {
                return fail(format!("pack[{i}] needs exactly one of v, vs, vb"));
            }
            if let Some(v) = r.v {
                in_range(&format!("pack[{i}].v"), v, -1.0e18, 1.0e18)?;
            }
        }
        Ok(())
    }
}

/// Payload bytes for a CAN FD data length code.
const FD_LEN: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

/// `can.frame.v1`: one CAN or CAN FD frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanFrame {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// Arbitration id: 11 bits, or 29 when `ext`.
    pub id: u32,
    /// Extended (29-bit) id.
    pub ext: bool,
    /// CAN FD frame.
    pub fd: bool,
    /// Data length code: 0..=8 classic, 0..=15 FD.
    pub dlc: u8,
    /// Data bytes, lower-case hex.
    pub data: String,
}

impl CanFrame {
    /// Check id width, dlc and that `data` matches it.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        let max_id = if self.ext { 0x1FFF_FFFF } else { 0x7FF };
        if self.id > max_id {
            return fail(format!("id {:#x} exceeds {:#x}", self.id, max_id));
        }
        let len = match (self.fd, usize::from(self.dlc)) {
            (false, d) if d <= 8 => d,
            (true, d) if d <= 15 => FD_LEN[d],
            _ => return fail(format!("dlc {} out of range", self.dlc)),
        };
        if !is_lower_hex(&self.data, len * 2) {
            return fail(format!("data must be {len} bytes of lower-case hex"));
        }
        Ok(())
    }
}

/// `transcript.v1`: one recognized utterance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// Recognized text, 1..=4096 chars.
    pub text: String,
    /// Language tag (`en`, `en-US`).
    pub lang: String,
    /// Utterance start, capture clock.
    pub start_ns: u64,
    /// Utterance end, capture clock.
    pub end_ns: u64,
    /// Recognizer confidence, 0..=1.
    pub confidence: f64,
}

impl Transcript {
    /// Check text, language tag, ordering and confidence.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        let n = self.text.chars().count();
        if n == 0 || n > 4096 {
            return fail("text must be 1..=4096 chars");
        }
        let mut parts = self.lang.split('-');
        let primary = parts.next().unwrap_or_default();
        let lang_ok = (2..=3).contains(&primary.len())
            && primary.bytes().all(|b| b.is_ascii_lowercase())
            && self.lang.len() <= 16
            && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric()));
        if !lang_ok {
            return fail(format!("bad lang {:?}", self.lang));
        }
        if self.end_ns < self.start_ns {
            return fail("end_ns precedes start_ns");
        }
        in_range("confidence", self.confidence, 0.0, 1.0)
    }
}

/// One telemetry value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryItem {
    /// `[a-z][a-z0-9_.]*`, at most 64 chars.
    pub name: String,
    /// The value.
    pub value: f64,
    /// Unit, at most 16 chars; empty for a bare count.
    pub unit: String,
}

/// `telemetry.v1`: node or cog health values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Telemetry {
    /// Common fields.
    #[serde(flatten)]
    pub env: Envelope,
    /// 1..=64 items.
    pub items: Vec<TelemetryItem>,
}

impl Telemetry {
    /// Check item names, values and units.
    pub fn validate(&self) -> Check {
        self.env.validate()?;
        if self.items.is_empty() || self.items.len() > 64 {
            return fail("items must hold 1..=64 entries");
        }
        for (i, it) in self.items.iter().enumerate() {
            let b = it.name.as_bytes();
            let name_ok = !b.is_empty()
                && b.len() <= 64
                && b[0].is_ascii_lowercase()
                && b.iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.".contains(c));
            if !name_ok {
                return fail(format!("items[{i}].name {:?} is not valid", it.name));
            }
            in_range(&format!("items[{i}].value"), it.value, -1.0e18, 1.0e18)?;
            if it.unit.chars().count() > 16 {
                return fail(format!("items[{i}].unit longer than 16 chars"));
            }
        }
        Ok(())
    }
}
