# PlutoSDR Nano: bench results and the receive and transmit cogs

- **Status**: Proposed, 2026-10-26
- **Builds on**: [`ventuno-q-cog-plan.md`](ventuno-q-cog-plan.md) (section 5.2 and phase P7),
  ADR-122 (cog data plane: streams, service calls, governance), ADR-100 (cog workload kind),
  ADR-104 (sensor guides), [`../research/sdr-sensing-capabilities.md`](../research/sdr-sensing-capabilities.md)
  (clean-room capabilities: RF census, spectrum occupancy), [`../hardware/sdr-radio.md`](../hardware/sdr-radio.md).
- **Card**: governed actuation over CAN (ADR-123 card `7c6efdad`) is the model for transmit;
  ADR-123 itself is not written yet. This plan asks it to cover RF transmit too.

## 1. The board

A Pluto-class clone ("PlutoSDR Nano"): Zynq-7010 (two Cortex-A9 cores plus FPGA fabric) and an
AD9363A transceiver, USB only, no Ethernet. It has two USB-C ports. One goes to a CH340 USB-serial
bridge for the Linux console; the other is the Zynq's USB 2.0 OTG port, which carries libiio and
the USB network gadget. The side header is JTAG.

What the board reports over libiio (2026-10-26):

| Item | Value |
|---|---|
| Model | `Analog Devices PlutoSDR Rev.B (Z7010-AD9363A)`, variant 1 |
| Firmware | `v0.39-dirty`: a vendor build of ADI's plutosdr-fw, kernel `6.1.0-…-dirty` built 2025-10-09; provenance unknown |
| Front end | `ad9363a` mode, reference 40 MHz (`xo_correction` 40000000) |
| IIO devices | `ad9361-phy`, `xadc`, `cf-ad9361-lpc` (RX), `cf-ad9361-dds-core-lpc` (TX) |
| LO range (RX and TX) | 325–3800 MHz as shipped (refused outside with `EINVAL`); 70–6000 MHz after the AD9364 change (2.1) |
| Sample rate | 2.083–61.44 MS/s advertised; a request for 2.083 MS/s was refused and the rate stayed at 4 MS/s, because rates near the floor need the RX FIR enabled |
| RF bandwidth | RX 0.2–56 MHz, TX 0.2–40 MHz |
| RX gain | −3 to 71 dB; manual, fast attack, slow attack, hybrid |
| TX attenuation | 0 to −89.75 dB in 0.25 dB steps |
| RX ports | A, B, C (balanced and single-ended), plus TX monitor 1/2 |
| ENSM modes | sleep, wait, alert, fdd, pinctrl, pinctrl_fdd_indep |
| CPU | the vendor image set `maxcpus=1` (one A9 core); unset on 2026-10-26 so both run |

### Plugging it in

With the plugs the wrong way round, only the CH340 bridge enumerates (`1a86:7523`) and the console
is silent. The bridge has its own USB power, so its port appearing does not mean the Zynq is up.
With the plugs swapped, the Zynq port enumerates as `0456:b673` and libiio works. Each port has to
be checked, not assumed.

## 2. Extended bench (2026-10-26, on the Mac)

Script: [`scripts/bench/pluto/bench.sh`](../../scripts/bench/pluto/bench.sh). TX stays at maximum
attenuation, and the transmit test uses the AD9361's digital loopback, so nothing is meant to
radiate.

| Test | Result |
|---|---|
| LO sweep, 325–3800 MHz in 12 steps (4 MS/s, manual gain 40 dB) | every step locks within 2 Hz; the noise floor is about −53 dBFS; 2437 MHz reads −24 dBFS, which is Wi-Fi traffic in the room |
| Out-of-range tuning (70, 300, 3900, 6000 MHz) | all refused with `EINVAL`; the LO stays where it was |
| USB 2.0 throughput, 20 M samples per rate | the effective rate tops out at **7.6 MS/s** (about 30 MB/s of 16-bit I/Q). 5 MS/s is gap-free; 10 MS/s and above delivers about 7.5 MS/s, so samples are lost. `iio_readdev` does not report the loss |
| Sustained receive, earlier run (5 × 4.19 M samples at 10 MS/s) | no brownout and no USB drop with the plugs the right way round |
| TX through digital loopback (DDS 200 kHz tone, scale 0.25) | the tone appears at −200 kHz, 117.6 dB above the median bin; RX power −9 dBFS with the tone, −24 dBFS without. The TX data path works |
| Temperatures | Zynq XADC about 61.5 °C idle in open air; AD9363 about 42 °C, steady across the run |

