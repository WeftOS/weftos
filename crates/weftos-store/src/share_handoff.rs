//! Handoff half of `share`: the handoff record and the claim / merged / landed transitions. A child
//! of `share` so it shares that module's private helpers; `share` re-exports every item here, so
//! paths are unchanged.

use super::*;

pub fn read_handoff<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Option<Handoff>> {
    match b.get(bucket, HANDOFF_KEY)? {
        Some(d) => Ok(Some(serde_json::from_slice(&d)?)),
        None => Ok(None),
    }
}

/// The handoff record with the etag it was read at, for a conditional write. The etag is taken
/// *before* the body: if the record changes between the two calls the etag is the older one and the
/// conditional write fails (safe), whereas the other order could pair a new etag with an old body
/// and let a stale decision through.
fn read_handoff_versioned<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Option<(Handoff, String)>> {
    let Some(meta) = b.head(bucket, HANDOFF_KEY)? else {
        return Ok(None);
    };
    match b.get(bucket, HANDOFF_KEY)? {
        Some(d) => Ok(Some((serde_json::from_slice(&d)?, meta.etag))),
        None => Ok(None),
    }
}

/// Write the record only if it is still the version `expect` was read at (`None`: only if absent).
/// `false` means someone else changed it first: re-read and decide again.
pub(super) fn write_handoff_if<B: ObjectBackend>(b: &B, bucket: &str, h: &Handoff, expect: Option<&str>) -> Result<bool> {
    b.put_cas(bucket, HANDOFF_KEY, &serde_json::to_vec_pretty(h)?, expect)
}

/// Attempts at a read-decide-write cycle on the handoff record before giving up as contended.
const HANDOFF_RETRIES: usize = 6;

fn contended() -> S2Error {
    S2Error::Backend("handoff record is being changed by others; try again".into())
}

/// Refuse changes by anyone but the merger while a live claim exists.
pub(super) fn guard_live_claim(h: &Handoff, who: &Actor, now_ms: u64) -> Result<()> {
    if h.state == State::Claimed
        && let Some(m) = h.merger.as_ref().filter(|m| *m != who)
        && claim_is_live(h, now_ms)
    {
        return Err(S2Error::Claimed(format!("{}@{}", m.agent, m.host)));
    }
    Ok(())
}

/// The writer says the tree is ready for another agent to merge.
pub fn mark_handoff<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    writer: &Actor,
    note: &str,
    now_ms: u64,
) -> Result<()> {
    if read_manifest(b, bucket)?.is_none() {
        return Err(S2Error::Invalid(
            "nothing published to hand off; publish the tree first".into(),
        ));
    }
    for _ in 0..HANDOFF_RETRIES {
        let seen = read_handoff_versioned(b, bucket)?;
        if let Some((h, _)) = &seen {
            if matches!(h.state, State::Merged | State::Archived) {
                return Err(S2Error::Invalid(format!("worktree is already {:?}", h.state)));
            }
            guard_live_claim(h, writer, now_ms)?;
        }
        let next = Handoff {
            state: State::Handoff,
            note: note.into(),
            writer: writer.clone(),
            merger: None,
            updated_ms: now_ms,
            epoch: seen.as_ref().map_or(0, |(h, _)| h.epoch),
            claimed_head: None,
        };
        if write_handoff_if(b, bucket, &next, seen.as_ref().map(|(_, e)| e.as_str()))? {
            return Ok(());
        }
    }
    Err(contended())
}

/// Take the merge. The record is written with a compare-and-swap on the version the decision was
/// made on, so two claimers cannot both win; the loser re-reads and is refused. A claim older than
/// [`CLAIM_TTL_MS`] can be taken over.
pub fn claim_merge<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    me: &Actor,
    now_ms: u64,
) -> Result<Handoff> {
    // Not live: older than the TTL, or stamped from further in the future than the allowed skew.
    claim_merge_with(b, bucket, me, now_ms, |cur| !claim_is_live(cur, now_ms))
}

/// [`claim_merge`] with the expiry decision supplied by the caller: `expired(current)` says whether
/// a live claim by someone else may be taken over. `claim_watch` uses it to decide on elapsed time
/// the caller observed itself instead of on the stored wall-clock stamp.
pub fn claim_merge_with<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    me: &Actor,
    now_ms: u64,
    mut expired: impl FnMut(&Handoff) -> bool,
) -> Result<Handoff> {
    for _ in 0..HANDOFF_RETRIES {
        // The head is read before the record: a publish landing in between makes the recorded head
        // older than the published one, so the merge is refused (safe), never the reverse.
        let head = published_head(b, bucket)?;
        let (cur, etag) = read_handoff_versioned(b, bucket)?
            .ok_or_else(|| S2Error::Invalid("worktree has no handoff record".into()))?;
        match cur.state {
            State::Merged | State::Archived | State::Orphan => {
                return Err(S2Error::Invalid(format!("worktree is already {:?}", cur.state)));
            }
            State::Claimed => {
                let other = cur.merger.as_ref().filter(|m| *m != me);
                if let Some(m) = other
                    && !expired(&cur)
                {
                    return Err(S2Error::Claimed(format!("{}@{}", m.agent, m.host)));
                }
            }
            State::Open | State::Handoff => {}
        }
        let next = Handoff {
            state: State::Claimed,
            merger: Some(me.clone()),
            updated_ms: now_ms,
            epoch: cur.epoch + 1,
            claimed_head: head,
            ..cur
        };
        // Conditional on the version we decided on: of two claimers reading the same record, exactly
        // one write lands, and the other re-reads, sees a live claim and is refused.
        if write_handoff_if(b, bucket, &next, Some(&etag))? {
            return Ok(next);
        }
    }
    Err(contended())
}

