# WeftOS decisions and the draft

Two layers. Repo files are rules the expert obeys. The Desktop draft is a proposal the expert can quote and must not promote.

## Repo rules (documented)

### Copper CAN, already written

`docs/research/sonobuoy-min-test-and-copper-tow.md`:

- § around the bus table: ESP32-S3 TWAI is CAN 2.0. Transceiver named there is SN65HVD230. Pair is CAN_H / CAN_L at 125–500 kbit/s. 29-bit identifiers carry `buoy_id`. Shore gateway is USB-CAN (`slcan` or candleLight) or a spare S3.
- Same section: do not put 16 kHz PCM from three nodes on 125 kbit/s CAN. Events fit. Waveforms do not.
- §7: TDoA wants about 10 µs. A dedicated sync pair is that clock. "Do not timestamp off CAN frames — arbitration jitter is milliseconds under load."
- §7.1: acoustic detections publish `{t_pps, dt_us, peak}` after a GPIO capture of the PPS edge. CAN carries `pps_seq` and those detections. CAN is not the clock.
- §7.4, explicit do-not list: "Use CAN SOF as PPS."

`docs/research/antihunter-weftos-crosswalk.md` line near the backhaul row: the acoustic plan is copper CAN plus a USB-CAN gateway, Wi-Fi off on the hydrophone node.

These notes are about a hydrophone tow, not the carry deck. The constraint that travels is physical: CAN arbitration is not a time base, and a classic TWAI bus has a small payload budget.

### Clock and readings

`docs/adr/adr-124-node-clock-and-reading-time.md` is **Proposed** (2026-10-26). It is not Accepted. Until it is, it is still the in-repo decision text the clock review asked for, and this brain treats its constraints as binding on new CAN design so a later acceptance does not have to undo a bus.

- Clock status is `weavelogic.clock.v1` on `mesh.clock.v1.status`. Clock values are `pps`, `ntp`, `sntp`, `free`. There is no `can` value.
- The host owns chrony, slew, and step. A cog does not write chrony config.
- Scalar readings are SenML. `t` is capture time in Unix seconds. The envelope `timestamp_ms` is emission time. Arrival is a different stamp.
- `SensorTopic` has three kinds: `mesh.sensor.v1.encoded`, `.consensus`, `.control`. No reading kind.
- Device counters (PPS edge, DW3000, acoustic `t_pps`) stay inside the cog that read them. Ranges leave as `spatial.evidence.v1` with the node's capture time and `uncert_us`.
- `node_id` on a clock-status document is the hex sensor-node id. `SignedSensorFrame` keeps its `u64` node id. They are not unified.

`docs/architecture/sensor-node.md` points item 5 at ADR-124 for source and uncertainty of the clock on a frame the node originates.

### Catalog ids

Read from carry-deck `crates/cog-market/catalog/sensor_types.json` on branch `carry-deck-parts` while this brain was written. Twenty-eight ids:

`imu`, `temperature`, `environmental`, `gas-air-quality`, `thermal-imaging`, `camera`, `depth-camera`, `tof-lidar`, `ultrasonic-ranging`, `radar`, `uwb`, `wifi-csi`, `sdr`, `microphone`, `hydrophone`, `sonar`, `biopotential`, `ppg-optical`, `adc`, `force-pressure`, `capacitive`, `touch`, `presence-contact`, `vibration`, `power-monitor`, `gnss`, `magnetometer`, `input`.

`input` is the joystick / button / encoder class the draft maps to J1939 BJM1/EJM1. That mapping is `proposal`. `sensor_class` on a SenML record, when present, is one of these ids. It is not `EncodedObservation.sensor_class` (a free string on a latent) and it is not `cog.toml` `category`.

## Draft proposal (not accepted)

File: Desktop `weftos_drafts/CAN-STANDARDS.md`, status DRAFT, 2026-10-27. Research only. No code.

The proposal, in one paragraph: classic CAN 2.0B at 500 kbit/s on the shared bus; J1939-21 identifier layout and J1939-81 address claim as the wire; standard PGNs where a sourced SPN exists; WeftOS readings in proprietary B 0xFF00–0xFFFF; CANopen kept as an 11-bit adapter; CAN FD only on a separate VENTUNO segment; a hub cog decodes with a DBC and emits readings; the 2005 Civic is reached through a K-line ESP32 bridge at source address 0x81.

