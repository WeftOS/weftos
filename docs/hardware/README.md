# Hardware knowledge base — WeftOS / WeaveLogic

A living inventory of the hardware our cogs, sensors, Fleet nodes, and placement targets run on —
what each part *is*, how we use it, its gotchas, and a cited source. Public facts only (no appliance
credentials or partner-confidential details).

Two layers:
- **Knowledge** (these files) — what a part is + our usage + gotchas. Written by research sub-agents.
- **Procurement** (planned) — real part numbers, price, and stock pulled from a distributor API
  (Mouser, or Nexar/Octopart for Arrow). Lands alongside these as `*-catalog.*`.

A local **RVF `hardware` brain** indexes these files for semantic search (ruv stack, on-machine — no
signup). Rebuild it after editing a file or after a catalog pull.

## Files

| File | Entries | Covers |
|---|---|---|
| [sensors.md](sensors.md) | 5 + 5 candidates | DFRobot SEN0213 ECG (:8046), SEN0628 8×8 ToF (:8047), ADS1115 ADC, BME280; candidates BNO055/MPU-6050/SCD41/SGP40/VL53L1X |
| [sbcs.md](sbcs.md) | 9 | Pi Zero 2 W, Pi 5, Orange Pi Zero 2W, Banana Pi M4/M6/R4 Pro/WiFi6, the dual-Xeon x86 box |
| [microcontrollers.md](microcontrollers.md) | 7 | ESP32-S2/S3 (sonobuoy + Inkpad), ESP32-C3/C6 — the Fleet / edge sensor-driver nodes |
| [sdr-radio.md](sdr-radio.md) | 4 + baselines | bladeRF 2.0 xA4 (FPGA cogs), Zynq-7010 SDR, ESPARGOS/ESP-SDR; RTL-SDR/HackRF/LimeSDR/USRP baselines |
| [ai-edge.md](ai-edge.md) | 6 | Jetson Orin (Nano/NX/AGX), DGX Spark GB10, Coral, Hailo-8/8L, Radxa Rock 5B+/Orion O6 |

## Catalog: hardware -> software link

`crates/weftos-cog-market/catalog/catalog.json` is the typed catalog the console and dashboard read
(Projects -> Modules -> Chips). A module links to the software that drives it with `cogs: [<cog id>]`,
and may carry `firmware` facts (`version`, `read_with`, `read_config_key`, `update`, `url`, `notes`;
empty = unknown) and `docs: [{label, url}]` (`url` is a web link or a repo path). Availability and
install state are never stored here; the console joins the link with the marketplace and the host.
See [ADR-107](../adr/adr-107-catalog-hardware-software-link.md). `HwCatalog::validate()` (run by the
crate tests) checks the link shape.

## Cross-cutting findings (load-bearing)

- **Arch is per-target, from the userland not the kernel.** cog0's Pi 5 has an aarch64 kernel but an
  **armhf userland** → cogs + host build armv7/armhf there. Pi Zero 2 W is armv7l. Build per target.
- **x86 Xeon box has no AVX2** → SIGILL risk for AVX2-assuming builds; target generic/Westmere.
- **ESP32 Fleet constraints**: Xtensa PSRAM-atomics landmine (S2/S3; C3/C6 immune); OTA is a *hardware*
  decision (4 MB boards = single-app/no-OTA, N8R8 is OTA-capable); HIL needs USB-Serial/JTAG (absent on
  the S2); **ESP32-C6 has a native 802.15.4/Thread mesh radio** — relevant to the Fleet layer (COG-010).
- **AI TOPS are NOT cross-comparable**: DGX Spark = FP4, Jetson = sparse-INT8, Coral/Hailo = INT8.
  Compare on memory + precision + ecosystem, not a single TOPS number. DGX Spark power draw unconfirmed.

## Related
- Cogs/sensors: `~/Clients/cognitum/cogs-*` (COG-007 bridge, COG-009 host/console, COG-010 Fleet).
- Firmware substrate: `crates/clawft-edge-pad*` (no_std/std ESP32).
- Placement direction: WeftOS ADR-099 (placement), the mesh-workload-placement memory.
- Vendor writeups: [sipeed.md](sipeed.md), [pololu.md](pololu.md), [toradex.md](toradex.md), [debix.md](debix.md).
- ESP32-P4 Linux reference: [esp32-p4-nommu-linux.md](esp32-p4-nommu-linux.md) (NOMMU Linux on Stamp-P4 + C6; patterns, not a runtime target).
