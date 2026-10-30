//! Durable lineage manifest -- closes `BranchableMemory::open`'s documented
//! lossy-reopen gap for everything except a `working` node's uncommitted
//! (not-yet-checkpointed) edits, which are deliberately still not captured
//! here. Every topology-changing operation (`create`, `checkpoint`,
//! `rollback`, `promote`, `branch`/`fork` -- for the new lineage --,
//! `pause`, `resume`) writes a fresh manifest after it succeeds; plain
//! `ingest`/`delete` against `working` do not, by design, so a crash
//! between checkpoints still loses whatever `working` hadn't been
//! checkpointed yet.
//!
//! # What gets restored on `BranchableMemory::open`
//!
//! - The full node topology: `base`, this lineage's own checkpoint chain
//!   (`ancestors`), and anything inherited from a fork/branch source
//!   (`inherited`) -- each reopened from its own `.rvf` file (writable for
//!   `base`, read-only for everything else, matching their roles).
//! - Each node's tombstone set. This is NOT redundant with RVF's own
//!   on-disk deletion bitmap: `BranchableMemory::delete` deliberately never
//!   calls `RvfStore::delete` for a locally-present id (see its doc
//!   comment), so an ancestor's or `working`'s tombstones exist *only* at
//!   this crate-level bookkeeping layer, and would silently resurrect on
//!   reopen without this manifest.
//! - The process-wide id-counter watermarks (`CheckpointId` and vector-id
//!   allocators), bumped past whatever this lineage already used, so a
//!   freshly restarted process cannot hand out an id that collides with
//!   one already physically present in a restored `.rvf` file.
//! - The `paused` flag: a lineage parked via `BranchableMemory::pause`
//!   reopens still parked (`working` stays `None`); `resume()` afterward
//!   works exactly as it would have pre-restart.
//!
//! # What is NOT restored (documented loss, not a bug)
//!
//! - `working`'s own uncommitted edits/tombstones/texts/tags, when the
//!   lineage is reopened *not* paused -- `working` is always re-derived
//!   fresh from the restored head in that case, same as the pre-manifest
//!   `open()` behaved for `base`.
//! - Every restored node's `edit_log` (the raw ingested vector floats,
//!   kept in-process for `diff()`/`promote()`'s replay) and `texts`/`tags`.
//!   `rvf-runtime` 0.2's public `RvfStore` API has no vector-by-id getter
//!   (only `query`), so these cannot be reconstructed from the `.rvf` file
//!   alone, and this manifest does not duplicate the raw vector data to
//!   avoid doubling every lineage's on-disk footprint. Practical
//!   consequence: a restored ancestor/`base` node is fully **queryable**
//!   (the physical file has its vectors) but a restored ancestor is not
//!   **promotable** -- `promote()`'s replay reads from `edit_log`, which is
//!   empty for a just-reopened node. `texts`/`tags` were already documented
//!   as process-lifetime-only before this manifest existed (see
//!   `VectorItem::text`'s doc comment); that loss is unchanged, just now
//!   explicit here too.

use std::collections::HashSet;
use std::path::Path;

use rvf_runtime::options::DistanceMetric;
use rvf_runtime::{RvfOptions, RvfStore};
use serde::{Deserialize, Serialize};

use crate::branchable_memory::BranchableMemory;
use crate::error::{CowMemoryError, Result};
use crate::id_gen;
use crate::node::{CheckpointId, MemoryNode};

