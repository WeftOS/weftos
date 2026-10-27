# Cogs across hardware and software

A cog is the interface to one thing we build against: a sensor, a speaker, a display, a camera, or a software endpoint. The thing can sit on an ESP32-S2, an ESP32-C5, a Pi, or a Linux container on a machine with none of that hardware. The board must not decide what the interaction means, or which machine is allowed to carry it out. That holds when every board runs a WeftOS node, the container we already run stays a WeftOS host, and the mesh carries the cog's frames to whichever node is doing the work.

Ownership: the Cogs product (Cog Host, catalog, sources, Manager, guides, companion apps) lives in [WeftOS/weftos-cogs](https://github.com/WeftOS/weftos-cogs/tree/v0.1.3). WeftOS consumes its `cog-sources`, `cog-repo` and `cog-market` crates by tag, pinned by commit; the target boundary is the `weftos-cog-protocol` crate alone.

This is a spec for that split. It is the cog split we already planned. It is not a change to steps 1–3 of the RuView sensor-contracts plan, and it does not replace the container runtime in ADR-100.

## The cog is the unit

A cog is one interface. It is not a Linux process, and it is not a firmware image. Those are ways to run it. The package is a `cog.toml` id, the hardware or software it requires, a native half for each board that can hold that interface, and one wasm half. The host supplies the frames and accepts the emits. The cog does not name a device path, a container, or a mesh peer.

Three placements, one package:

| Placement | What the host supplies | What is running |
|---|---|---|
| Container, alone | A frame source: a replay file, a socket, or a device node mounted in. No mesh and no WeftOS supervisor. | Decoder if the device is mounted. Producer always. Stdout or a socket is the sink. This is how the cog is tested and how it runs as its own process. |
| One device | The local port and a wasm slot on that node. | Decoder and producer together. A Pi cog or an S2 image that has room for the module. |
| Mesh | Each decoder node publishes frames. One node holds the producer binding for that `source_id`. | The same cog, split. Several decoders may feed one producer (three ultrasound nodes, or an S2 mic plus a Pi radar) and the producer still sees frames, not ports. |

A caller asks for the cog by id and `source_id`. Which row of the table is live is a placement decision. The emits are the same SenML or `spatial.evidence.v1` lines in all three.

WeftOS does the hot-load. The cog package is data: the native builds and the signed RVF. It does not contain a loader, an HTTP upload server, or a wasm engine. On a Pi, and inside the Linux container below, `clawft-wasm-host` is that engine today for plugins (wasmtime, one store per module, fuel and memory caps). A cog's wasm half is a tighter profile of the same host: a frame in, an emit or a command out, a module small enough for a 128 KB slot. On an S2 or a C5 the same operations (load, start, stop, unload, bind `source_id`) belong to the small WeftOS node, using WASM3 when the board has the RAM and a forward when it does not. `weaver cog install` remains how the native half is installed. Swapping the wasm half does not reinstall the cog.

## The Linux container stays

The container is a placement we already have, and it stays. ADR-100 and the conformance harness run a cog binary under `docker`, `podman`, `apple-container`, `native`, or `ssh`. The harness feeds a synthetic ESP32 packet to `127.0.0.1:5006` and stubs ingest on `127.0.0.1:80`. The ingest bridge is the only path from a cog into a store. Stdout is evidence of the run, not data. `network=none` is refused when an upstream is set. An internal Docker or Podman network, or an Apple container host-only network, is how that cog is kept off the rest of the machine.

That container is extended, not retired.

- A cog that is still one native binary keeps running there, with the same `--once` / `--interval` sweep and the same `perf.cog.cycle_ms` probe.
- A split cog runs its native half in that container when the feed is synthetic or the device is mounted in, and WeftOS loads the wasm half in the same container.
- The same container can be the far side of a mesh cog. An S2 owns the microphone or the speaker. The container holds the wasm half and speaks to the S2 through the mesh. The harness feed remains how that wasm half is tested with no board attached.
- The cross-build image (`scripts/cross-build.sh`) is a different container. It compiles `armv7` and `aarch64`. It is not the runtime that places the cog.

## Hardware requirements

Some cogs are the interface, so they require that interface. The requirement is the bus the cog speaks, not a board. Any WeftOS node that has the bus can run the native half, provided the package has a build for that node's architecture. An S2, a C5, and a Pi with I²C are the same placement as far as a BME680 is concerned.

The BME680 chip speaks I²C and SPI. The cog we built speaks I²C, at 0x76 or 0x77, and does not speak UART. UART is the bus for a radar cog. The file on disk still lists boards, `hardware_requirement = ["pi-zero-2w", "v0-appliance"]`. ADR-100 turns those strings into capability ids. That list is the old shape. The requirement this spec uses is `i2c`.

| Cog | Native half must own | Wasm half may run |
|---|---|---|
| BME680 | I²C. Any node that exposes it. | The same node, the container, or another mesh node. Emits SenML. |
| Camera | The cable that board actually has (MIPI CSI-2, USB, or the vendor SDK on a host). A Pi Zero 2 W is the wrong host for a USB3 Vision or PCIe camera. | Anywhere. The frame stays a stream. A geometric product waits until the camera pose is known. |
| Display | The panel's bus. The native half shows state. It is not a measurement. | The container or any node, deciding what the panel should show. |
| Speaker (`max98357a-i2s-amp` and anything like it) | I²S on the node the amp is wired to. Sound leaves there. | The container or any node, deciding what is played. PCM chunks cross the mesh over Wi-Fi toward the amp, the reverse of the S2 microphone. |
| Software endpoint | Nothing physical. The native half is the socket, file, or bridge client. The container is enough. | The same container. |

The wasm half has no hardware requirement. A container with no device mounted still runs it against the harness feed. The native half is refused on a node that does not advertise the bus. A speaker cog's wasm can live in the container while its native half lives on the S2 that has the amp. One `source_id`, two nodes, the container we already know how to run.

## Requirement pass

Every cog gets this adjustment, not only the BME680. The manifests on disk name a board. In the trees checked for this spec, 11 cogs list `pi-zero-2w` and `v0-appliance`, and `catalog` and `mentra-live` list `v0-appliance` alone. ADR-100 maps each string to a capability id, so a node admits the cog only by advertising that board. The same shape is on the cogs built in this round (`bme680`, and the radar cogs beside it). The catalog is larger than this checkout. The pass covers every `cog.toml`, including ones not in these trees.

For each cog:

1. Read the code and name the interface it actually opens. I²C, SPI, UART, I²S, CSI, USB, GPIO, or none. Do not guess from the cog id. A BME680 that opens `/dev/i2c-1` is `i2c`. A radar that opens a serial port is `uart`. A speaker amp is `i2s`.
2. Replace the board list with that interface. `pi-zero-2w` and `v0-appliance` come off. A cog that needs the Seed store or the appliance API names that software interface. It does not name the board the store happens to run on.
3. Leave the native binary and the conformance sweep alone. The sweep does not admit on `hardware_requirement`. What changes is placement: any node that advertises the bus, and has a build for its architecture, can take the native half.
4. The wasm split is a later change to the same package. This pass stands on the cog as it runs today.

A board still records which buses it exposes. That list lives with the board, not in the cog. The S2 mini exposes I²C, SPI, UART, I²S, and ADC. A Pi exposes the header it has. Placement is the intersection.

## Two halves

Every cog has a native half and a wasm half. They compile separately. They can run on the same node or on two nodes. Inward cogs (sensor, microphone, camera) decode on the wire and emit from the wasm. Outward cogs (speaker, display) decide in the wasm and drive the pins from the native half. A cog that does both, a camera that is configured and also streams, keeps the cable in the native half and the policy in the wasm.

The **decoder** is native code on the node that has the wire. It opens UART, I²C, SPI, I²S, or the radio, resyncs, checks the frame, and hands up a decoded frame. The Pi build of that half is the binary `scripts/cross-build.sh` already produces (`armv7-unknown-linux-gnueabihf` for the Seed, `aarch64-unknown-linux-gnu` for the Pi 5). The S2 or C5 build is the firmware image. The port stays in this half. The cog id stays the same across those builds.

The **producer** is `wasm32-unknown-unknown`, packed as a signed RVF `WASM_SEG`. It exports `on_init`, `on_frame`, and `on_timer`. The host calls `on_frame` with a decoded frame. The module calls a host import to emit. It does not link `libc` and it does not open a device. The same module loads on a Pi (wasmtime, via WeftOS) and, when the board has the RAM, on an ESP (WASM3, via the small WeftOS node). WeftOS replaces the module. The decoder keeps running.

A decoded frame carries:

| Field | Meaning |
|---|---|
| `source_id` | The installed sensor. Stable if the part moves to another board. |
| `node_id` | The WeftOS node that decoded it. `hex(SHA-256(pubkey)[..16])`, ADR-099/103. |
| `t_ns` | The decoder's clock at capture. Arrival time on a far node is a second timestamp, not a replacement. |
| `kind` | What was decoded (`bme680`, `ld1125h`, `pcm`, `csi`, …). |
| `body` | Decoded fields. Wire bytes stay on the decoder. |

One active producer binding per `source_id`. A second node may hold a copy of the module. It does not emit for that source while the binding points elsewhere.

## A WeftOS node

"WeftOS on each thing" means each board speaks this contract. It does not mean the full `weft` binary on an S2.

Every node has:

1. An ed25519 key and a `node_id`.
2. Membership in the mesh.
3. A decoder vtable for the wires on that board. Arming CSI, SDR, a pulser, or an I²S mic is a call on that vtable. Those front-ends are compiled into the native image. Adding a front-end is a firmware change. Switching among front-ends already in the image is a mode message.
4. The producer host: load, start, stop, and bind a signed module, or a forward when the slot will not fit. The cog does not do this.
5. A clock on every frame it originates. Source and uncertainty for that clock are [ADR-124](../adr/adr-124-node-clock-and-reading-time.md).

The Pi-class node also supervises cogs and can host many producer modules. The MCU node hosts the decoders for its own pins and radios, and hosts a producer only inside the budget below.

## The mesh

The mesh is why placement stops mattering. The decoder publishes a frame. The node that holds the producer binding calls `on_frame`. The producer does not know whether that call was local or one hop away.

What moves across the mesh is the decoded frame, not UART bytes and not a firmware image. A small reading (temperature, a radar range, a CSI feature vector) is one mesh message. The mesh we ship today carries that inside the JSON `KernelMessage` envelope (ADR-031). RVF as the mesh encoding is reserved and is not required for this spec. The producer *container* is already an RVF; the hop can stay JSON until volume forces the change.

A fat stream is a sequence of frames, still with `source_id`, `node_id`, and `t_ns` on each one. Audio from an S2 is the case we care about: the S2 decodes I²S into timestamped PCM chunks and the mesh delivers those chunks to a node that can run the producer. ESP-NOW is the wrong pipe for that stream (a single datagram is about 250 bytes). The S2 already has Wi-Fi. Audio goes over Wi-Fi to the peer. ESP-NOW remains the right pipe for a small reading from the same board.

The S2 is a single-app image (4 MB flash, no dual-slot OTA). Hot-loading the producer is how that board changes mode without a reflash. The native image changes only when a new front-end is added.

## Where the work runs

| Sensor | Decoder lives on | Producer lives on | Why |
|---|---|---|---|
| BME680 on a Pi | Pi cog, I²C | Same Pi, or any other node | The module is small. Local is the default. SenML, not `spatial.evidence.v1`. |
| LD1125H or LD6004 on a C5 | C5 firmware, UART | A Pi, unless that C5 module has PSRAM | A C5 with no PSRAM is a decoder and a forwarder. The wasm runs where the slot fits. Range and track emit `spatial.evidence.v1` only for types we already have. |
| I²S mic on the S2 mini | S2 firmware | A Pi or an S3 | PCM chunks cross the mesh over Wi-Fi. The S2 can host a tiny module (a level, a clip flag). The room-scale or recognition producer runs on the far node. |
| CSI or SDR | The firmware image that contains that front-end | The node bound to that `source_id` | Arming the front-end is the vtable. The algorithm is the wasm. |

The S2 we measured is an ESP32-S2FNR2: one core, 4 MB flash, 2 MB PSRAM, Wi-Fi, USB-OTG. Two producer slots fit (two 160 KB arenas and two 128 KB modules, on the order of 576 KB). Mutexes and the Wi-Fi stack stay in internal SRAM. `on_frame` runs on the only core, after the frame is decoded, with a budget of a few milliseconds. A blown budget drops that call. It does not stall the decoder.

An S3 with PSRAM can host the four slots RuView already measured (128 KB module, 160 KB arena, 10 ms budget). A C3, a C6, or a C5 module without PSRAM forwards. Those boards still run the decoder and the mesh.

## What a producer may emit

The host import is the gate.

- A reading (temperature, humidity, pressure, gas, a level) leaves as SenML. `bn` ends in `/`. Temperature is `Cel`. Each record carries its own `proof_`.
- A range or a track leaves as `spatial.evidence.v1` only when the type already exists (`radar_range`, `radar_track_point`, `pose`, `shell_measure`, `tof_depth`, `uwb_echo`, `imu_event`, `human_confirm`). The frame includes `frame`, `region`, and `uncertainty_m`. No pose means no evidence line.
- Heart rate and breathing, when a part actually measures them, leave as SenML. They are not track fields.
- A PCM chunk, a CSI matrix, or an SDR IQ block stays a stream. The producer on the far node may emit a SenML value derived from it. The chunk itself is not an evidence record.

`CODE` and `SYNTHETIC` are labeled as such. They are not ground truth.

## What is already built

The cogs `bme680`, `ld1125h-radar`, `ld6004-radar`, and `ld1040adc` are the native half, still unsplit. Each one opens its own port. `spatial-evidence` is a compile-time feature of that binary. None of them is a `WASM_SEG`.

RuView's ESP32 CSI firmware already hot-loads the producer half for CSI: `POST /wasm/upload` on port 8032, Ed25519 against a key in NVS, `on_init` / `on_frame` / `on_timer`, WASM3, RAM slots that clear on reboot. Its host imports are named `csi_*`. Our host imports are the frame above, so one module can sit on a BME680 cog and on an S2 mic. Section 7a of `sensor-contracts-plan.md` still labels the wasm half as the byte decoder and has it call `io_read`. This spec keeps the wire parser in the native half.

## Build, per cog

1. Keep the native decoder on `armv7` and `aarch64`, and add the firmware target for each MCU that will hold that wire. Ship those next to the wasm inside one cog package.
2. Add `wasm32-unknown-unknown` for the producer only. No `serialport`, no `libc`. Emit through host imports so the module stays inside a 128 KB slot.
3. Pack and sign the RVF. A node loads it into RAM. A reboot needs the binding pushed again, unless one module is stored in a data partition. That partition is not a second app image.
4. Bind `source_id` to a node. Local binding calls `on_frame` in-process. Remote binding publishes the frame on the mesh. A container binding feeds a file or a socket and does not join the mesh.

## Done when

- One cog package runs three ways: in the Linux container we already have (docker, podman, or apple-container) against the harness feed, on the device that meets `hardware_requirement`, and split across the mesh. The emits match. An unsplit cog still passes the existing conformance sweep with no wasm half. In each case WeftOS loads the wasm half when the package has one. The cog package has no loader of its own.
- A speaker or a display cog refuses its native half on a node that does not have that bus, and still runs its wasm half in the container.
- Every cog manifest names an interface, not a board. A node with that bus admits the native half. The existing container sweep still passes.
- The same producer module accepts a decoded BME680 frame from a Pi cog and from an S2 firmware, and emits the same SenML.
- A radar cog and a C5 firmware publish the same decoded range frame. The producer runs on a Pi and emits `spatial.evidence.v1` only when pose, region, and uncertainty are present.
- An S2 I²S decoder publishes PCM chunks over Wi-Fi. A producer on another mesh node consumes them. The S2 image is not reflashed to change that producer.
- Unplugging the sensor and plugging it into a different WeftOS node changes `node_id` and leaves `source_id` and the producer module alone.
