# cogbus stream rings: iceoryx2 or our own memfd ring (ADR-122 B2)

- **Date**: 2026-10-25
- **Question**: ADR-122 says to evaluate `iceoryx2` for `stream` topics before
  writing our own `memfd` ring, and to write our own only if iceoryx2 does not fit.
- **Answer**: iceoryx2 does not fit, because the broker would no longer decide who
  may open a stream. B2 ships our own ring: `memfd` on Linux, `shm_open` on macOS,
  with descriptors passed over the broker socket.
- **Code**: `crates/weftos-cogbus/src/stream/`, tests in
  `crates/weftos-cogbus/tests/stream.rs` and `tests/stream_soak.rs`.

## 1. iceoryx2, checked

All facts below were checked against the published 0.10.0 crate (crates.io API and
the crate source in the local cargo registry), on 2026-10-25.

| Question | Finding |
|---|---|
| Version | `0.10.0`, published 2026-09-18 (0.9.3 on 2026-07-08). Repository `github.com/eclipse-iceoryx/iceoryx2` |
| Licence | `MIT OR Apache-2.0` (crates.io metadata and `Cargo.toml`) |
| MSRV | `rust-version = "1.89"`. Our pin is 1.95, so this is fine |
| Platforms | README support table: Linux x86_64 / aarch64 / 32-bit are "done", **tier 2** (a target of tier 1). macOS tier 2. Windows tier 2. FreeBSD tier 2, QNX tier 3, Android a proof of concept. Tier 2 means "works with a restricted security and safety feature set" |
| One writer, many readers, each with its own cursor | Yes. Publish-subscribe services give each subscriber its own buffer (`buffer_size` on the subscriber builder, `subscriber_max_buffer_size` on the service) |
| Overflow and blocking | `enable_safe_overflow(bool)` applies to the **whole service**: with it on, the oldest sample is overwritten and the publisher never waits. With it off, the publisher's `BackpressureStrategy` (`RetryUntilDelivered` or `DiscardData`) applies to every full subscriber, and a backpressure handler (`BackpressureInfo { receiver_port_id, retries, elapsed_time }`) can discard per receiver after a deadline. A topic with one `drop-oldest` reader and one `block` reader cannot be expressed: blocking needs safe overflow off for everyone, and then a full drop-oldest reader loses the *newest* sample, not the oldest. `sample` would be `buffer_size(1)` with overflow, which again needs overflow on for that reader only |
| How a reader in another process gets access | By **name**. A service is a set of files under `/tmp/iceoryx2/` (`ICEORYX2_ROOT_PATH`) plus POSIX shm objects prefixed `iox2_`. Any process that can open those files opens the service. There is no descriptor hand-over. The dynamic segment is created `OWNER_READ_WRITE` (`iceoryx2-cal` `dynamic_storage/file.rs`), so cogs running as different uids cannot share a service unless the permissions are widened for everyone |
| Fit with our identity model | Does not fit. ADR-122 §4–5 make the broker the authority: a cog presents its token, `SO_PEERCRED` is checked, and only then may it read a topic its manifest declares. With iceoryx2 the broker can refuse to *tell* a cog the service name, but names are guessable and discoverable from the root directory, so the check would be advice the cog can bypass. Per-uid isolation would be lost or would block sharing entirely |
| Writer death | iceoryx2 has dead-node detection and cleanup. That is useful, but our ring needs less: the broker keeps the descriptor, so the ring survives its writer anyway (§3) |
| Dependency weight | A scratch crate with `iceoryx2 = "=0.10.0"` (outside the workspace): 71 crates in the normal dependency tree, **33 of them `iceoryx2-*`**. Clean release build 29.9 s on the M-series dev Mac (67 compiled units). `bindgen` and `clang-sys` are in the build graph but only run for QNX targets; Linux and macOS use `libc` bindings |
| `unsafe` | 4,773 lines containing `unsafe` across the 0.10.0 `iceoryx2*` crates (2,393 in `iceoryx2-pal-posix`, its own POSIX layer, 657 in `iceoryx2`, 415 in `iceoryx2-bb-container`). It reimplements its own POSIX abstraction, containers, lock-free queues and allocator. That is a large surface to review for code that sits in the node daemon |
| Speed | The scratch test (one publisher, one subscriber draining, one stalled subscriber, 640-byte samples, safe overflow on) sent 200,000 samples in 46 ms, about 4.3 M/s. Speed is not the deciding factor: 16 kHz audio needs 50 records a second |

