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
| `WEFTOS_WT_REGISTRY_REQUIRE_SIGNED` | with the trusted file, entries that do not verify are dropped from search and leftovers and reported by `follow` as `unreadable`, never silently absent. **A trusted file turns this on by itself** (S3 review #4); `=0` is an audit mode that keeps them visible; `=1` without a trusted file is an error. `follow`, `search` and `leftovers` return a `trust` map (`verified` / `unsigned` / `bad` / `refused`) per worktree whenever a trusted file is set |

A named file that cannot be read or parsed is an error, not a quiet fall back to unsigned. With none set, behaviour is unchanged. Tests: `with_signing_on_the_daemon_signs...`, `signing_settings_come_from_files...`, and the `registry_sign` library tests.

**What is blocked on the owner (one decision, then one-line flips):** which key signs. The options and recommendation are in `adr126-s2-registry-signing-and-claim-clock.md`. Whichever is picked, the steps are:

1. Put the 64-hex seed in a 0600 file on the daemon host and set `WEFTOS_WT_REGISTRY_SIGN_KEY`. No install; a restart of the daemon that reads it. Per the standing rules I have not done this on any machine.
2. Distribute the matching public key to followers in a file and set `WEFTOS_WT_REGISTRY_TRUSTED` (and `REQUIRE_SIGNED=1` once every writer signs). Where followers read trusted keys from (a project manifest, config) is the open question in option 1; the daemon only reads the file it is pointed at.
3. Existing entries stay unsigned until each worktree is next published, claimed or landed. Turn `REQUIRE_SIGNED` on after that.

Limits: the CLI-only follower path (`worktree-store.sh` calling the daemon) trusts its own daemon; the check protects a daemon that reads a registry others can write (the default project-store placement). With `WEFTOS_WT_REGISTRY_STORE=1` the registry is already daemon-only, and signing then protects against a store admin.

## Real S2 code since the spike (2026-11-08, second pass)

- **Signing, real ed25519.** The daemon's `Ed` signer and `Trusted` verifier implement the existing `EntrySigner` / `EntryVerifier` traits. Live test `live_ed25519_signed_entries_verify_against_the_bytes_rustfs_holds` (clawft-weave, ignored, needs a throwaway RustFS): a good entry verifies, one signed by an untrusted key is `Bad`, an unsigned one is `Unsigned`, a strict listing refuses the last two and names them, an edit after signing is `Bad`. `weftos-store` itself still has no crypto dependency (a dependency would need the owner's diff-gate approval); the store crate's live spike uses a keyed hash through the same traits.
- **Strict mode** is `WEFTOS_WT_REGISTRY_REQUIRE_SIGNED=1` (needs the trusted-key file; refuses to start without it). Off by default.
- **Relay notification path.** `weftos_store::wake`: `registry_hints(body, registry_bucket)` turns an S3 event notification into the registry slugs it touches (entry or signature object, created or removed; other buckets, other keys, reads and path-like slugs are ignored), and `Waker` decides when to poll (settle after the last hint, so an entry and its signature object are read together; fallback poll if nothing arrives). A wake-up is only a hint; the registry stays the source of truth. Unit tests plus the live test `live_a_bucket_notification_wakes_the_follower_before_its_fallback_poll`: real RustFS posts a bucket notification to `scripts/store/notify-sink.py` (the site subscriber stand-in), the hint wakes a follower whose fallback poll is a minute away, and it sees New, HeadMoved and Gone, with no phantom wake-up afterwards.
  - **Measured on loopback RustFS 1.0.4:** the follower saw a registry change 0.3 s after the write on the best run and 5-8 s on others. The delay is RustFS's notification delivery, not the store round trips (a direct poll took ~0.1 s). The 30-60 s poll bound in the ADR holds with room to spare; do not promise sub-second wake-ups.
  - **What is not built:** the hop from the daemon's receiver to other devices (mesh relay) and calling `registry_hints` from `clawft-weave::store_ingest`. That file is S1's; the one-line hook is: after `ingest_notification`, call `registry_hints(body, <registry bucket>)` and, if non-empty, publish a wake message on the mesh relay topic for the project. S2 has no daemon receiver of its own on purpose.

### Starting the notify-enabled throwaway RustFS (podman, loopback only)

