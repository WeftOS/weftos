# Cog conformance harness

> **Cogs project:** the cogs, the Cog Host and their operator docs live in [WeftOS/weftos-cogs](https://github.com/WeftOS/weftos-cogs/tree/v0.1.4). Its decision records are the COG-NNN series. This WeftOS doc keeps only what WeftOS implements: the harness under `scripts/cogs/` and its baseline.

Card mesh-placement-08. Where a project gets its cogs: [cog-sources.md](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/cogs/cog-sources.md); running them: [operator-guide.md](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/cogs/operator-guide.md). Governing design: [ADR-100](../adr/adr-100-cog-workload-kind.md)
(cog workload kind, run modes, v1 scope) and
[ADR-099](../adr/adr-099-governed-workload-placement.md) sections 2 and 3
(capability provenance, `perf.cog.cycle_ms`, admission self-check).

The harness runs Cognitum cog binaries against a synthetic ESP32 feed and a stub
Seed ingest endpoint, classifies each run, and records measured cycle times. It is
the committed replacement for the 2026-09-28 prototype sweep, and it doubles as an
admission probe.

## What a run does

`scripts/cogs/harness.py` runs on the target (inside a container, or natively on a
Linux ARM node). It needs only Python 3.8+ and the standard library. For each cog it:

- sends a fake UDP feed to `127.0.0.1:5006`:
  - `features` (default, the baseline feed): ADR-069 `MAGIC_FEATURES` `0xC5110003`,
    48 bytes, 8 little-endian f32 at offset 16, 50 Hz, a small sine with a 0.95
    spike every 40th packet;
  - `vitals`: `MAGIC_VITALS` `0xC5110002`, the 32-byte `edge_vitals_pkt_t`
    (presence, 15 bpm breathing, 72 bpm heart rate, one person) at 5 Hz;
  - `both`: the two together.
- serves `POST /api/v1/store/ingest` on `127.0.0.1:80` and answers 404 to
  everything else;
- runs the cog with `--once` or `--interval N`, with a per-cog cap (default 15 s),
  and captures timestamped stdout, stderr and ingest POSTs;
- writes a raw JSON result: exit code, ingest count, stdout/stderr tails, binary
  sha256, and `cycle_ms`.

`cycle_ms` is the wall time of the process for `--once` (only when it exits 0),
and the median gap between consecutive cycle events (ingest POSTs, else stdout
lines) for `--interval`.

## Outcomes and groups

`scripts/cogs/classify.py` maps a raw result to an outcome: `clean` (at least one
ingest POST, and exit 0 for `--once` or still cycling at the cap for `--interval`),
`no-output`, `cli-error` (non-zero exit), `missing-binary` or `exec-error`.

`scripts/cogs/expectations.json` holds the per-cog ADR-100 catalog group for
aarch64: `clean`, `needs-interval` (with its `interval`), `needs-extra-cli` (with
`needs` tags such as `seed-peers`, `seed-api`, `mqtt-broker`, `model-assets`,
`tailscale-auth`, `cli-args`), or `no-build`. It also records `run_mode` (how the
cog must be run to be clean) and `once_outcome` (what a plain `--once` produces).

## Commands

All commands go through `scripts/build.sh`:

```bash
# Baseline sweep: every aarch64 cog with --once on OrbStack, checked against the baseline
scripts/build.sh cogs-conformance sweep --runtime docker --arch aarch64 --mode once \
    --label orbstack-aarch64-once --check-baseline

# Same on Apple container
scripts/build.sh cogs-conformance sweep --runtime apple-container --arch aarch64 \
    --mode once --label apple-container-aarch64-once --check-baseline

# Each cog in its expected run mode (needs-interval cogs get --interval 1)
scripts/build.sh cogs-conformance sweep --runtime docker --mode expected \
    --cogs cardiac-arrhythmia,sleep-apnea,fall-detect

# Admission probe: one cog, emit perf.cog.cycle_ms and upgrade node facts
scripts/build.sh cogs-conformance probe --runtime docker --cog anomaly-detect \
    --node-facts node-facts.json --out probed.json

# Re-classify a results file; unit tests
scripts/build.sh cogs-conformance summarize scripts/cogs/results/orbstack-aarch64-once/results.json
scripts/build.sh cogs-conformance selftest
```

Options shared by `sweep` and `probe`:

| Option | Meaning |
|---|---|
| `--runtime` | `docker` (OrbStack, Docker Engine or Desktop), `apple-container`, `native`, `ssh` |
| `--arch` | `aarch64` (default) or `arm` (armv7; needs an emulating runtime on Apple Silicon, which has no AArch32) |
| `--feed` | `features` (default, matches the baseline), `vitals`, `both` |
| `--timeout` | per-cog cap in seconds (default 15) |
| `--binary-dir` | use local binaries named `cog-<id>-<arch>` instead of downloading |
| `--image` | container image (default `python:3.12-slim-bookworm`) |

Binaries are downloaded on demand from the public Cognitum bucket
(`cogs/arm64/cog-<id>-aarch64`, `cogs/arm/cog-<id>-arm`) into
`scripts/cogs/.cache/<arch>/`, which is gitignored. A download must be an ELF for
the requested machine, or it is rejected. Binaries are never committed.

## Results

`sweep` writes `scripts/cogs/results/<label>/`:

- `results.json`: raw per-cog results plus the target's machine / kernel / Python;
- `summary.json`: outcomes, group lists and counts, unexpected outcomes, runtime,
  arch, feed, timestamp;
- `capabilities.json`: one `perf.cog.cycle_ms` capability per clean cog, in the
  ADR-099 section 2 shape with `provenance: "measured"`.

With `--check-baseline`, the summary is compared per cog and per group with
`scripts/cogs/baseline/aarch64-once-2026-09-28.json`, and the command exits
non-zero on any difference. Without it, the command exits non-zero when any cog
disagrees with its expectation.

## Baseline

The recorded baseline is the prototype sweep of 2026-09-28 (OrbStack linux/arm64,
every aarch64 cog `--once`, 15 s cap, features feed). It covers the Cognitum catalog
cogs recorded in `scripts/cogs/expectations.json` (108 entries), not the 29 cogs
released from WeftOS/weftos-cogs, so its counts below are not the Cogs repository's cog count. The raw prototype lines are in
`baseline/aarch64-once-2026-09-28.summary.txt`. Of 107 cogs with an aarch64 build
(`presence-field` has none):

- 93 clean;
- 5 need `--interval` (persistent-listener health cogs: cardiac-arrhythmia,
  health-monitor, respiratory-distress, ruview-densepose, sleep-apnea). With
  `--once` they fall back to the Seed sensor stream and print `error: no JSON`;
  with `--interval 1` they ingest once per second;
- 9 need seed peers or other CLI: cloud-inference (rejects `--once`), tailscale
  (needs tailscaled auth), cognitive-pipeline (model assets and agent wiring), and
  six swarm-* cogs (backup-restore, delta-sync, distributed-store, deploy,
  edge-orchestrator, mqtt-bridge).

ADR-100 and the card text say "7 need seed peers or other CLI", but 93 + 5 + 7 is
105, not 107. The raw results list nine, and the harness reports nine.

## Honesty rules

- **Emulation is not measurement.** A result counts as native only when the
  machine that ran it matches the arch (`aarch64`/`arm64` for aarch64; `armv7l`,
  `armv8l` for arm, plus an aarch64 kernel on a raw node). For a container
  harness the driving host's machine is checked as well, because an emulated
  container reports the emulated machine. When `DOCKER_HOST` is set, the
  engine is asked (`docker info --format {{.Architecture}}`); if that fails the
  run is not native. An unknown machine is not native. A
  non-native run emits no `perf.cog.cycle_ms`, upgrades no node capability, and
  is listed under `emulated` in the sweep summary and the probe output.
- **Binaries are hash-checked.** Pass `--sha256-manifest` (a JSON map of
  `cog-<id>-<arch>` to its sha256, at the top level or under `binaries`). With a
  manifest, every binary must be listed and match, whether downloaded or from
  `--binary-dir`; an unlisted one is refused. A download that does not match is
  never written and a mismatching cache entry is discarded. The manifest's trust
  root is whoever supplies it: the harness proves the files match the manifest,
  not that the manifest is honest. `--binary-dir` with no manifest runs
  unverified binaries and needs an explicit `--insecure-local` (it prints a
  warning). On the node the harness copies each binary into a directory it
  owns, makes it read-only, hashes that copy, and runs that copy, so the file
  cannot be swapped between the check and exec.
- **Interval cogs must cycle.** An `--interval` run is clean only when it was
  still running at the deadline (or stopped by the launcher) after at least two
  cycle events. One POST and then silence is `no-output`.
- **Malformed input is a one-line error** (`error: ...`, exit code 2), not a
  traceback: node facts, results files, expectations and the manifest.

## Admission probe and provenance

`probe` runs one cog in its expected run mode. It emits a `perf.cog.cycle_ms`
capability (`attrs.cog_id`, `value` in ms, `mode`, `interval_s`, `arch`,
`runtime`, `feed`, binary `sha256`, `measured_at`) with `provenance: "measured"`.
Given `--node-facts` (a JSON list of capabilities, or `{"capabilities": [...]}`),
it returns the list with the exercised `cpu.arch.*` and runtime capability
(`runtime.container.docker`, `runtime.container.apple`, `runtime.native`) upgraded
to `measured`, but only when the cog ran clean. It never downgrades, leaves
unexercised capabilities alone, and replaces any older cycle time for the same cog,
arch and runtime. The exit code is 0 only for a clean run, so a placement admission
step can gate on it.

## Remote-node mode

Use `--runtime ssh` to test a real ARM node, such as a Pi 5 or an ARM server,
without installing WeftOS on it. The driver:

1. stages `harness.py`, `plan.json` and the binaries locally;
2. `ssh <host> 'rm -rf cog-conformance && mkdir -p cog-conformance'`;
3. `scp`s them into `~/cog-conformance/` on the node;
4. runs `python3 cog-conformance/harness.py --plan ... --out ...` there, prefixed
   with `sudo -n` when you pass `--sudo`;
5. copies `results.json` back and classifies it locally.

Requirements on the node: Linux, the target arch, `python3`, key-based SSH, and
the right to bind `127.0.0.1:80`, since cogs post to the Seed ingest port. Get
that with `--sudo` (passwordless sudo for `python3`), by running as root, or by
lowering `net.ipv4.ip_unprivileged_port_start`. UDP 5006 must be free, so stop
any running cog or `presence-field` broker first (upstream cogs#14).

The host comes from `--ssh-host` or `COG_HARNESS_SSH_HOST`. Use an alias from
`~/.ssh/config`. It is never written into results, and labels should not name
hosts:

```bash
COG_HARNESS_SSH_HOST=pi5 scripts/build.sh cogs-conformance sweep --runtime ssh \
    --arch aarch64 --sudo --mode expected --label remote-aarch64-expected
```

On the node itself, `--runtime native` does the same without SSH. `--remote-dir`
sets the work dir (relative to the remote login directory, default
`cog-conformance`); it is removed and recreated on every run.

The real Pi 5 lane, `scripts/build.sh test-pi` ([test-pi.md](test-pi.md)), runs
this mode twice (plain harness, and the Pi-built launcher with the native
adapter) together with the kernel tests and the native adapter live test. That
replaces the aarch64 container standing in for a Linux ARM node below.

Cognitum Seeds are not driven by this mode. They run cogs through their own HTTP
API (ADR-100 section 5), and that adapter is card 09.

## Adapter-driven runs (card 09)

With `--launcher`, the harness does not spawn the cog itself. It runs
`<launcher> -- <cog argv>`, where the launcher is `examples/cog_adapter_run.rs`:
it packs and signs the binary with a throwaway operator key, verifies it, and
runs it through a `WorkloadRuntime` adapter under `WorkloadHost` (governance on
every transition, an in-memory chain, never operator data). `--runtime` still
says where the harness runs; `--adapter-runtime` picks the adapter.

```bash
scripts/build.sh cogs-launcher --linux-arm64      # launcher for a Linux ARM node / container
scripts/build.sh cogs-launcher                    # launcher for this host

# native adapter, aarch64 container standing in for a Linux ARM node
scripts/build.sh cogs-conformance sweep --runtime docker --cogs anomaly-detect \
    --launcher target/linux-arm64/debug/examples/cog_adapter_run \
    --adapter-runtime native --adapter-run-as 65534:65534 --label adapter-native-aarch64-container-once

# docker adapter (OrbStack): harness and cog share the VM's host network, so the
# cog reaches the harness ingest stub on 127.0.0.1:80. The harness container
# gets the engine socket and a static Linux docker CLI.
scripts/build.sh cogs-conformance sweep --runtime docker --cogs anomaly-detect \
    --launcher target/linux-arm64/debug/examples/cog_adapter_run \
    --adapter-runtime docker --adapter-network host --adapter-base-image python@sha256:<digest> \
    --harness-engine-arg=--net=host \
    --harness-engine-arg=-v --harness-engine-arg=/var/run/docker.sock:/var/run/docker.sock \
    --harness-engine-arg=-v --harness-engine-arg=<linux docker cli>:/usr/local/bin/docker:ro \
    --timeout 60 --label adapter-docker-aarch64-once

# Apple container adapter, harness on the Mac, feed published to the container.
# A cog posts to 127.0.0.1:80 inside its own VM, so the adapter fronts it with
# the ingest relay (workload_runtime/container_relay.rs) pointed at the VM
# gateway (`container network inspect default` -> ipv4Gateway), where the
# harness stub listens.
scripts/build.sh cogs-conformance sweep --runtime native --cogs anomaly-detect \
    --launcher target/debug/examples/cog_adapter_run --adapter-runtime apple \
    --adapter-feed-port 25006 --udp-port 25006 --ingest-port 18080 \
    --ingest-bind <gateway> --adapter-ingest-upstream <gateway>:18080 \
    --adapter-base-image python@sha256:<digest> --timeout 60 --label adapter-apple-aarch64-once

# docker adapter on a bridged network with the same relay (OrbStack's host
# address from inside a container is host.docker.internal = 0.250.250.254)
scripts/build.sh cogs-conformance sweep --runtime native --cogs anomaly-detect \
    --launcher target/debug/examples/cog_adapter_run --adapter-runtime docker \
    --adapter-feed-port 25006 --udp-port 25006 --ingest-port 18080 \
    --ingest-bind 0.0.0.0 --adapter-ingest-upstream 0.250.250.254:18080 \
    --adapter-base-image python@sha256:<digest> --timeout 60 \
    --label adapter-docker-bridge-relay-aarch64-once
```

Results (2026-09-29, `scripts/cogs/results/adapter-*`): native and docker are
clean for anomaly-detect `--once`, and for anomaly-detect, fall-detect,
sleep-apnea and health-monitor in their expected modes (the last two
`--interval`, exercising start / stop). Apple container is clean for
anomaly-detect in `--once` and expected mode: the relay carries the cog's one
ingest POST from its VM loopback to the harness stub on the gateway. Docker on
a bridged network with the relay is clean too. The relay is a Python script, so
it needs `python3` in the operator-pinned base (the image build checks for it);
the container starts as root with only `NET_BIND_SERVICE`, `SETUID` and
`SETGID`, binds `127.0.0.1:80`, and the cog drops to `nobody` before `exec`.
The node-local ingest bridge the relay targets in production is card 10.
