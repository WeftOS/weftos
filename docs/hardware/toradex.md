# Toradex — industrial Arm SoMs, carriers & the Torizon platform — WeftOS/WeaveLogic hardware KB

Scope: **Toradex** (toradex.com) — a Swiss maker of **industrial Arm System-on-Modules
(SoMs)**, matching **carrier boards**, and the **Torizon** embedded-Linux + cloud
platform. Toradex's reason to exist is *long-term, ruggedized, productizable* edge
compute: SoMs that run −40/+85 °C, carry a **10–15-year availability guarantee**, and
drop onto a custom carrier you design for your product. For a commercial
WeftOS/Cognitum edge node this is the "industrial-grade alternative to a Raspberry Pi"
tier — and **Torizon is a mature, shipping reference for the exact fleet + signed-OTA
layer WeftOS is building**.

Each entry is PUBLIC-facts-only (vendor-claimed specs) with a source URL per family.
Newer / preliminary numbers are flagged **(verify)**. Last compiled 2026-10-04.

Roles used below:
- **Evaluated-only (industrial SoM)** — a candidate ruggedized node; not in active use.
- **NPU sensing node** — SoM with an on-die NPU, candidate for on-device inference.
- **Architectural reference** — Torizon, a commercial precedent for our fleet/OTA layer.