```
podman run -d --name s2-rustfs-spike --network host \
  -e RUSTFS_ACCESS_KEY=<admin> -e RUSTFS_SECRET_KEY=<secret> -e RUSTFS_VOLUMES=/data \
  -e RUSTFS_ADDRESS=127.0.0.1:19400 -e RUSTFS_CONSOLE_ADDRESS=127.0.0.1:19401 \
  -e RUSTFS_OUTBOUND_ALLOW_ORIGINS=http://127.0.0.1:19411 \
  -e RUSTFS_NOTIFY_ENABLE=true -e RUSTFS_NOTIFY_WEBHOOK_ENABLE_WAKE=on \
  -e RUSTFS_NOTIFY_WEBHOOK_ENDPOINT_WAKE=http://127.0.0.1:19411/n -e RUSTFS_NOTIFY_WEBHOOK_QUEUE_DIR_WAKE=/queue \
  -v <data dir>:/data:Z -v <queue dir>:/queue:Z docker.io/rustfs/rustfs:1.0.4
scripts/store/notify-sink.py 19411 <events file> &      # the site subscriber stand-in; kill it afterwards
WEFTOS_STORE_TEST_RUSTFS_ENV=<env file> WEFTOS_STORE_TEST_NOTIFY_FILE=<events file> \
  cargo test -p weftos-store --features rustfs --test s2_follower_rustfs -- --ignored
```

(The data and queue directories must be writable by the container user. Stop and delete the container afterwards.)

## What the out-of-scope tests would need

**Two physical machines (gate item 1 on ADR-126 §4a):**
1. A RustFS reachable from both (the PG instance is excluded; use a throwaway or the Phase A one once it exists), with one scoped key per machine and the follower's key read-only on the project and worktree buckets.
2. A store-capable `weaver` on each machine, or the S3-remote path: `scripts/worktree-store.sh` pointed at a daemon on one host. Both are installs, so barred now.
3. Machine A publishes (`worktree-store.sh publish`), machine B runs `follow` / `search` / `sync-docs`, then claims and lands. Pass: B sees the publish within one poll interval, `sync-docs` bytes match, a claim from B is refused while A's claim is live, and a land from B removes the entry on A's next poll.
4. For wake-ups: RustFS notify target plus the daemon's `WEFTOS_STORE_INGEST_ADDR` receiver on the PG host (S1), and the relay hook above.

**A live daemon:** the verbs run through `daemon.rs` `store.worktree.*` with `StoreService` on the RustFS backend (`WEFTOS_STORE_RUSTFS_ENV`). It needs a built `weaver` binary started with `WEFTOS_STORE_RUSTFS_ENV` and, for signing, `WEFTOS_WT_REGISTRY_SIGN_KEY` / `_TRUSTED` / `_REQUIRE_SIGNED=1`, then `weaver store worktree create/publish/follow/search/promote` against it. Covered today by `store_worktree_tests` on a local-directory backend, which exercises the same verbs minus the RPC hop and the RustFS transport.

## Review fixes to signing (2026-11-08, S3 review of S2)