/// The head the worktree has published now (`None`: nothing published).
fn published_head<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Option<String>> {
    Ok(read_manifest(b, bucket)?.map(|m| m.head_commit))
}

/// The fencing checks on a `Claimed` record, shared by [`mark_merged_at`] and [`fence_claim`].
///
/// Invariant (WO-069 6a-1): a claimer can record a merge only of the head it claimed. Epochs fence a
/// claim against another claim; the head fences it against a republish. A republish under a claim
/// that is not live (older than the TTL, or stamped more than [`CLAIM_SKEW_MS`] ahead, ADR-126 §4)
/// is allowed, so a bad clock cannot block the writer forever, but it moves the published head away
/// from the claimed one and with that voids the claim for merging: the claimer must claim again
/// (and so see, and fetch, the new head). A record from before `claimed_head` existed is not fenced
/// on head.
fn fence_claim_on<B: ObjectBackend>(b: &B, bucket: &str, cur: &Handoff, epoch: Option<u64>) -> Result<()> {
    if let Some(e) = epoch
        && e != cur.epoch
    {
        return Err(S2Error::Claimed(format!(
            "claim epoch {e} is stale (now {}); the claim was taken over",
            cur.epoch
        )));
    }
    if let Some(claimed) = cur.claimed_head.as_deref() {
        let now = published_head(b, bucket)?;
        if now.as_deref() != Some(claimed) {
            return Err(S2Error::Claimed(format!(
                "the published head moved from {claimed} to {} since the claim (epoch {}); claim again to merge it",
                now.as_deref().unwrap_or("nothing"),
                cur.epoch
            )));
        }
    }
    Ok(())
}

/// For a caller about to land under a claim (the daemon's `promote --epoch`): refuse when the claim
/// was taken over since `epoch` or the published head is no longer the one claimed. Passes when the
/// worktree is not `Claimed` (nothing to fence) or has no handoff record.
pub fn fence_claim<B: ObjectBackend>(b: &B, bucket: &str, epoch: u64) -> Result<()> {
    match read_handoff(b, bucket)? {
        Some(h) if h.state == State::Claimed => fence_claim_on(b, bucket, &h, Some(epoch)),
        _ => Ok(()),
    }
}

/// The merger finished: git has landed the branch. Promotion and bucket cleanup follow (§3, §4).
pub fn mark_merged<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    me: &Actor,
    note: &str,
    now_ms: u64,
) -> Result<()> {
    mark_merged_at(b, bucket, me, None, note, now_ms)
}

/// [`mark_merged`] that also demands the claim epoch the merger claimed under. If the claim was
/// taken over and re-taken by the same actor in between, or the record was rewritten, the epoch has
/// moved and the merge is refused (fencing), whatever the clocks say.
pub fn mark_merged_at<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    me: &Actor,
    epoch: Option<u64>,
    note: &str,
    now_ms: u64,
) -> Result<()> {
    for _ in 0..HANDOFF_RETRIES {
        let (cur, etag) = read_handoff_versioned(b, bucket)?
            .ok_or_else(|| S2Error::Invalid("worktree has no handoff record".into()))?;
        if cur.state != State::Claimed {
            return Err(S2Error::Invalid(format!(
                "claim the merge before marking it merged (state is {:?})",
                cur.state
            )));
        }
        if cur.merger.as_ref() != Some(me) {
            return Err(S2Error::Claimed(
                cur.merger.map(|m| format!("{}@{}", m.agent, m.host)).unwrap_or_default(),
            ));
        }
        fence_claim_on(b, bucket, &cur, epoch)?;
        // Conditional: the epoch, head and merger checks above hold for exactly the version written over.
        let next = Handoff {
            state: State::Merged,
            note: note.into(),
            merger: Some(me.clone()),
            updated_ms: now_ms,
            ..cur
        };
        if write_handoff_if(b, bucket, &next, Some(&etag))? {
            return Ok(());
        }
    }
    Err(contended())
}

/// The branch landed through git, so the worktree is merged whoever held the claim. Unlike
/// [`mark_merged`] this needs no claim: the landed tree is the source of truth once the land
/// happened, and a claim that is someone else's, or has expired, cannot undo it. The lander is
/// recorded as the merger and a displaced claimer is named in the note. Returns false, writing
/// nothing, when there is no handoff record (the tree was never published) or the worktree is
/// already merged or archived.
pub fn mark_landed<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    lander: &Actor,
    note: &str,
    now_ms: u64,
) -> Result<bool> {
    // Conditional, like the other handoff writes: the decision holds for exactly the version written over.
    for _ in 0..HANDOFF_RETRIES {
        let Some((cur, etag)) = read_handoff_versioned(b, bucket)? else { return Ok(false) };
        if matches!(cur.state, State::Merged | State::Archived) {
            return Ok(false);
        }
        let note = match cur.merger.as_ref().filter(|m| *m != lander && cur.state == State::Claimed) {
            Some(m) => format!("{note} (claim was held by {}@{})", m.agent, m.host),
            None => note.to_string(),
        };
        let next = Handoff { state: State::Merged, note, merger: Some(lander.clone()), updated_ms: now_ms, ..cur };
        if write_handoff_if(b, bucket, &next, Some(&etag))? {
            return Ok(true);
        }
    }
    Err(contended())
}
