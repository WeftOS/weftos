# Sensors — WeftOS/WeaveLogic hardware KB

Types, shared concerns, and which output can carry a reading are in
[module-types.md](module-types.md). This file stays the per-part notes.

Public hardware facts for the sensors our WeftOS / Cognitum-cogs stack reads (and
candidates worth turning into future cogs). Specs are from vendor wikis and
datasheets; each entry cites its source. No appliance credentials or partner-internal
details here — hardware only.

---

## AD8232 Heart Rate / ECG Analog Front End (DFRobot SEN0213)

- **Role in our stack**: cog `sen0213-ecg`
- **Interface**: Analog output (single-lead ECG), 0–3.3 V swing. Gravity 3-pin
  (PH2.0-3P: `-` GND, `+` VCC, `A` analog signal). Not an I2C device — the analog
  output is digitized by a separate ADC (ADS1115, below). 3-electrode lead wire
  (RA/LA/RL) terminates in a 3.5 mm jack.
- **Key specs**: Input voltage 3.3–6 V (5 V recommended); operating current < 10 mA;
  output 0–3.3 V analog; board 35 × 22 mm. AD8232 front end provides instrumentation
  amp + filtering to recover clear P-QRS-T morphology (PR / QT intervals).
- **Our usage**: Analog `A` pin feeds an ADS1115 I2C ADC adapter on a Cognitum Seed
  (Pi Zero 2 W, now Pi 5); the cog samples the digitized waveform (~10 Hz on the Seed's
  native ADS1115 driver) and exports it. Companion app `cog-ecg-scope` ([WeftOS/weftos-cogs](https://github.com/WeftOS/weftos-cogs/tree/v0.1.4/crates/cog-ecg-scope)) polls the
  cog's export on port **:8046**.
- **Gotchas**: Lead/electrode placement dominates signal quality; motion artifacts are
  large. It is a biopotential **demo/education** front end, not a medical device. The
  AD8232 can also emit a Lead-Off Detection (LO+/LO-) signal, but the SEN0213 Gravity
  breakout exposes only the analog output. Sample fast enough to resolve the QRS complex
  (≥ ~100–250 Hz ideal; the 10 Hz Seed path captures heart-rate trend, not full
  morphology).
- **Source**: <https://wiki.dfrobot.com/sen0213/> ·
  AD8232 datasheet <https://www.analog.com/media/en/technical-documentation/data-sheets/ad8232.pdf>

## 8×8 Matrix ToF 3D Distance Sensor (DFRobot SEN0628)

- **Role in our stack**: cog `sen0628-tof`
- **Interface**: I2C (Gravity), UART, or USB-C. PH2.0-4P Gravity connector.
  Operating voltage **3.3–5 V**. I2C address: 4 switchable options
  **0x30 / 0x31 / 0x32 / 0x33** (we use **0x33** as default); up to 4 can be
  cascaded on one bus. UART fixed at 115200 bps.
- **Key specs**: VL53L7CX multizone ToF imager behind an onboard **RP2040** MCU.
  Depth matrix **8×8 (64 zones)** or 4×4 mode. Range **20–4000 mm** (our cog scans the
  20–3500 mm band). Frame rate **15–60 Hz** (8×8 tops out lower than 4×4). FOV **60°
  horizontal × 60° vertical (90° diagonal)**. Accuracy ±11–12 mm from 20–200 mm, ±5–6%
  from 200–4000 mm (white/gray targets).
- **Our usage**: Read over I2C on the Seed; the cog publishes the 8×8 depth frame on its
  export. Companion app `weftos-tof-scope` polls the cog on port **:8047**.
- **Gotchas**: RP2040 does the VL53L7CX housekeeping, so the host sees a simpler frame
  interface — but higher frame rates force the lower 4×4 resolution. Ambient IR and
  highly specular/absorptive targets degrade accuracy. Keep the cover glass clean; FOV is
  wide, so near-field clutter bleeds into edge zones. Only 4 distinct I2C addresses, so a
  bus caps at 4 of these sensors.
- **Source**: <https://wiki.dfrobot.com/sen0628/> ·
  product page <https://www.dfrobot.com/product-2999.html>

## 16-bit I2C ADC (ADS1115)

- **Role in our stack**: ADC adapter — digitizes the SEN0213 ECG analog output
- **Interface**: I2C, address **0x48** default; **0x48–0x4B** selectable via the ADDR
  pin (to GND/VDD/SDA/SCL). Supply 2.0–5.5 V. 4-wire breakout (VDD, GND, SCL, SDA) plus
  4 analog inputs and an ALERT/RDY pin.
- **Key specs**: 16-bit delta-sigma ADC; 4 single-ended or 2 differential inputs via
  internal MUX; programmable gain amp (PGA) with full-scale ranges from ±6.144 V down to
  ±0.256 V (gain 2/3–16); data rate **8–860 SPS**; internal reference + oscillator;
  continuous-mode current ~150 µA.
- **Our usage**: The ECG front end's analog `A` output wires to one ADS1115 channel;
  the Seed reads conversions over I2C and the `sen0213-ecg` cog turns them into the ECG
  waveform export. Our Seed path runs it ~10 Hz.
- **Gotchas**: 860 SPS is the hardware ceiling and is marginal for full ECG morphology —
  fine for heart-rate trending. PGA full-scale must bracket the 0–3.3 V ECG swing (don't
  clip). Don't confuse with the 12-bit ADS1015 (same pinout/addresses, lower resolution).
  Set the address pin deliberately if sharing the bus with other 0x48-family parts.
- **Source**: <https://www.ti.com/product/ADS1115> ·
  datasheet <https://www.ti.com/lit/ds/symlink/ads1115.pdf>

## Temperature / Humidity / Pressure Sensor (BME280)

- **Role in our stack**: environmental sensing — the Seed agent ships a built-in driver
- **Interface**: I2C (addresses **0x76** default, **0x77** when SDO tied to VCC) or SPI.
  Supply 1.71–3.6 V (run at 3.3 V). On breakouts: VCC, GND, SCL/SCK, SDA/SDI, plus
  SDO + CSB for SPI/address select.
- **Key specs**: Temperature −40 to +85 °C (±1 °C), relative humidity 0–100% (±3% RH),
  pressure 300–1100 hPa (±1 hPa absolute; ~±0.12 hPa relative, ≈ ±1 m altitude).
  Resolution 0.01 °C / 0.008% RH / 0.01 hPa. Current ~3.6 µA at 1 Hz (all three).
- **Our usage**: Read over I2C by the Seed agent's native BME280 driver; no external ADC
  needed. Good baseline environment channel to pair alongside any cog.
- **Gotchas**: Self-heating raises the temperature reading under fast continuous
  sampling — use forced mode / slow rates for accurate ambient temp. Humidity needs
  burn-in/equilibration after exposure to extremes. **BMP280** (pressure+temp only, no
  humidity) shares the package and address — verify the part. SDO pin floating gives an
  undefined address; tie it.
- **Source**: <https://www.bosch-sensortec.com/products/environmental-sensors/humidity-sensors-bme280/> ·
  datasheet <https://www.bosch-sensortec.com/media/boschsensortec/downloads/datasheets/bst-bme280-ds002.pdf>

---

## Candidate sensors (not yet a cog)

Common I2C parts that fit the Seed / Gravity + ADS1115 pattern and would make good
future cogs.

### 9-DOF Absolute Orientation IMU (Bosch BNO055) — candidate (not yet a cog)
- **Interface**: I2C, address **0x28** default / **0x29** (also UART). Supply 3.3 V
  (3–5 V on breakouts with regulator).
- **Key specs**: Accelerometer + gyroscope + magnetometer with an on-chip sensor-fusion
  core that outputs absolute orientation (quaternion / Euler) at 100 Hz — no host-side
  fusion math. ±2–16 g accel, ±125–2000 °/s gyro.
- **Fit**: Drop-in orientation/tilt cog; the onboard fusion keeps the host cheap, like
  the RP2040 on the SEN0628.
- **Gotchas**: Magnetometer needs figure-8 calibration and is thrown off by nearby
  metal/magnets; clock-stretching quirks on some I2C hosts.
- **Source**: <https://www.bosch-sensortec.com/products/smart-sensor-systems/bno055/>

### 6-Axis IMU (TDK InvenSense MPU-6050) — candidate (not yet a cog)
- **Interface**: I2C, address **0x68** default / **0x69** (AD0 high). Supply 2.375–3.46 V
  (breakouts add a 3.3 V regulator for 5 V in).
- **Key specs**: 3-axis gyro (±250–2000 °/s) + 3-axis accel (±2–16 g), 16-bit, with an
  onboard DMP and FIFO. The low-cost motion baseline.
- **Fit**: Cheap vibration / motion / tilt cog when absolute heading isn't needed.
- **Gotchas**: No magnetometer (relative heading only, drifts); many clone boards. For
  absolute orientation prefer the BNO055.
- **Source**: <https://invensense.tdk.com/products/motion-tracking/6-axis/mpu-6050/>

### Photoacoustic CO₂ Sensor (Sensirion SCD41) — candidate (not yet a cog)
- **Interface**: I2C, address **0x62**. Supply 2.4–5.5 V (3.3 V typical).
- **Key specs**: True NDIR/photoacoustic CO₂ **400–5000 ppm** (±(50 ppm + 5% of
  reading)); also reports temperature and humidity. Low-power single-shot modes.
- **Fit**: Indoor air-quality / occupancy cog; complements the BME280 environment
  channel with real CO₂.
- **Gotchas**: Needs periodic automatic self-calibration (ASC) with fresh-air exposure, or
  manual forced recalibration; self-heating biases its temp output. Not the SCD30
  (larger, different pinout).
- **Source**: <https://sensirion.com/products/catalog/SCD41>

### VOC Air-Quality Sensor (Sensirion SGP40) — candidate (not yet a cog)
- **Interface**: I2C, address **0x59**. Supply 1.7–3.6 V.
- **Key specs**: MOx gas sensor outputting a raw signal converted by Sensirion's VOC
  Algorithm into a 1–500 **VOC Index** (100 = typical baseline). Fast response, small
  2.44 × 2.44 mm package.
- **Fit**: Relative indoor-air-quality cog, pairs with SCD41 (CO₂) and BME280 (T/RH/P)
  for a full environment bundle.
- **Gotchas**: Output is a *relative* index, not absolute ppb; the VOC algorithm needs a
  running baseline. For humidity compensation feed it RH/T (e.g. from the BME280). SGP41
  adds an NOx channel.
- **Source**: <https://sensirion.com/products/catalog/SGP40>

### Single-Zone ToF Distance Sensor (ST VL53L1X) — candidate (not yet a cog)
- **Interface**: I2C, address **0x29** default (software-reprogrammable). Supply 2.6–3.5 V.
- **Key specs**: Single-zone ToF ranging up to **~4 m**, up to 50 Hz, programmable
  region-of-interest, 940 nm VCSEL. Simpler/cheaper than the SEN0628 8×8 matrix.
- **Fit**: Lightweight presence / single-point distance cog where a full depth matrix is
  overkill; naturally a sibling to `sen0628-tof`.
- **Gotchas**: Only one address out of reset, so multi-sensor buses need XSHUT
  sequencing to reassign addresses at boot. Ambient IR and cover-glass crosstalk affect
  range.
- **Source**: <https://www.st.com/en/imaging-and-photonics-solutions/vl53l1x.html>
