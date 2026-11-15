//! [`SensorClass`]: sensor classes hosting edge-intelligence small models.

/// Sensor classes that host edge-intelligence small models.
///
/// Classes 0-5 match the sensor plane in the LeWM diagram (RGB-D, IMU,
/// proprio, LiDAR, audio, plus a generic / unknown bucket for future
/// classes). Classes 6-10 follow the ADR-111 / ADR-384 HAL mapping (plan
/// `docs/plans/world-model-data-and-training.md` section 1a): mmWave radar
/// carries `radar_range` and `radar_track_point`, UWB carries `uwb_echo` and
/// `radio_delay`, ToF carries `tof_depth`, CSI carries `rf_gaussian` and
/// `rf_link_observation`, and Reading carries SenML readings (ADR-111 s2).
/// Class 11 Acoustic carries `acoustic_range` (owner 2026-11-14: its own
/// class, not under Audio).
/// Discriminants are wire-stable and only ever appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
#[repr(u8)]
pub enum SensorClass {
    /// Unclassified / multi-class residual.
    #[default]
    Generic = 0,
    /// RGB-D camera.
    RgbD = 1,
    /// Inertial measurement unit.
    Imu = 2,
    /// Proprioception / joint state.
    Proprio = 3,
    /// LiDAR / depth clouds.
    Lidar = 4,
    /// Audio / microphone.
    Audio = 5,
    /// mmWave radar (LD2450 / LD6002): `radar_range`, `radar_track_point`.
    MmWave = 6,
    /// Ultra-wideband ranging: `uwb_echo`, `radio_delay`.
    Uwb = 7,
    /// Time-of-flight depth: `tof_depth`.
    Tof = 8,
    /// Wi-Fi channel state information: `rf_gaussian`, `rf_link_observation`.
    Csi = 9,
    /// SenML readings per ADR-111 section 2.
    Reading = 10,
    /// Acoustic ranging: `acoustic_range` (sound ranging, snapshot sounding).
    Acoustic = 11,
}

impl SensorClass {
    /// Stable wire / log name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::RgbD => "rgb_d",
            Self::Imu => "imu",
            Self::Proprio => "proprio",
            Self::Lidar => "lidar",
            Self::Audio => "audio",
            Self::MmWave => "mmwave",
            Self::Uwb => "uwb",
            Self::Tof => "tof",
            Self::Csi => "csi",
            Self::Reading => "reading",
            Self::Acoustic => "acoustic",
        }
    }

    /// ADR-111 record type names this class carries (empty when the class
    /// has no mapped evidence type).
    pub const fn evidence_types(self) -> &'static [&'static str] {
        match self {
            Self::Imu => &["imu_event"],
            Self::Acoustic => &["acoustic_range"],
            Self::MmWave => &["radar_range", "radar_track_point"],
            Self::Uwb => &["uwb_echo", "radio_delay"],
            Self::Tof => &["tof_depth"],
            Self::Csi => &["rf_gaussian", "rf_link_observation"],
            Self::Generic
            | Self::RgbD
            | Self::Proprio
            | Self::Lidar
            | Self::Audio
            | Self::Reading => &[],
        }
    }

    /// All named classes (tests / docs).
    pub const ALL: [SensorClass; 12] = [
        Self::Generic,
        Self::RgbD,
        Self::Imu,
        Self::Proprio,
        Self::Lidar,
        Self::Audio,
        Self::MmWave,
        Self::Uwb,
        Self::Tof,
        Self::Csi,
        Self::Reading,
        Self::Acoustic,
    ];
}

impl TryFrom<u8> for SensorClass {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Generic),
            1 => Ok(Self::RgbD),
            2 => Ok(Self::Imu),
            3 => Ok(Self::Proprio),
            4 => Ok(Self::Lidar),
            5 => Ok(Self::Audio),
            6 => Ok(Self::MmWave),
            7 => Ok(Self::Uwb),
            8 => Ok(Self::Tof),
            9 => Ok(Self::Csi),
            10 => Ok(Self::Reading),
            11 => Ok(Self::Acoustic),
            other => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensor_class_roundtrip() {
        for c in SensorClass::ALL {
            assert_eq!(SensorClass::try_from(c as u8), Ok(c));
            assert!(!c.as_str().is_empty());
        }
        assert_eq!(SensorClass::try_from(99), Err(99));
        assert_eq!(SensorClass::try_from(12), Err(12));
    }

    #[test]
    fn sensor_class_all_is_twelve_and_dense() {
        assert_eq!(SensorClass::ALL.len(), 12);
        for (i, c) in SensorClass::ALL.iter().enumerate() {
            assert_eq!(*c as u8 as usize, i);
        }
    }

    #[test]
    fn sensor_class_names_unique() {
        let mut names: alloc::vec::Vec<&str> =
            SensorClass::ALL.iter().map(|c| c.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SensorClass::ALL.len());
    }

    #[test]
    fn sensor_class_original_six_pinned() {
        let pinned = [
            (SensorClass::Generic, 0u8, "generic"),
            (SensorClass::RgbD, 1, "rgb_d"),
            (SensorClass::Imu, 2, "imu"),
            (SensorClass::Proprio, 3, "proprio"),
            (SensorClass::Lidar, 4, "lidar"),
            (SensorClass::Audio, 5, "audio"),
        ];
        for (c, d, n) in pinned {
            assert_eq!(c as u8, d);
            assert_eq!(c.as_str(), n);
        }
        let hal = [
            (SensorClass::MmWave, 6u8, "mmwave"),
            (SensorClass::Uwb, 7, "uwb"),
            (SensorClass::Tof, 8, "tof"),
            (SensorClass::Csi, 9, "csi"),
            (SensorClass::Reading, 10, "reading"),
            (SensorClass::Acoustic, 11, "acoustic"),
        ];
        for (c, d, n) in hal {
            assert_eq!(c as u8, d);
            assert_eq!(c.as_str(), n);
        }
    }

    #[test]
    fn sensor_class_evidence_types() {
        assert!(SensorClass::Generic.evidence_types().is_empty());
        assert!(SensorClass::RgbD.evidence_types().is_empty());
        assert!(SensorClass::Proprio.evidence_types().is_empty());
        assert!(SensorClass::Lidar.evidence_types().is_empty());
        assert!(SensorClass::Reading.evidence_types().is_empty());
        assert_eq!(SensorClass::Imu.evidence_types(), &["imu_event"]);
        assert!(SensorClass::Audio.evidence_types().is_empty());
        assert_eq!(SensorClass::Acoustic.evidence_types(), &["acoustic_range"]);
        assert_eq!(
            SensorClass::MmWave.evidence_types(),
            &["radar_range", "radar_track_point"]
        );
        assert_eq!(SensorClass::Uwb.evidence_types(), &["uwb_echo", "radio_delay"]);
        assert_eq!(SensorClass::Tof.evidence_types(), &["tof_depth"]);
        assert_eq!(
            SensorClass::Csi.evidence_types(),
            &["rf_gaussian", "rf_link_observation"]
        );
    }
}
