# ADR-124: Node clock status and reading time

- **Status**: Proposed (draft, 2026-10-26)
- **Date**: 2026-10-26
- **Deciders**: Owner / platform. Not accepted until the owner says so.
- **Depends-On**: ADR-025 (Ed25519 node identity), ADR-100 (cog workload kind), ADR-106 (Seed licence clock floor)
- **Relates-To**: ADR-099 / ADR-103 (node id), ADR-107 (catalog module `gt-u7-gps`), `docs/architecture/sensor-node.md`, `docs/hardware/module-types.md`, `docs/cogs/weft-licence.md`, `docs/research/copper-bus-timing-grounding/weftos-stack-map.md`
- **Does not assign** a cog-repo ADR number. Cog-repo ADR-167 is HLK-LD1040C on the resolved tree. This decision is a WeftOS ADR.

## Context

Cog JSON and `SignedSensorFrame.timestamp_ms` carry wall time with no source and no uncertainty. Fusion across nodes needs both. The Seed has no RTC. Below `CLOCK_FLOOR` (1,780,000,000 unix seconds, ADR-106) the licence cog answers `clock_not_set` and does not sign.

Two identities and several clocks already exist, and this ADR does not merge them:

| Place | What it is |
|---|---|
| `docs/architecture/sensor-node.md` | `node_id` is `hex(SHA-256(pubkey)[..16])`. `source_id` is the installed sensor and stays put when the part moves. `t_ns` is the decoder clock at capture. Arrival time is a second stamp. |
| `SignedSensorFrame` in `weftos-sensor-pipeline-wire` | `node_id` is a `u64`. `timestamp_ms` is host milliseconds on `mesh.sensor.v1.encoded`, good enough for a latent and not a ranging clock (`weftos-stack-map.md`). |
| BME680 SenML (`weftos-cogs-bme680`) | `bn` is `source_id` and ends in `/`. `t` is Unix seconds. Units are `Cel`, `%RH`, `Pa`, `Ohm`. Names include `gas_resistance`. `proof_` is `MEASURED`, `CODE`, or `SYNTHETIC` on every record. A missing quantity is omitted. |
| Licence | `CLOCK_FLOOR` as above. |
| Acoustic bus | `t_pps` and `dt_us` on the PPS frame. Not wall time. |
| Civil hold | An RTC such as the Epson RX8130CE. One tick is 30.5 µs (`module-types.md`). Echo and chirp timers stay faster counters. |

`SensorTopic` is a closed set of three (`encoded`, `consensus`, `control`) in both `weftos-sensor-pipeline-wire` and `clawft-kernel` `mesh_sensor.rs`. The signed payload is an `EncodedObservation`, a `ConsensusFrame`, or a `ControlFrame`.

The catalogue already has the PPS module this is for: `gt-u7-gps`. No cog writes chrony, gpsd, or `/dev/pps` today.

## Decision

### 1. Clock status is its own topic

Each node that originates sensor frames publishes one clock-status record on `mesh.clock.v1.status`. That topic is not a `SensorTopic` variant. It is not carried inside `SignedSensorFrame`. The three `mesh.sensor.v1` topics stay as they are.

The record is JSON:

| Field | Meaning |
|---|---|
| `schema` | `weavelogic.clock.v1` |
| `node_id` | The sensor-node hex id, `hex(SHA-256(pubkey)[..16])`. |
| `boot_id` | New on each process start of the publisher. Stable until that process exits. |
| `clock` | `pps`, `ntp`, `sntp`, or `free`. |
| `uncert_us` | Bound on the capture clock, microseconds, integer. |
| `stratum` | NTP stratum. `free` uses 16. |
| `offset_us` | Signed offset the host last applied or observed, microseconds. |
| `drift_ppb` | Oscillator drift, parts per billion. |
| `last_lock_unix_s` | Unix seconds of the last lock. Absent when the node has never locked. |
| `serve_mesh` | Whether this node is willing to serve NTP to the mesh. |
| `simulated` | True when the record is synthetic. |

While `clock` is `free`, uncertainty grows from the last lock:

`uncert_us = last_uncert_us + (drift_ppb * elapsed_s) / 1000`

One ppb is 0.001 µs per second. A node that has never locked uses 50 ppm, stored as `drift_ppb = 50000`.

Source order, best first: `pps`, then `ntp`, then `sntp`, then `free`. The live source is the eligible one with the smallest `uncert_us`. Ties break by that order, then by stratum. `pps` is eligible only when gpsd reports a fix (mode at least 2), a PPS edge has arrived within 5 seconds, and chrony has selected the PPS refclock. A better source is adopted after it has been better for 30 seconds. A worse source is adopted as soon as the current one is ineligible.

