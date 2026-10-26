# Adversarial review of the overnight security-relevant branches

**Date:** 2026-10-26. **Reviewer:** world-builder agent (read-only; no code changed, nothing built or run).
**Method:** static reading of the diffs and the code they touch. A "concrete case" below is a sequence
traced through the code, not an executed test. Where a claim could not be established it says so.

Branches and commits reviewed:

| Branch | Commits |
|---|---|
| `wt/cogbus-b5-gaps` | `fb8a6236e` (B5 blob store), `8c9c2e4bf` (gap 4: idle, caps) |
| `wt/venq-p3` | `d9908ce6d` (device ledger), `b028d6d0b` (netdev claims) |
| `wt/native-restart` | `169e56f20` (host journal, restore, `stage()`) |

## Ranked findings

| # | Sev | Branch | Finding |
|---|---|---|---|
| 1 | High (certain, data loss) | native-restart | `stage()` deletes the whole instance directory, including the cog's `data/`, on every restore |
| 2 | High (conditional, privilege) | venq-p3 | A `socket` device selector accepts any absolute path, and the privileged grant chowns it (following symlinks); `path="/dev/*"` has no deny-list |
| 3 | Medium-high (conditional on journal write) | native-restart | Restore bypasses request authentication; `requester`, `project_id`, `decision_id` and `desired_running` are trusted from an unauthenticated file |
| 4 | Medium | venq-p3 | One-owner-per-device is bypassed by aliases (`/dev/v4l/by-id/...`, stale USB resolution) |
| 5 | Medium | cogbus-b5 gap 4 | A connection slot leaks when the hello reply fails to send; unauthenticated connections are uncapped |
| 6 | Medium | venq-p3 | A netdev claim is reported `enforced` but nothing is enforced |
| 7 | Low-medium | cogbus-b5 | Blob store: quota is held by instance, not project, and live blobs are never evicted (node-wide availability) |
| 8 | Low | several | Smaller items, listed at the end |

---

## 1. native-restart: restore wipes the cog's data directory (certain)

`crates/clawft-kernel/src/workload_runtime/native.rs`, `stage()`. The new first step is
`remove_dir_all(dir)` when the directory exists. `dir` is `<root>/<iid>`, and `instance_id()` is
deterministic (workload id, version, package id, mode, args, csi bind, node id). So the restore of a
placed cog lands on the same directory the old run used, and `data/` (the cog's
`COGNITUM_COG_DATA_DIR`, 0700, chowned to the cog uid) is deleted before being recreated empty.

Concrete case: place a cog that writes `data/state.db`; restart the daemon; `restore_instances()` calls
`place()`, then `load()`, then `stage()`; `state.db` is gone. The new test
(`tests_restart.rs:210`) only asserts that `data/` exists afterwards, which is true because it was
recreated. The same wipe happens on any re-place of an unloaded cog, where before the change the stale
directory made `load` fail loudly.

Fix direction: clear only the read-only binary (and re-create it), keep `data/`; or stage into a
temporary sibling and move only the program.

## 2. venq-p3: the device ledger lets a cog manifest name arbitrary files, and the grant chowns them

Files: `workload_runtime/cog_devices.rs`, `workload_ctl/device_owner.rs`, `device_sys.rs`;
`workload_place_rpc.rs:556` installs `SystemApplier` (libc `chown` and `chmod`).

- `parse_devices`: the `path` selector must start with `/dev/`, but the `socket` selector goes through
  `plain_path(..., globs=false)` only, which checks "plain absolute path without `..`". `/etc/passwd`,
  `/etc/shadow`, `/root/.ssh/id_rsa` all pass. The resolver only checks `Path::exists`.
- In enforced mode (the cog has its own uid) `plan_grants` plans `Chown{path, uid=cog uid, mode 0600/0400}`
  and `SystemApplier::apply` runs `chown` then `chmod`. Both follow symlinks (no `lchown`, no
  `fchownat(AT_SYMLINK_NOFOLLOW)`).
- Concrete case: a cog package whose `cog.toml` has `[[devices]] socket="/etc/passwd"` (access defaults
  to `rw`) placed on a node running per-cog uids with the root applier: `/etc/passwd` becomes
  `cog-uid 0600`. The cog can now edit it (root escalation); every other account loses read access to it.
  A socket path under a world-writable directory (`/tmp/x.sock`) additionally lets a local user plant a
  symlink to any target before the chown.
