# Module types

A catalog card is one family. This page is what a type shares: what it measures, which concerns repeat, and which output can carry the reading. Numbers live on the card, copied from the vendor page named there. This page does not add a spec the card does not have.

Written 2026-10-06. The five catalog kinds are `sensor`, `board`, `display`, `actuator`, and `tool` (`crates/weftos-cog-market/src/hw.rs`). A camera, a radar, a microphone, and a temperature chip are all `sensor`. The sections below are sensing types, carried in the summary and `spec`, not a sixth kind.

## How a row is cut

One row is one family the vendor sells as a line. Color and monochrome, connector, housed versus board-level, RAM size, and a cooling option stay inside that row. Cases, heatsinks, cables, and mounts stay off the list until someone chooses one.

A contribution submitted with `parts__add_part` lands pending. The live explorer shows it after review. Editing `catalog.json` does not reseed the database.

`chips` lists catalog chip ids only. An empty list means the silicon is named in `spec` and is not itself a chip row.

## Concerns on every sensing type

### Interface and the host

The decoder is the firmware on the device that has the wire. On a Seed that decoder is the cog's frame parser. A producer compiled to `wasm32-unknown-unknown` receives decoded frames. It does not open the port.

USB3 Vision and PCIe cameras are host-machine interfaces. The host SDK (GenICam / GenTL, or the vendor API) is the decoder. A 5 Gbit/s to 64 Gbit/s camera needs that host. The Cog Seed image on a Pi Zero 2 W is a different machine.

### Pose

A spatial reading needs the installed object's pose and a region. The catalog id is the type. `bn` / `source_id` on a reading is the installed instance. No pose means no spatial evidence line.

### What leaves the device

Three outputs, and a reading picks one.

- `spatial.evidence.v1` carries `radar_track_point`, `radar_range`, `pose`, `shell_measure`, `human_confirm`, `tof_depth`, `uwb_echo`, and `imu_event`. A line needs `frame`, `region`, and `uncertainty_m` in metres.
- SenML (RFC 8428) carries the state of an object. `bn` ends in `/`. `proof_` is on every record (`MEASURED`, `CODE`, or `SYNTHETIC`). Temperature is `Cel`. Unknown names are stored and counted.
- A stream stays a stream on the object: an image, a hyperspectral cube, an audio buffer, a CSI block.

Heart rate and breathing leave as SenML. An image frame stays a stream. `CODE` and `SYNTHETIC` are not ground truth.

### Clock and trigger

An RTC such as the Epson RX8130CE holds civil time across power loss. One of its ticks is 30.5 µs. The echo stopwatch and a radio chirp timer stay faster counters on the MCU.

A hardware trigger marks an exposure or a ping. The time the frame arrives on the host is a different event. Sound speed moves about 0.6 m/s per °C. A radio chirp does not. Compare clocks on the radio path. Measure distance on the acoustic path. A shared temperature error moves absolute acoustic ranges together, so a difference holds a position better than one range holds a room size.

### Power, heat, and sync

Frame rate, bit depth, and cooling are properties of a named family page, tied to an interface and a power figure on that page. An opto-isolated trigger is how industrial cameras share an instant. It does not replace the clock above.

## Cameras

A camera integrates radiance through a lens into a frame. Shutter (global, rolling, or global-reset), lens mount, trigger versus free run, and the cable's bandwidth decide whether the frame is usable. Color versus monochrome is a variant of the same family when the mechanical and electrical line is the same.

Already catalogued before XIMEA: `pi-camera-module` (MIPI CSI-2; Module 3, HQ, or global-shutter, one row), `ov5640-camera` (DVP plus SCCB, held off by default), `flir-lepton-35` (thermal, below), `orbbec-gemini-2` (depth, below).

Output is a stream. A later geometric product can consume the stream only after the camera pose is known. Privacy already shows up on the existing cards: the OV5640 row is off unless a training capture asks for it, and the Orbbec row is a bench registration camera.

