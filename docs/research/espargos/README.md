# ESPARGOS research pool

Date: 2026-09-30. Status: research, nothing built. Every claim in the per-item notes is tagged [V] (read in the source, with location) or [I] (inference). This README's Relevance section is mostly [I], since it applies the papers to other projects.

Sibling note (written separately): [espsdr-and-linux-csi-nodes.md](espsdr-and-linux-csi-nodes.md), on ESP-SDR raw I/Q capture and Linux-based CSI nodes.

## What ESPARGOS is

ESPARGOS is a low-cost, real-time-capable, phase-coherent WiFi channel sounder from the Institute of Telecommunications at the University of Stuttgart (Euchner, ten Brink and co-authors). One board carries eight ESP32 chips (ESP32-S2 in the datasets paper), each behind its own patch antenna in a 2x4 grid, operated receive-only. All chips share one 40 MHz reference clock. A separate WiFi "phase reference" packet is fed through a splitter and microstrip lines of known length to every receiver, and software uses it to remove the random LO phase that each chip's PLL picks up after every reset or channel change. Several boards can be chained through one coax carrying clock and reference to make a larger array (4 boards = 4x8, 32 antennas). The chips report CSI from the WiFi preamble (L-LTF, HT-LTF) in hardware, so the system sniffs ordinary WiFi traffic without transmitting. A central controller streams CSI over Ethernet to the Python library pyespargos, which does calibration, packet clustering across boards, angle-of-arrival, delay, polarisation and camera-overlay demos. The group publishes labelled CSI datasets (millimetre-level total-station ground truth) and channel-charting code. The commercial version, ESPARGOS One (4x2, dual-polarised, 2.4 GHz), is listed as available at buy.espargos.net [V, espargos.net]. The ESP-SDR spin-off (raw I/Q from ESP32 chips) appeared on 2026-09-28.

## Index

Checked against https://espargos.net/research/ on 2026-09-30. The page lists eight items and four datasets. Nothing was missing from the lead's list. The extra items are the four datasets, the pyespargos library and two ESP-SDR repos.

| # | Item | Note | Source |
|---|---|---|---|
| 1 | AR visualisation of WiFi channel measurements, arXiv 2608.25996 (Aug 2026) | [papers/ar-visualization-2608-25996.md](papers/ar-visualization-2608-25996.md) | https://arxiv.org/abs/2608.25996 |
| 2 | Passive Channel Charting, arXiv 2504.09924 (SPAWC 2025) | [papers/passive-channel-charting-2504-09924.md](papers/passive-channel-charting-2504-09924.md) | https://arxiv.org/abs/2504.09924 |
| 3 | ESPARGOS datasets + channel charting, arXiv 2408.16377 (Kleinheubach 2024) | [papers/espargos-datasets-2408-16377.md](papers/espargos-datasets-2408-16377.md) | https://arxiv.org/abs/2408.16377 |
| 4 | ESPARGOS hardware architecture, arXiv 2502.09405 (ITG Smart Antennas 2023) | [papers/espargos-hardware-2502-09405.md](papers/espargos-hardware-2502-09405.md) | https://arxiv.org/abs/2502.09405 |
| 5 | Jeija/ESPARGOS-Passive-ChannelCharting | [papers/repo-passive-channelcharting.md](papers/repo-passive-channelcharting.md) | https://github.com/Jeija/ESPARGOS-Passive-ChannelCharting |
| 6 | Jeija/ESPARGOS-WiFi-ChannelCharting | [papers/repo-wifi-channelcharting.md](papers/repo-wifi-channelcharting.md) | https://github.com/Jeija/ESPARGOS-WiFi-ChannelCharting |
| 7 | SDR Academy 2025 talk (30 min) | [papers/video-sdr-academy-2025.md](papers/video-sdr-academy-2025.md) | https://www.youtube.com/watch?v=GrlRUA7dW44 |
| 8 | "This ESP32 Antenna Array Can See WiFi" (11 min) | [papers/video-esp32-antenna-array-can-see-wifi.md](papers/video-esp32-antenna-array-can-see-wifi.md) | https://www.youtube.com/watch?v=sXwDrcd1t-E |
| + | ESPARGOS/pyespargos (LGPL-3.0), esp-sdr, esp-web-sdr | [papers/repo-pyespargos.md](papers/repo-pyespargos.md) | https://github.com/ESPARGOS/pyespargos |