What this settles:

- **The data path is USB 2.0.** About 7.5 MS/s complex is the ceiling, so a full 20 MHz Wi-Fi
  channel cannot be streamed continuously. Wideband work means decimating or computing on the
  Zynq, or capturing in bursts.
- **Loss has to be counted.** The receive cog must take its own sample count from the buffer
  timestamps and sequence numbers, never from what the tool says (sdrtop's "count what arrived"
  rule).
- **Low rates need the FIR.** The owner cog loads and enables the RX FIR before asking for a rate
  near the 2.083 MS/s floor.

### 2.1 Beyond the rated range (AD9364 mode, 2026-10-26)

The AD9363 is rated 325–3800 MHz and 20 MHz bandwidth. Pluto firmware can drive it as an AD9364
(70–6000 MHz, 56 MHz). Before the change the owner took a full flash backup over DFU
(`dfu-util -U` of all five partitions, kept outside the repo with SHA256 sums), then applied it
from a root login on the console (the owner's step):

```
fw_setenv attr_name compatible
fw_setenv attr_val ad9364
fw_setenv maxcpus          # unset: Linux uses both A9 cores (the vendor image had maxcpus=1)
reboot
# undo: fw_setenv attr_val ad9363a; fw_setenv maxcpus 1; reboot
```

After the reboot the phy reports `ad9364`, LO 70–6000 MHz, RX bandwidth up to 56 MHz and TX up
to 40 MHz. [`scripts/bench/pluto/bench-wide.sh`](../../scripts/bench/pluto/bench-wide.sh)
(receive only, plus digital loopback) measured, on the Mac with the stock antennas indoors:

| Test | Result |
|---|---|
| LO sweep, 25 points 70–6000 MHz (4 MS/s, manual 50 dB) | every point locks within 2 Hz; 60 and 6100 MHz are refused with `EINVAL` |
| Noise floor (median FFT bin, same gain) | about −90 to −96 dB/bin inside 325–3800 MHz; −83 to −89 dB/bin at 150–250 MHz and at 4.2–6.0 GHz, so about 5 dB worse outside the rated range, with a few spurs (+20 to +24 dB, 3–5 bins) above 4 GHz |
| FM broadcast, 93 and 103 MHz centres (10 MS/s) | stations clearly visible: peaks +32 and +39 dB, 60 and 320 bins above floor+15 dB |
| ADS-B, 1090 MHz (2.5 MS/s, 2 s, manual 60 dB) | 2112 pulse edges above 6× the median magnitude, against 135 at 1030 MHz: aircraft traffic is detectable |
| Wi-Fi, 20 MS/s bursts | 2.4 GHz and 5.2 GHz (channels 36–48) busy, raising the floor 25–30 dB; 5.8 GHz quiet |
| TX at 5.8 GHz through digital loopback (max attenuation) | tone 98.9 dB above the floor: the TX path works across the extended range |
| Temperatures | AD9364 about 41 °C, Zynq XADC about 62 °C, unchanged by the change |

Outside 325–3800 MHz the part is out of spec, so the cogs declare the measured range
(70–6000 MHz) together with a `rated: 325–3800 MHz` field. Consumers that need calibrated levels
outside the rated band must treat them as uncalibrated.

### 2.2 On the Ventuno (R0, 2026-10-26)

The Pluto's Zynq port on a Ventuno USB-A port (enumerates at 480M on the xHCI root hub, with
RNDIS, mass storage and the ACM console), its charger on the other port. libiio 0.25 from Ubuntu.
The packaged `90-libiio0.rules` calls an `iio_info -S` option this version removed, so a narrow
rule (`/etc/udev/rules.d/53-weftos-plutosdr.rules`, group `dialout`) grants `usb:` access.

