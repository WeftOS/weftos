# Ventuno Q cog plan, and how cogs pass data

Status: Accepted, 2026-10-25. The owner took every recommendation in section 7.
P0 and P1 shipped (f854f9611); the brick spike confirmed decision 3 (own the MCU).

Scope: every cog the Arduino Ventuno Q (ABX00181) could run, and the changes
WeftOS needs so cogs can hand data to each other on one board and across the
mesh. This builds on [`docs/architecture/sensor-node.md`](../architecture/sensor-node.md)
(native decoder plus wasm producer, frames keyed by `source_id`). It does not
replace ADR-099/100/101 placement.

Sources: a read-only survey of the board on 2026-10-25, the official datasheet
(`arduino/docs-content`, `content/hardware/ventuno/boards/ventuno-q/datasheet/en-datasheet.md`),
the benchmark in [`docs/research/ventuno-q-npu-bench.md`](../research/ventuno-q-npu-bench.md),
and a code survey of the cog host, ingest bridge and placement plane on
`target-0.8.4`.

## 1. Summary

The Ventuno is two computers. The Linux side (Qualcomm IQ8, 8 cores, 14 GiB,
a 40 TOPS Hexagon NPU, three MIPI-CSI connectors, an audio codec, 1 TB NVMe)
does vision, speech and language. The STM32H5 microcontroller owns almost all
of the low-level I/O: all five CAN-FD ports, the Qwiic bus, analog inputs, PWM,
the SPI header and the RGB LEDs. Linux reaches the MCU only through
`arduino-router`, a msgpack-RPC socket.

Today a cog can publish only one way that other software consumes: an
8-float vector posted to the ingest bridge. That is enough for an ESP32
radar. It cannot carry a camera frame, a PCM stream, CAN traffic or SDR IQ
samples, and no cog can subscribe to another cog's output. WeftOS also has no
Qualcomm NPU probe, so the placement plane cannot see the board's main asset.

The plan has two halves:

1. **Data plane for cogs** (section 4): one device owner per device, typed
   topics with declared schemas, three size classes (readings, streams,
   blobs-by-handle), a local stream broker in the node daemon, and the mesh for
   cross-node delivery. Shared hardware (NPU, MCU, camera, microphone) is a
   service cog that others subscribe to or call, never a device every cog opens.
2. **Ventuno cog set** (section 5): about 20 cogs in four groups (MCU I/O,
   media, inference, board telemetry), with end-to-end pipelines that run
   entirely on the board: camera → detect → evidence, mic → wake word → ASR →
   LLM → TTS, SDR → spectrum → device census, CAN → decode → anomaly.

## 2. What the board offers

Status: **present** = visible on Linux and usable now; **MCU** = exists but only
through a sketch on the STM32; **empty** = the controller exists, nothing is
plugged in; **gap** = needs hardware or a driver.

| Interface | Where | Access path | Status |
|---|---|---|---|
| Hexagon NPU (HTP V73/V75) | Linux | `/dev/fastrpc-cdsp*`, QNN libs, GenieX server (loopback, no auth) | present |
| Adreno GPU | Linux | `/dev/dri`, OpenCL (adreno), Vulkan (freedreno) | present, unprobed |
| Video encode/decode | Linux | V4L2 M2M (H.264/H.265 encode; H.264/H.265/VP9/AV1 decode) via GStreamer v4l2 | present |
| MIPI-CSI cameras ×3 (+2 lanes on JMEDIA) | Linux | camss/Spectra ISP, `/dev/media0` | empty: no sensor bound |
| USB cameras | Linux | 2× USB 3 Type-A, USB-C | empty |
| Audio codec (MAX98091) | Linux | ALSA card, 2 playback, 1 capture; MIC IN on JOMEGA | present, no mic fitted |
| HDMI / MIPI-DSI (muxed) | Linux | DRM; a kiosk compositor already runs | present |
| I²C (Linux side) | Linux | i2c-12/13: INA232 power monitor, EMC2305 fan, LED driver | present |
| GPIO (Linux side) | Linux | gpiochip0–3 | present, header map not done |
| 2.5 GbE | Linux | `end0` | present, no carrier |
| Wi-Fi 6E / BT 5.3 | Linux | WCN6855: managed/AP only, **no CSI or monitor mode** | present, not a sensor |
| NVMe 1 TB | Linux | `/data` | present |
| CAN-FD ×1 with PHY (screw terminal, split termination) | MCU | sketch + router RPC | MCU |
| CAN-FD ×3 no PHY (JOMEGA), ×1 no PHY (UNO D4/D5) | MCU | sketch + router RPC, needs external transceivers | MCU |
| Qwiic / Modulino (3.3 V I²C) | MCU | sketch + router RPC | MCU |
| Analog inputs (JANALOG A0/A1 direct ADC, not 5 V-tolerant) | MCU | sketch + router RPC | MCU |
| PWM, timers, JDIGITAL, JSPI | MCU | sketch + router RPC | MCU |
| UNO shield headers | MCU | sketch + router RPC | MCU |
| 40-pin Pi HAT header | Linux | overlays shipped (audio codec HAT, automation HAT), none active | gap until a HAT is fitted |
| 8×13 LED matrix, 4 RGB LEDs | MCU | sketch | MCU |
| SDR (PlutoSDR Nano, USB only) | Linux | libiio over USB, USB network gadget | arriving 2026-10-25 |