- `path="/dev/*"` (valid: glob in the last component) claims every node, and the grant chowns
  `/dev/null`, `/dev/tty`, `/dev/ptmx`, disks (`/dev/sda*`), `/dev/mem`, `/dev/kmsg` to the cog uid at 0600.
  There is no deny-list of sensitive nodes and no check that a `socket` selector is a socket or lives
  under a service directory. Governance sees the placement, but nothing in the ledger restricts which
  devices a signed manifest may ask for.

Trust note: this needs a signed cog package (the signing key is the root of trust), so it is a
privilege-escalation path from "signed cog" to root on enforced nodes, not from an anonymous party.
Advisory mode does no chown, so it is not affected.

Fix direction: require `socket` to be an actual socket (`FileTypeExt::is_socket`) under an allow-listed
prefix; use `fchownat`/`fchmodat` with no-follow or open the node first and operate on the fd; add a
sensitive-node deny-list and a cap on how many nodes a glob may cover.

## 3. native-restart: restore does not go through request authentication (conditional on journal write)

Question asked: can a tampered `workload-host-journal.json` restore something governance would refuse?

What restore **does** go through (so a tamper cannot beat it): `place()` fetches the package from the
local artifact store and verifies it against the anchors, runs the licence gate, and `host.load` and
`host.start` go through the workload gate (revocation list, permit rules, effect rules). So an unsigned
binary, a revoked package or a package with no permit cannot be restored. (A revoked package that the
gate refuses is dropped from the journal; this holds.)

What restore **skips** (it calls `place()` directly with a fabricated `CtlRequest`, bypassing
`handle_checked_raw` -> `verify_request` and `dispatch`):

- controller signature and `ControllerPolicy` allow-list, nonce guard, expiry, `valid_decision`;
- re-checking that the journaled `requester` is still an authorized controller (a revoked controller's
  instances come back);
- the "mutations carry a 64-hex decision id" rule: restore stamps whatever `decision_id` the file holds
  (or none) into the chain's `workload.place` record.

Concrete tamper cases (write access to the file only, no key):
1. Set `requester` to a controller that is authorized for project X and `body.project_id` to X. The
   ingest `authorize()` check in `place()` evaluates the file's own claims, so it passes; the cog is
   bound to X's ingest lease and cogbus project without that controller ever signing for it.
2. Add an entry for any package already in the artifact store but never placed on this node, with
   attacker-chosen `config.args`, `csi_port` and `mode`. It passes the gate if a permit matches its kind.
3. Flip `desired_running` to true for an operator-stopped instance ("an operator stop is final"
   becomes false across a restart).

Also: the file is written with `std::fs::write` and chmod 0600 afterwards (a short window at default
permissions), has no MAC, and `read()` follows symlinks and checks no owner or mode (compare the
`user.key` check in boot). The threat needs write access to the runtime directory, which is normally
daemon-uid only, so this is defence in depth, but it is a real gap in "same governance".

Related, not tamper: any restore error (including transient ones such as the artifact store being
momentarily unreadable or the licence service down) calls `j.remove()` and the instance is dropped for
good; the comment relies on the controller's reconcile to re-place it.

Fix direction: HMAC the journal with the node key and verify on read; at restore re-check the requester
against the live `ControllerPolicy` and drop (not restore) on mismatch; keep `decision_id` only if it
validates; retry transient failures instead of removing.

## 4. venq-p3: aliases defeat one-owner-per-device

`DeviceLedger::admit` compares selectors and the node lists resolved at admit time. `SysResolver::expand`
returns a literal path unchanged if it exists (no `canonicalize`), and `usb_nodes` records the
`/dev/videoN` names seen at that moment.

- Concrete case A (symlinks): cog A claims `/dev/video0`; cog B claims
  `/dev/v4l/by-id/usb-Cam_X-video-index0` (a symlink to `../../video0`; the name passes `plain_path`).
  `globs_intersect("/dev/video0", "/dev/v4l/by-id/...")` is false and the node sets
  `{"/dev/video0"}` and `{"/dev/v4l/by-id/..."}` are disjoint, so B is admitted for the same physical
  device. In enforced mode both `chown` calls follow the symlink: the second silently takes the node from
  the first, and the restore ops (the owner captured at admit) restore in the wrong order, which can leave
  the node owned by a dead cog's uid.
- Concrete case B (stale resolution): cog A claims `usb=1bcf:0b15`, resolved to `/dev/video0` at admit.
  The camera is replugged and enumerates as `/dev/video2`; A's `nodes` are never refreshed, so cog B
  claiming `/dev/video2` is admitted. Equally, a USB device that is not plugged in at admit resolves to
  no nodes, so the claim holds nothing until someone else takes the node.
