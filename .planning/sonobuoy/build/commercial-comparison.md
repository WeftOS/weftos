# Sonar Buoy — Commercial Comparison, Visualization, and Gap Analysis

**Status**: Planning. Companion to `requirements.md`, `architecture.md`,
`roadmap.md`.
**Date opened**: 2026-05-11
**Purpose**: Place the clawft buoy fleet in the landscape of commercial
sonar systems, surface the visualization opportunities the WeftOS mesh
opens up, sketch a "Garmin-familiar but data-richer" UI direction, and
do a use-case-by-use-case gap analysis.

**Research-half anchors** (`.planning/sonobuoy/` one level up): the
visualization surfaces in §5 and the ML-classification overlay (§5.9)
ground onto `../SYNTHESIS.md` §2.5 classification head (Perch /
BirdNET-2.3 / Bergler / SurfPerch) and the HNSW per-namespace schema
in `../SYNTHESIS.md` §5.4. The active-imaging visualization (§5.8
bathymetric mosaic) grounds onto `../SYNTHESIS.md` §2.4 active-imaging
branch (ADR-063/064/065 — Hayes-Gough SAS review, Gerg-Monga deep
autofocus, Kiang multistatic SAS with stationary sonobuoy).

---

## 1. What clawft actually is (architectural classification)

Before the comparison, it is worth naming the thing precisely, because
"sonar" in marketing language covers nine different machines that
share almost no design.

clawft v1 is, in commercial-sonar vocabulary:

- a **multistatic** acoustic positioning network — separate TX and RX
  nodes, no monostatic transducer doing both;
- with **omnidirectional point sources** (1.8 kHz audio-band piezo
  discs ≪ wavelength → no beam, no bearing from a single element);
- whose nodes form a **Long-Baseline (LBL)** acoustic geometry —
  multiple anchor nodes at known positions, target inferred from
  pairwise time-of-flight;
- using **two-way travel time (TWTT)** ranging (Sonardyne / Evologics
  pattern) rather than synchronized one-way TDoA, so no shared
  µs-class clock is needed in v1;
- with a **passive-acoustic-monitoring (PAM)** capability free as a
  side-effect (every RX is a hydrophone that can publish raw events);
- streaming via the **WeftOS substrate** — every detection and every
  ranging exchange lands on chain as a signed, time-stamped event,
  and the math runs on a desktop-class host that subscribes to those
  streams.

It is **not** an imaging sonar, a fish-finder, a bottom mapper, a
side-scan, a 360-PPI, or a forward-looking nav sonar. None of those
work with a single omnidirectional 1.8 kHz element per node. We are
in a different family entirely — closer to a hobbyist Sonardyne
Ranger LBL kit cross-pollinated with an Ocean Sonics icListen
hydrophone network, on a mesh-substrate data plane.

### Geometry 4-tuple per phase (added 2026-05-11 by P5)

Per `sonar-systems-engineer` persona §"Geometry classifications", every
sonar has a 4-tuple `(aperture-type, transmit-type, receive-type, scan-
mechanism)`. clawft's evolution across phases:

| Phase | Aperture | TX | RX | Scan |
|-------|----------|-----|-----|------|
| 1, 1b | Sparse-aperture network (3 nodes) | Multistatic | Distributed single-element | Fixed-beam (omni) |
| 2, 3 | Same + in-buoy TDoA | Multistatic | Distributed multi-element | Fixed + per-buoy interferometric |
| 4 | Same + georeferenced | Multistatic | Distributed | Fixed |
| 5a-c | Same + directional imaging tier | Multistatic | Distributed directional | Fixed-down |
| 5d | Same + mechanical-scan on one node | Multistatic | Distributed | Per-node mechanical scan |
| 5e-f | Same + acoustic dome | Multistatic | Distributed omni via dome | Mechanical scan |
| 6 | Many-node sparse aperture | Multistatic | Distributed | Per-node scan + mesh aperture |
| 7 | High-band sparse aperture | Multistatic | Distributed | Per-node scan + mesh aperture + FPGA |

This geometry framing answers most "can clawft do X" questions: look
at what 4-tuple X needs, see whether the phase ladder reaches it.

The strategic question this document is meant to inform is: **what
visualizations and what use cases does this multistatic mesh unlock
that the commercial imaging products cannot do**, and where do we
have to bolt on extra hardware (a downward beam, ultrasonic, etc.)
to cover the use cases people *expect* a "sonar" to do.

---

## 2. Commercial sonar taxonomy

For each class: physics, typical product, primary use, native
visualization, why a user buys it. This is the field we are
comparing against.

### 2.1 Single-beam bottom sounder / "fish finder"

- **Physics**: 50 / 200 kHz monostatic transducer below the hull,
  pulse-echo, narrow conical beam (~20°).
- **Products**: Garmin Striker, Lowrance Hook, Humminbird Helix.
  $100–$500 consumer.
- **Use**: depth under boat, bottom hardness (colorized return),
  fish targets (the famous "arches"), thermocline.
- **Visualization**: scrolling **echogram** — depth (Y) vs. time (X),
  intensity colorized. Right-most column is "now". Bottom is a
  thick contour line; fish are small parabolas above it.
- **Why bought**: cheapest path to "where's the fish, how deep am I".

### 2.2 CHIRP single-beam

- **Physics**: same form factor, but pulse is a frequency-swept
  chirp (e.g., 40–60 kHz, 130–210 kHz). Matched filter at the
  receiver gives finer range resolution and better target
  separation than a fixed-frequency ping.
- **Products**: Garmin GT-series, Lowrance Active Imaging CHIRP,
  Airmar transducers.
- **Visualization**: same echogram, but targets are crisper and
  closely-spaced fish are now distinguishable.
- **Why bought**: upgrade from §2.1; same UI mental model.

> Note: clawft already plans chirp-spread waveforms for ranging
> precision (`architecture.md` §"Modulation v2"). Same DSP idea,
> different geometry — we use chirps for *pairwise TWTT
> precision*, not for monostatic range resolution.
>
> **Canonical references for the matched-filter / pulse-compression
> math supporting both §2.2 CHIRP and clawft v2 chirp-spread
> TWTT**: Turin 1960 (*IRE TIT* 6:311, DOI
> 10.1109/TIT.1960.1057571 — matched-filter foundations); Klauder
> et al. 1960 (*Bell Sys. Tech. J.* 39:745, DOI
> 10.1002/j.1538-7305.1960.tb03942.x — LFM chirp theory); Cook &
> Bernfeld 1967, *Radar Signals: An Introduction to Theory and
> Application*, Academic Press (ISBN 0-12-187650-7 / Artech 1993
> reprint ISBN 0-89006-733-3 — canonical pulse-compression
> textbook). One-page paper-analysis cards for each will land at
> `papers/analysis/{turin-matched-filter,klauder-chirp-radar,
> cook-bernfeld-pulse-compression}.md` per panel P1 §1.10
> (`.planning/symposiums/sonobuoy/panels/P1-acoustic-physics.md`)
> closing the §2.2 P0 paper gap from gap card 02.

