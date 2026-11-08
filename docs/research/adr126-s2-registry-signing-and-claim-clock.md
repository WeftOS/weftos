# ADR-126 S2: signing the worktree registry, and claim timeouts off the client clock

Status: mechanism and claim fixes implemented; daemon signing waits on the owner's key choice. 2026-11-08. Belongs to ADR-126 S2.

## Problem

1. `worktrees/<slug>.json` in the project bucket is written by the daemon, but anyone with project
   write can replace it. A follower trusts it to say which bucket holds a worktree, so a forged
   entry can send a follower at the wrong bucket. `verify_entry` already checks that the target
   bucket's own `.wt/meta.json` names the same slug; that stops cross-bucket redirects but not a
   forged state, merger or head.
2. A claim expires after 30 minutes of `now_ms - updated_ms`. With a client calling the library
   directly, `now_ms` is that client's wall clock, so a skewed client can steal a live claim or
   keep a dead one alive.

## Registry signing (implemented as a mechanism)

`weftos_store::registry_sign`: a sidecar object `worktrees.sig/<slug>.json` holds
`{signer, alg, sig}`, a signature over the exact bytes stored at `worktrees/<slug>.json`. Signing
the stored bytes avoids any canonical-JSON question. Signer and verifier are two traits, so the
crate carries no crypto and invents no key scheme: the daemon injects the node identity it
already has (ed25519 via `ed25519-dalek`, already a `clawft-weave` dependency) and followers inject
a verifier holding the trusted public keys.

A follower classifies each entry `Verified`, `Unsigned` or `Bad`, and with `require_signed` drops
everything but `Verified`. An edited entry, a signature by an untrusted signer and a mangled
sidecar are all `Bad`; none is an error that stops the listing.

Not done: the daemon does not sign yet, and the follower verbs do not call
`list_shared_verified`. Both need the daemon's node signing key made available to
`store_worktree.rs`, which is a wiring decision for the owner (which key, where followers get the
trusted public keys). Until then entries are `Unsigned` and followers behave as before. A signature
does not stop replay of an older, validly signed entry; the entry's `updated_ms` and the handoff
`epoch` are the freshness signals.

## Claim clock (implemented)

- **One clock per store.** Every `store.worktree.*` verb takes `now_ms` from the daemon, not from the
  caller. Library callers choose their own, which is why the next two items exist.
- **Fencing epoch.** `Handoff.epoch` counts claims; `claim` returns it. `promote` takes an optional
  `epoch` and, through `mark_merged_at`, refuses to record a merge made under a different one.
- **Observer-side expiry.** `claim_watch::claim_merge_observed`: a taker treats a claim by someone
  else as expired only after it has watched the same `(epoch, updated_ms)` unchanged for the TTL on
  its own monotonic clock. The stored wall-clock stamp decides nothing, so skew can neither steal a
  live claim nor keep a dead one. A live claimer refreshes the record and resets the watch. The
  daemon `claim` verb uses it with a process-lifetime watch; a daemon restart forgets the watch,
  which only lengthens the wait. The library `claim_merge` keeps the wall-clock rule for callers
  that have no place to keep a watch.

## Conditional writes (implemented, S1's `put_if_match`)

Promotion copies now go through `put_cas`: `Some(etag)` is S1's `put_if_match`, `None` is a put
with `if_none_match`. A lost race is a conflict and nothing is written. Deletes still re-check the
etag just before (no conditional delete exists), and the `Overwrite` policy stays unconditional by
design. A CAS needs the object in memory, so a promoted file passes through the daemon rather than
being copied server-side.

## Signing key: options for the owner

1. **Daemon node identity (ed25519, the key that signs the kernel chain).** The daemon signs each
   entry; followers hold the daemon public keys the project trusts. End to end, survives a
   compromised store or a project-write principal. Needs: a decision on where followers read the
   trusted keys (project manifest, pinned in config), and key rotation.
2. **Store governance only (no new crypto).** Grant `worktrees/` and `worktrees.sig/` write to the
   daemon principal alone, via S1's per-principal grants. Zero key management, but it trusts the
   store and the operator credentials; a store admin can still forge entries.
3. **A dedicated per-project signing key.** Cleanest separation, but a new secret to store, hand out
   and rotate, which is what the owner asked not to invent.

Recommendation: do 2 now (it needs only grants), and 1 as the real fix once the owner names the
trusted-key location. Skip 3. Whichever is picked, the mechanism in `registry_sign` is unchanged;
only the `EntrySigner` and `EntryVerifier` implementations differ.