Runtime check: the scratch publisher created `/tmp/iceoryx2/nodes` and
`/tmp/iceoryx2/services`, mode `drwxr-x---`, owned by the running user. That is the
discovery directory described above. (The scratch directory was deleted afterwards.)

## 2. Decision

Use our own ring. iceoryx2 is a good library. It is rejected here for one reason
that settles the question and three that add to it:

1. **Authority.** The broker must stay the only way to open a stream. Passing a
   descriptor after the broker's checks does that; a name in a shared directory does
   not.
2. Reader policies are per reader in ADR-122 §6 (drop-oldest, sample, and block with a
   deadline, mixed on one topic). iceoryx2's overflow setting is per service.
3. 33 extra crates and a second POSIX layer in the daemon, for a ring that needs
   about 250 lines.
4. Linux support is tier 2, the same as ours would be. It gives no extra safety
   guarantee on the platforms we run.

Revisit iceoryx2 (or Zenoh's shared memory, ADR-122 alternatives) if we ever need
zero-copy loans of large frames across many readers, typed samples, or its
event/waitset machinery. Those are not B2 needs.

## 3. The ring we built

**Carrier.** One shared-memory object per stream topic, created by the broker.

- Linux: `memfd_create(MFD_CLOEXEC | MFD_ALLOW_SEALING)`, sized, then sealed
  `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`, so no holder can truncate the ring and
  make readers fault. A read-only descriptor is reopened through `/proc/self/fd/N`
  with `O_RDONLY`.
- macOS (development only): `shm_open` under a random name with `O_EXCL`, opened a
  second time `O_RDONLY`, then `shm_unlink`ed at once, so no name outlives the call.
- Other OSes: `stream_open` / `stream_subscribe` answer `unsupported`. Windows is not
  supported; the client and server are already Unix-only.

The broker passes descriptors over the existing socket with `SCM_RIGHTS`, at most 4
per frame. An answer or notice that carries descriptors says how many in its `fds`
field. The writer gets the ring read-write. Each reader gets it read-only, so
`mmap(PROT_WRITE)` fails with `EACCES` (Linux) or `EPERM` (macOS). A `block` reader
also gets a one-word cursor page, read-write for itself and read-only for the writer.
No stream byte goes through the socket.

**Layout.** A 128-byte header (magic `COGRING1`, version, slot count, slot size,
max record, ring id, `head` on its own cache line, writer epoch), then fixed-size
slots of `round64(32 + round8(max_record))` bytes: `seq`, `len`, `t_ns` (the
producer's clock), `w_ns` (wall clock at write), then the payload. Record `seq` goes
in slot `(seq - 1) % slot_count`. With a 640-byte PCM chunk and the default 4 MiB
ring that is 5,957 slots of 704 bytes, about 119 s of audio.

**Protocol.** Each slot is a seqlock, and every access is an atomic, so the ring
involves no data race in Rust's memory model. The writer marks the slot `BUSY`,
issues a release fence, writes the fields, stores `seq` (release), then `head`
(release). A reader loads `seq` (acquire), copies the slot, issues an acquire fence,
re-reads `seq`, and keeps the copy only if `seq` did not change. A reader keeps one
slot clear of the record the writer may be writing, so the readable window is
`head + 2 - slot_count ..= head`. A reader below that window jumps to its oldest
record. The jump shows up as a gap in `seq`, and `StreamReader::skipped()` counts it.

**Hardening.** Readers take the geometry from the broker's answer, never from the
header. They check the object's size with `fstat` before mapping, clamp `len` to
`max_record`, and bound every read loop. A writer that scribbles on its own ring can
garble its own records, but it cannot make a reader read out of bounds, fault, or
spin forever.

**Policies (ADR-122 §6).**

- `drop-oldest`: the cursor is local to the reader. The writer never sees it and
  never waits for it.
- `sample`: before each read, the reader moves its cursor to `head`.
- `block`: before writing a record that would push a `block` reader's next record
  out of its window, the writer waits, spinning briefly, then yielding, then sleeping
  50 µs. `write_async` uses tokio sleeps instead. The wait lasts at most the deadline
  (50 ms by default, from the broker). Past it, the writer stops waiting for that
  reader and sends `stream_demote`. The broker checks that the sender is the topic's
  writer, logs a `cogbus` warning (B3 routes this to the activity log), counts it in
  `cogbus list` `drops`, and pushes the reader a `policy` notice. From then on the
  reader is `drop-oldest`. One write never waits longer than one deadline.

**Lifecycle (ADR-122 §11).**

- *Writer death*: the broker holds the descriptor, so readers keep reading what is
  there. The next `stream_open` by the same instance continues in the same ring at
  the next `seq`. If it asks for a different shape, the broker makes a new ring whose
  head is the old head, and moves every reader to it with a `ring` notice; readers
  resume at its head. Another instance may take the topic only after the writer
  instance is revoked, as with readings. The broker notices a revoked writer
  lazily, by asking whether the instance is still registered, so `revoke` needs no
  stream hook. A revoked reader's connections close, and that is where its
  subscriptions go.
- *Broker restart*: rings are lost. A writer that reopens with `seq_base` (its last
  seq) keeps `seq` increasing, and readers resume at the head, so the gap is visible.
- A ring with no writer instance and no readers is freed.

**Limits.** Ring size comes from the writer's `stream_open` (`ring_bytes`, default
4 MiB, at most `max_ring_bytes` = 64 MiB). The sum of all rings is capped per node
(`node_cap_bytes`, default 512 MiB), and records may be at most 1 MiB. All of these
are `StreamConfig` fields (`Broker::new(..).with_stream_config(..)`). `Broker::stream_bytes()` reports
the current total for node facts.

**Rules (ADR-122 §5).** `stream_open` needs a declared output of class `stream` with
the declared schema. `stream_subscribe` needs a declared input with its schema, in
the same project. It names one source (no `<kind>/*`). A `stream` output cannot be
published as readings.

## 4. Measured

Debug builds, M-series Mac, macOS `shm_open` path.

- In-process ring, 200,000 records written while a second thread reads: no torn
  record (every byte of each record checked).
- Writer throughput over the socket rig, 640-byte records, 5 runs: 360k–433k/s
  alone, and 428k–441k/s with a subscribed `drop-oldest` reader that never reads.
  The stalled reader costs nothing, because the writer does not know it exists.
- `block`: a stalled reader held the write for 50.01–50.12 ms (5 runs), was then
  demoted and told; the next 92 writes took under 45 ms in total. A `block` reader
  that keeps up (8-slot ring, 400 records, a 2 ms pause every 16 records) lost
  nothing.
- Soak (`tests/stream_soak.rs`): 16 kHz s16le mono, 20 ms chunks (640 bytes,
  50/s), with the writer and the reader each in its own process and the broker in
  the test process. The reader checks every sample.

  | Run | Chunks | Received | Skipped | Corrupt | Mean / max latency | Worst write |
  |---|---|---|---|---|---|---|
  | macOS (`shm_open`), 180 s | 9,000 | 9,000 | 0 | 0 | 1.44 ms / 6.8 ms | 80 µs |
  | Linux aarch64 (`memfd`, OrbStack Debian trixie VM), 150 s | 7,500 | 7,500 | 0 | 0 | 1.89 ms / 9.8 ms | 155 µs |

  The latency is mostly the reader's 1 ms poll. The 1-hour soak is
  `COGBUS_SOAK_SECS=3600 scripts/build.sh test weftos-cogbus --filter pcm_soak`
  (`COGBUS_SOAK_POLICY=block` for a block reader, `COGBUS_SOAK_REPORT=<path>` for
  JSON).
- The full test set (36 tests) also passes on Linux aarch64 through
  `scripts/build.sh test-pi weftos-cogbus`, which exercises the `memfd`, seal and
  `/proc/self/fd` read-only path.

## 5. Not done in B2

- **Declared ring size in `cog.toml`.** The ring size comes from `stream_open`. Adding
  `ring_bytes` to `[[outputs]]` touches `OutputDecl` in the kernel and
  `OutputSpec`'s struct literals in `clawft-weave` and the B1 tests, so it waits
  until B3 and B4 have merged.
- **Binary record layout per schema.** The ring carries `seq`, `t_ns`, `w_ns` and
  opaque bytes. `pcm.s16le.v1` and `iq.cs16.v1` are JSON schemas with base64
  payloads. A binary profile for each (rate and channels in a small header, or fixed
  per ring) belongs in `contracts/cogs/`. The soak writes raw samples.
- **Reader wake-up.** Readers poll (`next()` sleeps 1 ms by default, settable). A
  futex or eventfd per ring would remove the polling.
- **Wildcard stream subscriptions** (`<kind>/*`) are refused as `unsupported`.
- **No broker receive time.** Stream records carry `w_ns`, the writer's wall clock at
  write time, in place of the broker's `recv_ns`, because no stream byte passes
  through the broker.
- Governance decisions and the activity log for stream open, subscribe and demote
  are B3. Today they are `tracing` records with target `cogbus`.