- Argument check bypass: `args_naming` only looks at values that start with `/` after a split on the first
  `=`. `-d/dev/video0`, `--camera=video0`, a by-id alias, `/dev/char/81:0` or a bare index are not seen. In
  advisory mode the ledger is the only guard (the module says so); in enforced mode the chown is, but see
  case A.
- `usb_nodes` for sound cards: `n.contains("C{card}")` matches `pcmC10D0p` when asking for card 1, so card
  1's claim covers cards 10 to 19 (only on hosts with 10 or more cards).

`globs_intersect` and `glob_match` themselves read as correct (memoised, `*` and `?` handled on both
sides, `*` crossing `/` only makes them over-approximate, which is the safe direction). Not established
as a defect.

## 5. cogbus gap 4: slot leak, and nothing caps connections before hello

Question: can an attacker use the caps to exhaust *another* instance's slots? **Not established.** The cap
is keyed by instance id and is checked in `authenticate` after the token lookup, revocation and uid
checks, so a connection can only consume the slots of the instance whose token it holds.

Defects found instead:

- **Slot leak** (`server.rs`, `hello()`): `authenticate` inserts the connection into `conns` and returns
  the `Session`; then `send(Event::Ok).await.then_some(s)`. If that send fails, the session is dropped
  without `broker.close`, and `Session` has no `Drop`. Concrete case: connect, send a valid hello, close
  the socket before the ok frame is written (or reset it). Repeat 8 times: the instance is `busy` until
  revoke or daemon restart. A cog that crashes or is killed at the wrong moment while looping on reconnect
  does this to itself by accident.
- **No limit before hello**: the accept loop spawns a task per connection with no cap; only the 5 s
  `HELLO_TIMEOUT` bounds one. The socket is group `cogbus` (mode 0660), the group every cog uid is in. Any
  cog can hold thousands of idle connections for 5 s each, repeatedly; once the daemon's descriptor limit
  is reached `accept` fails (`EMFILE`) and the loop sleeps 100 ms, which also delays the sweep ticks that
  share the `select!`. That starves other cogs' new connections. Established by reading; not run (needs
  the daemon's `RLIMIT_NOFILE`, commonly 1024 under systemd).
- **Subscription cap is two caps**: the reading path checks `s.subs.len()` only, the stream path checks
  `s.subs.len() + streams.reader_count(conn)`. A connection can take 64 stream readers first and then 64
  reading subscriptions, 128 in all. Low.
- A zero-byte blob chunk (valid: `offset == received`) resets an upload's `last_ms`, so an upload can be
  kept alive indefinitely (its declared size stays reserved). Bounded by the per-instance quota. Low.

## 6. venq-p3: a netdev claim is labelled `enforced` and enforces nothing

`admit` sets `isolation = Enforced` whenever the cog has a uid, whatever the selectors are. For a `netdev`
claim the nodes are `net:can0`; `plan_grants` calls `stat("net:can0")`, which is a relative path that does
not exist, so the plan is empty and nothing is chowned. `cogbus list` and `claim_of` still report
`device_isolation: enforced`. Another uid can open a raw CAN socket on that interface (SocketCAN raw
sockets need no capability by default). The module's own rule is "the listing says which, never implies
it", and this breaks it for netdev (and for any `socket` that is not owned by the cog's uid).

Fix direction: derive isolation per claim from whether the plan actually changes every node (enforced) or
not (advisory), and report `advisory` for netdev.

## 7. cogbus B5 blob store

Asked: access and handle passing, quota and TTL, path traversal in the blob dir.

**Path traversal: not established.** Blob file names are `<hash>.blob`, `<hash>.meta`, `up-<n>.tmp`. For
puts the hash is computed server-side (BLAKE3 of the received bytes), not supplied. For gets the supplied
hash is used only as a `HashMap` key; a path is built only for keys already in the map. At open the key is
a file name, which cannot contain `/`. The directory is created `0700` (`DirBuilder` mode). A directory
that already exists keeps its mode, and `open` does not check it (the blobs inside inherit the umask, so
an existing 0755 directory would expose blob bytes to other cog uids, which is exactly what per-cog uids
are meant to prevent). Low, config-dependent.

**Access rules**: `may_read` is owner, a granted reader, or the same `project_id`. Two project-less
instances share the "node bucket" (documented). Handle passing is transitive, since `grant` lets any
current reader pass a handle on. The doc comment on `grant` says a service "cannot hand on someone else's
handle" while the code (and ADR-122 section 13: "Only the owner's or a holder's handles pass") allows
holders to pass on, so the comment is wrong, not the code. Delivery is limited by the publish and call
routing rules. Not established as a defect.

