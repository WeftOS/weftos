//! Spatial BVH as a kernel [`SystemService`] (ADR-035 / WEFT-718).
//!
//! Wraps an [`Arc<dyn SpatialBackend>`] (typically [`BvhBackend`]) with
//! health/status counters. Mirrors [`crate::hnsw_service::HnswService`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use clawft_bvh::{
    Aabb, BranchId, BranchMeta, DiffEntry, Frustum, Leaf, LeafId, Ray, RayHit, Vec3,
};
use clawft_types::config::SpatialConfig;
use serde::{Deserialize, Serialize};

use crate::health::HealthStatus;
use crate::service::{ServiceType, SystemService};
use crate::spatial_backend::{SpatialBackend, SpatialResult};
use crate::spatial_bvh::{BvhBackend, KernelBvhChainSink};

#[cfg(feature = "exochain")]
use crate::chain::ChainManager;

// ── Config bridge ────────────────────────────────────────────────────────

/// Service-level config (mirrors types `SpatialConfig`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialServiceConfig {
    /// Whether the service should be considered active.
    pub enabled: bool,
    /// Max leaves per branch.
    pub max_leaves: usize,
    /// Phase commit threshold (Phase D).
    pub phase_commit_threshold: usize,
}

impl Default for SpatialServiceConfig {
    fn default() -> Self {
        Self::from_spatial_config(&SpatialConfig::default())
    }
}

impl SpatialServiceConfig {
    /// Build from `clawft-types` kernel spatial config.
    pub fn from_spatial_config(cfg: &SpatialConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            max_leaves: cfg.max_leaves,
            phase_commit_threshold: cfg.phase_commit_threshold,
        }
    }

    /// Convert to `clawft_bvh::BvhStoreConfig`.
    pub fn to_store_config(&self) -> clawft_bvh::BvhStoreConfig {
        clawft_bvh::BvhStoreConfig {
            max_leaves: self.max_leaves,
            phase_commit_threshold: self.phase_commit_threshold,
        }
    }
}

// ── Status ───────────────────────────────────────────────────────────────

/// Operator-facing status snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialServiceStatus {
    /// Backend name (`"bvh"`).
    pub backend: String,
    /// Leaf count on active branch.
    pub leaf_count: usize,
    /// Monotonic epoch.
    pub epoch: u64,
    /// Number of branches.
    pub branch_count: usize,
    /// Lifetime insert ops.
    pub insert_count: u64,
    /// Lifetime query ops.
    pub query_count: u64,
    /// Configured max leaves (0 = unbounded).
    pub max_leaves: usize,
    /// Whether the service is enabled in config.
    pub enabled: bool,
}

// ── Service ──────────────────────────────────────────────────────────────

/// Kernel service for the spatial BVH index.
pub struct SpatialService {
    backend: Arc<dyn SpatialBackend>,
    /// Concrete BVH handle for snapshot / seal helpers (optional for future
    /// non-BVH backends).
    bvh: Option<Arc<BvhBackend>>,
    config: SpatialServiceConfig,
    insert_count: AtomicU64,
    query_count: AtomicU64,
    started: std::sync::atomic::AtomicBool,
}