- **Registry entries name their own key** and are verified on the bytes that were parsed (#1). **`verify_strict`** and weak-key refusal (#2).
- **What is signed (#3):** `"weftos-registry-v1\0" || registry bucket || \0 || slug || \0 || generation || \0 || entry bytes`. The signature object carries the generation (one more than the previous signature object, or the entry's `updated_ms` if larger, so it keeps rising across a remove-and-recreate). A signature for one project's registry or one worktree name does not verify anywhere else, and the old bare-bytes format is `Bad`. **Rollback:** `FollowState` keeps the highest verified generation per worktree (also after the worktree leaves) and `poll_verified` reports an older signed entry as `unreadable`, keeping the follower's previous view. Limits: a follower with no state file has no earlier generation to compare, so protection starts at its first sight of an entry; the generation rises with the daemon's clock, so a daemon clock stepped far backwards would be refused by followers until it catches up.
- **Seed file (#5):** the signing key file must not be readable by group or others (mode `& 0o077`), or the daemon refuses to start with it; the seed in an error message is never echoed. Not done: wiping the seed from memory (needs a zeroize dependency; `ed25519-dalek` zeroizes its key on drop, but the hex text read from the file is a plain `String`).
- **Left as is (#6):** the entry and its signature object are two writes, so a strict follower polling between them sees one `unreadable` poll and the previous view is kept; the next poll (or the settle delay in `Waker`) clears it. Making it atomic means putting the signature inside the entry object, a format change.

## Decision note: signature inside the entry (versioned registry format), for the owner

Status: proposal, no code written. Closes review finding #6 (two writes, so a strict follower can see one `unreadable` poll) and removes the orphan-sidecar class of bug. It is a format change on a shared object, so it needs a decision and a rollout order.

### Today (format 1)

`worktrees/<slug>.json` is the bare entry (`RegistryEntry` as pretty JSON). `worktrees.sig/<slug>.json` is the signature object `{signer, alg, generation, sig}`. Writing is entry then signature; removing is entry then signature. A follower reads both and checks the signature over `domain || bucket || slug || generation || entry bytes`. An entry with no signature object reads as `Unsigned`. Failure mode of the two-object layout: a strict follower polling between the two writes sees the new entry with the old signature (`bad`), or a deleted entry with its signature still there; the previous view is kept and the next poll clears it. It fails safe and is only noisy.

### Proposal (format 2)

One object per worktree, still at `worktrees/<slug>.json`, so the key space, the listing prefix and the wake-up hints (`registry_hints`) do not change:

```json
{ "v": 2,
  "payload": "<the entry JSON, as a string>",
  "signature": { "signer": "<hex pubkey>", "alg": "ed25519", "generation": 41, "sig": "<hex>" } }
```

- **`payload` is a string holding the entry's JSON text**, and the signature covers exactly those string bytes (plus the same domain prefix, bucket, slug and generation as today). That avoids needing a canonical JSON form: nobody re-serializes before verifying. The price is readability: a human reads it with `jq -r .payload | jq .`.
- An unsigned daemon writes `"signature": null`, so unsigned and signed entries share one shape and `Unsigned` stays a state a follower can show.
- Write, replace and remove are each one object, so there is no window and no orphan. The existing conditional write (`put_cas`) can guard the replace if two daemons ever shared a registry.
- The `v` field is the version switch: absent means format 1.

### How followers read both

A follower parses the object once. If it has `"v": 2` it takes `payload` and `signature` from the envelope; if it parses as a bare entry (no `v`, has `slug`) it is format 1 and the follower looks for the sidecar as today. The rest of the follower (poll, search, leftovers, rollback generations, trust map) works on the same internal `(entry, trust, generation)` triple, so the change is confined to the reader in `share_fetch` / `registry_sign`. The slug-equals-key check and the "verify the parsed bytes" rule carry over unchanged (the signed bytes are the payload string).

### Migration, in order (the order is the whole risk)

1. **Readers first.** Ship followers that understand both formats. Nothing writes format 2 yet. A daemon at this stage is the only thing that must not move.
2. **Wait until every follower reads both.** An old follower that meets a format 2 object fails to parse it as a bare entry and reports that worktree `unreadable`; it fails safe but sees nothing. There is no way to detect old followers from the store, so this is a statement the owner makes ("all followers are at least version X").
3. **Flip the writer** with an off-by-default switch, `WEFTOS_WT_REGISTRY_FORMAT=2`. The daemon rewrites each entry in format 2 the next time it refreshes it (publish, claim, handoff). Entries nobody touches stay format 1 until then. A one-shot `registry rewrite` verb could convert them all.
4. **Clean up** sidecars for converted entries (the daemon deletes `worktrees.sig/<slug>.json` when it writes format 2), and later remove the format 1 reader in a release once nothing writes or holds format 1.

A dual-write transition (keep writing format 1 and also write format 2 elsewhere) would let old followers keep working, but it keeps the forgeable unsigned-looking format 1 object alive, and see the next point.

### Downgrade attack (must be handled with the format)

Once entries are signed in format 2, someone with project write could delete the format 2 object and put a format 1 bare entry in its place. A strict follower already refuses an unsigned or bad entry, so strict mode is safe. Followers that are not strict would accept it as `Unsigned`, as they do today. Add to `FollowState` a per-worktree "has been seen as format 2" bit (next to the generations) and refuse a format 1 reading afterwards; and a `REQUIRE_FORMAT_2` setting for when the whole fleet has moved.

### Costs and risks

- Registry objects get a little bigger (the payload is escaped JSON inside JSON, about 10-20% more) and are harder to read by eye.
- Anything outside this repo that reads `worktrees/<slug>.json` as a bare entry breaks on format 2. In this repo that is only `share_fetch` and the follower; the shell script goes through the daemon verbs and never reads the object.
- Tests needed: read both formats; mixed registry (some of each); format 2 with a tampered payload, a swapped signature, a key/slug mismatch and a replay into another project; the downgrade refusal; an old-format follower (the current code) seeing a format 2 object as `unreadable` and nothing worse; rollback generations continuing across the format change; the rewrite verb; live RustFS round trip. About a day of work, all in `weftos-store` plus a switch in the daemon.

### Options and a recommendation

| Option | What it does | Cost |
|---|---|---|
| A. Do nothing | keep two objects; one noisy `unreadable` poll at worst, which fails safe | none |
| B. Retry in the follower | on `bad` for a previously verified worktree, re-read that one entry after the settle delay before reporting it; no format change | small code change, no rollout |
| C. Format 2 as above | atomic, no orphans, one object | the rollout above, a downgrade guard, a day of work |

Recommendation: **B now if the noise is ever seen in practice; C only when the registry format has to change for another reason** (for example signed worktree heads read from an untrusted relay, which ADR-126 §4a lists as an open question), so the rollout is paid once. Today nothing is wrong, only untidy: the two-write window fails safe, and the rollback and replay protections already hold. One-line decision for the owner: "A", "B", or "C, readers first".
