//! `led.set` and the BES serial rendering of it (design section 3.6).
//!
//! Sequence on the BES2700 path: claim, set, off, release. Field order follows the
//! glasses' UART protocol doc (`color,on_ms,off_ms,cycles,brightness`), which gate G-LED
//! must confirm by eye. This module only renders lines; it never opens a port.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum LedColor {
    Red = 0,
    Green = 1,
    Blue = 2,
    Orange = 3,
    White = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LedPattern {
    pub color: LedColor,
    pub on_ms: u32,
    pub off_ms: u32,
    pub cycles: u32,
    pub brightness: u8,
}

impl LedPattern {
    /// A steady colour is one long `on_ms`; the cycle count is the only repetition control.
    pub const STEADY_ON_MS: u32 = 60_000;

    pub fn steady(color: LedColor, brightness: u8) -> Self {
        Self {
            color,
            on_ms: Self::STEADY_ON_MS,
            off_ms: 0,
            cycles: 1,
            brightness,
        }
    }
}

pub fn bes_claim() -> String {
    "{\"C\":\"android_control_led\",\"V\":1,\"B\":\"1\"}\n".into()
}

pub fn bes_release() -> String {
    "{\"C\":\"android_control_led\",\"V\":1,\"B\":\"0\"}\n".into()
}

pub fn bes_off() -> String {
    "{\"C\":\"cs_ledoff\",\"V\":1,\"B\":\"\"}\n".into()
}

/// Field order of the `cs_ledon` argument string. The glasses' own notes disagree (design
/// section 3.6); gate G-LED settles it by eye, so the glasses agent takes it as config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum LedFieldOrder {
    /// `color,on_ms,off_ms,cycles,brightness` (UART protocol doc, primary).
    #[default]
    ColorOnOffCyclesBrightness,
    /// `color,brightness,on_ms,off_ms,cycles` (testing notes).
    ColorBrightnessOnOffCycles,
}

/// One whole line; callers serialize writes with a lock so lines never interleave.
pub fn bes_set(p: &LedPattern) -> String {
    bes_set_ordered(p, LedFieldOrder::default())
}

/// [`bes_set`] with an explicit `cs_ledon` field order.
pub fn bes_set_ordered(p: &LedPattern, order: LedFieldOrder) -> String {
    let c = p.color as u8;
    let args = match order {
        LedFieldOrder::ColorOnOffCyclesBrightness => {
            format!("{c},{},{},{},{}", p.on_ms, p.off_ms, p.cycles, p.brightness)
        }
        LedFieldOrder::ColorBrightnessOnOffCycles => {
            format!("{c},{},{},{},{}", p.brightness, p.on_ms, p.off_ms, p.cycles)
        }
    };
    format!("{{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"{args}\"}}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_documented_example() {
        // Design example: green, 3100 on, 100 off, 1 cycle, brightness 180.
        let p = LedPattern {
            color: LedColor::Green,
            on_ms: 3100,
            off_ms: 100,
            cycles: 1,
            brightness: 180,
        };
        assert_eq!(
            bes_set(&p),
            "{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"1,3100,100,1,180\"}\n"
        );
        assert!(bes_claim().contains("\"B\":\"1\""));
        assert!(bes_release().contains("\"B\":\"0\""));
        assert!(bes_off().contains("cs_ledoff"));
    }

    #[test]
    fn field_order_is_selectable() {
        let p = LedPattern {
            color: LedColor::Green,
            on_ms: 500,
            off_ms: 400,
            cycles: 3,
            brightness: 200,
        };
        assert_eq!(
            bes_set_ordered(&p, LedFieldOrder::ColorOnOffCyclesBrightness),
            "{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"1,500,400,3,200\"}\n"
        );
        assert_eq!(
            bes_set_ordered(&p, LedFieldOrder::ColorBrightnessOnOffCycles),
            "{\"C\":\"cs_ledon\",\"V\":1,\"B\":\"1,200,500,400,3\"}\n"
        );
        assert_eq!(bes_set(&p), bes_set_ordered(&p, LedFieldOrder::default()));
    }

    #[test]
    fn lines_are_valid_json() {
        for l in [
            bes_claim(),
            bes_release(),
            bes_off(),
            bes_set(&LedPattern::steady(LedColor::Blue, 120)),
        ] {
            assert!(l.ends_with('\n'));
            serde_json::from_str::<serde_json::Value>(l.trim_end()).unwrap();
        }
    }
}