**Smaller access issues**:
- `readers` is `#[serde(skip)]`, so passed handles are forgotten on restart (fails closed); the owner and
  project still read.
- A put of bytes that are already stored adds the putter as a reader and pushes the expiry out. Anyone who
  has the bytes can keep another project's blob alive for as long as they keep re-putting it (the cost
  stays on the original owner's quota). An expired blob that the sweep has not yet removed is resurrected
  by such a put, together with its old reader list.
- `get` answers `CrossProject` for a blob held by another project and `NotFound` for one that does not
  exist: a confirmation oracle for "does the node hold this exact content" to someone who already has the
  bytes. Low.
- Governance is asked on the first slice of a get only (offset 0); later slices are checked by the store
  but cannot be cut off by a new governance decision mid-download. Low.
- Instance ids are reused: the native `instance_id` hashes workload, version, args and csi bind but not the
  project. A cog re-placed after revoke under another project gets the same id, so `owner == instance`
  gives it the old instance's unexpired blobs (up to 24 h) if it knows a hash. Low.

**Quota and TTL**: node quota 2 GiB, per instance 512 MiB, TTL up to 24 h, and a live blob is never
evicted. Quota is per instance, not per project, so one project that runs 4 instances (or one cog with
four tokens) can fill the node: each puts 512 MiB with a 24 h TTL, and every other put gets
`QuotaExceeded` until they expire. Declared sizes are also reserved at `begin`, so open uploads count.
Design-accepted ("a handle that was handed out stays good") but unbounded per project; add a per-project
cap or a short TTL for the 4th instance. Upload idle expiry is only enforced when a `begin` or the 30 s
sweep runs, so a chunk arriving after the idle window can still succeed in between. Low.

## 8. Not established (checked, nothing found)