### 2.3 Down-imaging / down-scan

- **Physics**: high-frequency (455 / 800 kHz) linear-array
  transducer below the hull, narrow fan beam pointed straight down.
- **Products**: Garmin ClearVü, Lowrance DownScan, Humminbird
  Down Imaging.
- **Use**: photo-like image of the bottom directly under the boat.
- **Visualization**: waterfall image, X = boat track, Y = depth,
  intensity = return strength. Reads almost like a vertical core
  sample under the boat path.
- **Why bought**: see structure (sunken trees, rocks, wrecks)
  under the boat, not just a depth number.

### 2.4 Side-scan / side-imaging

- **Physics**: same idea as §2.3, but the fan beam is rotated 90°
  to point port and starboard. Often 455 / 800 / 1200 kHz.
- **Products**: Humminbird Side Imaging, Garmin SideVü; survey-
  grade towed fish from Klein, EdgeTech, JW Fishers.
- **Use**: scan a wide swath of seafloor as the boat moves. The
  go-to tool for searching for wrecks, dropped gear, bodies.
- **Visualization**: waterfall image, Y = slant range to either
  side, X = along-track. Bottom shows as a vertical "nadir line"
  with the seabed pattern flanking it. Objects cast acoustic
  shadows.
- **Why bought**: search efficiency. One slow pass covers an area
  no down-scan ever could.

### 2.5 Forward-looking sonar (FLS) and "live" sonar

- **Physics**: phased-array transducer, electronically steered
  narrow beam in front of the boat. Some products (Garmin
  LiveScope, Lowrance ActiveTarget, Humminbird MEGA Live) refresh
  fast enough to show fish swimming in near-real-time.
- **Use**: bass-fishing-quality target acquisition; nav-grade
  obstacle avoidance (EchoPilot, FarSounder).
- **Visualization**: a 2D forward-looking *scope* — slant range on
  one axis, bearing on the other — refreshed at video rate. Looks
  like a sonar UI in a movie.
- **Why bought**: actually *see* fish before casting; or, on bigger
  boats, see the rock you're about to hit.

### 2.6 360° imaging sonar

- **Physics**: mechanically rotating or fully phased-array
  transducer below the boat. One full sweep per second-ish.
- **Products**: Garmin Panoptix LiveScope Plus (omni mode),
  Humminbird MEGA 360, Lowrance ActiveImaging 360, BlueView P900.
- **Use**: situational awareness — what is around me, not just
  under or in front.
- **Visualization**: **PPI (Plan Position Indicator)** — the
  classic radar-style top-down circular sweep, boat at center,
  returns painted in polar coordinates.
- **Why bought**: spatial awareness, structure mapping, search.

### 2.7 Multibeam echosounder (survey grade)

- **Physics**: hull-mounted projector emits a wide cross-track fan;
  receiver is an orthogonal hydrophone array; beam-forming yields
  hundreds of simultaneous beams across-track.
- **Products**: Kongsberg EM-series, Reson SeaBat, Norbit iWBMS,
  R2Sonic. $50k–$1M+.
- **Use**: bathymetric mapping at survey resolution; harbor
  surveys, dredge monitoring, hydrography.
- **Visualization**: dense bathymetric point cloud, gridded into a
  DEM, draped on a chart. Often shown as a colored 3D surface
  with hillshade.
- **Why bought**: producing charts you can sell to mariners.

### 2.8 Synthetic Aperture Sonar (SAS)

- **Physics**: tow body or AUV moves a small array; coherent
  combination of returns over a long synthetic aperture gives
  centimeter-resolution imagery, often at hundreds of meters
  altitude above bottom.
- **Products**: Kraken AquaPix MINSAS, EdgeTech, ATLAS.
- **Use**: mine countermeasures, archaeology, pipeline inspection.
- **Visualization**: photographic-quality grayscale imagery of
  the seabed.

### 2.9 LBL / USBL acoustic positioning

This is the family clawft actually belongs to.

- **LBL (Long Baseline)**: several transponders at known seabed
  positions; an AUV or ROV interrogates them, computes its
  position from round-trip times. Sub-meter accuracy at km ranges
  is routine.
- **USBL (Ultra-Short Baseline)**: one hull-mounted transducer
  array (elements 10s of cm apart) interrogates a single
  transponder on the target. Computes range from time-of-flight
  and bearing from phase across the array.
- **Products** (full vendor anchor list per P5 §1.9):
  - **Sonardyne**: Ranger 2 / Fusion 2 / Mini-Ranger 2 ($50k-
    $300k per system `[NEEDS-VERIFY]`); SPRINT-Nav INS+DVL+USBL
    ($80k-$200k `[NEEDS-VERIFY]`).
  - **iXblue**: GAPS M-series ($50k-$250k `[NEEDS-VERIFY]`).
  - **Evologics**: S2C R-series acoustic modem with ranging
    ($8k-$25k per node `[VERIFIED]`) — **closest commercial peer
    to clawft on per-node cost**.
  - **Blueprint Subsea**: SeaTrac X-series ($5k-$15k `[VERIFIED]`).
  - **Tritech**: MicronNav ($10k-$25k `[VERIFIED]`).
  - **LinkQuest**: TrackLink ($10k-$30k `[NEEDS-VERIFY]`).
  - **WHOI Micro-Modem** — research-grade reference design.
- **Use**: positioning AUVs, ROVs, divers; mooring anchor
  monitoring; subsea construction metrology.
- **Visualization**: usually a chart overlay — a "puck" on a map
  showing the tracked object's position, with a confidence ellipse;
  plus engineering panels for SNR, raw ranges, geometry health.
