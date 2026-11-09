//! Promote and archive (ADR-126 section 6): lifecycle moves of whole stores.

use serde_json::json;

use super::{ARCHIVE_BUCKET, PromoteReport, StoreService, archive_key};
use crate::access::Op;
use crate::audit::{Outcome, Source};
use crate::backend::BackendError;
use crate::name::StoreName;
use crate::StoreError;

/// Key areas that are the worktree overlay's (S2: `.wt/` holds the tombstones, the recorded bases
/// and the worktree's own metadata) or the project's own bookkeeping of worktrees. No promotable
/// prefix may name or cover them, and an object under them is never copied as a plain object.
pub const RESERVED_PROMOTE_AREAS: [&str; 2] = [".wt/", "worktrees/"];

/// A promotable prefix is checked like S2's `PromotionPolicy::validate` and more strictly: it must
/// name a directory (end in `/`), be a clean key path (no empty, `.` or `..` segment, no NUL or
/// backslash, no leading `/`), and not be, or sit under, a reserved area. The empty prefix, which
/// would promote the whole store, is refused by name.
pub fn check_promote_prefix(p: &str) -> Result<(), StoreError> {
    let bad = |why: &str| Err(StoreError::Refused(format!("promotable prefix {p:?}: {why}")));
    if p.is_empty() {
        return bad("empty (that would promote everything, state and tombstones included)");
    }
    if !p.ends_with('/') {
        return bad("must end in '/'");
    }
    if p.starts_with('/') || p.contains('\0') || p.contains('\\') || p.trim_end_matches('/').split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return bad("not a clean key path");
    }
    if let Some(r) = RESERVED_PROMOTE_AREAS.iter().find(|r| p.starts_with(**r) || r.starts_with(p)) {
        return bad(&format!("is, or covers, the reserved area {r:?}"));
    }
    Ok(())
}

impl StoreService {
    // ---- promote and archive ------------------------------------------------------------------

    /// Copy a worktree store's objects under `promotable` prefixes into its project store.
    /// Everything else is archived (`archive_rest`) or left where it is.
    ///
    /// This is the plain-store path, and it cannot stand in for the worktree overlay's landing
    /// (S2): a worktree whose bucket holds overlay state under `.wt/` (tombstones, recorded bases,
    /// its metadata) is refused, because copying it object by object would resurrect what the
    /// worktree deleted and skip the conflict check against the recorded base. Prefixes are checked
    /// by [`check_promote_prefix`], and nothing under a reserved area is ever copied.
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
        if promotable.is_empty() {
            return Err(StoreError::Refused("promote needs at least one promotable prefix".into()));
        }
        for p in promotable {
            check_promote_prefix(p)?;
        }
        let (wb, pb) = (worktree.bucket(), project.bucket());
        if !self.objects.list(&wb, ".wt/")?.is_empty() {
            return Err(StoreError::Refused(format!("{worktree} holds worktree overlay state under .wt/ (tombstones, bases, metadata): land it with the worktree landing path, not store.promote, which would copy deleted objects back and skip the conflict check")));
        }
        let mut rep = PromoteReport::default();
        let ts = self.now();
        let mut step = || -> Result<(), StoreError> {
            for m in self.objects.list(&wb, "")? {
                if RESERVED_PROMOTE_AREAS.iter().any(|r| m.key.starts_with(r)) {
                    // Never a plain object, whatever prefix would have matched it.
                    rep.left += 1;
                    continue;
                }
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

    pub fn ensure_archive(&self) -> Result<(), StoreError> {
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
        if !self.has_store(name) && matches!(name, StoreName::Worktree { .. }) {
            return self.archive_foreign_worktree(name, delete, actor);
        }
        let rec = self.known(name)?;
        if rec.archived && rec.archive_done && !delete {
            return Err(StoreError::Refused(format!("{name} is already archived")));
        }
        self.ensure_archive()?;
        let b = name.bucket();
        let principals = self.principals_of(name);
        if !rec.archived {
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
                // Every grant on the store went off at once: one stamped event says so, recorded
                // once the principals' backend keys match (see `flush_close`).
                r.close_pending = Some((actor.to_owned(), principals.len()));
            }
            self.persist(&st)?;
        }
        if self.lock().stores.get(&name.to_string()).is_some_and(|r| r.close_pending.is_some()) {
            let _one = self.apply.lock().unwrap_or_else(|e| e.into_inner());
            for p in &principals {
                self.apply_locked(p)?;
            }
            self.flush_close(name)?;
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

    /// Archive and delete a worktree store that another daemon created on the same backend. Store
    /// records and grants live in each daemon's own state, so this daemon has neither to close;
    /// the objects and the bucket are on the shared backend, and a worktree's bucket belongs to its
    /// project, not to the daemon that happened to create it. Only deletion is accepted: closing
    /// without deleting means disabling grants this daemon does not hold. The creating daemon's
    /// record and grants for the store remain until it drops them (`store archive` there, or a
    /// restart that reconciles), which is harmless because the bucket is gone.
    fn archive_foreign_worktree(&self, name: &StoreName, delete: bool, actor: &str) -> Result<u64, StoreError> {
        if !delete {
            return Err(StoreError::Refused(format!(
                "{name} was created by another daemon; only `--delete` can archive it from here (closing it means disabling grants this daemon does not hold)"
            )));
        }
        self.ensure_archive()?;
        let b = name.bucket();
        let ts = self.now();
        let objs = self.objects.list(&b, "")?;
        let run = || -> Result<(), StoreError> {
            for m in &objs {
                self.objects.copy(&b, &m.key, ARCHIVE_BUCKET, &archive_key(&b, ts, &m.key))?;
            }
            for m in &objs {
                self.objects.delete(&b, &m.key)?;
            }
            self.admin.delete_bucket(&b)?;
            Ok(())
        };
        if let Err(e) = run() {
            self.emit(actor, name, "Archive", None, 0, Outcome::Error(e.to_string()), Source::Governed);
            return Err(e);
        }
        self.chain.record("store.archive", json!({"store": name.to_string(), "objects": objs.len(), "deleted": true, "actor": actor, "foreign": true}));
        self.emit(actor, name, "ArchiveDelete", None, objs.iter().map(|m| m.size).sum(), Outcome::Ok, Source::Governed);
        Ok(objs.len() as u64)
    }
}
