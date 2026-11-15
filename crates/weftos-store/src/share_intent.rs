//! Publish announcements (WO-069 lane Y, A4/A5; design note
//! `reports/wo069-fence-atomicity-design-20261115.md`). A child of `share`.
//!
//! The handoff record is the one object every decision about a worktree commits through with a
//! conditional write. The manifest is written with a plain PUT, so a fence that read the manifest
//! could not tell whether a publish was about to replace it. Now a publish announces the manifest
//! it is about to write (`Handoff::publishing`, by digest of the exact bytes) with a conditional
//! write right before the PUT, and a fenced merge or land refuses while an announcement is pending.
//! Every manifest PUT is then preceded by a record write: one after the merger read the record
//! makes the merger's conditional write fail; one before it is visible to the merger, which waits.
//!
//! B1 (the lease residual of the design note), closed by lane D3: the manifest PUT is now a
//! conditional write on the version the publish read, and right after it [`settle`] re-reads the
//! record. Merged or archived meanwhile (a publisher stalled past the TTL, or with a clock that
//! far behind, whose announcement a merger no longer counted as pending) → [`restore_base`] puts
//! the previous manifest back, conditionally, and the publish fails without sweeping; with
//! content-addressed objects the previous manifest is the whole previous tree. Otherwise the
//! announcement is marked done with a conditional write, so a merger that read the record before it
//! fails its own write and re-reads both the record and the (new) manifest.

use super::*;

/// What [`announce`] did, for the publish that called it.
pub(super) struct Announced {
    /// The record, when this announcement created it (no record existed): exactly what is stored.
    pub created: Option<Handoff>,
    /// The announcement this one replaced, put back by [`withdraw`] if the manifest PUT fails.
    pub previous: Option<PublishIntent>,
}

/// Is `i` still pending: its manifest is not the stored one (`manifest_sha`) and its stamp is live,
/// or it holds an earlier announcement whose stamp is live.
/// A `done` announcement (its conditional manifest write succeeded and the publisher settled it) is
/// never pending.
fn pending(i: &PublishIntent, manifest_sha: Option<&str>, now_ms: u64) -> bool {
    !i.done
        && ((manifest_sha != Some(i.manifest_sha256.as_str()) && stamp_is_live(i.ms, now_ms))
            || i.held_ms.is_some_and(|h| stamp_is_live(h, now_ms)))
}

/// For a fenced merge or land: refuse while the record's announcement is pending against the
/// manifest whose digest is `manifest_sha` (read after the record).
pub(super) fn refuse_pending(cur: &Handoff, manifest_sha: Option<&str>, now_ms: u64) -> Result<()> {
    match &cur.publishing {
        Some(i) if pending(i, manifest_sha, now_ms) => Err(S2Error::Backend(format!(
            "a publish of this worktree by {}@{} is in flight (announced at {} ms), so the published tree may \
             still change; the merge was not recorded, try again in a moment",
            i.writer.agent, i.writer.host, i.ms
        ))),
        _ => Ok(()),
    }
}

/// Announce `intent` just before the manifest PUT. `seen` is the record and etag the publish read
/// at its start and `manifest_sha` the digest of the manifest it read then; on a lost write both
/// are read again. Refuses (nothing announced, the manifest not written) when the worktree became
/// merged or archived, or someone else took a live claim, while the publish uploaded. A pending
/// announcement of another publish does not block this one: its stamp is carried as `held_ms`, so
/// merges wait for it instead (a bad clock must not block the writer).
pub(super) fn announce<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    writer: &Actor,
    now_ms: u64,
    mut seen: Option<(Handoff, String)>,
    mut manifest_sha: Option<String>,
    intent: PublishIntent,
) -> Result<Announced> {
    for attempt in 0..HANDOFF_RETRIES {
        if attempt > 0 {
            seen = read_handoff_versioned(b, bucket)?;
            manifest_sha = read_manifest_bytes(b, bucket)?.map(|(_, d)| sha256_hex(&d));
        }
        let mut next_intent = intent.clone();
        let Some((h, etag)) = &seen else {
            let fresh = Handoff {
                state: State::Open,
                note: String::new(),
                writer: writer.clone(),
                merger: None,
                updated_ms: now_ms,
                epoch: 0,
                claimed_head: None,
                claimed_tree: None,
                publishing: Some(next_intent),
            };
            if write_handoff_if(b, bucket, &fresh, None)? {
                return Ok(Announced { created: Some(fresh), previous: None });
            }
            continue;
        };
        if matches!(h.state, State::Merged | State::Archived) {
            return Err(S2Error::Invalid(format!(
                "worktree became {:?} while this publish ran; the manifest was not written",
                h.state
            )));
        }
        guard_live_claim(h, writer, now_ms)?;
        if let Some(old) = h.publishing.as_ref().filter(|o| !o.done) {
            let old_pending = (manifest_sha.as_deref() != Some(old.manifest_sha256.as_str())).then_some(old.ms);
            next_intent.held_ms = [old_pending, old.held_ms].into_iter().flatten().filter(|s| stamp_is_live(*s, now_ms)).max();
        }
        let next = Handoff { publishing: Some(next_intent), ..h.clone() };
        if write_handoff_if(b, bucket, &next, Some(etag))? {
            return Ok(Announced { created: None, previous: h.publishing.clone() });
        }
    }
    Err(contended())
}

