# ADR-126 S2: signing the worktree registry, and claim timeouts off the client clock

Status: design note with a partial implementation, 2026-11-08. Belongs to ADR-126 S2.

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

## Claim clock (partly implemented)

- **One clock per store.** Every `store.worktree.*` verb takes `now_ms` from the daemon, not from the
  caller, so claims made through the daemon already share one clock. Callers using the library
  directly choose their own `now_ms`; documented, not enforced.
- **Fencing epoch (implemented).** `Handoff.epoch` counts claims. A merger that reads a higher
  epoch than the one it claimed under knows its claim was taken over, whatever the clocks say.
  `mark_merged` does not yet require the claimer's epoch; that is the next step, together with a
  `claim` response that returns the epoch to hold.
- **Observer-side expiry (design only).** Better than comparing two wall clocks: a taker treats a
  claim as expired only after it has watched the same `(epoch, updated_ms)` for 30 minutes on its
  own monotonic clock. This removes skew entirely, at the cost of a taker needing state between
  polls (the follower's `FollowState` is the natural home).
