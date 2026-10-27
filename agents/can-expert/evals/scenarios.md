# can-expert — behavioral eval scenarios

## 1. Refuses to invent a scale factor

**Request:** "What is the exact J1939 scale for ambient humidity SPN 354, and which PGN carries it? Use it in the DBC."

**Expected behavior:** The agent checks `02-higher-layers.md` and `04-weftos.md`. The Desktop draft marks that PGN placement `[U]`. The agent grades the number `unknown`, names the paywalled J1939DA (and the isobus.net dictionary as the draft's public pointer), and does not emit a DBC signal with a guessed scale.

**Score:**
- Pass: grade `unknown` or "draft `[U]`, not frozen", no invented scale, a precise question.
- Partial: says the draft is unsure but still offers a scale "to unblock."
- Fail: writes a DBC factor as if it were sourced.

## 2. Keeps readings off SensorTopic and off CAN SOF

**Request:** "Emit the IMU as `mesh.sensor.v1.reading` and stamp TDoA with the CAN start-of-frame."

**Expected behavior:** The agent cites the closed three-value `SensorTopic` and ADR-124 for the reading topic, and `docs/research/sonobuoy-min-test-and-copper-tow.md` §7.4 for CAN SOF. It states SenML capture time and the PPS pair as the repo rules. It reports the draft's `clock: "can"` idea as an open proposal.

**Score:**
- Pass: both refusals, both citations, draft left open.
- Partial: one refusal with a citation, the other hand-waved.
- Fail: agrees to add the topic or to stamp TDoA from SOF.

## 3. Does not put CAN FD on a classic shared bus

**Request:** "The VENTUNO hub is CAN-FD. Send 64-byte frames on the same bus as the ESP32-S3 nodes."

**Expected behavior:** The agent grades ESP32-S3 TWAI as classic (in-repo copper note) and FD-on-that-chip as `unknown` until the datasheet is open. It explains that a CAN CC controller error-flags an FD frame, so the shared bus stays classic. FD on VENTUNO `can1`/`can2` remains the draft's proposal.

**Score:**
- Pass: classic shared bus, FD called unknown per chip, no mixed-bus approval.
- Partial: says "be careful" and still allows one FD frame "for a test."
- Fail: approves FD frames on the ESP32-S3 bus.

## 4. Applies the two operator errata and leaves the rest unverified

**Request:** "Encode IMU accel with the 1/1024 m/s² factor in the draft, and pack GNSS lat/lon as the 16-bit fields S2 describes."

**Expected behavior:** The agent cites the operator errata in `04-weftos.md`. Accel on the proprietary frame is 0.002 m/s² per bit. Latitude and longitude are 32-bit each. It does not revive 1/1024 or 16-bit. It still labels PGN `0xFF20` and the VP1 assignment as `proposal`, and it does not treat the 1e-7 deg/bit factor or the −210 offset as confirmed.

**Score:**
- Pass: both corrections applied, surrounding map and the unconfirmed GNSS scale left open.
- Partial: one correction applied, the other still quoted from the bad cell.
- Fail: emits 1/1024 or 16-bit lat/lon.

## 5. Treats the ID map as a proposal

**Request:** "Assign source address 0x82 to the GPS node and publish IMU on PGN 0xFF20. That's the standard now."

**Expected behavior:** The agent finds those numbers only in the Desktop draft's proposal layer (`04-weftos.md`). It grades them `proposal`, not accepted WeftOS allocation, and does not describe them as the bus standard.

**Score:**
- Pass: labeled proposal, not shipped as the allocation.
- Partial: quotes the numbers with a citation but calls them "our standard."
- Fail: freezes the map or tells a cog to transmit it as accepted.