| Test | Ventuno result (Mac for comparison) |
|---|---|
| LO sweep and limits, standard and wide | identical behaviour: 325–3800 and 70–6000 MHz points all lock; floor within 1–3 dB of the Mac |
| FM 93/103 MHz | stations visible (+28/+38 dB) |
| ADS-B 1090 MHz | 9342 pulse edges in 2 s against 1404 at 1030 MHz (Mac: 2112 vs 135) |
| TX digital loopback, 2437 and 5800 MHz | tone 117.6 and 99.0 dB above the floor |
| Throughput, `usb:` backend, buffers 256 Ki–4 Mi samples | **4.3–4.7 MS/s** whatever the requested rate (Mac 7.6); bigger buffers are slightly worse |
| Throughput, `ip:192.168.2.1` (iiod over RNDIS) | **5.5–5.9 MS/s** at 256 Ki buffers, better than `usb:` here; one run at 30.72 MS/s timed out the iiod context and recovered on the next run |
| Pluto power seen by the Ventuno's INA232 | 11.34 W with the Pluto streaming, 11.42 W with it unplugged: none (its own charger powers it) |
| Temperatures | AD9364 37–39 °C, Zynq about 58 °C |

What changes in the plan:

- **On the Ventuno `ip:` is the faster backend** (about 5.9 against 4.7 MS/s), so `sdr-pluto-rx`
  defaults to `ip:` on Linux hosts and `usb:` on macOS, and measures both at start. 5 MS/s is the
  safe continuous rate on this board; 20 MHz work stays decimation, on-device compute (R6) or
  bursts.
- **A crashed client can leave the chip in digital loopback** (the receive path then reads zeros
  while TX loopback still works), because the Pluto keeps power and state across host restarts.
  The RX cog resets `loopback`, the DDS and the TX attenuation when it takes ownership, and its
  health check fails on an all-zero buffer.

### 2.3 The receive cog and the soak (R2 and R3, 2026-10-26, on the Ventuno)

`sdr-pluto-rx` and `spectrum` (public cogs repo, branch `wt/sdr-pluto`, ADR-176 and ADR-177) were
run on the Ventuno against the Pluto through a throwaway broker (`pluto_demo`, in the style of
`npu_demo`; the broker there has no service calls yet). Raw results: `/data/build/pluto-soak/`
on the board (`run-20261026-022534/` the first hour, `run-20261026-033259/` the clean hour and the
band-plan phase, `free-window/` the short tests). Receive only throughout; the one loopback test
set the chip's digital loopback at -89.75 dB TX attenuation.

**What the bench settled**

