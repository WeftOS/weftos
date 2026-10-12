//! Mirror of recorded versions into `clawft-cow-memory` lineages.
//!
//! One lineage per object at `<dir>/lineage/<kind>/<object>`. A version is a
//! single vector (its digest bytes, with the record as text) that is
//! `promote`d into the lineage's base, so base holds one vector per version.
//!
//! Derivations (a version whose parent belongs to another object) are NOT
//! `branch`ed: `clawft-cow-memory` cannot reopen a branched lineage (its
//! manifest names inherited nodes relative to the child directory, but they
//! live in the source directory, and the source's working node is deleted on
//! its next promote), so a second version of a derived object would fail to
//! mirror. The derivation edge is recorded in the JSON-lines file, which is
//! the record of truth and what the tree queries read.

use std::path::PathBuf;

use clawft_cow_memory::{BranchableMemory, CowMemoryError, VectorItem};

use crate::{VersionRecord, VersionsIndex};

/// Embedding width: one component per digest byte.
const DIM: u16 = 32;

fn embed(digest: &str) -> (u64, Vec<f32>) {
    let bytes: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&digest[7 + 2 * i..9 + 2 * i], 16).unwrap_or(0))
        .collect();
    let id = u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8]));
    // Offset keeps the all-zero digest from being a zero vector.
    (id, bytes.iter().map(|b| f32::from(*b) / 255.0 + 0.001).collect())
}

impl VersionsIndex {
    pub(crate) fn lineage_dir(&self, kind: &str, object: &str) -> PathBuf {
        self.dir().join("lineage").join(kind).join(object)
    }

    /// Whether `object` has a lineage.
    pub fn has_lineage(&self, kind: &str, object: &str) -> bool {
        self.lineage_dir(kind, object).join("manifest.json").exists()
    }

    /// The lineage of `object`, if mirrored.
    pub fn open_lineage(&self, kind: &str, object: &str) -> Result<BranchableMemory, CowMemoryError> {
        BranchableMemory::open(self.lineage_dir(kind, object))
    }

    pub(crate) fn mirror(&self, kind: &str, rec: &VersionRecord) -> Result<(), CowMemoryError> {
        let dir = self.lineage_dir(kind, &rec.object);
        let mut mem = if self.has_lineage(kind, &rec.object) {
            BranchableMemory::open(&dir)?
        } else {
            BranchableMemory::create(&dir, DIM)?
        };
        let (id, vector) = embed(&rec.digest);
        let text = serde_json::to_string(rec).unwrap_or_default();
        mem.ingest(&[VectorItem::new(vector).with_id(id).with_text(text)])?;
        mem.promote()?;
        Ok(())
    }
}