- **clawft alignment**: identical geometry family at **$50-$120/
  node** — **100-500× cheaper per node** than the cheapest
  commercial peer (Blueprint SeaTrac X110 at $5k). The architecture
  is structurally different (mesh of cheap nodes vs centralized
  transponder + interrogator pair) but the geometry 4-tuple is
  the same: multistatic distributed-element. See P5 §6.1 for the
  cost-parity analysis.

### 2.10 Passive acoustic monitoring (PAM)

- **Physics**: hydrophone(s), no transmit. Capture, classify,
  archive. Calibrated sensitivity in dB re 1 V/µPa is the price
  discriminator — uncal hobby SoundTrap is $2k, factory-calibrated
  is $6k `[VERIFIED]`.
- **Products** (full vendor anchor list per P5 §1.10):
  - **Wildlife Acoustics**: SoundTrap ST600 STD / HF / 4300 HF
    ($2k-$6k `[VERIFIED]`); Song Meter SM4 / Mini ($1k-$3k
    `[VERIFIED]`).
  - **Ocean Sonics**: icListen HF / LF / smart hydrophone
    ($4k-$15k `[VERIFIED]`).
  - **Loggerhead Instruments**: DSG Ocean / DSG-ST ($3k-$8k
    `[VERIFIED]`).
  - **JASCO**: AMAR G4 / OceanObserver ($10k-$50k `[NEEDS-VERIFY]`).
  - **Research reference**: HARP (Wiggins-Hildebrand 2007 per
    `papers/analysis/wiggins-harp.md`), Cornell BRP / MARU.
- **Use**: marine-mammal monitoring, ship traffic, ambient noise,
  fishery and regulatory studies.
- **Visualization**: long-term **spectrograms** (frequency vs. time,
  intensity colorized) plus detection event lists with species or
  class tags.

### 2.11 Marine radar (for the UI lessons, not the physics)

The user's prompt explicitly says "look at commercial radar
systems". Marine radars (Furuno DRS-A / DRS-NXT, Raymarine
Quantum 2 Doppler, Garmin GMR / Fantom) are not sonar, but their
UI conventions are *exactly* the mental model boaters bring to
any "puck on a screen" display.

- **Visualization**: PPI sweep, boat-up or north-up, range rings,
  bearing line, MARPA targets, AIS overlay, guard zones, EBL/VRM
  (electronic bearing line / variable range marker).
- **Chart-plotter integration**: radar overlay on the chart with
  range-correct alignment.
- **Why this matters for clawft**: the *mesh* visualization we
  want is fundamentally a PPI / chart-overlay metaphor, not a
  fish-finder echogram metaphor. Users already know how to read
  a PPI.

### 2.12 Acoustic modem (data-link, adjacent class — added 2026-05-11 by P5 §1.12)

- **Physics**: phase-coherent acoustic carrier with chirp-spread
  or PSK modulation for data transfer; not strictly sonar but
  shares the underwater acoustic-channel substrate.
- **Products**:
  - **Evologics**: S2C M-series / Pro ($5k-$30k per node
    `[VERIFIED]`); the S2C R-series above (§2.9) does ranging +
    modem combined.
  - **Teledyne Benthos**: ATM-9XX series ($10k-$30k per node
    `[NEEDS-VERIFY]`).
  - **LinkQuest**: UWM-2000 ($10k-$20k `[NEEDS-VERIFY]`).
  - **WHOI Micro-Modem-2** — research reference design.
- **Use**: AUV mission data uplink, ROV telemetry, subsea
  sensor-network bulk data.
- **clawft alignment**: clawft's chirp-payload encoding (ADR-084
  Rule 1 + P3 §2.2) is a **time-sequencing primitive with embedded
  payload**; it is **not** a modem per se but covers the data-link
  half of the modem product space at hobby price. Net 50 bps at
  1.8 kHz (P4 §4.1.4) vs S2C's ~5 kbps at similar carrier — clawft
  is **~100× slower at ~100× cheaper per node**. Light-telemetry
  use cases only; bulk subsea data is wrong tool.

---

## 3. Capability matrix — clawft v1/v2 vs commercial classes

Rows are commercial classes from §2; columns are dimensions where
clawft either competes, complements, or is silent. "v1" = pool
fleet from `requirements.md`. "v4" = open-water fleet from
`roadmap.md` Phase 4 (GPS + LoRa + solar). "v5" = multi-band
(roadmap Phase 5; adds 40 kHz). "v6" = N-buoy fleet (Phase 6).