Headroom at idle with weaver and GenieX loaded: 13 GiB RAM free, CPU 99 % idle,
thermals 35–40 °C. Power budget matters: the datasheet puts the SoM at 23–25 W
worst case, and the 7 V barrel input tops out at 35 W, so a USB-heavy build
(SDR, camera) should run from 12 V or USB-PD.

The MCU runs no sketch today beyond the router handshake. `arduino-router`
has built-in `tcp/*`, `udp/*`, `hci/*`, `mon/*` and `$/serial/*` methods.
Application methods exist only once a sketch registers them with
`Bridge.provide`. The socket needs the `arduino-router` group.

## 3. How cogs pass data today

| Path | What it carries | Verdict |
|---|---|---|
| stdout JSON → `host.log` | Evidence of a run; never control input (ADR-100) | exists |
| `POST /api/v1/store/ingest` on the ingest bridge (`clawft-weave/src/cog_ingest_serve.rs`) | An 8-float vector per call, rate-limited, forwarded to the project store as a signed mesh envelope | exists; binds `127.0.0.1:80`, one daemon per host |
| Per-cog loopback HTTP export (`/frames`, `/raw.csv`) | Detail a companion UI polls | exists, ad hoc per cog |
| `spatial.evidence.v1` JSONL adapter | Ranges, tracks, poses (13 record types) | exists, off by default |
| Bridge cog (signed `POST /ingest`) | 8-float vectors across nodes | exists; the only cross-node sensor path |
| Cog-to-cog subscribe, topics, schemas, backpressure | — | **missing** |
| Shared accelerator access | — | **missing**; `accel.npu.qualcomm` is vocabulary only, no probe |
| Bus/feed capabilities beyond ESP32 CSI UDP and HTTP sensor | — | **missing** |
| Device isolation per cog | appliance host: none; governed path: rlimits only | **partial** |

The two runtimes (`weft-cog-host` with no placement, and governed placement
through the kernel) are still separate. This plan targets the governed path and
keeps `weft-cog-host` working as the appliance fallback.

## 4. Design: how cogs pass things around

### 4.1 One owner per device

Exactly one cog opens a given device: the camera, the ALSA capture device, the
router socket, `/dev/fastrpc-cdsp`, the Pluto. Every other cog gets that data
by subscribing to the owner's topic or calling its service. This removes the
contention we already hit with the ESP32 UDP socket on :5006, lets the owner
schedule a scarce resource (the NPU), and gives one place to enforce access.

Owner cogs are the native **decoder** half from `sensor-node.md`. Consumers are
**producers** (wasm where they fit, native where they need a library).

### 4.2 Topics with declared schemas

A cog declares what it publishes and what it reads in `cog.toml`:

```toml
[[outputs]]
topic  = "audio.pcm"          # kind; the full topic adds source_id
schema = "pcm.s16le.v1"       # registered schema id
class  = "stream"             # reading | stream | blob
rate   = "16000 Hz, 20 ms chunks"

[[inputs]]
topic  = "audio.pcm"
schema = "pcm.s16le.v1"
policy = "drop-oldest"        # drop-oldest | block | sample

[mesh_limits]                 # required with any [[outputs]] (ADR-122 section 14)
readings_per_min       = 600      # per remote subscriber
bytes_per_sec          = 4194304  # per remote subscriber, stream chunks and blob slices
subscriptions_per_peer = 16
credit_timeout_s       = 30
```

`[mesh_limits]` is the cog's own ceiling on what another node may take from its topics; the
values above are the starting values, to be set per cog (a 1 Hz telemetry cog and a 2 MS/s IQ
cog need very different numbers). A manifest with outputs and no table is refused at pack;
an already released one still installs and places with the starting values, but its topics
cannot be exposed to other nodes until a version declares the table.