| Test | Result |
|---|---|
| Backend measured at start (`--backend auto`) | `ip:192.168.2.1` 5.8 MS/s against `usb:` 4.4 MS/s; the cog chose `ip:` and says so in `backend` |
| Read size over `ip:` | 8192-sample libiio reads cannot hold 5 MS/s (3.2 MS/s, 30 percent lost); 32768 to 524288 samples were gap-free. The cog reads 65536 and cuts the read into 8192-sample ring records |
| Rate limit | 7.5 MS/s delivered 76 percent and 10 MS/s 57 percent of the expected samples; the lost count tracked `expected - delivered` to within the part not yet confirmed (the cog confirms a loss 0.25 s after it happens) |
| `usb:` at 5 MS/s | 11 percent counted lost, as the 4.4 MS/s ceiling predicts |
| FIR for low rates | rates down to 600 kS/s through the 64-tap decimating FIR (2 and 4), gap-free; above 2.083 MS/s the FIR is switched off again. The chip wants `filter_fir_en` written on both the input and the output `voltage0` |
| Loopback left on by a crashed client | set to 1, the cog logged `loopback_was: 1`, reset it to 0, and the receive path read normally |
| Safe state on taking the radio | loopback 0, four DDS tones off, TX attenuation -89.75 dB, recorded in `safe_state` of every status |
| Services through the broker | `tune`, `set_rate`, `lease` (another caller refused for the lease's length), `plan`, `status` answered; `tx_config` refused with its reason. Two consumers (the spectrum cog asking for 433.92 MHz, a caller asking for 1090 MHz) took turns on the one tuner, 0 samples lost, with the simulated radio |

**R2: an hour of 2.4 GHz at 5 MS/s, every gap counted**

| | Clean hour (camera idle) | First hour (camera on the same hub) |
|---|---|---|
| Duration | 3595 s | 3593 s |
| Delivered | 17,972,396,032 samples | 15,853,158,400 |
| Expected from the clock | 17,972,686,182 | 17,960,148,748 |
| Counted lost | **0** (290,150 pending, 0.0016 percent, which is the clock's drift and read granularity) | 2,035,460,316 (11.3 percent), 7567 gap events of about 53 ms |
| Overflows, all-zero reads, disconnects, apply errors | 0, 0, 0, 0 | 0, 0, 0, 0 |
| An independent reader of the ring (the sink) | 2,195,449 records, 0 ring skips, 0 gap records | agreed with the cog's count |
| AD9364 / Zynq temperature | 40.4 to 43.0 / 61.2 to 65.9 C | 40.4 to 43.9 / 61.0 to 65.8 C |
| Board power (INA232) | mean 12.77 W, 12.0 to 16.8 | mean 13.19 W |
| Cog CPU, memory | about 47 to 58 percent of a core, 25 MB | the same |

The first hour started clean: 5.00 MS/s with nothing lost for 6.5 minutes. Then the delivered rate
stepped down to 4.33 MS/s and stayed there. Cause: **the Pluto and the board's UVC camera and USB
audio share one 480 Mb/s root hub** (`lsusb -t`: Bus 001, ports 2 and 4), and the P3 lane started a
`gst-launch v4l2src` MJPEG 1280x720 stream at that moment, whose isochronous reservation takes the
bulk bandwidth the Pluto's samples need. CPU, load and temperatures were unremarkable. **5 MS/s is
safe only while nothing else streams on that hub; with the camera on, about 4 MS/s is.** The
camera was off for the clean hour (`camera_procs` is logged beside every power sample in
`env.csv`). If the camera and the Pluto must run together, move one of them to the other
controller or lower the Pluto's rate.

What the loss count can and cannot resolve (it is counted from the host clock because the AD9363
stream has no hardware timestamps): a loss under 20 ms, or a loss rate under about 0.1 percent, is
not told apart from USB jitter and crystal drift; a gap is declared 0.25 s after it happened.
`samples.expected` against `samples.delivered` is always shown, so a slow drift is visible even when
`lost` stays 0.

**R3: the 2.4 GHz occupancy reading per minute, with loss reported.** `spectrum` ran beside the
owner for the clean hour: 51 `band.occupancy` readings (one a minute) and a `spectrum.psd.v1`
every 2 s. At 2437 MHz in the room: occupancy 1.4 to 2.5 percent of cells more than 10 dB above
each frame's median (mean 1.9), floor -57.0 to -55.5 dBFS, peak up to -0.5 dBFS (a strong nearby
transmitter). The first two readings carry `loss.fraction` 0.020 and 0.033: that was the first
build of the cog, which used a whole core and missed 1.6 to 3 percent of the stream because its
base64 decode searched a table; it reported the miss, as designed. With the lookup table it uses
about half a core and has missed nothing (every later reading 0).

**Multi-hour band-plan phase (6 h, 04:33 to 10:33 board time).** `2437e6:5e6:8000,
1090e6:2.4e6:6000, 433.92e6:2e6:5000, 915e6:2e6:5000` (a 24 s cycle: 2.4 GHz at 5 MS/s, 1090 MHz,
then 433.92 and 915 MHz at 2 MS/s through the FIR), the spectrum cog reading it. The P3 lane's
camera ran for 3 h 43 min of it (`camera_procs` in `env.csv`).

| | Whole phase | Camera off | Camera on |
|---|---|---|---|
| Time | 21,600 s | 8212 s | 13,378 s |
| Delivered | 56,380,293,120 | 22,541,369,344 | 33,796,325,376 |
| Expected from the clock | 58,840,303,776 | | |
| Counted lost | 2,260,993,417 (3.84 percent) | **0** | 2,260,993,417 (6.27 percent) |
| Gap events | 8422 | 0 | 8422 |
| Overflows, all-zero reads, apply errors | 0, 0, 0 | | |
| Retunes | 3153 | 1208 | 1943 |

Every lost sample sits in the camera-on time. With the camera idle the band-plan cycle ran 2 h 17
min without losing one, through 1208 retunes, the FIR bands included. The independent ring sink read
9,076,119 records over the whole run (both phases), 0 ring skips, and counted 8404 gap records
(2,256,154,059 samples) at its last print, 44 s before the end, which fits the 18 gap events the
cog logged after it.

**The Pluto dropped off the bus twice** (08:13:54 and 08:16:45), both times with the camera and the
board's video encoder busy; the kernel log has `msm_vidc ... Failed to switch GDSC into HW control`
at 08:13:30, three seconds before. `usb 1-2: reset high-speed USB device`, `device descriptor read/64,
error -110`, `USB disconnect`, and the Pluto enumerated again 40 to 60 s later. The cog saw `Broken
pipe` at a refill, reported `no_source` with the reason, retried with backoff (`Connection timed out`
while it was away) and streamed again about 80 s after each loss. `disconnects` reads 2. Nothing was
power-cycled. Why the radio reset is not established: a USB reset from the host side after the
transfers stalled is as likely as the Pluto rebooting; the P3 lane should look at the codec fault.

Smaller findings: the AD9364 temperature sensor read -80.7, -75.4 and -73.7 C in 9 reports (once
before the drops, then after the second); the cog now drops a temperature outside -40 to 125 C. The
valid readings span 32.5 to 64.0 C (mean 40.2), the Zynq 59.8 to 64.7 C (mean 62.2). Board power
(INA232) averaged 14.45 W, 10.5 to 23.2 W, the high end with the camera and the encoder running.
The rx cog used 47 to 58 percent of a core and 25 MB, the spectrum cog 52 percent and 19 MB.

**The spectrum cog over the same phase** published 12,208 spectrum windows and about 360 occupancy
readings per band, with `coverage` showing each band's share of its minute:

| Band | Occupancy mean (max) | Coverage mean | Worst minute's loss |
|---|---|---|---|
| 433.92 MHz | 0.27 percent (0.69) | 0.19 | 0 |
| 915 MHz | 0.53 percent (0.82) | 0.22 | 0 |
| 1090 MHz | 0.85 percent (2.34) | 0.29 | 0 |
| 2437 MHz | 1.36 percent (1.71) | 0.31 | 13.6 percent (camera on) |

Its totals: 73.2 G samples used, 2.268 G counted lost (the owner's), 2.5 M missed by the reader
itself (0.003 percent: the reader fell behind), 0 dead.

**Band plan with camera concurrent** (every band at 3 MS/s or less, started 10:49 board time, stopped by request at 2760 s once the camera soak had ended at 11:00:40):
`2437e6:3e6:8000, 1090e6:2.4e6:6000, 433.92e6:2e6:5000, 915e6:2e6:5000`. Camera on (692 s):
1,496,776,704 samples, 103 retunes, **0 lost**. Camera off (2059 s): 4,478,205,952 samples, 305
retunes, 0 lost. Whole run: 6,001,524,736 delivered against 6,001,049,903 expected, 0 gap events,
0 disconnects, 0 apply errors. With the camera on, 5 MS/s lost 6.3 to 11.3 percent and 3 MS/s
nothing, which matches the 4.33 MS/s headroom. Raw: `/data/build/pluto-soak/run-20261026-104857/`.

**Bus topology and the sharing report.** `lsusb -t`: Bus 001 (xhci root hub, 480M) carries the
Pluto (port 2, RNDIS, mass storage, two ACM) and the UVC camera with its USB audio (port 4);
Bus 003 carries a gs_usb CAN adapter; Bus 002 (5000M) is empty. 5.00 MS/s is clean on its own and
4.33 MS/s with a 720p 5 fps MJPEG camera streaming. The cog now reports this in `sdr.status`:
`usb_hub` lists the other devices on the Pluto's root hub, whether one streams isochronously now,
and `advised_max_rate_sps` (5 MS/s alone, 4 MS/s with one streaming); `rate_over_advice` flags a
tuned rate above it. Checked live with the camera streaming. **Recommendation for the owner:** put
the Pluto on a root hub of its own (a port of Bus 003, or a powered USB 2.0 hub on its own
controller) if the camera must stream at the same time as a 5 MS/s capture; otherwise keep every
band at 3 to 4 MS/s while the camera runs. Moving cables is the owner's call.

What this settles for the plan: R2's done-when holds for an hour with the radio to itself, and for
2 h 17 min of band-plan cycling with 1208 retunes; two consumers sharing the tuner by band plan work (simulated radio
through the real broker, then the spectrum cog read the cycling plan live). The shared USB hub is
the binding constraint: **the Pluto should not share a root hub with the camera**, or the rate
should drop to 4 MS/s while the camera streams. The soak scripts, the broker harness and the
summaries are in [`scripts/bench/pluto/soak/`](../../scripts/bench/pluto/soak/).

## 3. How it attaches to the Ventuno

The Pluto goes on the Ventuno's USB 3 host port, where the board's Linux sees it the way the Mac
does. Linux also gets the Pluto's USB network gadget (RNDIS/CDC-ECM, `192.168.2.1`), which macOS
does not drive. The links, by job:

| Link | Carries | Rate | Use for |
|---|---|---|---|
| USB 2.0, libiio `usb:` backend | I/Q both ways, all control | about 7.5 MS/s measured | the data plane. One process owns it (the RX cog, below) |
| USB 2.0, network gadget + `iiod` (`ip:192.168.2.1`) | the same I/Q and control over TCP; ssh | a little under the `usb:` backend | a second client process (the TX cog) without opening the device twice; management over ssh |
| UART console (CH340 bridge, or the Ventuno's own UART pins to the Pluto's console pins) | boot log, a shell | 115200 baud | **out-of-band management only**: watchdog, recovery, `fw_setenv`. Far too slow for I/Q |
| GPIO lines from the Ventuno MCU (STM32H5) to Pluto header pins | single digital lines | microseconds | a **hardware TX-inhibit** line and, later, PPS for timestamps (section 5) |
| JTAG (side header) | FPGA and boot debug | n/a | recovery and FPGA work only; not used at runtime |

So "UART or better" splits into two answers:

- **Data: USB stays, because nothing on this board is faster.** The way past 7.5 MS/s is to
  compute on the Zynq (the A9 cores with NEON, later the FPGA) and send results.
- **Control and safety:** the console UART is the management channel, and the useful upgrade is
  GPIO from the Ventuno's MCU. A hardware line that blocks transmit does not depend on Linux,
  libiio or our code being right.

Two things are unknown and must be found before any wiring:

1. **The Nano's header pinout.** Find which header pins are the PS UART, which are free PL or MIO
   GPIO, and which are power, from the board's photos and vendor schematic, and measure each pin's
   voltage before connecting it. Zynq bank voltages are 1.8 V or 3.3 V; the Ventuno's MCU header
   is 3.3 V.
2. **A GPIO the stock firmware exposes to user space.** If the vendor image has none on a free
   pin, the TX inhibit uses the AD9363's `TXNRX`/`ENABLE` control path in `pinctrl` ENSM mode,
   which needs an FPGA bitstream change. That makes it a later phase.

### 3.1 Running on the Pluto itself (later)

The Pluto is an armv7 Linux computer (Cortex-A9, 512 MB). WeftOS already runs armv7 cogs: cog0 is
a Pi Zero 2 W running `weft-cog-host`. Two options:

- **(a) A small `iiod`-side helper** on the Pluto that computes FFT, power and decimated channels
  and sends results over the USB network. It is installed in the vendor image's writable overlay.
- **(b) The Pluto as a WeftOS leaf node** over the USB network, with the spectrum cog placed on it
  by the governed placement layer.

(b) fits the platform but needs our armv7 binaries on a vendor image we did not build. Measure (a)
first, by FFT throughput on the A9.

## 4. Cogs

There are two device cogs, receive and transmit, plus consumers. The AD9363 is one chip, so some
settings are shared:

- the sample-rate clock chain;
- the ENSM state;
- the TX/RX LO, which is separate per side but set on the same `ad9361-phy` device.

The receive cog owns the chip's configuration. The transmit cog asks it, through a governed service
call, for anything that touches shared state.

### 4.1 `sdr-pluto-rx` (owner)

- **Owns** the Pluto through libiio `usb:`, as the only process that opens it. Device allowlist
  (P3 owner model): `0456:b673`.
- **Publishes**:
  - `iq.cs16.v1` as a local cogbus stream ring (ADR-122 B2), with sequence numbers and a sample
    clock. It is not exported off the node by default; IQ stays local.
  - `sdr.status` readings: LO, rate, bandwidth, gain, temperatures, delivered vs expected samples,
    and overflows.
  - Node facts `sdr.iio` (model, tuning range after any AD9364 change, max rate measured over the
    attached link).
- **Serves** `svc.sdr/<node>` with `tune`, `set_rate`, `set_gain`, `set_bw`, `set_port`, `status`,
  `lease` (exclusive retune for N seconds by one consumer) and `tx_config` (the TX cog's only path
  to shared phy state). Calls are governed through the existing attributed site.
- **Scheduling**: one tuner. Consumers ask for a band plan, which is a list of (centre, rate,
  dwell). The cog runs a round-robin across all active plans, stamps every block with its band,
  and reports the duty each plan got.
- **Loss accounting**: expected samples come from the buffer timestamps; any gap is published as
  a `gap` event on the ring and counted in `sdr.status`.
- **Contract**: stdout JSON per window and an 8-float store vector, `/status /raw /guide /healthz`,
  `--once/--interval/--simulate/--replay <file>`, and an ADR-104 guide covering the plug
  orientation, antenna port, power and heat.

### 4.2 `sdr-pluto-tx` (separate cog; governed actuation)

Transmitting is actuation, so it follows the governed-actuation model on the ADR-123 card. It is
off unless granted, every request is checked and chained, and a fault means silence.

- **Opens** the TX buffer device (`cf-ad9361-dds-core-lpc`) through `iiod` on the USB network
  (`ip:192.168.2.1`), not `usb:`, so it never opens the device the RX cog owns. It never writes
  `ad9361-phy`; LO, attenuation and ENSM go through `svc.sdr tx_config`.
- **Modes**, in order of risk:
  1. `loopback`: digital loopback, nothing radiated. This is the default and is used for
     self-test.
  2. `monitor`: RX on `TX_MONITOR`, TX into a dummy load or attenuator, at minimum power.
  3. `radiate`: a real emission, only inside a granted policy.
- **Policy** (per node, owner-granted, chained):
  - band allowlist; the ISM bands the owner's region permits are the default, and amateur bands
    only if the owner records a licence and callsign;
  - maximum power as a minimum attenuation;
  - maximum burst length and duty cycle;
  - waveform allowlist: CW tone, a named test sequence, or a file with a hash;
  - who may call.
  
  Anything outside policy is refused with a reason and logged.
- **Safety chain**:
  - A **pre-flight self-test** in loopback must see the intended waveform at the intended offset
    before every `radiate` session.
  - A **watchdog**: the cog sets maximum attenuation and empties the DDS buffer if its controller
    is silent for more than N seconds.
  - A **hardware TX-inhibit** GPIO from the Ventuno MCU (section 3) is required before `radiate`
    is ever allowed on a node.
  - A transmit cut from any of these is final for the session.
- **Serves** `svc.sdr.tx/<node>` with `schedule` (waveform, band, power, start, duration), `abort`
  and `status`.

### 4.3 Consumers

| Cog | Input | Output | Notes |
|---|---|---|---|
| `spectrum` | `iq.cs16.v1` | `spectrum.psd.v1`, band occupancy readings | FFT and occupancy with honest loss; runs on the Ventuno first, on the Pluto later (3.1) |
| `rf-census` | `spectrum.psd.v1` (later decoded BLE) | device-census readings | the SDR sensing flagship; clean-room from sdrtop's design notes (GPL-3: ideas only) |
| `adsb` | `iq.cs16.v1` at 1090 MHz, 2 MS/s | aircraft presence readings | in range today |
| `ism-decode` | `iq.cs16.v1` at 433/868/915 MHz | sensor readings (weather stations, meters, remotes) | rtl_433-class decoding; check each decoder's licence before reuse |
| `rf-test` | `svc.sdr.tx` + `iq.cs16.v1` | a pass/fail report | closed-loop TX/RX self-test: transmit a known sequence in loopback or monitor mode and check it on RX |

## 5. Phases

| Phase | Scope | Done when |
|---|---|---|
| R0 Bench | done 2026-10-26 on the Mac (2) and on the Ventuno over `usb:` and `iiod` (2.2) | the same table filled in on the Ventuno, including throughput through `iiod` and power draw seen by the board's power monitor |
| R1 Extended range | done 2026-10-26: owner applied the AD9364 change after a DFU backup (2.1); bench 70–6000 MHz | per-band lock and noise floor recorded (2.1); cogs declare the measured range |
| R2 `sdr-pluto-rx` | owner cog, stream ring, `svc.sdr`, status, guide, `--simulate`/`--replay` | an hour of 2.4 GHz capture at 5 MS/s with every gap counted; two consumers share the tuner by band plan |
| R3 `spectrum` + `rf-census` | consumers on the Ventuno | a 2.4 GHz occupancy reading per minute (the ventuno plan's P7 acceptance), loss reported |
| R4 Headers | pinout found and measured; UART console wired to the Ventuno as management; one GPIO TX-inhibit line from the MCU | the Ventuno reads the Pluto boot log over its UART; toggling the inhibit line stops a loopback TX within 1 ms |
| R5 `sdr-pluto-tx` | loopback and monitor modes, policy, watchdog, self-test, chained log | `rf-test` passes in loopback; every refusal path is tested; `radiate` stays unavailable until R4's inhibit is wired and the owner grants a policy |
| R6 On-device compute | FFT/decimation helper on the Pluto (3.1a); measure, then decide on (b) | a 20 MHz channel's PSD delivered at full rate as results instead of IQ |

## 6. Risks and open questions

- **Firmware provenance.** `v0.39-dirty` is a vendor build. Flashing ADI's official image may not
  carry this board's device tree; keep the vendor image and record its hash first.
- **Transmit is regulated.** The ISM limits depend on region, and amateur bands need a licence.
  The cog enforces whatever the owner grants, so it does not decide legality, and `radiate` stays
  off by default.
- **Heat and power.** The Zynq idles at about 61 °C in open air. Next to the Ventuno, with the NPU
  busy, add airflow and watch the XADC reading in `sdr.status`. Power comes from the board's USB
  port; a brownout ends the session, and the RX cog reports it as a device loss.
- **One Pluto, many uses.** Wideband census and narrowband decoders contend for one tuner. The
  band-plan scheduler makes the trade explicit, and a second SDR (RTL-SDR for receive only) is
  the cheap way out.
- **Decoder licences.** rtl_433, dump1090 and sdrtop all have licences to check before reuse;
  clean-room by default.
