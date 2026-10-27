# CAN brain index

Read this, then one topic file. Grade every claim you carry out of here.

## Grades

| Grade | Meaning |
|---|---|
| documented | A page opened for this brain, or a file in the WeftOS repo, says it. |
| inferred | Two documented facts, read together. Show both. |
| unknown | No clause in hand. Ask. Do not supply a number. |
| proposal | Only the Desktop draft `CAN-STANDARDS.md` (2026-10-27) says it. Not accepted. |

The draft marks its own lines `[S]` (it named a URL) or `[U]` (it was unsure).
A `[U]` line is unverified: grade it `unknown` until a spec is open. Do not
promote it by repeating it. A `[S]` line is still `proposal` until someone
re-fetches the URL and a human accepts the map.

Operator errata in `04-weftos.md` override a contradictory cell in the draft.
Two are recorded: IMU accel is 0.002 m/s² per bit, and GNSS latitude and
longitude fields are 32-bit. They do not accept the rest of the draft.

## Layers, highest trust first

1. Spec and vendor pages opened while this brain was written. URLs are in the topic files. ISO, SAE J1939DA, and most CiA PDFs are paywalled: cite the document id, do not paste clause text, do not recall a table from memory.
2. This repo. Copper bus and clock files named in `04-weftos.md`. ADR-124 is Proposed, not Accepted.
3. Carry-deck catalog `sensor_types.json` (branch `carry-deck-parts`), ids listed in `04-weftos.md`. The file is not vendored into this package.
4. Desktop draft `CAN-STANDARDS.md`. Proposal layer only.
5. Anything else. `unknown`.

## Files

| File | Holds |
|---|---|
| [01-physical.md](01-physical.md) | CAN CC, CAN FD, CAN XL, PHY, termination, bit timing, errors, bus-off |
| [02-higher-layers.md](02-higher-layers.md) | CANopen, J1939, ISOBUS, NMEA 2000, DeviceNet, DroneCAN, Cyphal, ISO-TP, UDS, OBD, K-line, LIN |
| [03-tooling.md](03-tooling.md) | SocketCAN, can-utils, DBC, cantools, python-can, TWAI, Zephyr, FDCAN |
| [04-weftos.md](04-weftos.md) | Repo rules, the draft's proposal, the conflicts |
| [05-open.md](05-open.md) | Questions that are still open |

## How to answer

Repo rule, then draft proposal, then unknown. Do not collapse those into one "the standard is."
