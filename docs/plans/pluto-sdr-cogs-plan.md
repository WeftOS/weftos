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
| LO range (RX and TX) | 325–3800 MHz; requests outside are refused with `EINVAL` |
| Sample rate | 2.083–61.44 MS/s advertised; a request for 2.083 MS/s was refused and the rate stayed at 4 MS/s, because rates near the floor need the RX FIR enabled |
| RF bandwidth | RX 0.2–56 MHz, TX 0.2–40 MHz |
| RX gain | −3 to 71 dB; manual, fast attack, slow attack, hybrid |
| TX attenuation | 0 to −89.75 dB in 0.25 dB steps |
| RX ports | A, B, C (balanced and single-ended), plus TX monitor 1/2 |
| ENSM modes | sleep, wait, alert, fdd, pinctrl, pinctrl_fdd_indep |

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

### 2.1 Beyond the rated range (not yet run)

The AD9363 is rated 325–3800 MHz and 20 MHz bandwidth. Pluto firmware can drive it as an AD9364
(70–6000 MHz, 56 MHz). Units often work there, with sensitivity outside the rated range
unspecified. The change is persistent and reversible, but it needs a root login on the Pluto's
console, so the **owner runs it**:

```
# on the Pluto console (/dev/cu.usbmodem* on the Mac, or ssh over the USB network on Linux)
fw_setenv attr_name compatible
fw_setenv attr_val ad9364
reboot
# undo:
fw_setenv attr_val ad9363a; reboot
```

Once it is set, rerun the bench with the sweep extended to 70–6000 MHz, and add per-band noise
floor and a known-source check:

- FM broadcast 88–108 MHz;
- ADS-B at 1090 MHz, already in range;
- 5.8 GHz Wi-Fi.

Record which bands lock and what the floor is. Until that run, the cogs declare 325–3800 MHz.

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
| R0 Bench | done on the Mac (section 2); repeat on the Ventuno over USB 3 host and over `iiod`/USB network | the same table filled in on the Ventuno, including throughput through `iiod` and power draw seen by `board-telemetry` |
| R1 Extended range | owner applies the AD9364 change (2.1); bench 70–6000 MHz | per-band lock and noise floor recorded; cogs declare the measured range |
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