Datasets (DaRUS, all CC BY 4.0 per the DaRUS API): espargos-0001 (23.9 GB), 0002 (86.5 GB), 0005 (61.3 GB), 0007 (17.4 GB). DOIs are in the datasets note.

Reading depth: the four papers were read in full (they are four to six pages each, so this is a full read and not abstracts). Both repos' READMEs and the relevant notebooks were read for hyperparameters. Videos were reviewed from descriptions, chapters and auto-captions only, not watched.

## Findings that matter for decisions

1. ESPARGOS phase coherence needs three things together: a shared 40 MHz clock, a distributed WiFi phase-reference packet, and per-boot/per-channel software calibration. A shared clock alone is not enough, because the chip PLL picks a new random LO phase at every reset or retune [V, hardware paper Sec. II-A; video description "once after each ESP32 has booted up or in case we switch the Wi-Fi channel"]. Independent ESP32 nodes therefore cannot be made phase-coherent by software alone.
2. The hardware paper's phase-stability evidence is qualitative: one plot over about 400 s, no numbers, no accuracy in degrees or metres [V, Sec. IV-A, V]. The authors say quantitative characterisation is future work.
3. Passive channel charting halves triangulation error for a foil-wrapped robot (MAE 0.257 vs 0.434 m) but loses to triangulation for a human (0.532 vs 0.322 m) when trained on the robot [V, PCC paper Table I]. Cross-target generalisation is the stated main open problem.
4. It needs a lot of infrastructure: four phase-, time- and frequency-synchronised 2x4 receive arrays over a 4.5 x 4.5 m area, four fixed ceiling transmitters, and about 480k training datapoints. Only a single target is handled [V, PCC paper Sec. II, I-B].
5. Single-array NLoS charting on espargos-0002 reaches MAE 0.44 m, CEP 0.42 m, CT/TW 0.96 with a moving transmitter carried by a robot [V, datasets paper Table I]. The 0.13 m LoS-only result on espargos-0001 is effectively filtered triangulation, per the notebook's own conclusion [V, repo].
6. CSI quality is limited: 8-bit signed I/Q, quality depends on reference-signal amplitude and packet type, "not great with the ESP32" compared with SDR [V, SDR Academy Q&A, ASR captions; datasets paper Sec. V "considerably more noisy"].
7. Licences: pyespargos is LGPL-3.0. The two channel-charting repos, esp-sdr, esp-web-sdr and the other org repos have no licence file. The four datasets are CC BY 4.0. The hardware and firmware licences were not found [V, GitHub API, DaRUS API].
8. The channel-charting recipes are small enough to reimplement: for example the triplet model is a 512-256-128-64-2 MLP with BatchNorm on per-tap antenna covariance features and a time-window triplet mining schedule, and the passive pipeline uses clutter order 2, kNN 20 for geodesics and a 1%-quantile margin [V, repo notebooks]. But training used a GPU workstation.

## Relevance

### (a) RuView