| Class                  | clawft v1 | clawft v4 | clawft v5 | clawft v6 | Commentary |
|------------------------|-----------|-----------|-----------|-----------|------------|
| Single-beam (§2.1)     | no        | partial   | yes       | yes       | A 40 kHz down-ping in v5 gives depth-under-buoy at each node. v6 with N nodes gives sparse area depth coverage no single boat does. Phase 5b/c with modern Airmar transducer at $400/buoy is competitive with Garmin Striker $99 per node at hobby parity. |
| CHIRP (§2.2)           | partial   | partial   | partial   | partial   | We chirp for TWTT precision (architecture.md v2 modulation). Same DSP, different geometry; not a CHIRP fishfinder. Phase 5b/c gives true CHIRP fish-finder capability via the imaging-tier transducer. |
| Down-imaging (§2.3)    | no        | no        | partial   | partial   | Single 40 kHz element is not an array; no imagery, only depth. Closing this needs a real array. Phase 5d gimbal scanned-down approximates the view at lower refresh rate. |
| Side-scan (§2.4)       | no        | no        | no        | partial   | Phase 6 mesh-side-scan at Phase 7 hardware gives **multi-buoy** strip — wider area, different geometry; structurally different image. Per-ping resolution lower than commercial towed fish. |
| FLS / live (§2.5)      | no        | no        | no        | no        | Needs a phased array. Out of family. Phase 5d mechanical gimbal at 1 Hz refresh **does not match** Garmin LiveScope 60 fps. |
| 360° imaging (§2.6)    | no        | no        | partial   | yes       | Phase 5d gimballed mechanical-scan matches Tritech Micron / BlueView P900 / Imagenex 881A geometry at **0.05× cost ratio**. A *fleet of buoys* can produce a 360 PPI of acoustic contacts at the mesh scale, not the boat scale — different product. |
| Multibeam (§2.7)       | no        | no        | no        | partial   | Phase 6 mesh-mosaic at sparse-aperture compounding gives the **temporal change-detection** product commercial doesn't have. Per-ping resolution gap vs. R2Sonic / Norbit unclosed. |
| SAS (§2.8)             | no        | no        | no        | partial   | Phase 6 multistatic with Tier-1 anchor as TX + mesh as C-node array literally matches Kiang-Kiang 2022 geometry. Coherent SAS image quality requires Phase 7+ clock budget. |
| **LBL/USBL (§2.9)**    | **yes**   | **yes**   | **yes**   | **yes**   | **This is the family.** v1 is a pool-grade LBL. v4 adds GPS anchoring → georeferenced LBL. **100× cheaper per node than Sonardyne / Evologics / Blueprint** at parity geometry. |
| **PAM (§2.10)**        | **yes**   | **yes**   | **yes**   | **yes**   | Every RX is a hydrophone. Free side-effect of the architecture. Calibration to dB re 1 V/µPa is the v2+ closure with per-buoy bench cal (~$400 amortized fleet-wide). |
| Marine radar (§2.11)   | UI only   | UI only   | UI only   | UI only   | Borrow the *interface idiom*, not the physics. |
| Acoustic modem (§1.12 P5) | partial 50 bps | partial | partial | partial | Chirp-payload encoding gives ~50 bps net at 1.8 kHz vs Evologics S2C ~5 kbps. **100× slower at ~100× cheaper**. Light-telemetry use cases only. |
| **Bearing recovery without compass (Tier-1 4-face)** | no | partial (single Tier-1) | yes (Tier-1 mature) | yes (multi-anchor cross-validation) | NEW row from P4 §4.4 + ADR-084 4-face arch. Uniquely-clawft — no commercial product ships this transmitter geometry. σ_θ ≈ 3-5° at σ_SPL = 1 dB matches MEMS compass in clean conditions, dominates in any ferrous-corrupted environment. |
| **Distributed multispectral tomography** | no | no | partial (single wavelength) | yes (8-wavelength) | NEW row from ADR-084 §"Multi-wavelength". WET Labs ECO-FL is single-point at $2-10k; clawft is distributed and on chain. |
| **Joint self-calibration (position+clock+sensitivity+SSP)** | partial (pool TWTT) | partial (georef) | yes (calibration as side-effect) | yes (over-determined ~9×) | NEW row from P4 §4.8 + ADR-083. Sonardyne charges separately for calibration deployments; clawft does it as free side-effect. |
| **Cross-deployment vector-DB queries** | yes (records start) | yes | yes (rich queries) | yes (dominant) | NEW row from P4 §4.7 + `phase-economics.md` §4. No marine vendor sells this; closest analogs are observability products (Splunk, Datadog) in a different industry. |
| **Multistatic SAS at hobby scale (Kiang-2022 geometry)** | no | no | partial (incoherent) | partial (geometry; coherence requires Phase 7 clock) | NEW row from P4 §4.11.3 + `papers/analysis/multistatic-sas.md`. Kiang 2022 explicitly models a stationary sonobuoy as a C-node — clawft mesh is the C-node array. |

Two cells in bold: that is the deliverable category. Everything
else is either a partial side-benefit, a NEW capability where no
commercial competitor exists, or a deliberate out-of-scope. The
five NEW rows (added 2026-05-11 by P4 + P5) are capabilities no
commercial product offers at any price.

---

## 4. The unique-to-us capability: layered sensor mesh on a chain

Commercial sonars are **single-perspective devices**: one boat,
one transducer head, one screen, one moment. Even multibeam
multibeams from one boat.

clawft inverts this. We have **N nodes, each independently
streaming events on a signed, ordered chain**, and a desktop-class
host that consumes those streams into world-frame visualizations.

This is the architectural advantage we should hammer in the UI.
The chain is not an implementation detail; it is the product
feature. Every visualization layer can be:

- **time-scrubbable** — slider goes anywhere in recorded history;
- **per-buoy filterable** — toggle each node in or out;
- **stream-typed and layered** — presence, TWTT distances, raw
  acoustic events, derived positions, environmental, passive
  classification, health/telemetry; layers stack like map layers;
- **provenance-checkable** — every event is signed; the UI can
  show which node produced which datum, with what battery state
  and water temperature it was produced under.

No Garmin display can do any of this. The chain is the moat.

---

## 5. Visualization possibilities

Concrete UI surfaces to build in the WeftOS desktop, in roughly
the order they should appear.

### 5.1 Pool / chart view (top-down, world-frame)

The canonical primary surface. PPI / chart-plotter idiom.

- **Stage**: pool rectangle (Phase 1–3) or chart background
  (Phase 4+, GPS-anchored).
- **Buoy glyphs**: one per active buoy, colored by health
  (battery, last-heartbeat age). Glyph contains node id and is
  surrounded by its position-estimate confidence ellipse.
- **Range rings**: drawn from each selected buoy in meters; user
  toggles "anchor buoy" the way a radar user toggles EBL origin.
- **Contact glyphs**: acoustic detections rendered as polar
  rays *from each buoy* (range and arrival time), color-coded by
  SNR. Intersections of rays from multiple buoys are the
  trilateration hypothesis.
- **Layer toggles** (see §5.6).
- **Time slider** along the bottom: scrub to any point in chain
  history. Live position auto-resumes when slider is at "now".

This view is the analogue of a chart-plotter with radar overlay,
not of a fishfinder echogram. That choice is deliberate — the
mesh produces a top-down spatial product, not a depth-vs-time
product.

### 5.2 Per-buoy PPI view (single-node-centered)

Pick any buoy, center the display on it, draw all detections that
buoy has heard as polar rays in slant range. Visually identical to
a Garmin 360 PPI but the data underneath is the chain stream from
that one node.

This is the surface that will look most familiar to a Garmin /
Furuno user, and that is exactly its purpose — onboarding.

### 5.3 3D water-column view

The pool / lake / harbor as a transparent volume.

- Surface plane with buoy positions floating on it.
- Vertical "drop lines" from each buoy down to its hydrophone
  depth, terminating in an RX-disc icon.
- Water temperature rendered as a vertical color gradient on each
  drop line (we already collect it per RX event).
- Acoustic contacts rendered as 3D points with covariance
  ellipsoids; emitter trajectories as fading polylines.
- Pool walls / bathymetry mesh (Phase 5+ when 40 kHz down-pings
  give us coarse depth grids).

This is the surface where the "mesh advantage" is most legible:
seven seven streams contributing to one spatial picture.

### 5.4 Per-buoy spectrogram (PAM-style)

For each RX, a long-time spectrogram of the raw analog band,
streamed live and scrollable back. This is what an Ocean Sonics
or Wildlife Acoustics user expects. Detections from the matched
filter are overlaid as labeled brackets.