The live topic name is `<kind>/<source_id>`, for example
`audio.pcm/venq-mic-0`. `source_id` is the installed sensor and stays the same
if the part moves to another node (`sensor-node.md`). Every message carries
`source_id`, `node_id`, `t_ns` (capture clock) and `seq`.

Schemas live beside the existing contracts (`contracts/sensors/`) with golden
vectors, so a producer is tested against fixtures without the hardware. The
first set: `senml.v1`, `spatial.evidence.v1` (exists), `pcm.s16le.v1`,
`video.frame-ref.v1`, `detections.v1`, `can.frame.v1`, `iq.cs16.v1`,
`spectrum.psd.v1`, `transcript.v1`, `telemetry.v1`.

Placement uses these declarations. A consumer is admitted on a node only if
its input topic is reachable there: local, or over the mesh within a declared
bandwidth.

### 4.3 Three size classes

| Class | Examples | Local delivery | Cross-node delivery |
|---|---|---|---|
| **reading** (≤ 4 KB) | temperature, a radar range, a detection list, a CAN frame batch, a transcript line | broker message | one mesh `KernelMessage` (JSON today) |
| **stream** (sustained, ordered) | PCM audio, IQ samples, CAN at line rate, H.264 NAL units | shared-memory ring per topic, one writer, N readers, each reader with its own cursor and policy | mesh stream channel with credits (backpressure), chunked; only when a consumer elsewhere subscribes |
| **blob by handle** | a raw camera frame, a 1 s IQ capture, a snapshot JPEG | written once to the node's content store (`/data`), the message carries a content hash and TTL | the handle travels; the bytes are fetched on demand, or never |

The rule that keeps the mesh usable: **raw frames never cross the mesh by
default.** A camera at 1080p30 is about 90 MB/s raw. The owner publishes
`video.frame-ref.v1` handles locally and an H.264 stream on request. Consumers
that want pixels run on the same node, which is where the NPU is anyway. What
leaves the board is the derived reading: detections, a transcript, a spectrum
summary, a SenML value.

The 8-float ingest vector stays as the store-summary path. It becomes one more
consumer (a "store sink" cog that subscribes to readings and posts summaries),
not the only way out.

### 4.4 The broker lives in the node daemon

The local broker is a weaver service, not a cog. That puts it in the daemon
that already holds the node key, the mesh connection and governance, and means
cross-node delivery reuses `MeshIpcEnvelope` and `MessageTarget::RemoteNode`
instead of a second network stack. Cogs talk to it over a Unix socket in the
cog's runtime dir (control: subscribe, publish, declare), with shared-memory
rings handed over as file descriptors for streams.

Governance gates subscriptions: a cog subscribes only to topics its manifest
declares and its placement grant allows. Cross-project topics need an explicit
grant.

The ingest bridge's port-80 bind moves behind the broker. Released cogs that
post to :80 keep working through a compatibility listener, and new cogs never
need a privileged port.

### 4.5 Shared hardware as service cogs

Some hardware is called, not streamed. The NPU is the main case.

- **NPU inference service** (ADR-101 `inference` kind). It owns GenieX and is
  the only thing that starts the Edge Impulse model servers (App Lab brick
  runners), which open `/dev/fastrpc-cdsp` themselves: one scheduler for the
  NPU rather than one process holding it (brick spike, 2026-10-25). It restarts
  GenieX through its wrapper when a model load fails. Other cogs call `infer` on a topic-like service address
  (`svc.infer/venq-npu`) with a model id and an input handle (blob or stream
  cursor). It queues by priority, keeps hot models resident within a memory
  budget (Gemma 4 E4B already does not fit next to others; the bench showed an
  OOM), and reports load to node facts. GenieX stays bound to loopback; the
  service is the only client.
- **MCU bridge** owns `/run/arduino-router.sock` and the WeftOS sketch (5.1).
- **Camera**, **audio capture**, **audio out**, **SDR** each own their device
  and publish streams.

A service cog advertises itself in node facts, so placement can put a vision
consumer only on a node whose NPU service has that model.

### 4.6 Capabilities the placement plane needs

Add facts, probed by the node, not declared by hand:

- `accel.npu.qualcomm` with `model.loaded.*`, memory budget and queue depth
  (probe: `/dev/fastrpc-cdsp` plus the GenieX health endpoint).