RuView (ruvnet's WiFi CSI sensing project) uses ESP32-S3 CSI mesh nodes and the rvCSI adapter. Local checkout: `~/dev/ruview/RuView`. Relevant docs: `docs/adr/ADR-012-esp32-csi-sensor-mesh.md`, `ADR-029-ruvsense-multistatic-sensing-mode.md`, `ADR-031-ruview-sensing-first-rf-mode.md`, `ADR-024-contrastive-csi-embedding-model.md`, `docs/research/rf-topological-sensing/`. Facts below about RuView are read from those ADRs [V]; the comparisons are [I].

**Hardware gap.** ADR-012 describes 1-2 RX antennas per node, 52-56 subcarriers at HT20, about 5-15 dollars per node, and the ESP-IDF CSI API [V, ADR-012]. An ESPARGOS array has 8 coherent RX chains per board and 117 subcarriers at 40 MHz (HT40) [V, datasets paper Fig. 3]. RuView's nodes are independent, non-coherent single-antenna receivers. RuView ADR-031 names bandwidth as a fidelity lever and notes ESP32 HT20 is the limit [V]. ESPARGOS shows HT40 on ESP32-class chips is achievable in their setup, at least on the S2.

**Phase-coherent arrays vs the multistatic design.** ADR-029 builds multistatic sensing from N nodes with TDMA slots and a GPIO sync pulse, and claims clock drift of about 0.5 us over 50 ms is within a 1 ms guard interval [V, ADR-029 Sec. 2.4]. That is time slotting, not phase coherence. ADR-029 also removes channel-hop phase rotation with a solver that fits delta offsets from static subcarriers [V, Sec. 2.3]. The ESPARGOS hardware paper explains why those offsets exist and why they cannot be predicted: the PLL re-acquires lock at every retune, so the initial phase changes after each channel change [V, hardware paper Sec. II-A]. That supports RuView's hop-offset estimation design and warns that any phase feature crossing a hop is untrustworthy unless recalibrated [I]. What ESPARGOS adds that RuView lacks is angle of arrival, which needs cross-antenna phase within a node. That requires the reference-distribution hardware. Software cannot fix it on stock ESP32-S3 boards [I from finding 1].

**Channel charting vs RuView's pose and occupancy work.** RuView's pipelines are CSI to pose, vitals, motion and a signal field, with contrastive embeddings (AETHER, ADR-024) and SONA drift adaptation. I found no channel-charting content in the RuView docs (grep of `docs/` for "channel chart" returned nothing). Channel charting is a self-supervised localisation map from CSI similarity. It could give RuView a room-scale position prior for a person without labels, and its dissimilarity trick (phase-insensitive cosine similarity over antenna covariance, fused with timestamps, then geodesic) is compatible with non-coherent nodes because it discards phase [I]. But the reported accuracy comes from four synchronised arrays. A mesh of single-antenna S3 nodes has far less spatial information per link, so 0.26 m is not a number to expect [I]. Pose estimation (limbs) is a much finer task than the position of one blob; nothing in these papers speaks to it.

**Passive localisation.** PCC removes clutter with CRAP (rank-K subspace projection, K = 2 in the code) and localises one moving target. RuView's ADR-029 does per-link coherence and multi-person separation by min-cut. Both are about "what changed on the links". The overlap is the clutter-subspace idea, which RuView could try on its own per-link CSI as a background model, an alternative to its baseline coherence gating [I]. PCC's failure to transfer between a foil robot and a human is a warning for RuView's cross-environment work (its MERIDIAN ADR-027 addresses cross-environment generalisation; not read for this note).

**Practical use.** Dataset espargos-0007 (CC BY 4.0, 17.4 GB, tfrecords with CSI shape 4x2x4x117) is a labelled passive-target benchmark. RuView could replay a single antenna at reduced subcarrier count as a sanity check of its passive pipelines against total-station ground truth [I]. Sizes and the 117-subcarrier shape are read from the notebooks [V].

### (b) WeftOS

Links: [ADR-056 BVH spatial index](../../adr/adr-056-bvh-spatial-index.md), [ADR-099 governed workload placement](../../adr/adr-099-governed-workload-placement.md), [ADR-100 cog workload kind](../../adr/adr-100-cog-workload-kind.md), [spatial-intelligence-2026/urth-applicability.md](../spatial-intelligence-2026/urth-applicability.md), [spatial-intelligence-2026/ruv-parallels-and-gaps.md](../spatial-intelligence-2026/ruv-parallels-and-gaps.md), [antihunter-vs-ruview-world-models.md](../antihunter-vs-ruview-world-models.md), [copper-bus-timing-grounding/array-telemetry-papers.md](../copper-bus-timing-grounding/array-telemetry-papers.md), and the sibling [espsdr-and-linux-csi-nodes.md](espsdr-and-linux-csi-nodes.md).

**Urth and the BVH world model.** The existing Urth notes fix that the geometric index is BVH and "not ... CSI occupancy", that occupancy from CSI is a prior or feature and not geometry source of truth, and that generative or learned fill is non-metric [V, urth-applicability.md Sec. 1; ruv-parallels-and-gaps.md]. ESPARGOS fits that framing in a useful way. Its outputs are of three kinds: (1) measured angle/delay/polarisation per path, which is metric evidence about the direction and relative delay to a transmitter (the AR paper's "all displayed quantities remain directly derived from measured channel data"); (2) triangulated positions, metric with uncertainty (von Mises likelihood with a concentration parameter); (3) channel-chart coordinates, which are learned, arbitrary up to an affine transform until aligned with labelled positions, so they are non-metric until calibrated [V, datasets paper Sec. IV]. Under Urth's honesty rule, (1) and (2) could be ingested as observations with covariance into a room-scale leaf, and (3) only as a labelled soft feature [I]. A phase-coherent array is also a plausible sensor-node type for a room leaf, but ESPARGOS gives directions to transmitters and, with several arrays, a target position for one target. It does not give geometry of the room. The camera-registered overlay is a neat example of RF aligned to a camera frame. The mapping is a pinhole model with boresights assumed aligned and parallax ignored [V, AR paper Sec. II-C], so registration accuracy is a modelling assumption, not a measured number.

**Sensor nodes and mesh placement (ADR-099).** ADR-099 places workloads on nodes by capability, with Ed25519 node identity and governed placement [V, ADR-099 Context]. An ESPARGOS array is not a node in that sense. It is a peripheral with an Ethernet controller and a Python host. If used at all, the host (Pi 5 class) would be the mesh node advertising a capability like "csi-array", running the acquisition and calibration workload next to the hardware. Calibration state (per-board phase offsets, reference-path offsets, combined-array splitter offsets) is durable and hardware-specific, so it should follow the array and not the workload [I]. Timing: a multi-array setup needs a wired shared clock and phase reference, not just mesh clock sync. WeftOS's mesh timing cannot substitute for that. The copper-bus timing note, which grounds clock distribution over cables, is the closer neighbour [I].

**Do not** take from this material: the assumption that arbitrary WiFi nodes in the mesh can be combined into one coherent array; the ESPARGOS results as evidence for room geometry recovery.

### (c) Cognitum cogs

Cogs run on Seeds (Raspberry Pi Zero 2 W, armv7l, 512 MB) and on other ARM nodes. See `<client workspace>/cognitum/cogs-main/README.md`, `docs/devices/seed.md`, and `docs/research/bare-metal-cogs.md`. Facts from those docs [V]: the Seed cannot capture CSI itself (its Broadcom Wi-Fi is not supported by nexmon_csi, per cogs `esp-sdr-and-sensing-nodes.md`, the chip-support part is inference there); cogs receive CSI from ESP32 nodes over UDP 5006; the packet contract is 48-byte ESP32 CSI packets (magic `0xC5110003`, eight little-endian f32 at offset 16) and 32-byte vitals packets (`0xC5110002`); UDP 5006 is contended (cogs#14) and Cognitum's `presence-field` broker (ADR-151) is the intended fix; a documented cap of 3 concurrent cogs; `presence-field` has no aarch64 build.

**What transfers.**
- The Seed as a consumer of derived features, not raw CSI. A Seed cog could run the small last stage of a pipeline: an angle-of-arrival or presence score computed elsewhere, sent as an eight-float vector over the existing 5006 contract [I]. This suits the Zero 2 W.
- The clutter-subspace idea (rank-2 projection) is cheap: eigen-decomposition of a covariance of a few hundred entries. It could run on a Seed for a presence cog if the input is small [I]. Q = arrays x rows x cols x subcarriers is 4x2x4x117 = 3,744 complex numbers in the paper's setup, about 30 KB per packet as complex float32, and the paper's covariance features are much larger. That is a poor fit for a 512 MB Zero 2 W. Reduced inputs (one array, tap-limited) would be needed [I].

**What does not transfer.**
- The array itself. It needs an Ethernet controller, a Python 3.11 host, and a 40 MHz clock / phase-reference chain. The Seed's 512 MB and its Wi-Fi radio are not a sound host for it. A Pi 5 is more plausible [I, from Seed hardware in cogs docs].
- Training. Channel charting was trained on GPU workstations (RTX 4080, 64 GB RAM recommended) [V, repo READMEs]. A cog can carry an inference network (five dense layers) but not the training pipeline, and the trained chart is specific to a room and its array placement [I].
- The 48-byte packet contract carries eight floats, so raw phase-coherent CSI does not fit. A cog would consume a summary, and any ESPARGOS-derived cog needs a new packet type or a different transport [I].
- Presence and fall detection cogs work from per-node amplitude/variance features. ESPARGOS-class angle information is a different sensor class, not a drop-in upgrade [I].
- Commercial: the hardware and the ESP-SDR repos are unlicensed or LGPL; shipping a signed cog that bundles them needs licence review first [I].

## Gaps and things not reviewed

- Videos were reviewed from descriptions and captions only. Slides and on-screen numbers were not seen.
- pyespargos source, its wire protocol and radar mode were not read; only the README and docs index.
- Hardware and firmware licences, ESPARGOS One price and the espargos.net docs pages were not found in the fetched content.
- The two extra author repositories cited in the video description (ToA-AoA-Augmented-ChannelCharting, Geodesic-Uncertainty-Loss-ChannelCharting) were not opened.
- Cited background papers (CRAP, Le Magoarou's dissimilarity, Ferrand's triplet loss, Stephan et al., Poeggel et al. UWB passive charting) were not read.
- ESPARGOS phase-stability figures under temperature drift or long durations are not published in what I read.