/// The manifest PUT failed: put back the announcement this publish replaced, so merges are not held
/// for the TTL by a publish that is over. Only while the record still carries this publish's
/// announcement (`mine`, its manifest digest); one written since is someone else's and stays.
pub(super) fn withdraw<B: ObjectBackend>(b: &B, bucket: &str, mine: &str, previous: Option<PublishIntent>) -> Result<()> {
    for _ in 0..HANDOFF_RETRIES {
        let Some((h, etag)) = read_handoff_versioned(b, bucket)? else { return Ok(()) };
        if h.publishing.as_ref().map(|i| i.manifest_sha256.as_str()) != Some(mine) {
            return Ok(());
        }
        let next = Handoff { publishing: previous.clone(), ..h };
        if write_handoff_if(b, bucket, &next, Some(&etag))? {
            return Ok(());
        }
    }
    Err(contended())
}

/// What [`settle`] found after this publish's manifest write.
pub(super) enum Settled {
    /// Not merged or archived. The record as this call wrote it, when it settled the announcement.
    Kept(Option<Box<Handoff>>),
    /// Merged or archived while the publish ran: restore the previous manifest.
    Finalised(State),
}

/// The post-check after the manifest write (module doc). `mine` is the digest of the manifest
/// bytes this publish wrote. Only this publish's own announcement is settled (marked `done`, its
/// hold dropped: an earlier publish it held for can no longer land, its conditional write is on a
/// version that is gone); one written since is another publish's, decided by its own conditional
/// manifest write. The announcement is marked, not removed: removing it could return the record to
/// the exact bytes a merger read before the announcement, whose conditional write would then pass.
pub(super) fn settle<B: ObjectBackend>(b: &B, bucket: &str, mine: &str) -> Result<Settled> {
    for _ in 0..HANDOFF_RETRIES {
        let Some((h, etag)) = read_handoff_versioned(b, bucket)? else { return Ok(Settled::Kept(None)) };
        if matches!(h.state, State::Merged | State::Archived) {
            return Ok(Settled::Finalised(h.state));
        }
        if h.publishing.as_ref().map(|i| i.manifest_sha256.as_str()) != Some(mine) {
            return Ok(Settled::Kept(None));
        }
        let settled = h.publishing.clone().map(|i| PublishIntent { done: true, held_ms: None, ..i });
        let next = Handoff { publishing: settled, ..h };
        if write_handoff_if(b, bucket, &next, Some(&etag))? {
            return Ok(Settled::Kept(Some(Box::new(next))));
        }
    }
    Err(contended())
}

/// Put the previous manifest (`base`, its exact bytes) back over the one this publish wrote
/// (`mine`), only while the stored manifest is still `mine`. `false`: nothing to restore, or the
/// manifest changed again since.
pub(super) fn restore_base<B: ObjectBackend>(b: &B, bucket: &str, mine: &[u8], base: Option<&[u8]>) -> Result<bool> {
    let Some(base) = base else { return Ok(false) };
    let Some(h) = b.head(bucket, MANIFEST_KEY)? else { return Ok(false) };
    if b.get(bucket, MANIFEST_KEY)?.as_deref() != Some(mine) {
        return Ok(false);
    }
    b.put_cas(bucket, MANIFEST_KEY, base, Some(&h.etag))
}
