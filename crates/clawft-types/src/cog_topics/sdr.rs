//! `sdr.v1`: the calls on `svc.sdr/<node>` (ADR-122 section 12), served by an
//! SDR receive cog. Like `infer.v1` it carries no envelope: the broker stamps
//! the caller, the call id and the clock on the call.
//!
//! The request body is `{"method": "...", ...fields}`; the reply body is
//! [`SdrReply`], or the call fails with a message. A band names a centre
//! frequency, a complex sample rate, an analog bandwidth and a gain.

use serde::{Deserialize, Serialize};

use super::{Check, fail, in_range};

/// Lowest tunable centre frequency, Hz.
pub const MIN_CENTRE_HZ: u64 = 70_000_000;
/// Highest tunable centre frequency, Hz.
pub const MAX_CENTRE_HZ: u64 = 6_000_000_000;
/// Lowest complex sample rate, S/s.
pub const MIN_RATE_SPS: u32 = 520_833;
/// Highest complex sample rate, S/s.
pub const MAX_RATE_SPS: u32 = 61_440_000;
/// Narrowest analog bandwidth, Hz.
pub const MIN_BW_HZ: u32 = 200_000;
/// Widest analog bandwidth, Hz.
pub const MAX_BW_HZ: u32 = 56_000_000;
/// Longest lease, seconds.
pub const MAX_LEASE_SECS: u64 = 3600;
/// Most bands in one plan.
pub const MAX_PLAN_BANDS: usize = 32;
/// Longest plan id, characters.
pub const MAX_PLAN_ID: usize = 48;

/// Gain: a manual setting in dB, or `"auto"` (the chip's slow-attack AGC).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Gain {
    /// Manual gain, dB.
    Db(f64),
    /// Only the string `"auto"`.
    Auto(AutoGain),
}

/// The literal `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoGain {
    /// Automatic gain control.
    #[serde(rename = "auto")]
    Auto,
}

/// One band of a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdrBand {
    /// Centre frequency, Hz.
    pub centre_hz: u64,
    /// Complex sample rate, S/s.
    pub rate_sps: u32,
    /// Time to stay on the band, milliseconds.
    pub dwell_ms: u32,
    /// Analog bandwidth, Hz; the service picks one from the rate when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bw_hz: Option<u32>,
    /// Gain; the service keeps its default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<Gain>,
}

impl SdrBand {
    fn validate(&self) -> Check {
        check_centre(self.centre_hz)?;
        check_rate(self.rate_sps)?;
        if self.dwell_ms == 0 {
            return fail("dwell_ms must be above 0");
        }
        self.bw_hz.map_or(Ok(()), check_bw)
    }
}

fn check_centre(hz: u64) -> Check {
    in_range("centre_hz", hz as f64, MIN_CENTRE_HZ as f64, MAX_CENTRE_HZ as f64)
}
fn check_rate(sps: u32) -> Check {
    in_range("rate_sps", f64::from(sps), f64::from(MIN_RATE_SPS), f64::from(MAX_RATE_SPS))
}
fn check_bw(hz: u32) -> Check {
    in_range("bw_hz", f64::from(hz), f64::from(MIN_BW_HZ), f64::from(MAX_BW_HZ))
}
fn check_gain(g: &Gain) -> Check {
    match g {
        Gain::Db(db) if !db.is_finite() => fail("gain_db must be finite"),
        _ => Ok(()),
    }
}