On a mesh with no upstream NTP, the node with the best locked source may serve NTP at stratum 10. If several are equal, the lexicographically lowest hex `node_id` serves. `serve_mesh` is `auto` on a `pps` node, and a node may force it off.

### 2. The host owns the clock. A cog may only advertise it.

A host service on the node selects the source, writes chrony, and slews. It is the only thing that steps the clock.

A step is allowed in two cases. The clock is below `CLOCK_FLOOR` and the step lands at or above it, which is how a Seed with no RTC becomes able to sign. Or the absolute offset is greater than one second, and the step does not move time backward past a timestamp already used on a signed licence request. ADR-106 is unchanged: below the floor the licence cog still answers `clock_not_set` and does not sign.

A cog does not write `sources.d`, reload chrony, or step the clock. If a package is added later to mirror status onto the console, it only reads `chronyc tracking` and the host's status record. Its export port is **8058**, loopback only. 8055 is `ld6004-radar` and is not used. The cog category `system` is added to `scripts/cog_lint.py` `KNOWN_CATEGORIES` before that manifest is valid. This ADR does not create that package and does not take a cog-repo ADR number.

### 3. A scalar reading stays SenML

Temperature, humidity, pressure, gas, a level, heart rate, and breathing leave as a SenML pack (RFC 8428), as `sensor-node.md` already requires.

- `bn` is `source_id` and ends in `/`.
- `t` is the capture instant in Unix seconds. Fractional seconds are allowed. It is not the emission time and not the arrival time.
- `proof_` on every record is `MEASURED`, `CODE`, or `SYNTHETIC`.
- `boot_id_` on every record is the decoder process's `boot_id`. A reboot is visible even when `t` jumps.
- Units and names already shipped stay: `temperature`/`Cel`, `humidity`/`%RH`, `pressure`/`Pa`, `gas_resistance`/`Ohm`. A missing quantity is omitted.
- When a cog also declares mesh units, they are the SenML names. Acceleration `m/s2`, angular rate `rad/s`, magnetic field `T`, length `m`. The JSON export (`temp_c`, `pressure_hpa`, `mag_ut`, nested `accel_g`) stays the export. Conversion happens in the emitter.
- `sensor_class_` on the first record, when present, is an id from `crates/weftos-cog-market/catalog/sensor_types.json`. It is not `EncodedObservation.sensor_class` and not the cog's `category`.

`t_ns` on a decoded frame is the same capture instant in nanoseconds. SenML `t` is that instant in seconds. Arrival time stays a different stamp. Envelope `timestamp_ms` on `SignedSensorFrame` stays the emission time of the latent frame.

`[outputs]` in a `cog.toml`, when a cog declares it, is checked by `cog_lint.py`: `sensor_class` is a catalog id, each unit is from the set above, and the names match the SenML names. `weftos-cog-manager` does not parse that table.

### 4. Ranging clocks stay off the latent stream

A DW3000 device counter, a PPS edge counter, and an acoustic `t_pps` stay inside the cog that read them. A range leaves as `spatial.evidence.v1` with type `uwb_echo` (or another type that spec already lists), the node capture time, and `uncert_us`. No pose means no evidence line. TDoA is not stamped from `mesh.sensor.v1`.

## Out of scope

- A fourth `mesh.sensor.v1` topic, a `SensorTopic::Reading` variant, or a wire-version bump.
- Unifying the hex `node_id` with the `u64` on `SignedSensorFrame`. The latent envelope keeps its integer.
- Filing this as cog-repo ADR-167, or renumbering ADR-166 again. Sound-detect keeps 166. HLK-LD1040C keeps 167 on the resolved tree.
- Linking `gt-u7-gps` to a cog in the catalog. That is an ADR-107 edit when a package exists.
- Changing `CLOCK_FLOOR` or the licence request layout.

## Consequences

- Implementers publish `mesh.clock.v1.status` from the host service. They do not extend `SENSOR_TOPICS` or `MeshSensorTopic::ALL` for it.
- Sensor cogs keep emitting SenML. A pack gains `boot_id_` and may use a fractional `t`. Existing integer-second packs remain valid.
- A future clock-status cog, if built, binds 8058 on loopback and is refused write access to chrony. `default_export_port` gains that port only when the package exists.
- `cog_lint.py` learns `system` and the `[outputs]` checks in the same change as the first manifest that needs them.
- `docs/architecture/sensor-node.md` keeps the emit rules. The sentence that every node has a clock points here for source and uncertainty.
