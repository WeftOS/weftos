//! Promote and archive (ADR-126 section 6): lifecycle moves of whole stores.

use serde_json::json;

use super::{ARCHIVE_BUCKET, PromoteReport, StoreService, archive_key};
use crate::access::Op;
use crate::audit::{Outcome, Source};
use crate::backend::BackendError;
use crate::name::StoreName;
use crate::StoreError;

impl StoreService {
    // ---- promote and archive ------------------------------------------------------------------

    /// Copy a worktree store's objects under `promotable` prefixes into its project store.
    /// Everything else is archived (`archive_rest`) or left where it is.
    pub fn promote(&self, worktree: &StoreName, promotable: &[String], archive_rest: bool, actor: &str) -> Result<PromoteReport, StoreError> {
        if !matches!(worktree, StoreName::Worktree { .. }) {
            return Err(StoreError::Refused(format!("{worktree} is not a worktree store")));
        }
        let project = worktree.project_store().ok_or_else(|| StoreError::BadName(worktree.to_string()))?;
        if self.known(worktree)?.archived {
            return Err(StoreError::Refused(format!("{worktree} is archived")));
        }
        let prec = self.known(&project).map_err(|_| StoreError::Refused(format!("the project store {project} does not exist; create it first")))?;
        if prec.archived {
            return Err(StoreError::Refused(format!("{project} is archived")));
        }
        let (wb, pb) = (worktree.bucket(), project.bucket());
        let mut rep = PromoteReport::default();
        let ts = self.now();
        let mut step = || -> Result<(), StoreError> {
            for m in self.objects.list(&wb, "")? {
                if promotable.iter().any(|p| m.key.starts_with(p.as_str())) {
                    match self.objects.head(&pb, &m.key) {
                        Ok(h) if h.etag == m.etag => {
                            rep.unchanged += 1;
                            continue;
                        }
                        Ok(_) => {
                            // Keep what is being replaced.
                            self.ensure_archive()?;
                            self.objects.copy(&pb, &m.key, ARCHIVE_BUCKET, &archive_key(&pb, ts, &format!("replaced/{}", m.key)))?;
                            rep.replaced.push(m.key.clone());
                        }
                        Err(BackendError::NoObject(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                    self.objects.copy(&wb, &m.key, &pb, &m.key)?;
                    self.emit(actor, &project, Op::Copy.name(), Some(&m.key), m.size, Outcome::Ok, Source::Governed);
                    rep.promoted += 1;
                } else if archive_rest {
                    self.ensure_archive()?;
                    self.objects.copy(&wb, &m.key, ARCHIVE_BUCKET, &archive_key(&wb, ts, &m.key))?;
                    rep.archived += 1;
                } else {
                    rep.left += 1;
                }
            }
            Ok(())
        };
        if let Err(e) = step() {
            self.emit(actor, worktree, "Promote", None, 0, Outcome::Error(e.to_string()), Source::Governed);
            self.chain.record("store.promote.failed", json!({"from": worktree.to_string(), "to": project.to_string(), "promoted_before_failure": rep.promoted, "error": e.to_string(), "actor": actor}));
            return Err(e);
        }
        self.chain.record("store.promote", json!({"from": worktree.to_string(), "to": project.to_string(), "promoted": rep.promoted, "replaced": rep.replaced.len(), "archived": rep.archived, "actor": actor}));
        self.emit(actor, worktree, "Promote", None, 0, Outcome::Ok, Source::Governed);
        Ok(rep)
    }

    fn ensure_archive(&self) -> Result<(), StoreError> {
        match self.admin.create_bucket(ARCHIVE_BUCKET, &[("kind".into(), "archive".into())]) {
            Ok(()) | Err(BackendError::BucketExists(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Close the store (every grant disabled, requests refused), then copy every object to the
    /// archive bucket; with `delete`, remove the bucket and forget the store. Closing comes first,
    /// so nothing is accepted after the copy starts. A run that failed part-way is resumed by
    /// calling `archive` again (same archive keys, so nothing is duplicated); a finished archive
    /// is refused again unless `delete`. Returns the number of objects archived.
    pub fn archive(&self, name: &StoreName, delete: bool, actor: &str) -> Result<u64, StoreError> {
        let rec = self.known(name)?;
        if rec.archived && rec.archive_done && !delete {
            return Err(StoreError::Refused(format!("{name} is already archived")));
        }
        self.ensure_archive()?;
        let b = name.bucket();
        let principals = self.principals_of(name);
        if !rec.archived {
            {
                let now = self.now();
                let mut st = self.lock();
                Self::seq_room(&st)?;
                for p in &principals {
                    st.grants.set_enabled(p, name, false);
                }
                if let Some(r) = st.stores.get_mut(&name.to_string()) {
                    r.archived = true;
                    r.archive_done = false;
                    r.archive_ts = Some(now);
                }
                // Every grant on the store went off at once: one stamped event says so.
                let seq = self.stamp(&mut st);
                self.persist(&st)?;
                self.chain.record("store.close", json!({"seq": seq, "store": name.to_string(), "actor": actor, "grants_disabled": principals.len()}));
            }
            for p in &principals {
                self.apply_principal(p)?;
            }
        }
        // The archive keys carry the time the archive began, kept across a resume.
        let ts = {
            let now = self.now();
            let mut st = self.lock();
            match st.stores.get_mut(&name.to_string()) {
                Some(r) => *r.archive_ts.get_or_insert(now),
                None => now,
            }
        };
        let objs = self.objects.list(&b, "")?;
        let run = || -> Result<(), StoreError> {
            for m in &objs {
                self.objects.copy(&b, &m.key, ARCHIVE_BUCKET, &archive_key(&b, ts, &m.key))?;
            }
            if delete {
                for m in &objs {
                    self.objects.delete(&b, &m.key)?;
                }
                self.admin.delete_bucket(&b)?;
            }
            Ok(())
        };
        if let Err(e) = run() {
            self.emit(actor, name, "Archive", None, 0, Outcome::Error(e.to_string()), Source::Governed);
            return Err(e);
        }
        if !delete {
            let mut st = self.lock();
            if let Some(r) = st.stores.get_mut(&name.to_string()) {
                r.archive_done = true;
                r.archive_count = objs.len() as u64;
            }
            self.persist(&st)?;
        }
        if delete {
            {
                let mut st = self.lock();
                st.grants.drop_store(name);
                st.stores.remove(&name.to_string());
                self.persist(&st)?;
            }
            // The principals' keys must not keep a policy naming the deleted bucket.
            for p in &principals {
                self.apply_principal(p)?;
            }
        }
        {
            let mut st = self.lock();
            let seq = if Self::seq_room(&st).is_ok() { Some(self.stamp(&mut st)) } else { None };
            self.persist(&st)?;
            self.chain.record("store.archive", json!({"seq": seq, "store": name.to_string(), "objects": objs.len(), "deleted": delete, "actor": actor}));
        }
        self.emit(actor, name, if delete { "ArchiveDelete" } else { "Archive" }, None, objs.iter().map(|m| m.size).sum(), Outcome::Ok, Source::Governed);
        Ok(objs.len() as u64)
    }
}
