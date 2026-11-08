//! Unpair and re-pair, the S3 side of S1's archive-on-unpair (ADR-126
//! section 3).
//!
//! S1 will archive a node's bucket when the node is unpaired, so a later node
//! that reuses the id does not inherit its objects. Until S1 posts the real
//! calls, this module holds the contract as two traits and the order of
//! steps, tested against a mock:
//!
//! 1. apply the unpair event and cut the node off (the existing
//!    `apply_and_enforce`: revoked set plus RustFS user disable);
//! 2. **seal** the id, so it cannot be paired again;
//! 3. archive the bucket ([`NodeArchiver`], S1);
//! 4. only then remove the user and policy ([`NodeProvisioner`]).
//!
//! A failure at 3 leaves the user disabled, not deleted, and the id sealed;
//! calling [`NodeLifecycle::unpair`] again resumes. A sealed id can be paired
//! again only when the archiver says its bucket is empty or gone.
//!
//! The sealed set is memory only. After a restart, rebuild it with
//! [`NodeLifecycle::seal_from_history`] from the chain's unpair events; the
//! bucket-empty probe, not a stored receipt, is what unseals.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::broker::{AuditSink, EdgeBroker, StsError, StsMinter};
use crate::clock::Clock;
use crate::grants::{apply_and_enforce, Applied, GrantEvent, GrantLedger, GrantOp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveReceipt {
    pub node_id: String,
    /// Where S1 put the archive (a prefix or bucket name; opaque here).
    pub location: String,
    pub objects: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    #[error("archive store unavailable: {0}")]
    Unavailable(String),
    #[error("archive refused: {0}")]
    Refused(String),
}

/// S1's archive-on-unpair, as this crate needs it.
pub trait NodeArchiver {
    /// Archive the node's bucket and return only when EVERY object is copied;
    /// a partial copy must be an `Err`, never an `Ok`, because the caller deletes
    /// the user on `Ok`. Emptying the bucket is optional: S1's
    /// `retire_node_store` copies and leaves the bucket, and refuses the id for
    /// good, so against S1 the unseal path below never fires (the id stays
    /// retired). Repeating after success is harmless.
    async fn archive_node(&self, node_id: &str) -> Result<ArchiveReceipt, ArchiveError>;
    /// True when the node's bucket holds nothing (or does not exist). S1 has
    /// no such call; a `list` does it.
    async fn bucket_is_empty(&self, node_id: &str) -> Result<bool, ArchiveError>;
}

/// Removes the node's RustFS user and policy. `RustfsMinter` implements it.
pub trait NodeProvisioner {
    async fn deprovision(&self, node_id: &str) -> Result<(), StsError>;
}

impl<H: crate::wire::ObjectTransport, K: crate::rustfs::NodeKeys, C: Clock> NodeProvisioner for crate::rustfs::RustfsMinter<H, K, C> {
    async fn deprovision(&self, node_id: &str) -> Result<(), StsError> {
        self.deprovision_node(node_id).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    #[error("node {0:?} was unpaired and its bucket is not archived and empty; it cannot be paired again")]
    IdRetired(String),
    #[error("not an unpair event")]
    NotUnpair,
    #[error("revoke failed: {0}")]
    Revoke(String),
    #[error("archive: {0}")]
    Archive(#[from] ArchiveError),
    #[error("deprovision failed: {0}")]
    Deprovision(String),
}

pub struct NodeLifecycle<R, P> {
    archiver: R,
    provisioner: P,
    sealed: Mutex<HashSet<String>>,
}

fn unpaired_node(ev: &GrantEvent) -> Option<&str> {
    (ev.op == GrantOp::RevokeAll && crate::grants::node_of_store(&ev.store) == Some(ev.principal.as_str())).then_some(ev.principal.as_str())
}

impl<R: NodeArchiver, P: NodeProvisioner> NodeLifecycle<R, P> {
    pub fn new(archiver: R, provisioner: P) -> Self {
        Self { archiver, provisioner, sealed: Mutex::new(HashSet::new()) }
    }

    pub fn is_sealed(&self, node_id: &str) -> bool {
        self.sealed.lock().expect("sealed").contains(node_id)
    }

    /// After a restart: seal every id the chain shows as unpaired.
    pub fn seal_from_history<'a>(&self, history: impl IntoIterator<Item = &'a GrantEvent>) {
        let mut s = self.sealed.lock().expect("sealed");
        for ev in history {
            if let Some(n) = unpaired_node(ev) {
                s.insert(n.to_owned());
            }
        }
    }

    /// Unpair a node: cut it off, seal the id, archive, then delete the user.
    /// Safe to call again after a failure.
    pub async fn unpair<M: StsMinter, A: AuditSink, C: Clock>(
        &self,
        ledger: &Arc<GrantLedger>,
        broker: &EdgeBroker<M, Arc<GrantLedger>, A, C>,
        ev: &GrantEvent,
    ) -> Result<ArchiveReceipt, LifecycleError> {
        let node = unpaired_node(ev).ok_or(LifecycleError::NotUnpair)?.to_owned();
        // Seal first: even if every later step fails the id cannot be reused.
        self.sealed.lock().expect("sealed").insert(node.clone());
        match apply_and_enforce(ledger, broker, ev).await {
            Ok(Applied::Changed { .. } | Applied::Stale) => {}
            Ok(Applied::OutOfRange) => return Err(LifecycleError::Revoke("event refused by the seq guard".into())),
            Err(e) => return Err(LifecycleError::Revoke(e.0)),
        }
        // A retry after a half-finished unpair must still disable the user.
        broker.revoke(&node).await.map_err(|e| LifecycleError::Revoke(e.0))?;
        let receipt = self.archiver.archive_node(&node).await?;
        self.provisioner.deprovision(&node).await.map_err(|e| LifecycleError::Deprovision(e.0))?;
        Ok(receipt)
    }

    /// May `node_id` be paired now? A never-unpaired id always may. A sealed
    /// one may only when its bucket is empty, and then it is unsealed.
    pub async fn check_pairable(&self, node_id: &str) -> Result<(), LifecycleError> {
        if !self.is_sealed(node_id) {
            return Ok(());
        }
        // Fail closed: a probe error is a refusal, not a pass.
        if self.archiver.bucket_is_empty(node_id).await.unwrap_or(false) {
            self.sealed.lock().expect("sealed").remove(node_id);
            Ok(())
        } else {
            Err(LifecycleError::IdRetired(node_id.to_owned()))
        }
    }
}
