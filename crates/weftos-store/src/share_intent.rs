//! Publish announcements (WO-069 lane Y, A4; design note
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
//! Residual (design note B1, owner): a publisher stalled for longer than the TTL between its
//! announcement and its PUT, or with a clock that far behind, can still land a manifest after a
//! merge. Closing that needs a conditional manifest write.

use super::*;

/// What [`announce`] did, for the publish that called it.
pub(super) struct Announced {
    /// The record, when this announcement created it (no record existed): exactly what is stored.
    pub created: Option<Handoff>,
}

/// Is `i` still pending: its manifest is not the stored one (`manifest_sha`) and its stamp is live,
/// or it holds an earlier announcement whose stamp is live.
fn pending(i: &PublishIntent, manifest_sha: Option<&str>, now_ms: u64) -> bool {
    (manifest_sha != Some(i.manifest_sha256.as_str()) && stamp_is_live(i.ms, now_ms))
        || i.held_ms.is_some_and(|h| stamp_is_live(h, now_ms))
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
                return Ok(Announced { created: Some(fresh) });
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
        if let Some(old) = &h.publishing {
            let old_pending = (manifest_sha.as_deref() != Some(old.manifest_sha256.as_str())).then_some(old.ms);
            next_intent.held_ms = [old_pending, old.held_ms].into_iter().flatten().filter(|s| stamp_is_live(*s, now_ms)).max();
        }
        let next = Handoff { publishing: Some(next_intent), ..h.clone() };
        if write_handoff_if(b, bucket, &next, Some(etag))? {
            return Ok(Announced { created: None });
        }
    }
    Err(contended())
}
