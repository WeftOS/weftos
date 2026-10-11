# ADR-119: The field slate: a portable WeftOS node in an aluminium storage clipboard

- **Status**: Proposed (2026-10-11; owner direction)
- **Deciders**: owner
- **Builds on**: ADR-117 (display endpoints, objects, bindings; the leaf as a sensor), ADR-118
  (chief-of-staff console), ADR-100 and ADR-104 (cogs with sensor guides), ADR-108 (mesh,
  pairing), ADR-116 (tailnet router), the edge-pad bring-up (CrowPanel DIS08070H, 2026-10-10).

## Context

The owner wants the edge pad to become a field device built into an **aluminium storage
clipboard**: a sealed screen cut-out, an e-ink panel, physical inputs, a fingerprint sensor,
radio (LoRa and more), and **plugins**. The plugins are 3D-printed modules that fit under the
clip, so new devices and sensors can be added. The clipboard's storage cavity can hold a real
computer, or several.

Parts on hand, from the owner's photos of 2026-10-10:
- an aluminium storage form holder with a hinged lid, a riveted base and a stainless clip;
- a **CrowPanel ESP32 7"** (DIS08070H), the edge pad;
- a **small e-paper module**, plus a Waveshare 2.13" e-Paper HAT;
- a **TI-30X-style calculator**, whose keypad is used as physical keys;
- a **round capacitive fingerprint sensor** (6-wire, R503-class, UART);
- a stock of **C&K miniature toggles** (bat and paddle levers) and **C&K DIP switches** (2 to 12
  positions).

A larger ~10" panel was considered and **rejected** by the owner (CCFL backlight, poor
condition).

## Decision

### 1. Layout A (owner's choice)

Top to bottom, under the clip:

| Zone | Part | Role |
|---|---|---|
| Top | Calculator keypad, with the e-ink panel in the calculator's display window | Physical keys and a low-power status panel |
| Middle-bottom right | CrowPanel 7", behind a sealed cut-out | The main touch display (ADR-117 endpoint; chief-of-staff console) |
| Bottom left | Round fingerprint sensor | Local identity; confirming gated actions |
| Bottom strip | Switch panel (C&K toggles, DIP switches) under a cover | Modes, power and radio kill switch, module enables |
| Under the clip | Plugin rail with printed module sleeves | LoRa, GPS, sensors, extra compute |
| Storage cavity | The brain (computer), battery, wiring | Runs WeftOS |

### 2. The brain lives in the cavity

The CrowPanel's ESP32-S3 has one free GPIO (IO38, inventory 2026-10-10), so it can't host the
inputs. A **Raspberry Pi 5 or CM5**, or another compact board chosen by the cavity depth, runs:
- the WeftOS daemon, the chief-of-staff agent, local speech-to-text and the router;
- the **e-ink panel over SPI**;
- the **fingerprint sensor over UART**;
- the **I/O controller over USB**.

The CrowPanel stays a **display leaf**. It links to the brain over Wi-Fi now, and later possibly
over its USB-C serial (CH340), so the slate works with no Wi-Fi. Several computers can share the
cavity and mesh together.

### 3. Inputs are sensor events

A small **I/O controller**, an RP2040 or a spare ESP32-S3, scans the **calculator key matrix**,
the **toggles** and the **DIP switches**, and reports them as sensor events (ADR-117 §4):
`key.<name>`, `switch.<n>`, `dip.<bank>`. The chief of staff and any app can react to them:
- number keys answer surveys (ADR-118 §1a);
- a DIP bank selects the profile at boot;
- a toggle is a hardware radio kill switch, wired to cut power to the radio modules, not just
  reported.

### 4. Fingerprint as local identity

The fingerprint sensor unlocks the slate and **confirms gated actions**: the chief of staff's
outward-facing or production-touching steps, and kicking off work. A touch replaces a typed
password. Templates are stored on the sensor itself, and never leave the device.

### 5. Plugins under the clip

- **One keyed connector:** a printed rail under the clip carries USB-C (data and power) plus a
  row of pogo pins: 5 V, 3.3 V, GND, I²C, UART, and an ID line.
- **Printed sleeves:** each module is a sleeve that slides onto the rail.
- **Each module is a cog with a sensor guide** (ADR-100/104). Plugging one in triggers discovery,
  shows its guide, and installs its cog through the existing flow (guide before install).
- **First modules:** LoRa (SX1262), GPS, an environment sensor, extra compute. SDR and radar
  follow.

### 6. Radio and the metal case

The case is aluminium, so **every antenna (Wi-Fi, BLE, LoRa, GPS) sits in a printed part or
behind a printed window**, never inside the metal. **LoRa** carries low-rate messages between
slates and a base when there's no Wi-Fi or tailnet: status, check-ins, short notes, as an ADR-108
transport. The metal is used as a heat spreader and ground plane for the brain.

### 7. Sealing

- **Screen:** the cut-out gets a printed bezel with a gasket groove. The CrowPanel mounts from
  behind on its 4 × Ø3.2 holes (174 × 97 mm pattern).
- **Fingerprint and e-ink windows:** sealed the same way.
- **Switches:** the panel has a hinged cover.
- **Ports:** covered.

### 8. Printed parts are parametric and in the repo

OpenSCAD sources live in `docs/hardware/field-slate/cad/`, with all dimensions as parameters:
- `crowpanel-bezel.scad`: screen bezel, gasket groove and mounting bosses;
- `switch-plate.scad`: C&K toggle bushing holes and DIP windows;
- `module-sleeve.scad`: plugin sleeve and rail;
- antenna windows.

STLs are exported from them.

## Phases

| Phase | Delivers | Done when |
|---|---|---|
| S1 | Measurements, parts list, bezel and switch-plate models | the bezel test-prints and the CrowPanel seats on its bosses |
| S2 | Brain in the cavity, battery and power, the CrowPanel as a leaf | the slate boots on battery and shows the chief-of-staff console |
| S3 | I/O controller: calculator matrix, toggles, DIPs, fingerprint | keys answer a survey; a fingerprint touch confirms a gated action |
| S4 | E-ink status panel (a second ADR-117 endpoint) | the agent's status and the next checklist item show on e-ink with the main screen off |
| S5 | Plugin rail and the first modules (LoRa, GPS) as cogs | plugging in the LoRa sleeve shows its guide, installs its cog, and two slates exchange a check-in with no Wi-Fi |

## Open measurements and choices

1. **Clipboard:** inner length, width, and **storage cavity depth**; lid thickness at the
   cut-out.
2. **CrowPanel thickness:** glass to the back of the components (the STEP model has it; a quick
   caliper check).
3. **Calculator:** exact model, so the key matrix can be traced; whether its case is kept or only
   the keypad.
4. **E-ink:** which module goes in the calculator window, the 2.13" HAT or the smaller one.
5. **Brain:** Pi 5 or CM5, chosen by the depth; battery capacity.
6. **Fingerprint sensor:** the model or markings, for the cut-out diameter.