### 5.5 Garmin-mimic dashboard ("familiarity mode")

A composite surface that visually mimics what a Garmin/Lowrance
combo unit shows on the helm — depth, temp, position, contact
list, alerts — but every panel is sourced from chain streams
instead of a vendor NMEA bus.

Purpose: the boater opening clawft for the first time should be
able to read the screen without a manual. Underneath every panel
is a "show me the chain" button that drops into the actual
event view, which is what differentiates us.

Panels to mimic:

| Garmin panel        | clawft source                                    |
|---------------------|--------------------------------------------------|
| Depth               | 40 kHz down-ping per buoy (v5)                   |
| Water temp          | DS18B20 per RX (v1 already)                      |
| GPS fix             | u-blox per surface buoy (v4)                     |
| Sonar contacts list | `acoustic.event` stream filtered by SNR + decode |
| Range / bearing     | `acoustic.position` and per-buoy `rx_id` TDoA     |
| Battery / health    | Buoy telemetry events                            |
| AIS overlay         | (deferred; AIS receivers are out of scope)       |

### 5.6 Layer stack (the cross-cutting feature)

Every surface above is a *projection* over the same underlying
chain stream set. Layers are toggleable per surface:

1. **Buoy positions** (the anchor mesh)
2. **TWTT distances** (live pairwise edges between nodes)
3. **`acoustic.event` detections** (per-buoy polar rays)
4. **Derived `acoustic.position` contacts** (trilaterated targets)
5. **Environmental** (water temp color field, computed sound speed)
6. **Passive classification** (PAM tags: boat noise, biological,
   etc.) — Phase 5+ aspirational
7. **Health / telemetry** (battery, SNR, packet loss)
8. **Replay overlay** (ghost of a prior time slice for comparison)

A surface composes layers; a layer composes events. Both are
addressable from the WeftOS substrate, so visualizations are
declarative views over chain history. That makes screenshots
reproducible from chain replay, which is a property no boat
display has.

### 5.7 "Storyboard" / forensic mode

Long-form replay surface. Given a time window, render an event
timeline (events, TWTT exchanges, position estimates), a chart
view at the slider's "now", and a transcript panel. Designed for
post-deployment review — "what did the fleet hear between 14:32
and 14:36, and where was each contact estimated to be?"

This is the surface that justifies the chain. Nothing in
consumer marine electronics looks like this; it is closer to a
seismograph network's event-review tool, or to a network-IDS
forensic console.

### 5.8 Persistent bathymetric mosaic (P79 + gimbal + GPS)

Once Phase 5d (gimbal) and Phase 4 (GPS) land, every active ping
contributes a 3D depth sample to a persistent mosaic. Surface
renders:

- Top-down false-color depth map
- 3D point cloud with hillshade
- Difference overlay between any two deployment epochs
  ("what's changed since last week")
- Confidence layer (sample density per cell)

This is the visualization product that competes directly with
multibeam survey bathymetry, at a fraction of the per-node cost.
See `phase-economics.md` §5.5–5.6 for the data-plane discussion.

### 5.9 ML classification overlay

Every classifier trained on the vector DB (`acoustic.species`,
`acoustic.vessel`, `acoustic.bottom_type`, etc., per
`phase-economics.md` §5.4) becomes a toggleable visualization
layer: contact glyphs annotated with predicted class + confidence,
optionally filtered to show only one class at a time.

This is the "tell me which dot is a fish" UX that consumer
fishfinders gesture at with simplistic target classification, but
which becomes a serious feature once classifiers are trained on
thousands of hours of mesh-collected vectors.

### 5.10 Temporal animation / time-lapse

Any layer over any deployment window can be rendered as
time-lapse video — playable, scrubbable, exportable. Particularly
valuable for:

- Drift-track animations (Lagrangian current visualization)
- Fish-school trajectory animations (per `phase-economics.md` §5.3)
- Bathymetric-change animations across multiple deployments
- Ambient-noise rolling spectrograms

### 5.11 3D + AR fly-through

For the operator on a boat, or the researcher at a desk, with a
tablet or WebXR-capable headset:

- Live 3D view of the seabed beneath the boat
- Persistent fish-track trails from prior deployments overlaid
- ML-labeled contacts annotated in 3D space
- Current weather, water temperature, sound-speed profile fused in
- Past bathymetric mosaic as a textured surface; live pings update
  it in place

This is the visualization that converts the "data plane advantage"
from `phase-economics.md` §4 into an artifact a non-technical user
can use. No commercial marine electronics product has this surface
because no commercial vendor has the open vector-DB-backed data
plane underneath it.

---

## 6. Interface design notes — "Garmin-familiar, data-richer"

The user explicitly raised this trade-off. Make it explicit in
the spec.

### 6.1 Familiarity, not mimicry

We borrow Garmin's *idioms*, not its art direction:

- **PPI sweep affordance** for the per-buoy view (§5.2).
- **Range rings + EBL/VRM** in the chart view (§5.1).
- **Echogram-style "history strip"** along the bottom of any
  surface to scrub time.
- **MOB-style large alert button** (in any deployment surface).
- **Boat-up vs. North-up toggle** in chart-style surfaces.
- **Tap-to-pin** a contact, with persistent label + range/bearing
  panel.

We do *not* attempt to clone Garmin's exact pixel layout. That
path is brittle and trade-dress-risky. The mental model is what
transfers; the chrome is ours.

### 6.2 Information density: progressive disclosure

Default surface should look as clean as a Garmin Striker. Power
user invokes "engineering mode" (a hotkey, like Wireshark's
follow-stream) and the chain provenance appears: signed event
ids, SNR per detection, raw matched-filter peak, sound-speed
value used, contributing TWTT exchanges. This is the data-rich
half of the design.

Rule of thumb: the boater sees nothing they would not see on a
Garmin. The researcher sees everything, one keystroke away.

### 6.3 Night mode, daylight mode, color-blind safe

Marine displays live in two visual environments — bright sun
through polarized sunglasses, and pitch-dark wheelhouse. Any UI
that ships needs both. The WeftOS surfaces already theme via
DESIGN.md tokens; the sonar surfaces will pick high-contrast
palettes and confirm legibility with the existing `weftos-design`
audit scripts.

### 6.4 Latency budget

