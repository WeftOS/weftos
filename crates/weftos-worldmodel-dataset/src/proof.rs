//! Proof-tag counting shared by evidence and readings.

use serde::{Deserialize, Serialize};

/// Counts per ADR-111 proof tag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCounts {
    #[serde(rename = "MEASURED")]
    pub measured: u64,
    #[serde(rename = "CODE")]
    pub code: u64,
    #[serde(rename = "SYNTHETIC")]
    pub synthetic: u64,
}

impl ProofCounts {
    /// Count one tag. Returns false when the tag is not one of the three.
    pub fn add(&mut self, tag: &str) -> bool {
        match tag {
            "MEASURED" => self.measured += 1,
            "CODE" => self.code += 1,
            "SYNTHETIC" => self.synthetic += 1,
            _ => return false,
        }
        true
    }

    pub fn total(&self) -> u64 {
        self.measured + self.code + self.synthetic
    }

    pub fn merge(&mut self, other: &ProofCounts) {
        self.measured += other.measured;
        self.code += other.code;
        self.synthetic += other.synthetic;
    }
}
