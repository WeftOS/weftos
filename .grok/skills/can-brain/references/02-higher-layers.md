# Higher layers

Each family is a different identifier layout and a different dictionary. Sharing a pair of wires does not make them one protocol. 11-bit CANopen and 29-bit J1939 can be electrically present together; two different 29-bit layouts that both claim the same identifier bits cannot. The bitwise collision of J1939 with Cyphal is the Desktop draft's claim and stays `proposal` until the two layouts are placed side by side (`04-weftos.md`).

## CANopen

`documented` from CiA public pages opened this pass.

- Application layer: CiA 301 (classic, CAN CC). International restatement: EN 50325-4, as named on [CiA higher-layer protocols](https://www.can-cia.org/can-knowledge/standardized-higher-layer-protocols). The EN text was not opened.
- CANopen FD: CiA 1301, including longer PDOs and the universal service data object (USDO). Same page. A conformance plan CiA 1310-1 is listed on CiA technical documents; it was not read.
- Generic profiles, titles from [CiA generic device profiles](https://www.can-cia.org/can-knowledge/generic-device-profiles):
  - CiA 401 — I/O devices. Parts B (behavior), C (CANopen CC), F (CANopen FD). Joysticks are in scope. [CiA 401](https://www.can-cia.org/can-knowledge/cia-401-series-i/o-device-profile).
  - CiA 404 — measuring devices and closed-loop controllers. [CiA 404](https://www.can-cia.org/can-knowledge/cia-404-canopen-device-profile-for-measuring-devices-and-closed-loop-controllers). The page says a CiA 404 device uses object-dictionary indexes 0x6000–0x9FFF and does not follow the CiA 301 logical-device model, so one node-id does not host a second profile beside it.
  - CiA 406 — encoders. Parts include a J1939 mapping (406-J). [CiA 406](https://www.can-cia.org/can-knowledge/cia-406-encoder-profile).
  - CiA 410 — inclinometers. Parts include CANopen FD (410-F) and J1939 (410-J). [CiA 410](https://www.can-cia.org/can-knowledge/cia-410-series-inclinometer-profile).
  - CiA 418 — battery modules.
  - CiA 419 — battery chargers.
- Node-id range, SYNC (0x080), TIME (0x100), and the 1984 epoch of the TIME object are the draft's statements. They were not re-read from CiA 301 this pass. Grade `unknown` until the PDF is open. Do not put them in a cog from memory.

CANopen is the draft's adapter for bought industrial sensors, not its native WeftOS wire. That role is `proposal`.

## SAE J1939

`unknown` as clause text. SAE J1939-21 (data link) and J1939-81 (network management, address claim) are the documents the draft points at. J1939DA holds the PGN/SPN tables and is paywalled ([SAE](https://doi.org/10.4271/j1939da_201704), the draft's S3). This brain does not repeat SPN scales.

What is safe to say without the DA:

- `inferred` from the kernel documentation pointer in `03-tooling.md` and from the protocol's public descriptions: a 29-bit identifier, an 8-bit source address, a parameter group number, and a 64-bit NAME used in address claim. The field widths inside the 29-bit id (priority, EDP, DP, PF, PS) are the draft's §3. Confirm against J1939-21 before generating a DBC.
- Address claim, request, and transport PGNs in the draft (60928, 59904, 0xEC00, 0xEB00) are marked `[U]` there. Leave them `unknown`.
- Proprietary ranges: the draft proposes PDU2 0xFF00–0xFFFF for WeftOS readings and 0xEF00 for peer commands. `proposal`. Not an accepted allocation.
- Joystick PGNs BJM1 64982 and EJM1 64983 are the draft's `[S]` cites (Danfoss PDFs and isobus.net). Re-fetch those PDFs before a cog treats 1023 as "not available." The "1023 means N/A" rule is the draft's `[S]`, not re-opened here.

Linux calls this family J1939 and implements it for ISO 11783 as well. See `03-tooling.md`.

## ISOBUS (ISO 11783)

`documented` as a pointer only: the draft and the public dictionary at [isobus.net](https://www.isobus.net/) treat ISO 11783 as the agricultural profile on the J1939 data link. The ISO text was not opened. Use the dictionary as a lookup, then confirm the scale in J1939DA or the ISO part before a DBC factor is committed. The draft's S3 links are the starting list, still `proposal`.

## NMEA 2000

`unknown` beyond the family name. Marine instrument network, associated with IEC 61162-3, built on the same ideas as ISO 11783. The standard is licensed. Do not copy PGN layouts out of a remembered table. The draft's question 6 (GNSS PGNs 129025/129029 as a replacement for proprietary GNSS) stays open.

## DeviceNet

`unknown` as clause text. CIP on CAN, maintained by ODVA ([odva.org](https://www.odva.org/)). Typically 11-bit identifiers and tighter length and bitrate limits than a raw CAN trunk. No WeftOS node in the repo notes speaks DeviceNet. If a purchased module does, give it an adapter and do not overlay its identifiers on the J1939 proposal.

## DroneCAN and Cyphal

`unknown` as DSDL text this pass.

- DroneCAN is the maintained UAVCAN v0 line. Specification and DSDL list: [dronecan.github.io](https://dronecan.github.io/). The draft names `uavcan.equipment.ahrs.*`, `uavcan.equipment.gnss.Fix2`, and `uavcan.equipment.range_sensor.Measurement`. Those names were not opened here. Grade `proposal` / `[U]`.
- Cyphal is UAVCAN v1. Public regulated types: [OpenCyphal/public_regulated_data_types](https://github.com/OpenCyphal/public_regulated_data_types). The draft names `uavcan.si.sample.*` and `uavcan.time.Synchronization`. Not opened. The SI scalar types are why the draft wants Cyphal-like payload layout inside a J1939 proprietary PGN. That is a proposal about payload shape, not a decision to run Cyphal on the wire.
- libcanard (C) and pycyphal are the stacks the draft names. Not version-pinned here.

Do not put Cyphal and the J1939 proposal on one 29-bit bus until the identifier layouts are compared. The draft says they collide. This brain has not done that comparison.

## ISO-TP, UDS, OBD-II

`unknown` as clause text. Document ids the draft and the usual stack use:

| Doc | Role |
|---|---|
| ISO 15765-2 | ISO-TP, segmented transport on CAN |
| ISO 15765-4 | OBD diagnostics on CAN (legislated CAN OBD) |
| ISO 14229 | UDS services |
| SAE J1979 / ISO 15031 | OBD PIDs and modes |

The draft's Civic bridge table (mode 01 PIDs republished as J1939 PGNs) is `proposal`, and several rows are `[U]`, including whether a 2005 Civic DX even supports those PIDs. PID formulas the draft attributes to Wikipedia's OBD-II PID page were not re-fetched. Re-fetch before a bridge cog ships a scale. Support bitmaps (PIDs 0x00, 0x20, 0x40) are how you learn what the ECU actually has; do not assume a PID from the year of the car.

## K-line

`unknown` as clause text. ISO 9141-2 and ISO 14230 (KWP2000) are the UART-style diagnostic lines the draft assigns to a 2005 US Civic, with the caveat that CAN OBD is mandatory only from a later model year (`[U]` in the draft). Poll rate is slow relative to CAN. The bridge the draft describes is an ESP32 on K-line that republishes onto classic CAN. That architecture is `proposal`. The inbox request states it as the car we have. Treat "this car is K-line" as the operator's context, and treat each PID as unproven until the support bitmap says so.

## LIN

`unknown` as clause text. ISO 17987. Single-wire UART, not CAN, usually a commander and responders, often bridged onto CAN by a gateway chip. Mention it when a sensor module's second port is LIN. Do not describe a LIN frame as a CAN frame. No WeftOS LIN wiring is in the repo notes read for this brain.