Garmin LiveScope refreshes at video rates (~60 fps). Our chain
event cadence is ~1 Hz per ranging exchange and per detection.
The UI must not pretend otherwise — it should show "live" data
crisply (sub-second from event to render) but should never
extrapolate or animate motion between events. Honest latency is
a feature for forensic / research use; pretend latency is a
liability.

---

## 7. Feature analysis — what we have, what we are committing to

Inputs we know we will have on chain, by phase, from the existing
plan docs:

| Datum                                   | Phase first available | Stream                |
|-----------------------------------------|-----------------------|-----------------------|
| Per-buoy presence detection             | 1b                    | `acoustic.event`      |
| SNR + leading-edge timing per detection | 1                     | `acoustic.event`      |
| Water temperature at RX depth           | 1                     | `acoustic.event`      |
| Battery voltage                         | 1                     | `acoustic.event`      |
| Decoded MFSK frame payload              | 1                     | `acoustic.event`      |
| TWTT pairwise distance                  | 2                     | `acoustic.twtt`       |
| Trilaterated buoy position              | 2                     | `acoustic.position`   |
| Per-buoy in-buoy TDoA bearing           | 3                     | `acoustic.event` × N  |
| GPS lat/lon per surface buoy            | 4                     | (new) `nav.gps`       |
| 40 kHz down-ping depth                  | 5                     | (new) `acoustic.depth`|
| Passive classification label            | 5+                    | (new) `acoustic.tag`  |
| TDMA-scheduled fleet state              | 6                     | (new) `mesh.schedule` |

Every UI surface from §5 reduces to "subscribe to some subset of
these and project them". That symmetry is exactly the architectural
property to preserve.

---

## 8. Gap analysis — where commercial wins, where we win, what closes the gap

Group by use case. For each: what a commercial buyer expects, what
clawft can do today/soon, the gap, and the smallest plausible move
to close it.

### 8.1 Use case: "How deep is the water under me?"

- **Commercial**: §2.1/§2.2 single-beam, $100-$500 device on a boat.
- **clawft v1–v4**: no native answer; the 1.8 kHz disc is
  omnidirectional and is being used for ranging, not for vertical
  depth.
- **clawft v5+**: **closed at Phase 5b/c.** Modern Airmar B45 / B260
  transducer at $400/buoy gives true CHIRP fish-finder capability
  per `phase-economics.md` §2 Phase 5b/c. P1 §1.1.6 Mode A worked
  budget: SE = +106 dB at pool floor 235 kHz, **range 200-300 m in
  open water** before absorption dominates.
- **Cost ratio**: $400/buoy vs Garmin Striker $99 = **4× more per
  node**; at fleet scale (3-10 buoy mesh) **clawft delivers depth-
  under-buoy at every node** vs Garmin at one location, so the
  effective ratio at coverage parity is ~1×.
- **Gap**: closed at Phase 5b/c.

### 8.2 Use case: "Are there fish below me?"

- **Commercial**: §2.1/§2.2 again, with target arches.
- **clawft v5+**: covered at Phase 5b/c as a **side-effect** of the
  imaging-tier infrastructure. Not pursued as a primary product —
  Garmin sells this for $99 and bass-tournament-grade target
  acquisition is a different market. **Recommendation: pursue
  fish-finder UX as a Garmin-mimic dashboard side-effect, not as
  a competing product.**

### 8.3 Use case: "What does the bottom look like (imagery)?"

- **Commercial**: §2.3/§2.4 down-imaging / side-imaging at
  455–1200 kHz.
- **clawft v5d+**: **partial closure.** Phase 5d gimbal scanned-
  down approximates down-imaging at lower refresh rate. Phase 6
  fleet mesh-side-scan at Phase 7 hardware delivers multi-buoy
  side-scan strip — different geometry, different image.
- **Gap**: per-ping resolution lower than commercial towed fish.
  Per-survey-area coverage from a $35k Phase 7 fleet vs $30k
  single Klein 3000 is roughly **1.2×** at fleet level, with
  the fleet covering multi-node compound vs single-pass.
- **Closer**: Phase 7+ with HV pulser, AFE5832 receive, 500 kHz
  Imagenex transducer per `phase-economics.md` §2 Phase 7. ~$2k-
  $7k per node; commercial-parity at degraded per-ping resolution.

### 8.4 Use case: "What is in front of me right now?" (FLS)

- **Commercial**: §2.5 Garmin LiveScope ($1.5k), FarSounder Argos
  ($30k-$150k).
- **clawft**: **partial closure at Phase 5d (mechanical gimbal)**;
  **structural gap at hobby budget** for live phased-array refresh.
- **Why structural**: Garmin LiveScope's 60 fps refresh requires
  a phased array; mechanical scanners are intrinsically frame-
  rate-limited at ~1 Hz. clawft's geometry choice (per-buoy
  mechanical scan + mesh-aperture) is the right choice for **area
  coverage**, not single-vessel live imaging.
- **Closer**: not pursued. Phased-array LiveScope clone is
  out-of-family per the project mission.

### 8.5 Use case: "Where is the AUV / ROV / diver?" (the LBL win)

- **Commercial**: §2.9 Sonardyne Ranger ($50k-$300k), iXblue GAPS
  ($50k-$250k), Evologics S2C R-series ($8k-$25k per node),
  Blueprint SeaTrac X-series ($5k-$15k).
- **clawft v2**: pool-grade 3-buoy LBL with TWTT and shore-side
  trilateration. **This is our home turf.**
- **clawft v4**: GPS-anchored LBL in open water with LoRa
  backhaul.
- **clawft v6**: fleet-grade LBL with 10-25 buoys, joint solver
  over-determined ~9× per P4 §4.8.3, Tier-1 4-face anchor for
  bearing-from-amplitude-ratio (P4 §4.4).
- **Cost ratio per node**: $60/node (Phase 1b) vs $8k Evologics =
  **0.0075×, 133× cheaper**. Phase 6 fleet **$6.1k** (10 buoys
  + 2 Tier-1 anchors per `phase-economics.md` §2; canonical
  number after Deliverable 2 §3 coherence pass) vs Sonardyne
  $50k-$300k system = **0.02× – 0.12×, 8× to 50× cheaper at
  system level**.