- Cross-instance connection-slot exhaustion via the caps (needs the other instance's token).
- Blob path traversal; blob hash forgery (hash is computed, uploads are sequential and owner-bound).
- `globs_intersect`, `glob_match` correctness.
- `WEFTOS_DEVICES` hand-out to a non-owner: the variable is added only from the owner's reservation, the
  child is started with `env_clear`, and the separator `:` cannot occur in a path or interface name
  (the character sets exclude it). A non-owner placement carries no `WEFTOS_DEVICES`.
- Restore accepting an unsigned, revoked or un-permitted package (it goes through the package verify and
  the workload gate).
- Host-journal restore replay: restore builds a fresh request per entry and issues fresh tokens and leases;
  no secret is in the file.

## What would settle the "not run" items

A test that opens N unauthenticated connections to the cogbus socket and then connects a real cog; a test
that closes the socket between hello and the ok reply 8 times and then connects; a place of a cog with
`socket="/tmp/<name>"` under a fake applier that records the `GrantOp`s (the plan already shows the
path), and the same with `socket="/etc/passwd"`; a restart test that writes a file under the cog's
`data/` before the restart and reads it after.

---

## Re-check: findings 1 and 3 on `wt/native-restart` 291aa4309 (2026-10-26)

Tests run (foreground, `scripts/build.sh test clawft-kernel --filter ...`): `tests_restart` 10 of 10
pass; `links_inside_the_stage_directory_are_removed_not_followed` passes.

**Finding 1 (restore wipes `data/`): fixed.** `stage()` now clears everything in the instance
directory except an entry named `data`, keeps `data/` when it is a real directory (checked with
`symlink_metadata`), replaces a link or file where `data/` should be, and removes links with
`remove_file` rather than entering them. My failing case (state written to `data/`, restart, restore)
is covered by `a_cogs_own_data_survives_a_restart_and_a_re_place`. I traced two further cases and found
no problem: a symlink named `data` is skipped by the clearing loop and then replaced by the later step;
a subdirectory is removed with `remove_dir_all`, which does not follow links. The directory above `data/`
is daemon-owned and not writable by the cog uid, so the cog cannot swap `data` between the check and the
`chown` or `chmod`.

**Finding 3 (unauthenticated journal): the editing attack is fixed; replay is not.**

What holds:
- Canonicalisation is sound. The signed bytes are `"weftos.workload-host.journal.v1\n" + node_id + "\n" +
  serde_json::to_vec(entry)`. The entry and everything under it (`PlaceBody`, `CtlConfig`, `RunMode`) are
  plain structs, enums and vectors, so the field order is fixed by declaration and there is no
  `HashMap` whose order could differ between save and verify. The node id sits in the signature, so an
  entry copied from another node fails. The domain tag starts with letters while the node's control
  responses are signed over JSON starting with `{`, so the node's own signing key cannot be made to
  produce a valid journal signature through another protocol. `node_id` is a plain token, so it cannot
  blur the boundary with the entry.
- A changed `requester`, `project_id`, `decision_id`, `config` or `desired_running`, a missing or
  malformed signature, and another node's or key's entry are all refused (tests in `tests_restart`).
  Verification re-serialises the parsed entry, so unknown or defaulted fields that are dropped do not
  change meaning.

What does not hold, concrete failing case (replay of an older, validly signed entry):
- The signature covers the entry's content only. There is no sequence number, generation, timestamp or
  per-file signature, and nothing anchors the journal's state elsewhere. Any (entry, signature) pair that
  this node ever wrote stays valid forever.
- Case A (stale `desired_running`): the operator stops instance X (the journal now holds X with
  `desired_running=false`). An attacker who holds an earlier copy of the file from when X was running
  (a backup, a snapshot, or a copy taken in the brief window below) writes that older entry for X into the
  current file. It verifies. After the next restart X is restored and started: "an operator stop is final"
  is defeated, with no key needed.
- Case B (resurrection): X is unloaded or revoked, so its entry is removed. Pasting the old signed entry
  back restores X. The workload gate still refuses a revoked package, so revocation holds, but an
  operator unload does not.
- The new tests do not cover this; each one edits or forges an entry.
- Needs write access to the runtime directory plus an older copy of the file, so it is the same class of
  condition as before, narrower than the original bug.
- Fix direction: sign the whole file (sorted entries plus a generation number) instead of each entry, and
  record the generation in the node's chain at every save so that a rollback is visible at startup; or
  put a monotonic `seq` in each entry and a high-water mark in the chain. Per-entry signatures cannot
  detect rollback by themselves.

Residual items from my original finding 3 that this commit does not change:
- Restore does not re-check the journaled `requester` against the live controller policy, so an instance
  placed by a controller that has since been removed is restored.
- `save()` still writes the temporary file at default permissions and only then `chmod`s it to 0600.
- Any restore error, including a transient one, still removes the entry for good.

Verdict: finding 1 fixed and verified; finding 3 fixed for tampering, open for rollback and replay.

---

## Re-check: the four follow-ups on `wt/native-restart` b97a41e98 (2026-10-26)

Tests run (foreground): `tests_restart` 19 of 19 pass, including
`an_older_copy_of_the_journal_is_refused_even_though_it_is_validly_signed`,
`the_generation_goes_on_the_chain_and_survives_a_deleted_journal`,
`without_a_chain_instances_are_restored_loaded_and_nothing_is_started`,
`a_requester_the_policy_no_longer_allows_is_refused_and_dropped`,
`the_journal_file_is_created_private_even_over_a_wide_leftover_temp_file`,
`a_restore_that_fails_for_a_transient_reason_is_kept_and_retried_at_the_next_start` and
`a_restore_that_keeps_failing_is_given_up_after_five_starts`.

### My four cases

| Case | Verdict |
|---|---|
| Replay of an older validly signed entry (stale `desired_running`, resurrected unload) | **Fixed against a rollback of the file alone.** The signature now covers node id, generation and all entries, and a file below the chain's latest `workload.journal` generation is refused whole. Two ways round it remain, below (A1, A2). |
| Restore did not re-check the requester | **Fixed.** `allows_node(requester)` is asked of the live `ControllerPolicy`; the trait default is `false`, so a policy that cannot answer restores nothing; the node's own id is always allowed (the field is signed, so it cannot be forged to be it). |
| Journal temp file written at default mode, then chmod | **Fixed.** `write_private` removes any leftover, then `create_new` with mode 0600, `sync_all`, rename. `create_new` does not follow a planted symlink. |
| Any restore error dropped the entry | **Fixed for transient causes, with one wrong class (B1).** Fetch, runtime and admission failures keep the entry and count up to 5 in the signed `failures` field. |

Canonicalisation is still sound: `signed_bytes` is a domain tag, node id, generation and
`serde_json::to_vec(entries)` over plain structs, and `failures` is `#[serde(default)]`, so it is part of
what is signed.

### New problems the generation and chain ordering open

**A1. The chain is only durable at clean shutdown, so the replay window is "everything since the last
checkpoint" (known, card a93f7abe).** `workload.journal` events are appended to the in-memory chain on
every save; the chain file is written only at `Kernel::shutdown`. After a crash or kill, the chain on disk
holds the generations up to the last clean stop, while the journal file holds the newer ones. Concrete
case: chain at generation 10 from the last clean shutdown; the operator stops X (generations 11 to 15; the
daemon is then killed); an attacker puts back a copy of generation 12 with X running. `12 >= 10`, so it is
accepted and X starts. The new test shares one in-memory `ChainManager` across the simulated restart, so it
cannot see this. This closes properly only with the chain-durability fix.

**A2. A chain that has no journal events is treated as authoritative.** `chain_generation` returns 0 when
it finds none, and `restore_instances` only holds back starts when there is *no chain object*
(`chain_latest() == None`). A chain with zero journal events gives `Some(0)`: nothing is "below" it and
`may_start` stays true. Concrete case: an attacker with write access to the runtime directory removes the
chain checkpoint (or the daemon is started with a fresh chain), then restores an old signed journal; it is
accepted and its instances start. Fix direction: when the file's generation is above 0 and the chain holds
no `workload.journal` event at all, treat it like the no-chain case (restore loaded, start nothing).
Rolling back both the chain checkpoint and the journal to an older consistent pair is not detectable
without an anchor outside the directory (a mesh peer or a signed external record); that is inherent and
worth saying in the ADR.

**A3. One-step ambiguity from the write order (accepted by the author, agreed).** File first, chain second:
a crash between them leaves file generation N+1 and chain N. An attacker who then substitutes the
generation-N file (equal to the chain's latest) is accepted. Writing the chain first would turn the same
crash into a false "older copy" refusal and drop everything, so the chosen order is the right one; the
residue is one generation. Low. Recording the file's hash in the chain event would not remove it, only
shrink the ambiguity to a crash.

**A4. A forged chain event can brick restore, if replication accepts it.** `chain_generation` takes the
maximum `generation` over every event with source `workload.host` and kind `workload.journal` whose
payload `node` equals this node, without checking who produced it. The `chain.append` RPC cannot forge it
(its payload is a fixed `WitnessRecord` with no `node` or `generation`). But `workload.host` is neither a
reserved source nor is `workload.journal` a reserved kind (`RESERVED_KINDS` has only
`governance.overlay.applied`, and the comment there names the same hazard: a replicated
`parent_version = u64::MAX` bricking boot). Concrete case, by reading: a cluster peer replicates an event
with that source and kind and payload `{node: <this node>, generation: 18446744073709551615}`. Then
every restart sees a file "older than the chain" and refuses the whole journal; and `set_chain` raises the
in-memory generation to the maximum, so the next `save` does `g.generation += 1`, which wraps to 0 in a
release build (panics with overflow checks). I did not verify that replication accepts this event, so it is
"not established"; the cheap fix is to add `workload.journal` to `RESERVED_KINDS`, use `checked_add`, and
ignore events not appended locally.

**A5. The journal's rename is not followed by a directory fsync.** The file is `sync_all`ed but its parent
directory is not. Once the chain becomes durable on append (the planned fix for A1), a power loss can
leave the directory pointing at the previous generation while the chain has the next, and the next start
would refuse the genuine journal as a replay and drop every instance. Fsync the directory after the
rename.

**A6. Deleting the journal is silent.** A missing file reads as an empty journal at generation 0; the chain
event records the entry count, but restore does not compare, so removing the file unloads everything with
no report. A denial of service only. Low.

### Smaller issue in the retry change

**B1. `Governance` is classed permanent, but the gate also denies "revocation list unreadable (fail
closed)" with that code.** `runtime_refusal` maps `RuntimeError::Governance` to `RefusalCode::Governance`,
and `permanent()` includes it, so a revocation list that is momentarily unreadable at boot drops every
instance for good. Treat that denial as transient (or retry `Governance` once with a note). Also, the 5
tries are counted per start, so a daemon in an unrelated crash loop burns them quickly when a restore fails
transiently each time.

### Verdict

The four follow-ups are fixed as described. The replay protection is real against a rollback of the file
alone, and its limits are those of the chain's own durability and rollback resistance (A1, A2). The
transient and retry change is correct except for the gate's fail-closed denial (B1). A1 and A2 are the
ones I would close before relying on the generation as a security control; A4 and A5 are cheap hardening.

---

## Re-check: cross-project instance ids, `wt/native-restart` a8ab9ad35 (2026-10-26)

Tests run (foreground): `tests_restart` 19 of 19 and `tests_native` 9 of 9 pass, including the four new
project tests (restart then another project starts with no data and leaves the first project's directory
alone; same project keeps its data; unload for A then place for B starts empty; a restart restores each
project into its own instance).

**What is fixed.** The owner key is `"project=<project_id or ->\ncontroller=<requester>"`, folded into the
native `instance_id` hash. The caller does not pick it freely:
- `requester` is the authenticated controller (signature and allow-list in `handle`), or, on restore, the
  journal's signed field re-checked against the live policy.
- `project_id` comes from the body, but a 26-character id is required and, where the ingest bridge is
  wired, `authorize` ties it to what that controller may place.
- The encoding is injective: a project id is exactly 26 characters and can never equal `-`, and the
  requester is a node-id token with no newline, so two different (project, controller) pairs never produce
  the same bytes.
- The owner survives the builders (`with_cogbus`, `with_ingest_*`, `prepare` all take `self` and
  return it), so it reaches `cfg.host` on both a place and a restore.
- The container adapter calls the same `instance_id` and `--tmpfs /data`, so its container names inherit
  the owner and there is no host `data/` to leak. `logical` ids are `project-<project_id>` already.

**C1 (low-medium): the id is only 32 bits of hash, so an attacker can grind a collision.** The id is
`<cog>-<first 8 hex of the blake3>-<node>`. Everything in the hash except the victim's own project and
controller is known or chosen by an attacker who is an authorised controller: the owner string (their own),
`args` (a free plain value if the cog declares any `value_args`; otherwise repeats and order of flags),
`mode`, `csi_port`. Concrete case, by reading: the attacker computes the victim's id offline (the package
and node are public to a controller, and the victim's project and controller ids are visible in listings),
then searches over the free fields for an input whose hash has the same 8 hex digits. About 2^32 blake3
calls, minutes on one core. They place the same cog with those arguments. If the victim's instance is not
currently loaded in this adapter (unloaded with its directory kept, restore pending or failed), `stage()`
accepts `root/<iid>` and keeps its `data/`, so the attacker's instance of the same cog reads the victim's
state and sends it to the attacker's project. While the victim's instance is loaded the adapter answers
"already loaded", which blocks the case. Cogs with no value argument still leave `csi_port` and the
interval seconds, which is of the order of the 2^32 needed. Not run.
Fix direction (either): put the full owner hash in the path (`root/<64-hex owner hash>/<iid>`), or keep the
id and write an owner marker (full hash) into the instance directory that `stage()` compares, moving the
old `data/` aside on a mismatch. A 32-hex (128-bit) id would also do. The marker also covers the unowned
path.

**C2 (low): the unowned path (tests, `weaver run`, `examples/cog_adapter_run`).** `owner = None` leaves the
id exactly as before, so unowned runs of the same package and arguments share a directory among
themselves (a local, single-operator path). An unowned id never equals an owned one except by the
collision in C1, so it cannot reach an owned directory directly. Every production placement goes through
`WorkloadHostService::place`, which always sets the owner; the other `HostContract::new` / `default_feed`
callers in `plane_seed`, `infer_wire` and `project_supervisor` build configs for the Seed, inference and
logical runtimes, not the native adapter.

**C3 (migration, low): ids change for every existing owned placement.** The old directories stay on disk
under their old names, so a cog's `data/` from before this commit is orphaned (no migration, no cleanup).
Journal entries written before it are keyed by the old instance id, so after a restore the old key
lingers beside the new one; on the next starts the stale entry fails with "already loaded" (counted as a
transient failure) until it is given up after five starts. In `restore_instances`, `set_failures` and
`set_desired` use the entry's old key, not the new instance id, in that case. Matters only for nodes that
ran 169e56f20 or b97a41e98; say so in the release notes or add a one-time move of `<old id>/data`.

**C4 (design note): the controller is part of the key.** A controller key rotation gives the project a
new node id, hence a new instance directory; the old `data/` is not reachable. Acceptable, but it means
"same project, same data" holds only while the controller is unchanged.

**C5 (Seed adapter, by design, low).** `seed.rs` ids are `<cog>-seed-<node>` with no owner. The device
holds one installed copy of a cog and its state, and `load` attaches to an installed copy at the same
pin. A second project placing the same cog on the same Seed after the first unloaded (without
uninstalling) sees the device's configuration and data. This matches "one Seed, one mesh" (ADR-106) but is
not project-scoped; worth a line in the ADR.

Verdict: the leak I found (same package, another project, inherited `data/`) is fixed for the native and
container adapters and nobody can choose the owner key; the residual is the truncated id (C1), which an
authorised controller can grind into a collision when the victim's instance is not loaded.

---

## Re-check: journal round 3, `wt/native-restart` f1e051690 (2026-10-26)

Tests run (foreground): `tests_restart` 26 of 26 pass, including the new
`a_journal_the_chain_has_never_heard_of_is_restored_loaded_and_not_started`,
`a_peer_cannot_put_a_journal_generation_on_the_chain_and_foreign_events_do_not_count`,
`a_generation_at_the_end_of_its_range_is_not_wrapped_and_nothing_is_saved`,
`the_directory_is_flushed_after_the_journal_rename`,
`a_deleted_journal_is_reported_as_missing_not_as_an_empty_node`,
`an_unreadable_journal_is_reported_too_when_the_chain_says_there_was_one` and
`an_unreadable_revocation_list_is_transient_but_a_governance_denial_is_permanent`.

| Case | Verdict |
|---|---|
| A2: a chain with no journal events was authoritative | **Fixed.** `Some(0)` with a file above generation 0 now gives `unverified`: restored loaded, nothing started, noted in the report and in `JournalStatus`. Deleting the chain checkpoint then pasting an old journal no longer starts anything. |
| A4: forged or overflowing generation event | **Fixed on the doors I could find.** `workload.journal` is in `RESERVED_KINDS`, so `append_signed` (replication, line 1628) refuses it; the `chain.append` RPC and the tracing bridge now refuse it (`is_caller_reserved_kind`); `chain_generation` counts only events whose payload `node` is this node; `save()` uses `checked_add` and writes nothing at the end of the range. I did not trace every in-process `ChainManager::append` caller; the remaining ones I looked at (`substrate-acl` denials, `mesh.link`) set their own fixed kinds. |
| A5: no directory fsync | **Fixed.** `sync_dir` after the rename. If it fails the file is already in place but the chain event is skipped (file ahead of chain, which restore accepts); the log says "not saved", which is slightly off. Cosmetic. |
| A6: deleting the journal was silent | **Fixed.** A missing or unreadable file while the chain records generation above 0 restores nothing and says so in the report, the log and `JournalStatus` (also in the `include_departed` status listing). Fresh nodes (chain with no journal events) are not flagged. |
| B1: unreadable revocation list dropped instances | **Fixed.** `permanent()` now checks the reason for `REVOCATION_UNREADABLE`; other governance denials stay permanent. Text a package author controls cannot do harm here: matching the substring only makes a denial retry up to 5 times and then drop. |
| A1: chain durable only at shutdown | **Documented, not fixed (as agreed).** ADR-099 now says the generation is tamper-evidence only up to the chain's durability and that a rollback of both file and chain defeats it. Unchanged: replaying a copy between the last checkpointed generation and the real one is accepted after a crash. |

### Consequences of the round-3 changes

**D1 (medium, availability): the A2 rule disables auto-start after the first crash of a node that has never shut down cleanly.**
The chain reaches the disk only at shutdown. A node that has run since the journal was introduced, saved
generations 1 to k, and then crashed has an on-disk chain with *no* `workload.journal` event: `Some(0)`
with file generation k > 0. Restore now brings every instance back loaded and starts none, which is
the crash recovery the journal exists for. It is safe, loudly reported (`unverified`), but the operator
must start each instance by hand until one clean shutdown has written a journal event to the chain. It
goes away with the durable chain (card a93f7abe); until then say so in the release notes.

**D2 (low): a read error sets the journal aside.** `read()` returns `Err` for any I/O error (permission
denied after a uid change, EMFILE, a short read), and `open()` then renames the file to `.json.bad`,
overwriting an earlier `.bad`. With the new rule this is reported and restores nothing instead of
starting empty, which is right, but the evidence of an earlier tamper in `.bad` is lost on the next
failure. Keep numbered copies, and retry a read error once before renaming.

**D3 (low): a refused file is labelled `replayed`.** A file that fails its signature comes back as
generation 0, so against a chain above 0 the status reads "replayed ... older copy" instead of
"refused". The entries are listed as refused in the report; only the state word is off.

**D4 (low, information): the journal state reaches the status listing.** `include_departed` now carries
generation numbers and counts to any caller allowed to list instances. Not sensitive on its own; note it
if the listing is ever exposed beyond controllers.

C1 (the owner marker, or a longer id) is still open and is not part of this commit.

Verdict: A2, A4, A5, A6 and B1 are fixed against my cases; A1 is documented as a known limit. D1 is a
real regression in crash recovery that the durable chain removes; the others are minor.