XIMEA (XIMEA GmbH, <https://www.ximea.com/products>) is the industrial and scientific line added 2026-10-06. The finder reported 275 cameras that day. Twelve family rows are staged. Prices are quotes. The same page sometimes prints two figures; the card notes say so instead of picking one.

| Catalog id | Family | Interface, from the family page |
|---|---|---|
| `ximea-xiq` | xiQ | USB3 Vision, 5 Gbit/s. Compact global-shutter CMOS, in production since 2011. |
| `ximea-xic` | xiC | USB 3.1 Gen1, Sony Pregius, about 2.3–24.5 Mpix. Same mechanical idea as xiX with less bandwidth. |
| `ximea-ximu` | xiMU | USB3, 15×15 mm to 17×17 mm, 5 g class, under 1 W on the small models. Includes one IMX556 ToF model. |
| `ximea-xix` | xiX | PCIe, multi-camera and embedded, 26×26×31 mm class. USB twin is xiC. |
| `ximea-xix-xtreme` | xiX-Xtreme | PCIe Gen3 x4, 32 Gbit/s, Sony Pregius S, detachable head. |
| `ximea-xix-xl` | xiX-XL | PCIe Gen3 x4, 32 Gbit/s, large Sony BSI sensors up to 245.7 Mpix. |
| `ximea-xib` | xiB | PCIe Gen2 x4, 20 Gbit/s, 12.5–47.5 Mpix on the models table. |
| `ximea-xib-64` | xiB-64 | PCIe Gen3 x8, 64 Gbit/s, up to 65.4 Mpix and 3709 FPS on the fast sensor. |
| `ximea-xij` | xiJ | Cooled scientific sCMOS, USB3 or PCIe, Peltier. |
| `ximea-mx377` | MX377 | Cooled medium-format sCMOS, Gpixel GSENSE6060, PCIe Gen3 x4. |
| `ximea-xiray` | xiRay | Cooled direct-phosphor X-ray, scintillator and taper options. |
| `ximea-xispec` | xiSpec | Snapshot and line-scan hyperspectral, USB3 or PCIe, 32 g class. |

Left off the list: the finder's legacy set, xiD (the brochure still lists a USB 3.0 CCD scientific line; it is not on the current homepage series), xiT (four finder rows, not on that series list), cables, adapters, coolers, mounts, and the xCX compact computer. Board-level and flex-cable versions stay inside the family row. Color, monochrome, polarized, NIR, and HDR suffixes stay inside the family row.

Host software on these pages is GenICam / GenTL plus the vendor SDK, on Windows, Linux, macOS, and ARM, with Jetson named on the PCIe families. That SDK is the decoder. There is no cog.

## Thermal cameras

A thermal camera reports infrared radiance, sometimes calibrated to temperature. `flir-lepton-35` is the catalogued micro module: LWIR, radiometric, SPI (VoSPI) plus I2C. The frame is a stream. A single spot reduced to a temperature is SenML in `Cel`. The module is a different instrument from a thermopile array, which the Lepton card already says.

## Depth

Depth arrives three ways, and the output name follows the physics.

- Optical time of flight emits a range. `sen0628-tof` is an 8×8 zone ToF (VL53L7CX behind an RP2040), I2C or UART. A zone range with pose, region, and uncertainty in metres can be `tof_depth`. The matrix image stays a stream until that reduction.
- Stereo or structured light estimates depth from geometry. `orbbec-gemini-2` is the catalogued RGB-D example. Disparity is not optical time of flight, so it does not use the `tof_depth` name. The depth frame stays a stream beside the camera pose.
- A ToF image sensor inside a camera family (XIMEA lists Sony IMX556 on xiMU and Teledyne e2v Hydra3D+ on an xiX PCIe model) stays in that camera row. Pulling one range out of it is a later decision, under `tof_depth` only when the reading is a range in metres.

One depth frame does not mint a room. A shell needs persistent agreement and known poses. `shell_measure` is that later, geometric record.

## Hyperspectral and X-ray

`ximea-xispec` puts filters on the CMOS pixels (the family page spells them Fabry-Perrot) and returns many bands in one exposure or a line scan (the family page covers 460–960 nm, 15–150 bands). `ximea-xiray` is direct-phosphor X-ray with scintillator, fiber plate, and taper choices, radiation-hardened and cooled. `ximea-xij` and `ximea-mx377` are cooled scientific sCMOS: long exposure, low read noise, high bit depth.

These are laboratory instruments. The cube or the radiograph is a stream. Cataloguing them does not make them room sensors or cogs. Bit depth, quantum efficiency, full-well, and the cooling setpoint are the numbers that change meaning across this type, and they stay on the family card.

## Radar

A millimetre-wave radar reports presence, a range, or tracks from its own protocol. Catalogued lines include Hi-Link (LD2450 and the later ingest) and Infineon BGT60 parts. `radar_range` and `radar_track_point` are the evidence types when the feature is on, a pose exists, and the frame matches that part's protocol. Type numbers do not copy from one Hi-Link part to another. Breathing and heart rate leave as SenML.

## Ultrasound and sonar

Ultrasound measures distance along an aimed ray. Catalogued air modules include `hc-sr04`, `jsn-sr04t`, `marvelmind-ultrasonic`, `mb1240`, `mb1340`, `mb7388`, and `mb7853`. Marine transducers (Airmar and others) are a different medium with the same geometry problem.

Two sensors at known poses give two sides of a rectangle. A third closes the rectangle and separates a person's intersection from the other crossing. One ping heard by the other nodes is a different measurement: the direct arrival is the other node, and a reflection heard by three listeners can meet on a point.

Acoustic range is its own measurement. It is not `uwb_echo` and it is not optical `tof_depth`. It needs its own evidence type before it enters `spatial.evidence.v1`. Persistent wall ranges with known poses can already be `shell_measure`. One echo train is not a shell.

## Microphones

A microphone reports pressure at a point. Catalogued parts include `inmp441` (I2S, about 61 dBA), Knowles ICS-41350 / ICS-43432 / SPH0645, the sensiBel optical MEMS chips `sensibel-sbm100b` and `sensibel-sbm140b`, and the Vesper piezoelectric MEMS chip `vesper-vm1001` (array capsule, ±1 dB sensitivity match, 127 dB SPL overload, 3.76 × 2.95 × 1.3 mm).

PDM, I2S, and TDM are one chip when the acoustic specification is one specification. A sample rate of 192 kHz is not a claim that the diaphragm responds at 40 kHz. The published band on the SBM100B is 20 Hz–20 kHz. Eval boards stay off the list until one is chosen.

The buffer is a stream. A level reduced to a sample can be SenML. A microphone does not by itself emit a range. As a listener for an audible sweep it can supply the arrival that a separate ranging path turns into a distance.

## Environmental

Temperature, humidity, pressure, and gas are the state of the air at the sensor. BME280 and BME680 are the catalogued examples. Output is SenML, temperature in `Cel`.

Air temperature is also the input to a sound-speed estimate (about 0.6 m/s per °C). That estimate corrects a range. It is not a clock.

## Inertial and magnetic

An IMU reports specific force and rotation rate, sometimes fused attitude. `imu_event` is the evidence type when the reading is that event and the pose rules are met. A sample with no install pose stays SenML on the object. It does not become a world pose.

## Timing

A real-time clock holds civil time. `epson-rx8130ce` is the catalogued part: I2C, factory-adjusted 32.768 kHz, backup current in the hundreds of nanoamps. It timestamps a log and survives power loss. It does not time a 40 kHz echo or a radio chirp. Those stay on a faster counter, with the drift row applied when a shared clock corrects them.

## Biopotential

`sen0213` is an AD8232 analog front end. The cog digitizes one lead through an ADS1115. The waveform or a derived rate is SenML on that object. The unit is a named sample. The part is a demonstration front end. Electrode placement dominates the signal.

## Radio ranging and channel sounding

UWB (the Qorvo/Decawave DW1000 and DW3000 modules) measures a direct path. A qualifying echo can be `uwb_echo`. Wi-Fi CSI is a channel snapshot; the person shows up as a residual once the empty-room multipath is predicted from the shell and the sensor poses. Bluetooth Channel Sounding is a Core 6.0 feature. LoRa's bandwidth does not separate paths inside a room.

A radio chirp over a known distance peels flight time off the sample and leaves clock error. That is the shared clock in the trigger section. The snapshot receiver records a short I/Q block and correlates later. It does not replace the acoustic tape measure.

## Boards

A board is a host or a carrier. It has an architecture, a power budget, and a logic level. It does not emit evidence by itself. Catalogued hosts include `pi-zero-2w` (3.3 V GPIO, armhf userland on the Seed image) and the Turing pair `turing-pi-2-5` (mini-ITX 4-node carrier, no CPU of its own) and `turing-rk1` (RK3588 module, one row for 8/16/32 GB). DEBIX is written up in [debix.md](debix.md) and is not yet a card.

A carrier that accepts a compute module is not a Seed image target. GPIO on these boards is 3.3 V unless the card says otherwise.

## Displays

A display is a console. `elecrow-7in-rgb-tft-touch` is the catalogued HMI: an ESP32 plus a capacitive panel. It shows state. It is not a measurement.

## Actuators

An actuator moves or makes sound. `max98357a-i2s-amp` is an I2S speaker amp. Output leaves the system there. It is not a sensor stream in the other direction.

## Tools

A tool is bench equipment. The Hakko FX-888D and the other `kind: tool` rows are shop inventory. They are not installed in a room and they do not join a pose.

## Adding the next family

1. Open the vendor's family page and record the date.
2. Add one module. Keep connector, color, and capacity variants inside it.
3. Set `kind` to one of the five. Put the sensing type in `summary` and `spec`.
4. Copy specs as strings. Put the page URL in `spec.source`. When the page disagrees with itself, keep both figures in `notes`.
5. Leave `chips` empty unless each id already exists.
6. Hash is `wh_` plus the first 16 hex characters of SHA-256 of `module:<id>`.
7. Submit with `parts__add_part`. Leave it pending.
8. Add the manufacturer row in [manufacturer-catalogs.md](manufacturer-catalogs.md) and the JSON twin.
9. If the type gained a concern, edit this page. Leave the card as the part.