- **Gap to commercial**:
  - *Precision*: commercial 1–10 cm at km ranges; ours ~30 cm in
    a pool with chirp-spread, projected. Phase 7+ with coherent
    clock (CSAC or chip-scale) can close to ~cm at the cost of
    $3k/node CSAC line per RANGING.md §3.
  - *Range*: commercial works to several km in real water; ours
    will be tens to hundreds of meters at 1.8 kHz with hobby
    transducers; the Fonseca-Alves Tier-1 anchor at SL ≈ 132 dB
    reaches 20+ km in low-band per `papers/projector-mma/analysis/
    fonseca-alves-2012-rigid-plate.md` range plausibility check.
  - *Target hardware*: we range *between buoys*; commercial
    ranges to a *separate transponder on an AUV*. We need an
    "underwater transponder mode" buoy stripped down for AUV
    mounting if we want to pursue this seriously.
  - *Sound-speed calibration*: commercial uses CTD profiles or
    in-situ probes at depth. Ours **derives SSP EOFs from the
    fleet's own chirp data** per ADR-083 + `papers/analysis/
    ssp-from-ranging.md` Cornuelle 1999 — a free side-effect of
    joint inference; commercial sells it as a separate cal service.
- **Closer (small)**: add an AUV-mount transponder buoy variant
  in Phase 4 or 5 — same firmware, smaller form factor, no WiFi
  mast, just acoustic. **NEW CAPABILITY** from Tier-1 4-face
  arch: bearing-from-amplitude-ratio gives USBL-equivalent
  bearing with no compass, σ_θ ≈ 3-5° per P4 §4.4.3.
- **Closer (large)**: add multi-depth temperature probes per
  buoy and a sound-speed model that integrates the column;
  optionally upgrade to a small CTD module. The joint solver
  closes this automatically once Phase 4+ data is on chain.

**Verdict**: clawft **dominates** at the LBL/USBL category at
cost-per-node and **introduces new mesh geometries** that no
commercial vendor ships at any price (Tier-1 4-face bearing, mesh
sparse-aperture multistatic).

### 8.6 Use case: "What's swimming / driving by acoustically?" (PAM)

- **Commercial**: §2.10 Ocean Sonics, Wildlife Acoustics; $1k–$30k.
- **clawft**: every RX is already a hydrophone; we can stream
  raw or decimated audio events to chain at modest rates.
- **Gap**:
  - *Hydrophone sensitivity*: bare piezo is far below commercial
    spec. The JFET source-follower hydrophone (`build-hydrophone-
    epoxy.md`, Phase 1b) closes most of it for hobby work.
  - *Classifier*: we have none. Commercial ships with at least
    species-coarse classifiers (e.g., baleen vs. odontocete) or
    ship-noise vs. ambient.
  - *Calibration*: commercial hydrophones are calibrated in
    dB re 1 µPa; ours are not.
- **Closer (small)**: stream decimated audio plus a Goertzel-bank
  feature vector to chain; build a desktop-side classifier as a
  WeftOS service. Free side-effect of the architecture.
- **Closer (large)**: per-buoy hydrophone calibration, traceable
  to a reference projector. Doable but lab-intensive.

### 8.7 Use case: "I want a picture I can read like a Garmin"

- **Commercial**: §2.1–§2.6 every consumer marine product.
- **clawft**: §5.5 Garmin-mimic dashboard.
- **Gap**: zero, *as long as we ship the surface*. The data exists.
- **Closer**: prioritize §5.5 in Phase 2 alongside the chart view
  in §5.1. Reuse the WeftOS design tokens from DESIGN.md.

### 8.8 Use case: "I want to replay what happened"

- **Commercial**: limited; Garmin lets you scrub the last hour
  of sonar history if you remembered to record. Survey gear
  records to proprietary formats and ships with vendor replay
  tools.
- **clawft**: native. The chain *is* the recording. Surface
  §5.7 makes it legible.
- **Gap**: we are ahead. No close required.
- **Differentiator**: this is the headline demo — "play back the
  fleet from any moment, signed and verifiable".

### 8.9 Use case: "I want regulatory-grade certification"

- **Commercial**: §2.7 multibeam vendors have IHO survey
  certifications; PAM gear has calibration certificates.
- **clawft**: none. Hobby project today.
- **Gap**: large but irrelevant for the current users.
- **Closer**: not now. Acknowledge it openly in product
  positioning.

### 8.10 Cost-parity summary table (added 2026-05-11 by P5)

Quantified cost ratios per use case. clawft per-node BOM at the
phase that delivers the capability vs cheapest commercial single-
vessel unit. Phases from `phase-economics.md` Table 2. Commercial
prices from §2 with `[VERIFIED]` / `[NEEDS-VERIFY]` confidence.

| Use case | Commercial cheapest | clawft phase | clawft per-node | Cost ratio | Verdict |
|----------|---------------------|---------------|------------------|------------|---------|
| LBL/USBL hobby | Blueprint SeaTrac $5k `[VERIFIED]` | 1b ($60) / 4 ($115) | $60-$115 | **0.012× – 0.023×** | **dominant** |
| LBL/USBL commercial | Sonardyne $50k+ `[NEEDS-VERIFY]` | 6 fleet $6.1k | n/a | **0.12×** | **dominant** |
| PAM hobby uncal | SoundTrap $2k `[VERIFIED]` | 1b ($60) | $60 | **0.03×** | dominant on hw; calibration gap |
| PAM calibrated | Ocean Sonics icListen $6k `[VERIFIED]` | 1b + cal phase ~$120 | $120 | **0.02×** | dominant with cal investment |
| Single-beam | Garmin $99 `[VERIFIED]` | 5a ($255) | $255 | 2.6× per node | expensive per node; equal at fleet scale |
| CHIRP fish-finder | Lowrance $499 `[VERIFIED]` | 5b/c ($400) | $400 | 0.8× | competitive |
| Down-imaging | Garmin GT54 $700 `[VERIFIED]` | 6 fleet | $6.1k | 8.7× | fleet wins; per-node loses |
| Side-scan consumer | Humminbird SI+ $1.5k `[VERIFIED]` | 7 ($2k+) | $2k+ | 1.3× | competitive; fleet covers more area |
| Side-scan survey | Klein 3000 $30k+ `[NEEDS-VERIFY]` | 7 fleet $35k | n/a | 1.2× | competitive fleet for area coverage |
| FLS consumer | Garmin LiveScope $1.5k `[VERIFIED]` | 5d ($450) | $450 | 0.3× | **structurally different product** |
| 360° hobby | Humminbird MEGA 360 $2k `[VERIFIED]` | 5d ($450) | $450 | **0.23×** | **dominant** |
| 360° commercial | BlueView P900 $10k `[NEEDS-VERIFY]` | 5d-f ($500) | $500 | **0.05×** | **dominant** |
| Multibeam survey | R2Sonic $80k `[NEEDS-VERIFY]` | 6 fleet $6.1k | n/a | **0.08×** | dominant at compounding; per-ping gap |
| SAS | Kraken AquaPix $200k `[NEEDS-VERIFY]` | 7 fleet $35k | n/a | **0.18×** | dominant at degraded per-ping; new geometry |
| Acoustic modem | Evologics S2C $8k `[VERIFIED]` | 1b ($60) | $60 | **0.0075×** | dominant on cost; ~100× slower |

> **Canonical Phase 6 fleet cost = $6,072 ≈ $6.1k** (10 buoys × $580
> base + 2 Tier-1 anchors × $136 add-on). Older "$5.8k"
> formulations omitted the Tier-1 anchor add-on and are
> superseded after Deliverable 2 §3 coherence-pass; see
> `phase-economics.md` §2 Phase 6 for the BOM derivation.

The table's punchline: **clawft is structurally cheaper at every
use case where the underlying geometry is multistatic / distributed
/ passive**, and **structurally tied or more expensive** at use
cases where commercial sells a single-vessel phased-array or
dense-array product (live FLS refresh, multibeam per-ping
resolution). Cross-reference: P5 §6 of `.planning/symposiums/
sonobuoy/panels/P5-commercial-parity.md` for the full per-use-case
analysis.

---

## 9. Strategic positioning summary

clawft is **not** a cheaper Garmin. Pitching it that way loses
on every axis except price.

clawft is **a distributed acoustic mesh** whose product is the
*world model assembled from many cheap nodes streaming signed
events into a desktop-class consumer*. The closest commercial
analogues are LBL acoustic positioning (§2.9) and PAM (§2.10),
and the closest UI analogues are the chart-plotter / PPI idioms
of marine radar (§2.11). The fish-finder family (§2.1–§2.6) is
an adjacent product space we should respectfully not pretend to
compete in.

The wins to lean into:

1. **Mesh-native spatial view** — top-down chart with N buoy
   anchors, contacts as ray intersections, layers per stream.
2. **Time-scrub on a signed chain** — anyone can replay any
   moment from any node, with provenance.
3. **Hydrophone-array-for-free PAM** — every RX is a passive
   listener; the data plane already supports it.
4. **Open data, vendor-independent** — events are substrate
   records, not a sealed NMEA-2000 black box.
5. **Cheap nodes, expensive intelligence** — $50-ish BOM per
   buoy, all the cleverness in shore-side WeftOS services.
6. **Bearing without compass** (Tier-1 4-face anchor) — uniquely
   clawft per P4 §4.4; σ_θ ≈ 3-5° at σ_SPL = 1 dB; matches MEMS
   compass in clean conditions and dominates in any ferrous-
   corrupted environment. No commercial product ships this
   transmitter geometry.
7. **Joint self-calibration as side-effect** — position + clock +
   sensitivity + SSP all fall out of the same joint solver per
   ADR-083 + P4 §4.8. Commercial systems sell calibration as a
   separate annual paid service.
8. **Multistatic SAS at hobby scale** — Kiang-Kiang 2022 geometry
   explicitly models a stationary sonobuoy as a C-node; clawft mesh
   IS the C-node array per `papers/analysis/multistatic-sas.md`.
   No commercial vendor ships this shape at any price.

### Per-tier parity verdict (added 2026-05-11 by P5)

Following the parity-tier analysis in P5 §3, the per-tier verdict:

| Tier | clawft delivers at phase | Cost ratio at parity | Dominant moat |
|------|---------------------------|------------------------|------------------|
| Hobbyist ($100-$2k) | 1b (LBL/PAM), 5a (single-beam), 5b-c (CHIRP), 5d (360°) | 0.03× – 0.8× | **fleet coverage** |
| Prosumer ($2k-$10k) | 5b-c, 5d, 5e/f | 0.02× – 0.5× | **fleet coverage + replay** |
| Commercial ($10k-$200k) | 5d, 5e/f, 6 | 0.05× – 0.44× | **new mesh geometries + vector-DB queries** |
| Defense/Survey ($200k-$2M) | 6+, 7+ (limited) | 0.10× – 0.20× at degraded specs | **change-detection across deployments** |

The gaps to be honest about:

1. **No live phased-array FLS refresh** — structurally requires a
   real phased array; not closed at hobby budget at any phase.
   Phase 5d gimbal at 1 Hz mechanical scan is a **different product
   shape** than Garmin LiveScope at 60 fps.
2. **Sub-decimeter precision at km ranges** competes with five-
   figure commercial gear; we will not match it at hobby price
   without Phase 7+ CSAC clock investment (~$3k/node).
3. **Calibration to scientific traceability** is real work; the
   joint solver self-calibrates *relative* sensitivities for free
   but **traceable absolute** dB re 1 V/µPa requires a shared
   reference projector + bench cal infrastructure (~$400 per node
   amortized).
4. **Per-ping resolution at multibeam survey grade** — out of
   family. The compounding-over-time advantage of fleet mesh-
   mosaic is the right framing, not per-ping match.
5. **Type-approved navigation** — out of scope. Position as
   research / mesh-coverage product.
6. **Deep-sea pressure rating beyond 100 m** — out of scope at
   hobby budget; Phase 7 Delrin case extends to ~500 m, which
   is the practical ceiling.
7. **Live FLS refresh rate** — out of family per the geometry
   choice; mechanical scan ≠ phased array refresh.

### Strategic positioning commitment (proposed ADR-092)

P5 §13.1 surfaces a follow-up ADR commitment:

> The project asserts: **clawft is dominant at LBL/USBL + PAM +
> 360°; competitive at CHIRP + single-beam; we are not pursuing
> live FLS / phased-array refresh; we are not pursuing type-
> approved navigation.** Target customer segments per tier are
> hobbyist mesh / prosumer research / commercial AUV-positioning
> alternative; defense market positioning is dual-use civilian-
> first.

The next move (separate from this document) is to land the
Phase 1 build, measure self-coupling and reverb decay in the
pool, and start building the chart view (§5.1) as a WeftOS
surface against the v1 streams — so the visualization is ready
the moment the second buoy goes in the water. The Phase 4 LBL
demo per P5 §10.1 is the **single most credible commercial-
positioning artifact** the project can ship in the near term.
