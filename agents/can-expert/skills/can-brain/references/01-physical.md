# Physical and data link

Opened this pass: the CiA pages linked below. ISO PDFs were not opened. Numbers that live only inside those PDFs stay `unknown` until the clause is open.

## Three generations

`documented` from [CiA, ISO 11898-1:2024 released](https://www.can-cia.org/news/slider-2) (page dated 2024-05-27) and [CiA CAN XL](https://www.can-cia.org/can-knowledge/can-xl):

ISO 11898-1:2024 specifies three protocol generations.

| Name in ISO 11898-1:2024 | Also called | Data field | What CiA states in public pages |
|---|---|---|---|
| CAN CC | classic CAN, CAN 2.0 | up to 8 bytes | up to 1 Mbit/s (CiA brochure snippet, 2026) |
| CAN FD | flexible data rate | up to 64 bytes | arbitration up to 1 Mbit/s; data phase limited by the transceiver. CiA brochure: up to 8 Mbit/s |
| CAN XL | extended data-field length | 1 to 2048 bytes | CiA brochure: up to 20 Mbit/s. Optional PWM at the AUI. Error signaling can be switched off. CANsec is a CiA 613 topic, not something this brain has read |

CAN XL splits the identifier: an 11-bit priority field and a 32-bit acceptance field. `documented` on the CiA CAN XL page. CiA 610-1 (DLL) was withdrawn after submission into ISO 11898-1; CiA 610-3 relates to the PMA and ISO 11898-2. `documented` on the [CiA 610 series page](https://www.can-cia.org/can-knowledge/cia-610-series-can-xl-specification-and-test-plans).

CAN FD frame formats: FBFF (11-bit identifier) and FEFF (29-bit). The FDF bit marks FD. CAN FD does not support remote frames. `documented` on [CiA CAN FD](https://www.can-cia.org/can-knowledge/can-fd-the-basic-idea). That page also warns that non-ISO CAN FD controllers exist and are not ISO 11898-1.

## 11-bit and 29-bit

`inferred` from the same CiA FD page plus ordinary CAN CC teaching. CAN CC base frames use an 11-bit identifier (2.0A). Extended frames use 29 bits (2.0B). The Bosch CAN Specification 2.0 PDF was not opened this pass, so the 2.0A/2.0B labels are `inferred` from that long-standing usage, not from a clause quote. IDE recessive selects the extended format. Confirm in ISO 11898-1 before a silicon review.

A CAN FD controller can still send CAN CC frames. `documented` on the CiA FD page.

## Mixed classic and FD

`unknown` as a clause quote. The usual teaching is that a CAN CC controller sees the FD format bit as a form error and sends an error flag, which destroys FD traffic on that bus. The Desktop draft marks the same warning `[U]`. Do not approve an FD frame on a bus that still contains a CAN CC-only controller. Put FD on a separate segment, or make every controller FD-capable. VENTUNO's extra PHYs are the draft's proposed place for that segment (`04-weftos.md`).

## Physical medium

ISO 11898-2 is the high-speed physical layer (the CiA 610 page says CiA 610-3 is standardized there). The PDF was not opened.

`inferred`, and check the clause before a design freeze: high-speed CAN is a differential pair, CAN_H and CAN_L, terminated with 120 Ω at each physical end of the trunk. Split termination (two resistors near 60 Ω with the midpoint AC-coupled to ground) is one end, used for EMC. A star, a stub longer than the bitrate allows, or a third 120 Ω resistor changes the level and the edges. This brain does not give a stub-length table; that table is topology- and bitrate-specific and was not opened.

Transceiver mode names for CAN XL (arbitration / FAST TX / FAST RX, SIC) are `documented` as labels on the CiA CAN XL page. Which chip implements them is `unknown` until that datasheet is open.

SN65HVD230 is the transceiver named in the in-repo copper note for an ESP32-S3 (`04-weftos.md`). It is a 3.3 V classic high-speed part in that note. It is not a CAN XL PHY.

## Bit timing

`unknown` as a register recipe. Both CAN FD and CAN XL use two bit-timing settings (arbitration phase and data phase). `documented` on the CiA CAN XL page for XL, and on the CiA FD page for FD ("two different bit rates").

The parameters a controller actually programs (time quantum, prescaler, segment 1, segment 2, SJW, sample point) are per chip. Copying another board's sample-point percentage onto ESP32 TWAI or STM32 FDCAN is how a bus goes bus-off. Open the manual named in `03-tooling.md` for the chip in hand. This brain stores no sample-point number.

Oscillator tolerance stacks with cable delay. A number for "maximum nodes" or "maximum length at 500 kbit/s" is `unknown` here. ISO 11898-2 and the transceiver datasheet own it.

## Errors and bus-off

`unknown` as clause text. The mechanism ISO 11898-1 is cited for, and that this brain will not treat as a remembered threshold:

- Each controller keeps a transmit error counter and a receive error counter.
- The usual published thresholds are error-active at or below 127, error-passive above 127, bus-off when the transmit counter passes 255. Those integers were not read from ISO 11898-1:2024 this pass. Confirm them in the clause before software compares against a constant.
- A bus-off node must not resume by immediately transmitting. Recovery is a controller-specific wait (typically 128 occurrences of 11 recessive bits) plus an application decision. The "128 × 11" figure is the same class of unreopened clause: `unknown` until the manual for that controller is open.

Error flags are dominant overloads of the bus. One bad node, one bad termination, or one FD frame on a classic bus will raise counters on every node. Look at counters (`ip -details link show can0` on SocketCAN, the TWAI status registers on ESP-IDF) before you change a DBC.

## What this file will not do

It will not pick 500 kbit/s versus 125 kbit/s for a new bus. The copper note's range and the draft's 500 kbit/s choice are in `04-weftos.md`, with their grades.