Grade of that whole paragraph: `proposal`.

Details the expert may quote only with that grade, and only with the draft's `[S]` or `[U]` still attached:

- VENTUNO Q: CAN-FD, STM32 FDCAN, SocketCAN `can0`, ATA6563, split termination 2×60.4 Ω as one end, `can1`/`can2` PHY-less. Draft S1.
- Budget: 500 kbit/s classic, about 4,000 eight-byte extended frames/s at 100% load, stay near 40%. The arithmetic is the draft's. Not re-derived here.
- SA sketch: hub 0x80, OBD bridge 0x81, GPS/time 0x82, sensors 0x90–0xEF. NAME identity = low 21 bits of the WeftOS node id. Mesh envelopes carry `node_id`, never the source address.
- Proprietary frame sketch: instance, 8-bit seq, then values. J1939 not-available patterns become SenML null.
- Sensor-type table in the draft §5. Every `[U]` mark in that table is unverified. Humidity SPN 354's PGN, gyro and quaternion quanta, and the GNSS scale and offset are inside that caution. Two cells are corrected below and are not in the caution.
- Bandwidth classes the draft keeps off the bus except as a summary: vibration, microphone, hydrophone, biopotential, PPG, camera, depth, thermal, SDR, Wi-Fi CSI, sonar. That matches the copper note's "no continuous PCM" rule and is the one part of the table this brain will repeat as consistent with the repo.
- Signing: Ed25519 does not fit in an 8-byte frame. The draft leaves the trust boundary at the hub. Open question.

## Operator errata (2026-10-27)

Mathew, on the cogs channel, after the draft was seeded. The draft file on the Desktop was not edited. Where a draft cell disagrees with this list, use this list.

| Item | Draft cell | Use this |
|---|---|---|
| IMU accel on proprietary `0xFF20` | Primary column says `1/1024` m/s² per bit. Units column already says int16 at 0.002 m/s² per bit (±65 m/s²). | **0.002 m/s² per bit.** Drop `1/1024`. int16 at 0.002 spans about ±65.5 m/s², which is the units-column range. |
| GNSS latitude and longitude | Units column says 32-bit, then cites source S2 as saying 16-bit and marks the correction `[U]`. | **32-bit each.** The 16-bit reading of S2 is withdrawn. |

Grade of both rows: operator correction of the proposal. They are not a J1939DA clause quote, and they do not accept PGN `0xFF20` or VP1 as the WeftOS allocation. Gyro scale (0.001 rad/s per bit), quaternion scale (int16 / 2^14), and the GNSS factor (1e-7 deg/bit) and offset (−210) stay draft text. The factor and offset were inside the same `[U]` note as the width; only the width was confirmed.

## Conflicts to keep open

Do not "resolve" these by editing the draft or by silently following it.

| Topic | Draft | Repo |
|---|---|---|
| Reading topic | `mesh.sensor.v1.reading` | Closed three-value `SensorTopic`. SenML outside it. ADR-124. |
| Time base of a sample | Hub `SO_TIMESTAMPING` minus a latency, plus a CAN sync pair, and a new `clock: "can"` | SenML `t` is capture time. Arrival is separate. Clock enum has no `can`. Host owns chrony. |
| CAN as PPS | TX-complete pair `0xFF01` / `0xFF02`, claimed 10–100 µs `[U]` | Do not timestamp off CAN frames. Do not use CAN SOF as PPS. PPS is a wire. |
| Prior art | Repo scan found no CAN | Copper note and the antihunter crosswalk, cited above. |
| ID map | SA ranges and proprietary PGNs | No accepted allocation. 29-bit `buoy_id` in the tow note is a different scheme. |

A CAN sync pair may still be worth measuring. It does not become the capture clock, and it does not become the acoustic time base, unless a later accepted ADR says so. ADR-124's step rules and `CLOCK_FLOOR` stay the licence clock's problem, on the host, not on a CAN frame.

## What this package does not ship

No DBC. No cog. No change to `SensorTopic`. No acceptance of ADR-124. No edit to the Desktop draft.