A SoM is **not** a board you buy and run — it is a module that drops onto a **carrier
board you design** (or one of Toradex's eval/production carriers). That productization
step is the whole point, and the main cost, of this tier. See
[sbcs.md](sbcs.md) for the Pi / Lichee / Banana Pi boards this is weighed against and
[ai-edge.md](ai-edge.md) for the Jetson / Hailo inference tier.

---

## The two load-bearing things first: NPU SoMs + Torizon

**On-device AI (NPU) — the modules that matter for sensing/inference:**
- **Verdin iMX8M Plus** — NXP i.MX 8M Plus NPU, **up to 2.3 TOPS** (INT8). The mature,
  shipping Toradex AI module.
- **Verdin iMX95** — NXP i.MX 95 **eIQ Neutron NPU, up to ~2.0 TOPS** *(verify — newer
  architecture; TOPS not directly comparable to the 8M Plus figure)*, plus ECC memory.
- **OSM iMX93** — NXP i.MX 93 small **eIQ Neutron NPU** (entry AI, low power).

These are **small NPUs** — think classic vision / keyword / anomaly inference at low
power with long availability, **not** a Jetson/Hailo-class accelerator (see the honesty
section). Compare TOPS only within precision; Toradex NPU figures are NXP's INT8-class.

**Torizon — a commercial precedent for the WeftOS fleet + OTA layer (HIGH relevance):**
- **Torizon OS** — open-source, Yocto-built, **container/Docker-based** embedded Linux
  with Secure Boot, SBOM/CVE analytics, and EU Cyber Resilience Act (CRA) alignment.
- **Torizon Cloud** — **fleet OTA** (update bootloader, OS subsystem, and/or application
  packages to a **single device or an entire fleet**), **remote access** for support,
  **device monitoring**, and **device provisioning** (incl. "hibernating" devices to
  decouple provisioning from active cloud cost).
- **Why we care**: this is a mature, shipping implementation of the *exact* problem
  WeftOS's placement + signed-OTA + remote-ops layer targets — fleet-wide update
  rollout, per-component (bootloader/OS/app) update granularity, device provisioning,
  and fleet monitoring. It is the best commercial reference point to study/benchmark our
  fleet manager against, and the comparison worth drawing explicitly (see "Torizon ↔
  WeftOS fleet layer" below).
  Source: <https://www.toradex.com/torizon> ,
  <https://developer.toradex.com/torizon/torizon-platform/devices-fleet-management/> ,
  <https://developer.toradex.com/torizon/torizon-platform/torizon-updates/remote-ota-updates/remote-ota-updates-overview/>

---

## System-on-Modules — the core product

Three pin-standardized families, smallest/lowest-power to highest-performance:
**Colibri** (SODIMM-200) → **Verdin** (260-pin DDR4 SODIMM edge) → **Apalis** (MXM3
314-pin). Within a family, modules are pin-compatible, so you can re-SoM a product
across SoCs without redesigning the carrier — a core Toradex value proposition.

### Verdin family (260-pin DDR4 SODIMM edge connector)

The current mainstream family — the one to default to for a new industrial node.
Verdin carries more I/O than Colibri (260 vs 200 pins) on a cost-effective,
shock/vibration-resistant edge connector, and the whole family is pin-compatible.
Source: <https://www.toradex.com/computer-on-modules/verdin-arm-family>

- **Verdin iMX8M Plus** — **Role: NPU sensing node / evaluated-only.** NXP i.MX 8M Plus:
  up to quad **Cortex-A53 @ 1.8 GHz** + **Cortex-M7F @ 800 MHz** + VPU + **NPU up to
  2.3 TOPS**. RAM up to **8 GB LPDDR4** (32-bit); eMMC up to **32 GB** *(some listings
  cite up to 64 GB — verify per SKU)*. **−40 to +85 °C (IT grade)**; commercial grade
  also offered. WB (Wi-Fi 5 + BT) and IT-only (no wireless) SKUs. The proven Toradex AI
  module. Datasheet: <https://docs.toradex.com/108784-verdin_imx8m_plus_datasheet.pdf> ,
  product: <https://www.toradex.com/computer-on-modules/verdin-arm-family/nxp-imx-8m-plus>
- **Verdin iMX95** — **Role: NPU sensing node / evaluated-only (newer).** NXP i.MX 95:
  up to **6× Cortex-A55 @ 1.8 GHz** + Cortex-M7 + Cortex-M33 + Arm Mali GPU + **eIQ
  Neutron NPU up to ~2.0 TOPS (verify)**. RAM up to **16 GB LPDDR4x with inline ECC**;
  eMMC up to **128 GB**. The high-RAM, ECC, safety-enabled next-gen Verdin — strongest
  spec for a demanding node, but **newer/preliminary — pin exact shipping SKU + TOPS
  when quoting**. Datasheet: <https://docs.toradex.com/200007-verdin_imx95_datasheet.pdf> ,
  product: <https://www.toradex.com/computer-on-modules/verdin-arm-family/nxp-imx95>
- **Verdin iMX8M Mini** — **Role: evaluated-only (no NPU).** NXP i.MX 8M Mini: quad
  **Cortex-A53** (no NPU). RAM **1–2 GB LPDDR4**; eMMC **8–16 GB**. SKUs incl. DualLite
  1 GB and Quad 2 GB WB IT. The cost/power step below the 8M Plus when you don't need the
  NPU. Datasheet: <https://docs.toradex.com/107207-verdin_imx8m_mini_datasheet.pdf>
- **Verdin AM62 / AM62P** — **Role: evaluated-only (low-power, no NPU).** TI Sitara
  AM62x: up to **4× Cortex-A53 @ 1.4 GHz** + Cortex-M4F, optional 3D GPU, **no NPU**. RAM
  up to **2 GB LPDDR4** (16-bit); eMMC up to **16 GB**. Entry/low-power, second-source
  (TI, not NXP) within the same Verdin carrier. Source (family):
  <https://www.toradex.com/computer-on-modules/verdin-arm-family>

### Apalis family (MXM3 314-pin edge connector, 82 × 45 mm)

High-performance edge — the biggest SoCs, dual-display/dual-GPU, more I/O. Use when a
node needs real application-processor horsepower, not just sensing.
Source: <https://www.toradex.com/computer-on-modules/apalis-arm-family>

- **Apalis iMX8 (QuadMax / QuadPlus)** — **Role: evaluated-only (high-perf).** NXP
  i.MX 8QuadMax: **2× Cortex-A72 + 4× Cortex-A53 + 2× Cortex-M4F**, dual GPU, dual
  display/VPU. RAM up to **8 GB LPDDR4**; eMMC up to **32 GB**. The top Toradex compute
  module (no dedicated NPU; GPU-class graphics/vision).
  Source: <https://www.toradex.com/computer-on-modules/apalis-arm-family/nxp-imx-8>
- **Apalis iMX8X** — **Role: evaluated-only.** NXP i.MX 8X (Cortex-A35), QuadXPlus and
  DualX SKUs — the power-efficient sibling of the iMX8 on the Apalis form factor.
  Datasheet: <https://docs.toradex.com/107303-apalis_imx8x_datasheet.pdf>
- **Apalis iMX6** — **Role: evaluated-only (legacy / ultra-long availability).** NXP
  i.MX 6 (Cortex-A9). Still offered for its **extended availability to 2036** — the
  "design it once, build it for a decade+" case. Source (longevity):
  <https://www.toradex.com/news/nxp-imx6-socs-extended-availability-2036>

### Colibri family (SODIMM-200 edge connector, 67.6 × 36.7 mm)

The smallest, lowest-power, lowest-cost family — fits a standard SODIMM-200 socket.
For simple, rugged, very-long-life control/sensing nodes where compute is modest.
Source: <https://www.toradex.com/computer-on-modules/colibri-arm-family>

- **Colibri iMX8X** — **Role: evaluated-only.** NXP i.MX 8X: **4× Cortex-A35 @ 1.2 GHz**
  + Cortex-M4F; Wi-Fi 5 (802.11ac) + BT 5.3 options. DualX (1 GB DDR3L / 4 GB flash) and
  QuadXPlus (2 GB DDR3L; up to 16 GB flash on the WB SKU).
  Source: <https://www.toradex.com/computer-on-modules/colibri-arm-family/nxp-imx-8x>
- **Colibri iMX7** — **Role: evaluated-only.** NXP i.MX 7: up to **2× Cortex-A7 @ 1 GHz**
  + Cortex-M4F @ 200 MHz. RAM up to **1 GB DDR3L**; flash **512 MB NAND or 4 GB eMMC**.
  Source: <https://www.toradex.com/computer-on-modules/colibri-arm-family/nxp-freescale-imx7/>
- **Colibri iMX6ULL** — **Role: evaluated-only (lowest cost).** NXP i.MX 6ULL: single
  **Cortex-A7** @ 528/800/900 MHz. RAM up to **512 MB–1 GB DDR3L** (16-bit); eMMC up to
  **4 GB**. Toradex's cheapest module, with explicitly **extended availability**.
  Source: <https://www.toradex.com/computer-on-modules/colibri-arm-family/nxp-imx6ull>

### OSM family (solderable LGA — note on iMX93)

Toradex also ships **OSM** (Open Standard Module) solderable modules. The AI-ready
**OSM iMX93** (NXP i.MX 93: 2× Cortex-A55 + Cortex-M33 + small eIQ Neutron NPU, 2 GB
RAM / 16 GB flash) and **OSM iMX95** live here — **not** on the Colibri SODIMM form
factor. Flag for the catalog: iMX93 is an **OSM solderable** module, not a Colibri
SODIMM part *(verify placement before cataloguing)*. OSM/i.MX 9 modules carry the
longest availability (iMX93 → 2038, iMX95 → 2039).
Source: <https://www.toradex.com/computer-on-modules/osm-arm-family/nxp-imx93>

---

## Torizon ↔ WeftOS fleet layer (the comparison worth drawing)

| WeftOS concern | Torizon equivalent | Note for us |
|---|---|---|
| Governed workload placement / app delivery | Torizon OS **container/Docker** app model | They ship apps as OCI containers; a clean precedent for how cogs could be packaged + delivered. |
| Signed OTA, per-component | Torizon Cloud **Remote Updates** — bootloader / OS subsystem / app, per-device or **whole fleet** | Mirrors our signed-OTA + per-slice update goal; study their update-granularity + rollback model. |
| Fleet manager / Catalog | Torizon Cloud **Device & Fleet Management** + **Device Monitoring** | Direct analog to the `cog-manager` Network/Catalog view ([WeftOS/weftos-cogs](https://github.com/WeftOS/weftos-cogs/tree/v0.1.4/crates/cog-manager)) (fleet state, versions, health). |
| Licence/provisioning per mesh | Torizon Cloud **device provisioning** (+ "hibernate" to decouple provisioning cost) | Compare to the Seed-as-licence-proxy / one-checkout-per-mesh direction. |
| Secure boot + supply-chain integrity | Torizon **Secure Boot + SBOM/CVE analytics + EU CRA** alignment | A compliance bar (EU CRA) a commercial WeftOS/Cognitum product will also have to clear. |

Takeaway: **Torizon is the closest shipping commercial system to the WeftOS fleet/OTA
layer.** Treat it as the reference to benchmark against (update granularity, rollback,
provisioning cost model, CRA compliance), not as something we adopt — our placement +
cognitive-OS story is broader, but their OTA/fleet mechanics are proven and worth
copying where they're good.

---

## Carrier boards

A SoM needs a carrier. For a product you design your own; Toradex's carriers are for
eval and lower-volume production. Verdin carriers are pin-compatible across all Verdin
modules.
Source: <https://developer.toradex.com/hardware/verdin-som-family/carrier-boards/>

| Carrier | For family | Form factor / size | Intended use | Notes |
|---|---|---|---|---|
| **Verdin Development Board** | Verdin | Full-featured eval | **Eval / bring-up** | All Verdin I/O broken out: USB 3.0, GbE, HDMI, MIPI DSI + CSI, PCIe. |
| **Dahlia** | Verdin | 120 × 120 mm, compact | **Dev / demo** | Common interfaces only; for software dev + demonstration. |
| **Yavia** | Verdin | Compact | **Dev (supply-chain-friendly)** | Partner-designed (Linear Computing) to ease supply constraints; USB 3.x, GbE, HDMI, MIPI CSI, PCIe. |
| **Mallow** | Verdin | **Pico-ITX 100 × 72 mm** | **Volume production** | Low-cost, small, volume-intended. |
| **Ivy** | Verdin | Small | **Volume production** | Volume production carrier for Verdin. |
| **Ixora** | Apalis | Full-featured | **Eval + industrial** | Industrial I/O: CAN 2.0b, I2C, GPIO. |
| **Aster** | Colibri | Entry / Arduino-compatible | **Eval (entry)** | Low-cost Colibri eval carrier. |

Carrier datasheet example (Dahlia):
<https://www.mouser.com/catalog/specsheets/Toradex_145-0155%20dahlia_carrier_board_datasheet_v1.1.pdf>

---

## Why Toradex vs a Pi / Lichee / Banana Pi (honest take)

**What you actually pay for (the case *for* Toradex):**
- **Industrial temperature** — real **−40 to +85 °C** IT-grade modules, not a 0–50 °C
  consumer board.
- **Long-term availability** — Toradex only partners with SoC vendors guaranteeing
  **10+ years**, and provides **10+ years life-cycle for every module**; newer i.MX 9
  parts are guaranteed **~15 years** (iMX93 → 2038, iMX95 → 2039), i.MX 6 extended to
  2036. (The Colibri PXA270, launched 2005, shipped for 15+ years.) A Pi 5 has no such
  guarantee. Source:
  <https://developer.toradex.com/hardware/hardware-resources/general-product-information/long-term-availability/>
- **Productization** — the SoM-on-custom-carrier model means your product's I/O,
  mechanicals, and certification live on *your* carrier; you can re-SoM across SoCs
  (even swap NXP↔TI within Verdin) without redesigning it.
- **ECC + safety** — inline ECC memory and safety-enabled SoCs (iMX95) exist here; they
  do not on a hobby SBC.
- **Torizon** — a supported OTA/fleet/secure-boot/CRA-compliance stack out of the box.

**What it costs you (the case *against*, be balanced):**
- **Higher unit cost** and **carrier-design effort** — there is no $35-off-the-shelf
  option; you design (or buy) a carrier and do the integration work. A Pi 5 or Lichee/
  Banana Pi is far cheaper and runs as-is.
- **Lower raw compute / no big GPU-TPU** — Toradex NPUs are **small (~2–2.3 TOPS)**. For
  heavy local inference a **Pi 5 + Hailo-8 (26 TOPS)** or a **Jetson** beats any Toradex
  module on TOPS/$ (see [ai-edge.md](ai-edge.md)). Toradex wins on ruggedness +
  longevity + integration, not peak AI throughput.
- **Smaller maker ecosystem** — direct/distributor sales and an industrial support model,
  not the vast Raspberry Pi community/software base.

**Bottom line for WeftOS/Cognitum**: Toradex is the right tier for a **ruggedized,
long-life, certifiable commercial edge node** where −40/+85 °C, a decade+ of supply, and
productization matter more than price or peak TOPS. For a cheap lab node or a
TOPS-hungry vision node, a Pi-class board (+ Hailo) still wins.

---

## Comparison — Toradex SoMs

Specs are vendor-claimed; **(verify)** marks newer/preliminary numbers. NPU TOPS are
NXP INT8-class and **not** comparable to Jetson sparse-INT8 or DGX FP4 (see ai-edge.md).

| Module | SoC / CPU | NPU | RAM (max) | eMMC (max) | Temp | Form factor | Our fit |
|---|---|---|---|---|---|---|---|
| **Verdin iMX8M Plus** | i.MX 8M Plus, 4× A53 @1.8 + M7F | **2.3 TOPS** | 8 GB LPDDR4 | 32 GB *(64?)* | −40/+85 IT | Verdin 260-pin | **NPU sensing node (proven)** |
| **Verdin iMX95** | i.MX 95, 6× A55 @1.8 + M7 + M33 | **~2.0 TOPS (verify)** | 16 GB LPDDR4x **ECC** | 128 GB | −40/+85 *(verify)* | Verdin 260-pin | NPU node, high-RAM (newer) |
| **Verdin iMX8M Mini** | i.MX 8M Mini, 4× A53 | none | 2 GB LPDDR4 | 16 GB | −40/+85 IT | Verdin 260-pin | Non-AI cost step-down |
| **Verdin AM62 / AM62P** | TI AM62x, 4× A53 @1.4 + M4F | none | 2 GB LPDDR4 | 16 GB | −40/+85 *(verify)* | Verdin 260-pin | Low-power, TI second-source |
| **Apalis iMX8 (QuadMax)** | 2× A72 + 4× A53 + 2× M4F | none (dual GPU) | 8 GB LPDDR4 | 32 GB | industrial | Apalis MXM3 314 | High-perf edge / graphics |
| **Apalis iMX8X** | i.MX 8X, A35 (Quad/Dual) | none | — | — | industrial | Apalis MXM3 314 | Power-efficient high-perf |
| **Apalis iMX6** | i.MX 6, A9 | none | — | — | industrial | Apalis MXM3 314 | Legacy, availability → 2036 |
| **Colibri iMX8X** | i.MX 8X, 4× A35 @1.2 + M4F | none | 2 GB DDR3L | 16 GB | industrial | Colibri SODIMM-200 | Small rugged node (Wi-Fi 5/BT) |
| **Colibri iMX7** | i.MX 7, 2× A7 @1 + M4F | none | 1 GB DDR3L | 4 GB | industrial | Colibri SODIMM-200 | Simple long-life control node |
| **Colibri iMX6ULL** | i.MX 6ULL, 1× A7 ≤900 MHz | none | 1 GB DDR3L | 4 GB | industrial | Colibri SODIMM-200 | Lowest-cost, extended avail. |
| **OSM iMX93** | i.MX 93, 2× A55 + M33 | small eIQ Neutron | 2 GB | 16 GB | industrial | **OSM (solderable)** | Entry AI, longest avail (2038) |

### Takeaways for WeftOS placement

- **Standout to track for a ruggedized Cognitum node**: **Verdin iMX8M Plus** (proven,
  2.3-TOPS NPU, −40/+85 °C, shipping now) is the default; **Verdin iMX95** (6× A55, ECC,
  15-yr availability to 2039) is the one to watch as it matures. Both are Verdin
  pin-compatible, so a carrier designed once takes either.
- **Model NPU SoMs as "sensing nodes," not "inference tiers"**: ~2 TOPS is classic
  vision/keyword/anomaly inference at low power — WeftOS can place a *light* vision/sensor
  cog here, but heavy inference still goes to a Jetson/Hailo node (ai-edge.md).
- **Torizon is a reference, not a dependency**: study its OTA update-granularity,
  rollback, provisioning-cost model, and EU CRA compliance as a benchmark for the WeftOS
  fleet manager + signed-OTA layer; don't adopt it.
- **Longevity is the real differentiator**: 10–15-year guaranteed availability + −40/+85 °C
  is what a commercial product gets here that no Pi/Lichee/Banana Pi offers — weigh that
  against higher cost, carrier-design effort, and lower peak TOPS.
- **Honesty flags**: iMX95 is newer — TOPS (~2.0) and temp grade are preliminary, pin the
  exact SKU when quoting. iMX8M Plus eMMC max (32 vs 64 GB) varies by listing. iMX93 is
  **OSM solderable**, not Colibri SODIMM — correct before cataloguing.
