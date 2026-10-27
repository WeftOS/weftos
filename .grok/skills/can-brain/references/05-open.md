# Open questions

A gap is one of these, or a new line in the same shape. Do not close one by guessing.

## From the Desktop draft (still open)

1. Exact J1939DA scaling for the humidity SPN, altitude, and the GNSS factor and offset. Latitude and longitude **width** is closed: 32-bit each (operator errata in `04-weftos.md`). The draft's 1e-7 deg/bit and −210 offset were in the same `[U]` note and were not confirmed. The DA is paywalled. isobus.net is the draft's public dictionary. Neither was re-checked into a DBC.
2. How a reading is authenticated across the CAN boundary. Candidates the draft names (hub signature on ingress, truncated MAC in a proprietary frame) are unchosen.
3. Whether a future clock enum gains a `can` value. ADR-124, still Proposed, does not have one. Adding it is an ADR change, not a cog flag.
4. Whether any bought CANopen sensor will sit on this bus. If yes, the hub adapter is in scope. If no, do not spend the 11-bit identifier space.
5. Whether any node is FD-only. If yes, it belongs on a VENTUNO segment that does not contain a CAN CC controller.
6. Whether NMEA 2000 GNSS PGNs are usable under their license.
7. Whether the ESP32 variant on the bench gives a TX-complete timestamp tight enough for the draft's sync pair.

## Added while writing this brain

8. Which ESP32 is on each carry-deck node (S3, C6, other), and what that chip's datasheet says about TWAI versus CAN FD. The inbox names C6 and S3. The copper note documents S3 as CAN 2.0. FD remains unknown per chip.
9. Confirm the 2005 Civic DX ECU on the bench speaks ISO 9141-2 or ISO 14230, then read the mode 01 support bitmap before any PID in the draft's bridge table is treated as present.
10. Re-fetch the VENTUNO Q manual (draft S1) before quoting ATA6563, the 2×60.4 Ω split, or the `ip link` line in a cog.
11. Place the J1939-21 29-bit layout next to Cyphal's CAN identifier layout before claiming they cannot share a bus. The draft asserts the collision. This brain has not checked it.
12. Who accepts the draft. Until then every SA, proprietary PGN, and scale in it stays `proposal`, except the two operator errata in `04-weftos.md` (IMU accel 0.002 m/s² per bit, GNSS lat/lon 32-bit). Those two numbers are fixed inside an otherwise unaccepted map. Acceptance of the map belongs in a WeftOS ADR or an explicit note from the owner, not in this package.