impl SpatialService {
    /// Wrap an existing backend.
    pub fn new(config: SpatialServiceConfig, backend: Arc<dyn SpatialBackend>) -> Self {
        Self {
            backend,
            bvh: None,
            config,
            insert_count: AtomicU64::new(0),
            query_count: AtomicU64::new(0),
            started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Construct the default BVH backend from config.
    pub fn from_config(config: SpatialServiceConfig) -> Self {
        let bvh = Arc::new(BvhBackend::new(config.to_store_config()));
        let backend: Arc<dyn SpatialBackend> = bvh.clone();
        Self {
            backend,
            bvh: Some(bvh),
            config,
            insert_count: AtomicU64::new(0),
            query_count: AtomicU64::new(0),
            started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Construct from types-layer `SpatialConfig`.
    pub fn from_spatial_config(cfg: &SpatialConfig) -> Self {
        Self::from_config(SpatialServiceConfig::from_spatial_config(cfg))
    }

    /// Rebuild the BVH from `spatial_bvh` events on `cm` (restart path). Must run before
    /// [`Self::attach_chain`] so the replay is not appended to the chain again. Returns
    /// `(applied, skipped)`.
    #[cfg(feature = "exochain")]
    pub fn restore_from_chain(&self, cm: &ChainManager) -> (usize, usize) {
        match self.bvh {
            Some(ref bvh) => {
                let r = bvh.restore_from_chain(&cm.tail(0));
                (r.applied, r.skipped)
            }
            None => (0, 0),
        }
    }

    /// Attach ExoChain sink to the underlying BVH backend.
    #[cfg(feature = "exochain")]
    pub fn attach_chain(&self, cm: Arc<ChainManager>) {
        if let Some(ref bvh) = self.bvh {
            bvh.set_chain_sink(Arc::new(KernelBvhChainSink::new(cm)));
        }
    }

    /// Shared backend handle.
    pub fn backend(&self) -> &Arc<dyn SpatialBackend> {
        &self.backend
    }

    /// Concrete BVH backend when available.
    pub fn bvh(&self) -> Option<&Arc<BvhBackend>> {
        self.bvh.as_ref()
    }

    /// Insert a leaf.
    pub fn insert(&self, leaf: Leaf) -> SpatialResult<LeafId> {
        let id = self.backend.insert(leaf)?;
        self.insert_count.fetch_add(1, Ordering::Relaxed);
        Ok(id)
    }

    /// Remove a leaf.
    pub fn remove(&self, id: LeafId) -> bool {
        self.backend.remove(id)
    }

    /// Get a leaf.
    pub fn get(&self, id: LeafId) -> Option<Leaf> {
        self.backend.get(id)
    }

    /// Query point.
    pub fn query_point(&self, p: Vec3) -> Vec<LeafId> {
        self.query_count.fetch_add(1, Ordering::Relaxed);
        self.backend.query_point(p)
    }

    /// Query AABB.
    pub fn query_aabb(&self, bb: Aabb) -> Vec<LeafId> {
        self.query_count.fetch_add(1, Ordering::Relaxed);
        self.backend.query_aabb(bb)
    }

    /// Query sphere.
    pub fn query_sphere(&self, center: Vec3, radius: f32) -> Vec<LeafId> {
        self.query_count.fetch_add(1, Ordering::Relaxed);
        self.backend.query_sphere(center, radius)
    }

    /// Query ray.
    pub fn query_ray(&self, ray: Ray, max_t: f32) -> Vec<RayHit> {
        self.query_count.fetch_add(1, Ordering::Relaxed);
        self.backend.query_ray(ray, max_t)
    }

    /// Query frustum.
    pub fn query_frustum(&self, frustum: &Frustum) -> Vec<LeafId> {
        self.query_count.fetch_add(1, Ordering::Relaxed);
        self.backend.query_frustum(frustum)
    }

    /// Query k-NN.
    pub fn query_knn(&self, p: Vec3, k: usize) -> Vec<LeafId> {
        self.query_count.fetch_add(1, Ordering::Relaxed);
        self.backend.query_knn(p, k)
    }

    /// Derive a branch.
    pub fn derive_branch(&self, meta: BranchMeta) -> SpatialResult<BranchId> {
        self.backend.derive_branch(meta)
    }

    /// Branch diff.
    pub fn branch_diff(&self, a: BranchId, b: BranchId, region: Aabb) -> Vec<DiffEntry> {
        self.backend.branch_diff(a, b, region)
    }

    /// Status snapshot for CLI / health_detail.
    pub fn status(&self) -> SpatialServiceStatus {
        let branch_count = self.bvh.as_ref().map(|b| b.branch_count()).unwrap_or(1);
        SpatialServiceStatus {
            backend: self.backend.backend_name().to_string(),
            leaf_count: self.backend.len(),
            epoch: self.backend.epoch(),
            branch_count,
            insert_count: self.insert_count.load(Ordering::Relaxed),
            query_count: self.query_count.load(Ordering::Relaxed),
            max_leaves: self.config.max_leaves,
            enabled: self.config.enabled,
        }
    }
}

#[async_trait]
impl SystemService for SpatialService {
    fn name(&self) -> &str {
        "ecc.spatial"
    }

    fn service_type(&self) -> ServiceType {
        ServiceType::Core
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.started.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn health_check(&self) -> HealthStatus {
        if !self.config.enabled {
            return HealthStatus::Healthy; // disabled is not unhealthy
        }
        if self.started.load(Ordering::SeqCst) {
            HealthStatus::Healthy
        } else {
            HealthStatus::Degraded("spatial service not started".into())
        }
    }

    fn health_detail(&self) -> Option<String> {
        let s = self.status();
        Some(format!(
            "backend={} leaves={} epoch={} branches={} inserts={} queries={}",
            s.backend, s.leaf_count, s.epoch, s.branch_count, s.insert_count, s.query_count
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_bvh::IdentityKind;

    #[tokio::test]
    async fn service_lifecycle_and_status() {
        let svc = SpatialService::from_config(SpatialServiceConfig {
            enabled: true,
            max_leaves: 100,
            phase_commit_threshold: 16,
        });
        assert_eq!(svc.name(), "ecc.spatial");
        svc.start().await.unwrap();
        let id = svc
            .insert(Leaf::empty_payload(
                Aabb::unit(),
                IdentityKind::Object,
                1,
            ))
            .unwrap();
        assert!(svc.query_point(Vec3::ZERO).contains(&id));
        let st = svc.status();
        assert_eq!(st.leaf_count, 1);
        assert_eq!(st.insert_count, 1);
        assert!(matches!(svc.health_check().await, HealthStatus::Healthy));
        assert!(svc.health_detail().unwrap().contains("leaves=1"));
        svc.stop().await.unwrap();
    }
}
