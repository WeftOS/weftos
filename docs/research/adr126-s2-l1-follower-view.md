# ADR-126 S2: the L1 follower view, design note and loopback spike

Date: 2026-11-08. Closes the ADR-131 gap "L1 / follower view: worktrees visible to other nodes before a merge" on the S2 side. Code: `weftos_store::follower`, `registry_sign`, daemon verbs in `clawft-weave/src/store_worktree.rs`. Spike: `crates/weftos-store/tests/s2_follower_rustfs.rs`.

## Design in one page

- The registry (`worktrees/<slug>.json`, one small object per unmerged worktree) is written only by the daemon, from each worktree's own bucket (`summarize`). A follower reads the registry, then a worktree's manifest, with its own read grants. It never writes.
- A follower gets: `poll` (new / head moved / state changed / gone / unreadable), `search_docs`, `sync_docs` (read-only local mirror), `leftovers` (forgotten work) and `overlaps` (two worktrees changing the same path).
- Wake-up is a notification from the site subscriber; a 30-60 s poll is the source of truth (owner answer on ADR-131). One poll costs one list plus one get per worktree.
- Trust: a registry entry is a pointer. A follower checks the bucket's own `.wt/meta.json` names the same slug. With signing on, it also checks the entry's signature (see below).

## Spike: three identities on a loopback RustFS 1.0.4

A throwaway RustFS (podman, `127.0.0.1`, own data dir, not PG) with four real scoped keys: a writer per worktree (RW on its own bucket only), a follower (read-only on the project bucket and the worktree buckets) and a merger. The daemon is the root key. Result, one run, ~1 s:

- Both unmerged worktrees show up as `New` for the follower before any merge; `search` finds `docs/plan.md` in the other worktree; `sync_docs` mirrors it byte for byte; `overlaps` reports the two paths both worktrees changed.
- **Real IAM enforces what `MemBackend` cannot:** a writer key cannot write the project registry or another worktree's bucket; the follower key is refused every write, including `.wt/handoff.json`, and cannot claim.
- Publish, registry refresh and the follower's next poll saw `HeadMoved` in 80-180 ms on loopback. (Real latency is the poll interval; this only shows the store round trips are not the bottleneck.)
- Handoff is seen as `StateChanged`; a merger key claims (epoch 1); the writer's own late claim is refused; a worktree idle four days is flagged a leftover from the follower's view; after merge and registry removal the follower sees `Gone`.
- Signed registry on a real store: the signature over the bytes RustFS holds verifies for a read-only follower; an edited entry is `Bad`; an unsigned one is `Unsigned`; a strict follower sees neither; follower and worktree keys cannot write the signature objects.
- The test fails if the follower is over-granted (checked by giving it write on the project bucket: "follower wrote p-...").

Not covered by the spike: two physical machines, the relay notification path, the S3-remote path from a real second host, a real ed25519 key (the spike uses a keyed hash through the same traits; the daemon tests use ed25519).

## Bugs the spike pass found and fixed

- `promote` never removed or refreshed the registry entry. A landed worktree stayed `Claimed` in every follower's view and, after 30 minutes, showed up as an abandoned claim, which is the exact false "forgotten work" signal this view exists to avoid. `promote` now removes the entry and its signature once the land succeeded (test: `a_landed_worktree_leaves_the_registry...`).
- `remove_registry_entry` left the signature object behind. `remove_registry_entry_signed` removes both.

## Signing: what is built, what the owner supplies

Built, off unless configured (all on the daemon; nothing is installed):

| Setting | Effect |
|---|---|
| `WEFTOS_WT_REGISTRY_SIGN_KEY=<file>` | file with a 32-byte ed25519 seed as 64 hex chars; every registry entry the daemon writes gets a signature object `worktrees.sig/<slug>.json` over the stored bytes |
| `WEFTOS_WT_REGISTRY_TRUSTED=<file>` | one 64-hex public key per line (`#` comments); `follow`, `search` and `leftovers` then check each entry |
| `WEFTOS_WT_REGISTRY_REQUIRE_SIGNED=1` | with the trusted file: entries that do not verify are dropped from search and leftovers and reported by `follow` as `unreadable`, never silently absent |

A named file that cannot be read or parsed is an error, not a quiet fall back to unsigned. With none set, behaviour is unchanged. Tests: `with_signing_on_the_daemon_signs...`, `signing_settings_come_from_files...`, and the `registry_sign` library tests.

**What is blocked on the owner (one decision, then one-line flips):** which key signs. The options and recommendation are in `adr126-s2-registry-signing-and-claim-clock.md`. Whichever is picked, the steps are:

1. Put the 64-hex seed in a 0600 file on the daemon host and set `WEFTOS_WT_REGISTRY_SIGN_KEY`. No install; a restart of the daemon that reads it. Per the standing rules I have not done this on any machine.
2. Distribute the matching public key to followers in a file and set `WEFTOS_WT_REGISTRY_TRUSTED` (and `REQUIRE_SIGNED=1` once every writer signs). Where followers read trusted keys from (a project manifest, config) is the open question in option 1; the daemon only reads the file it is pointed at.
3. Existing entries stay unsigned until each worktree is next published, claimed or landed. Turn `REQUIRE_SIGNED` on after that.

Limits: the CLI-only follower path (`worktree-store.sh` calling the daemon) trusts its own daemon; the check protects a daemon that reads a registry others can write (the default project-store placement). With `WEFTOS_WT_REGISTRY_STORE=1` the registry is already daemon-only, and signing then protects against a store admin.