- `media.camera.csi`, `media.camera.usb` with resolution modes; `media.audio.capture`,
  `media.audio.playback`; `media.video.encode.h264`.
- `bus.mcu` (router present, sketch version), and through it `bus.can-fd.<n>`,
  `bus.i2c.qwiic`, `bus.adc`, `bus.pwm`, `bus.spi`, `bus.gpio`.
- `sdr.iio` with device model and tuning range (Pluto: 325 MHz–3.8 GHz rated).
- Board telemetry: `power.budget_w` from INA232, thermal headroom.

Cog manifests name the interface they open, not a board, per the requirement
pass in `sensor-node.md`. A Ventuno-specific cog is rare; most of the set below
also runs on a Pi with the same bus or on any node with a USB camera.

### 4.7 Isolation

Each device-owner cog gets only its device: a systemd `DeviceAllow=` (or
supplementary group) per device, an unprivileged uid, rlimits, and
`PrivateNetwork` unless it declares a network output. Consumers get no device
nodes at all, only the broker socket. That is the main safety gain of the
owner rule: the NPU, the router socket and the camera are each reachable by one
audited process. Landlock and seccomp stay on the existing deferred list.

*Updated 2026-10-26 (P3).* The device allowlist is implemented as the node's device ledger
(WeftOS ADR-122 section 4b): `[[devices]]` in `cog.toml` names what an owner opens; placement
refuses a second claim and a foreign device argument; each owner is handed `WEFTOS_DEVICES`;
with per-cog uids the node chowns the nodes to the owner (planned as data, tested without
root). On a shared-uid node (the Ventuno's user daemon, owner decision 2026-10-26) listings
say `device_isolation: advisory`, because file permissions cannot separate cogs there.

### 4.8 App Lab bricks

The board ships 31 App Lab bricks (object detection, pose, gestures, keyword
spotting, audio classification, ASR, TTS, VLM, time-series store and more) as
Docker images with their own APIs. Two options:

- **Wrap**: a cog that starts a brick container and adapts its API to topics.
  Fast, but couples us to Arduino's images and release cadence, and the model
  licences (Edge Impulse, Melo, Whisper) need checking per model before we
  redistribute anything.
- **Reuse the models, not the bricks**: run the same QNN models through our
  NPU service. More work, one runtime.

Recommendation: wrap two bricks (object detection, keyword spotting) as a
spike to learn their contracts, and go native for anything we ship.

## 5. The Ventuno cog set

Names are working names. "Owner" means it opens a device; "consumer" only
subscribes or calls.

### 5.1 MCU I/O (through the WeftOS sketch)

One WeftOS sketch on the STM32 turns the MCU into a decoder node: it registers
a fixed `Bridge.provide` vocabulary (`can.open/send/recv`, `i2c.scan/read/write`,
`adc.read/stream`, `pwm.set`, `gpio.*`, `led.*`) and pushes timestamped frames
with `Bridge.notify`. Flashing the sketch is an operator action with its own
guide (ADR-104), because it replaces whatever App Lab app is on the MCU.

| Cog | Role | Owns | Publishes | Notes |
|---|---|---|---|---|
| `mcu-bridge` | owner | router socket, the sketch | `can.frame.v1`, `senml.v1`, `adc.samples` | only cog in the router group |
| `can-fd` | consumer | — | decoded signals (DBC) as `senml.v1`, `can.stats` | port on the screw terminal first; the header ports need transceivers |
| `can-anomaly` | consumer | — | `events` | NPU-free first: rate and ID-set baselines |
| `modulino` | consumer | — | `senml.v1` per Modulino node | one cog for the family, keyed by I²C address |
| `analog-io` | consumer | — | `senml.v1` | A0/A1 are not 5 V-tolerant; the guide says so |
| `actuator` | consumer (outward) | — | — (sends `pwm.set`/`gpio.*`) | governance-gated: an outward cog needs an explicit grant |
| `status-leds` | consumer (outward) | — | — | node health on the matrix and RGB LEDs |

### 5.2 Media

| Cog | Role | Owns | Publishes |
|---|---|---|---|
| `camera` | owner | a CSI or USB camera via V4L2 | `video.frame-ref.v1` (local), H.264 stream on request |
| `audio-in` | owner | ALSA capture | `audio.pcm` (16 kHz mono, 20 ms chunks) |
| `audio-out` | owner | ALSA playback | — (plays `audio.pcm` it is sent) |
| `sdr-pluto` | owner | Pluto via libiio | `iq.cs16.v1` stream (local only by default) |
| `spectrum` | consumer | — | `spectrum.psd.v1`, occupancy readings |
| `rf-census` | consumer | — | device-census readings (the SDR sensing flagship) |
| `display` | consumer (outward) | — | renders a dashboard on HDMI |

The SDR side (Pluto bench results, the separate receive and transmit cogs, header wiring to the
MCU) is planned in [`pluto-sdr-cogs-plan.md`](pluto-sdr-cogs-plan.md) (2026-10-26).

### 5.3 Inference (through the NPU service, or CPU where measured better)

| Cog | Input | Output | Runs on | Measured |
|---|---|---|---|---|
| `npu-infer` | service calls | — | NPU | Qwen3-VL-4B 8/8, 12 s/item; Qwen3-4B 15 tok/s |
| `vision-detect` | `video.frame-ref.v1` | `detections.v1`, `spatial.evidence.v1` when pose is known | NPU | not yet (YOLOX nano model ships on the board) |
| `vision-describe` | a frame handle, on demand | text | NPU (Qwen3-VL-4B) | 0.5 s first token |
| `wake-word` | `audio.pcm` | `events` | CPU or NPU | not yet |
| `asr` | `audio.pcm` after wake | `transcript.v1` | CPU (Parakeet int8) | 1.8–4.4 % WER, 7–8× real time |
| `tts` | text | `audio.pcm` → `audio-out` | CPU (Piper; Phonon for quality) | Piper 8.4× real time |
| `assistant` | `transcript.v1` | text → `tts` | NPU (Qwen3-4B or Gemma 4 E2B) | 11/12 tool calls (Qwen3-4B) |
| `csi-aggregate` | ESP32 RuView CSI over UDP from the LAN | `spatial.evidence.v1` | CPU now, NPU model later | the board's own radio has no CSI |

### 5.4 Board telemetry

| Cog | Owns | Publishes |
|---|---|---|
| `board-telemetry` | INA232 (hwmon), EMC2305, thermal zones, NPU queue depth | `telemetry.v1`, feeds `power.budget_w` and thermal facts |

### 5.5 Pipelines that run entirely on the board

- **See**: `camera` → `vision-detect` → readings to the store sink and the
  dashboard. Pixels never leave the board.
- **Hear and answer**: `audio-in` → `wake-word` → `asr` → `assistant` → `tts`
  → `audio-out`. Every stage has a measured number except wake word.
- **RF**: `sdr-pluto` → `spectrum` → `rf-census`. IQ stays local; occupancy
  and device counts leave.
- **Vehicle/industrial**: `mcu-bridge` → `can-fd` → `can-anomaly`.
- **Room**: ESP32 RuView nodes → `csi-aggregate` on the Ventuno, joined with
  `vision-detect` and `rf-census` evidence for the world model.

## 6. Phases

Each phase ends with something observable. Card it on the dashboard board
before starting.

| Phase | Work | Done when |
|---|---|---|
| P0 Facts | Qualcomm NPU probe; media, MCU and SDR facts; `board-telemetry` cog | `weaver` node facts on the Ventuno list `accel.npu.qualcomm`, camera/audio/MCU/SDR facts, and live power draw; placement `explain` uses them |
| P1 Manifest | `[[outputs]]`/`[[inputs]]` in `cog.toml`; schema ids and golden vectors for the first ten schemas; interface-not-board requirement pass | `weaver workload pack` validates declarations; existing cogs still pass the conformance sweep unchanged |
| P2 Broker (local) | broker service in weaver, readings and shared-memory streams, governance-gated subscriptions, ingest-bridge compatibility listener | two cogs on one node exchange a PCM stream at 16 kHz with no drops for an hour; a slow reader with `drop-oldest` does not stall the writer |
| Spike: bricks | wrap object detection and keyword spotting; test them against a non-App-Lab sketch and a second router client (decision 4) | a written result with evidence for each question in decision 4, and a recorded choice: own or share the MCU |
| P3 Owners | `audio-in`, `audio-out`, `camera` (the owner's USB camera first, then CSI), `mcu-bridge` + WeftOS sketch (owned or shared, per the brick spike) | each owner runs under its device allowlist; a second cog that tries to open the device directly is refused |
| P4 NPU service | `npu-infer` with model residency, priority queue, facts | two consumers share the NPU without OOM; queue depth shows in facts; GenieX is reachable only by the service |
| P5 Pipelines | `asr`, `tts`, `assistant`, `wake-word`, `vision-detect` | the hear-and-answer loop runs on the board end to end; the see loop emits detections from a live camera |
| P6 Cross-node | mesh stream channel with credits; blob-by-handle fetch | a consumer on PG subscribes to a Ventuno reading topic and fetches one frame by handle; raw frames are not sent unless requested |
| P7 RF and CAN | `sdr-pluto`, `spectrum`, `rf-census`; `can-fd`, `can-anomaly` | a 2.4 GHz occupancy reading per minute; decoded CAN signals from a bench bus |
| P8 Hardening | wasm producers for the small consumers; isolation review; App Lab brick spike result | `modulino`, `analog-io` and `spectrum` run as wasm producers; review signed off |

*P3 camera soak, 2026-10-26.* The `camera` owner ran for 14,753 s (245 one-minute windows),
06:54:12Z to 11:00:40Z, with a reader on the cogbus checking every frame ref against the
bytes on disk. 1280x720 MJPEG at 5 fps from the USB camera (1bcf:0b15), no blob.

| Measure | Result |
|---|---|
| Frames published and read | 73,764 refs; 0 sequence gaps |
| Hash against bytes on disk | 73,764 of 73,764 match; 0 missing or mismatched |
| Frame rate | 5.00 to 5.02 fps per window; one short window (the first, 06:55) |
| Capture errors | 0 in every window |
| USB error lines on the hub | 12 at the peak (dmesg window): 6 at 08:14:40 and 6 more at 08:17:41, the two Pluto drops below; 0 at the end |
| CPU temperature | 41 to 44 C maximum; fan about 2.8 to 2.9 krpm |
| Power (INA232, bus 19.9 V) | mean 16.0 W, minimum 13.6 W, peak 23.4 W |
| Memory available | at least 3.56 GB |

The run overlapped the Pluto SDR band plan on the shared USB hub (uncapped 2.4 GHz at 5 MS/s
until about 10:33Z, then the restarted plan at 3 MS/s or less). The Pluto dropped off the bus
at about 08:13:54 and 08:16:45, each time back after about 80 s. The kernel logged an
`msm_vidc` fault at 08:13:30 (`__switch_gdsc_mode_iris3` could not read the core power status,
then firmware packet dumps). The camera did not use the video encoder (it passes MJPEG straight
through), and it kept streaming at 5 fps with no errors through both drops. The Pluto side
measured that the camera costs it 11 to 13 percent of samples at 5 MS/s on the shared hub.
Raw data: `/data/build/p3/camera-soak/` on the board.

P0 and P1 are small and unblock everything. P2 is the core of "how cogs pass
things around" and is worth an ADR of its own before code.

## 7. Decisions (owner, 2026-10-25)

1. **Broker placement**: in the weaver daemon (section 4.4).
2. **Raw-frame policy**: raw frames and IQ stay on the producing node by
   default, with an explicit per-subscription opt-in.
3. **MCU sketch**: WeftOS owns the MCU with one generic sketch. **Confirmed by
   the brick spike** ([result](../research/ventuno-q-app-lab-brick-spike.md)):
   bricks never touch the MCU or router; only App Lab apps that ship a sketch
   reflash it, so WeftOS does not run those where it owns the MCU.
4. **App Lab bricks**: wrap two bricks (object detection, keyword spotting) as
   a careful spike, then go native for anything we ship. The spike must answer,
   with evidence: does each brick run while a non-App-Lab sketch is on the
   MCU; does a brick need the MCU or the router at all; can two router clients
   (the brick and `mcu-bridge`) coexist; what breaks when App Lab restarts or
   updates. The answer decides decision 3, so the spike runs before the
   `mcu-bridge` sketch is flashed (P3).
5. **Outward cogs** (actuator, LEDs, speaker): each needs a per-cog governance
   grant.
6. **First camera**: USB. The owner has a USB camera for the board.

## 8. Risks

- MIPI-CSI needs a supported sensor module and device-tree overlay; nothing is
  bound today. USB cameras avoid this.
- The four header CAN-FD ports have no transceiver; only the screw terminal is
  bus-ready.
- GenieX is v0.9.x and unauthenticated; the NPU service must be its only
  client and must survive GenieX restarts.
- NPU memory: large models cannot co-reside (E4B already OOMs); residency
  policy has to be explicit.
- Power: camera + SDR + NPU on the 7 V input can exceed the connector rating.
- Pluto over USB 2.0 sustains about 7.5 MS/s (measured 2026-10-26); full 20 MHz channels need
  decimation or burst capture.
- Model and brick licences are unchecked; nothing is redistributed until each
  is cleared.