/// `sdr.v1`: a call on the tuner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum SdrRequest {
    /// The caller's own single-band plan; replaces its previous `tune`.
    Tune {
        /// Centre frequency, Hz.
        centre_hz: u64,
        /// Complex sample rate, S/s (default 5 000 000).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_sps: Option<u32>,
        /// Analog bandwidth, Hz.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bw_hz: Option<u32>,
        /// Gain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gain_db: Option<Gain>,
        /// Dwell, milliseconds (default 1000).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dwell_ms: Option<u32>,
    },
    /// Change the rate of the caller's `tune` band.
    SetRate {
        /// Complex sample rate, S/s.
        rate_sps: u32,
    },
    /// Change the bandwidth of the caller's `tune` band.
    SetBw {
        /// Analog bandwidth, Hz.
        bw_hz: u32,
    },
    /// Change the gain of the caller's `tune` band.
    SetGain {
        /// Gain.
        gain_db: Gain,
    },
    /// A band plan that takes turns with the others.
    Plan {
        /// Plan id, 1..=48 chars; `default`, `tune:*` and `lease:*` are reserved.
        id: String,
        /// The bands, 1..=32.
        bands: Vec<SdrBand>,
    },
    /// Remove one of the caller's plans.
    PlanRemove {
        /// Plan id.
        id: String,
    },
    /// Exclusive use of the tuner for up to an hour.
    Lease {
        /// Centre frequency, Hz.
        centre_hz: u64,
        /// Complex sample rate, S/s.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_sps: Option<u32>,
        /// Analog bandwidth, Hz.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bw_hz: Option<u32>,
        /// Gain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gain_db: Option<Gain>,
        /// Lease length, seconds, 1..=3600.
        secs: u64,
    },
    /// Give the lease back.
    Release {},
    /// The service's status document (free-form JSON).
    Status {},
    /// Transmit configuration. A receive-only service always refuses it; the
    /// schema lets a transmit service accept it.
    TxConfig {
        /// Attenuation, dB (at most 0).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attenuation_db: Option<f64>,
        /// Centre frequency, Hz.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        centre_hz: Option<u64>,
        /// Complex sample rate, S/s.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_sps: Option<u32>,
    },
}

impl SdrRequest {
    /// Check ranges and lengths.
    pub fn validate(&self) -> Check {
        match self {
            Self::Tune { centre_hz, rate_sps, bw_hz, gain_db, dwell_ms } => {
                check_band(*centre_hz, *rate_sps, *bw_hz, gain_db.as_ref())?;
                if *dwell_ms == Some(0) {
                    return fail("dwell_ms must be above 0");
                }
                Ok(())
            }
            Self::SetRate { rate_sps } => check_rate(*rate_sps),
            Self::SetBw { bw_hz } => check_bw(*bw_hz),
            Self::SetGain { gain_db } => check_gain(gain_db),
            Self::Plan { id, bands } => {
                check_plan_id(id)?;
                if matches!(id.as_str(), "default") || id.starts_with("tune:") || id.starts_with("lease:") {
                    return fail(format!("plan id {id} is reserved"));
                }
                if bands.is_empty() || bands.len() > MAX_PLAN_BANDS {
                    return fail(format!("bands must hold 1..={MAX_PLAN_BANDS} entries"));
                }
                bands.iter().try_for_each(SdrBand::validate)
            }
            Self::PlanRemove { id } => check_plan_id(id),
            Self::Lease { centre_hz, rate_sps, bw_hz, gain_db, secs } => {
                check_band(*centre_hz, *rate_sps, *bw_hz, gain_db.as_ref())?;
                if *secs == 0 || *secs > MAX_LEASE_SECS {
                    return fail(format!("secs must be 1..={MAX_LEASE_SECS}"));
                }
                Ok(())
            }
            Self::Release {} | Self::Status {} => Ok(()),
            Self::TxConfig { attenuation_db, centre_hz, rate_sps } => {
                if attenuation_db.is_some_and(|a| !a.is_finite() || a > 0.0) {
                    return fail("attenuation_db must be a finite number at most 0");
                }
                centre_hz.map_or(Ok(()), check_centre)?;
                rate_sps.map_or(Ok(()), check_rate)
            }
        }
    }
}

fn check_plan_id(id: &str) -> Check {
    if id.is_empty() || id.len() > MAX_PLAN_ID {
        return fail(format!("id must be 1..={MAX_PLAN_ID} chars"));
    }
    Ok(())
}

fn check_band(centre: u64, rate: Option<u32>, bw: Option<u32>, gain: Option<&Gain>) -> Check {
    check_centre(centre)?;
    rate.map_or(Ok(()), check_rate)?;
    bw.map_or(Ok(()), check_bw)?;
    gain.map_or(Ok(()), check_gain)
}

/// The reply body of a successful `sdr.v1` call. `status` replies with the
/// service's own status document instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdrReply {
    /// Always true; a failed call fails with a message instead.
    pub ok: bool,
    /// The plan the call set (`tune`, `set_*`, `plan`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Bands in the plan (`plan`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bands: Option<u32>,
    /// The lease number (`lease`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<u64>,
    /// The lease length granted, seconds (`lease`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<u64>,
    /// The band lies outside the part's rated range (`tune`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncalibrated: Option<bool>,
}

impl SdrReply {
    /// A reply is `ok: true`.
    pub fn validate(&self) -> Check {
        if !self.ok {
            return fail("ok must be true; a failed call fails with a message");
        }
        Ok(())
    }
}