const MANIFEST_SCHEMA_VERSION: u32 = 1;
const MANIFEST_FILE_NAME: &str = "manifest.json";
const MANIFEST_TMP_FILE_NAME: &str = "manifest.json.tmp";

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum ManifestRole {
    Ancestor,
    Base,
    Inherited,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestNode {
    role: ManifestRole,
    checkpoint_id: u64,
    label: Option<String>,
    file_name: String,
    created_at_ms: u64,
    tombstones: Vec<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LineageManifest {
    schema_version: u32,
    dimension: u16,
    paused: bool,
    next_checkpoint_id: u64,
    next_vector_id: u64,
    ancestors: Vec<ManifestNode>,
    base: ManifestNode,
    inherited: Vec<ManifestNode>,
}

fn manifest_node_for(node: &MemoryNode, role: ManifestRole) -> ManifestNode {
    ManifestNode {
        role,
        checkpoint_id: node.id.0,
        label: node.label.clone(),
        file_name: node
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string(),
        created_at_ms: node.created_at_ms,
        tombstones: node.tombstones.iter().copied().collect(),
    }
}

impl BranchableMemory {
    fn build_manifest(&self) -> LineageManifest {
        LineageManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            dimension: self.opts.dimension,
            paused: self.working.is_none(),
            next_checkpoint_id: CheckpointId::watermark(),
            next_vector_id: id_gen::watermark(),
            ancestors: self
                .ancestors
                .iter()
                .map(|n| manifest_node_for(n, ManifestRole::Ancestor))
                .collect(),
            base: manifest_node_for(&self.base, ManifestRole::Base),
            inherited: self
                .inherited
                .iter()
                .map(|n| manifest_node_for(n, ManifestRole::Inherited))
                .collect(),
        }
    }

    /// Atomically (temp file + rename) write this lineage's manifest into
    /// `self.dir`. Called after every topology-changing operation -- see
    /// module docs for the exact list. A crash mid-write leaves at most a
    /// stray `manifest.json.tmp`, which `open()` never reads (it only ever
    /// opens `manifest.json` by name), so it is silently ignored on the
    /// next open rather than mistaken for a real manifest.
    pub(crate) fn write_manifest(&self) -> Result<()> {
        let manifest = self.build_manifest();
        let json =
            serde_json::to_vec_pretty(&manifest).map_err(|e| CowMemoryError::ManifestCorrupt {
                path: self.dir.join(MANIFEST_FILE_NAME),
                reason: format!("failed to serialize manifest: {e}"),
            })?;
        let tmp_path = self.dir.join(MANIFEST_TMP_FILE_NAME);
        std::fs::write(&tmp_path, &json)?;
        std::fs::rename(&tmp_path, self.dir.join(MANIFEST_FILE_NAME))?;
        Ok(())
    }

    /// Best-effort read of `dir`'s manifest. `Ok(None)` means no manifest
    /// exists (a pre-manifest lineage, or one that was never checkpointed/
    /// paused/etc. before this reopen) -- `BranchableMemory::open` falls
    /// back to its original lossy base-only behavior in that case. A
    /// manifest that exists but fails to parse, or names a schema version
    /// this crate doesn't understand, is a hard error -- never a silent
    /// partial restore.
    fn read_manifest(dir: &Path) -> Result<Option<LineageManifest>> {
        let path = dir.join(MANIFEST_FILE_NAME);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let manifest: LineageManifest =
            serde_json::from_slice(&bytes).map_err(|e| CowMemoryError::ManifestCorrupt {
                path: path.clone(),
                reason: format!("failed to parse manifest json: {e}"),
            })?;
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(CowMemoryError::ManifestCorrupt {
                path,
                reason: format!(
                    "unsupported manifest schema_version {} (expected {})",
                    manifest.schema_version, MANIFEST_SCHEMA_VERSION
                ),
            });
        }
        Ok(Some(manifest))
    }

    /// Reopen a full lineage from `dir` using its manifest, if one exists.
    /// `Ok(None)` means no manifest was found -- the caller (`open()`)
    /// falls back to the legacy base-only reopen.
    pub(crate) fn open_with_manifest(dir: &Path) -> Result<Option<BranchableMemory>> {
        let manifest = match Self::read_manifest(dir)? {
            Some(m) => m,
            None => return Ok(None),
        };

        // Bump past this lineage's ids *before* reopening any node, so
        // that even if reopening a later node fails partway through, no
        // id this lineage already used can be handed out by a fresh
        // `ingest`/`checkpoint` elsewhere in this process.
        CheckpointId::bump_watermark(manifest.next_checkpoint_id);
        id_gen::bump_watermark(manifest.next_vector_id);

        let opts = RvfOptions {
            dimension: manifest.dimension,
            metric: DistanceMetric::L2,
            ..Default::default()
        };

        let mut base = Self::reopen_node(dir, &manifest.base, false)?;
        let mut ancestors: Vec<MemoryNode> = Vec::with_capacity(manifest.ancestors.len());
        for n in &manifest.ancestors {
            ancestors.push(Self::reopen_node(dir, n, true)?);
        }
        let mut inherited = Vec::with_capacity(manifest.inherited.len());
        for n in &manifest.inherited {
            inherited.push(Self::reopen_node(dir, n, true)?);
        }

        let working = if manifest.paused {
            None
        } else {
            let head = match ancestors.first_mut() {
                Some(n) => n,
                None => &mut base,
            };
            let working_id = CheckpointId::alloc();
            let working_path = dir.join(format!("node-{}.working.rvf", working_id.0));
            Some(MemoryNode::child_of(
                working_id,
                head,
                &working_path,
                opts.clone(),
                Some("working".into()),
            )?)
        };

        Ok(Some(BranchableMemory {
            dir: dir.to_path_buf(),
            opts,
            working,
            ancestors,
            base,
            inherited,
        }))
    }

    fn reopen_node(dir: &Path, n: &ManifestNode, read_only: bool) -> Result<MemoryNode> {
        let path = dir.join(&n.file_name);
        let store = if read_only {
            RvfStore::open_readonly(&path)?
        } else {
            RvfStore::open(&path)?
        };
        let mut node = MemoryNode::adopt(CheckpointId::alloc(), store, path, n.label.clone());
        node.created_at_ms = n.created_at_ms;
        node.frozen = read_only;
        node.tombstones = n.tombstones.iter().copied().collect::<HashSet<u64>>();
        Ok(node)
    }
}
