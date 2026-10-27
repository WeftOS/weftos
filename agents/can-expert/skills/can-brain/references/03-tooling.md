# Tooling

URLs here are the public entry points. Pages not fetched this pass are marked. A command line is a starting point for the controller you have open, not a bitrate decision.

## SocketCAN

`documented` as the Linux API name. Doc entry (not re-fetched this pass): [kernel can](https://docs.kernel.org/networking/can.html) and [kernel j1939](https://docs.kernel.org/networking/j1939.html).

Bring-up shape the draft quotes for a classic 500 kbit/s VENTUNO `can0`:

```
ip link set can0 type can bitrate 500000
ip link set can0 up
```

The draft also says to force `fd off` on the shared bus. Confirm the exact `ip link` keywords against the kernel doc for the running version before putting them in a cog. `fd on` with `dbitrate` is a different bus. Hardware timestamps, when the controller and the driver support them, are `SO_TIMESTAMPING`. Whether the VENTUNO ATA6563 path actually delivers them is `unknown` until `hwstamp_ctl` or the driver source is checked.

Error counters and state (`ERROR-ACTIVE`, `ERROR-PASSIVE`, `BUS-OFF`) show in `ip -details -statistics link show can0`. Read them before you blame a DBC.

J1939 sockets are `CAN_J1939` in the kernel, for ISO 11783 / J1939, not a userspace reimplementation of address claim. Enable the family in the kernel config. The doc page above is the spec for that socket. It was not re-read this pass.

## can-utils

[linux-can/can-utils](https://github.com/linux-can/can-utils). The tools this brain expects on a hub: `candump`, `cansend`, `cangen`, `canbusload`, `cansequence`. `candump -ta` prints arrival time. That timestamp is the host's view of arrival, not the sensor's capture time (`04-weftos.md`).

## DBC, cantools, python-can

DBC is the de-facto text database (Vector CANdb). It stores names, start bit, length, endianness, factor, offset, and min/max. It does not store WeftOS grades. A factor copied from an unconfirmed SPN is still unconfirmed after it lands in a `.dbc`.

- [cantools](https://github.com/cantools/cantools) — decode and generate from DBC. Not version-pinned.
- [python-can](https://python-can.readthedocs.io/) — host sockets, including socketcan. Not version-pinned.

A WeftOS bridge cog, if one is written later, should generate its DBC from an accepted map. There is no accepted map yet. Do not generate one from the draft as if it were.

## ESP32 TWAI

Espressif's classic CAN controller is TWAI. Reference entry (not re-fetched this pass): the ESP-IDF `twai` peripheral doc for the chip you are compiling. The in-repo copper note uses TWAI on ESP32-S3 as CAN 2.0 (`04-weftos.md`).

`unknown`: which of ESP32, S3, C3, C6, C5, P4 have TWAI, and which of those have CAN FD. The draft says original ESP32/S3/C3 are classic and that C5/P4 might add FD (`[U]`). Open the datasheet for the module on the bench. Do not infer FD from "it is a newer Espressif chip."

Bit timing on TWAI is the driver's timing macro or the advanced timing struct for that IDF version. Do not paste a struct from a different IDF major without reading its fields.

TX-complete timestamps precise enough for the draft's sync pair are an open question (`05-open.md`). This brain does not claim the ISR path has them.

## Zephyr CAN

[Zephyr CAN](https://docs.zephyrproject.org/latest/hardware/peripherals/can/index.html) (not re-fetched). The API is controller-agnostic (`can_set_bitrate`, `can_send`, filters, state change callbacks). CANopenNode on Zephyr is a separate module; the draft points at [CANopenNodeZephyr](https://github.com/zephyrproject-rtos/CANopenNodeZephyr) and the Zephyr sample. Not built here.

Use Zephyr's timing calculation for the chosen bitrate. Do not hand-copy ESP-IDF quanta into a Zephyr `can_timing` struct.

## STM32 FDCAN

The draft says the VENTUNO Q hub is an STM32H5 FDCAN exposed as SocketCAN `can0`, PHY ATA6563, with `can1`/`can2` PHY-less. Sources it names: the Arduino VENTUNO Q user manual and the ABX00181 datasheet (draft S1). Those PDFs were not re-fetched this pass. Grade the pinmux, the transceiver part, and the split-termination values as `proposal` until the manual is open.

FDCAN can be configured classic-only. That is the configuration the draft wants on the shared bus. The reference manual for the exact STM32H5 die owns the bit-timing registers and the timestamp counter. Do not program them from an FDCAN example for a different series (G4, H7) without checking the clock tree.

## ESP-IDF CANopen component

The draft's S8 is `espressif/canopennode` on the component registry. Not opened this pass. It is relevant only if a node speaks CANopen. The draft's native wire is the J1939-style proposal, with CANopen as an adapter.
