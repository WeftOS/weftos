# Board triage proposal — 2026-10-12

Prepared by the steward agent for owner review. **No board changes were made.** Evidence is
commits and files on `target-0.8.4` and dashboard `origin/main`; builds, tests and hardware were
not re-run, so "done" means the work exists, not that behaviour was re-verified. Cards being
worked tonight were excluded.

| Card | Proposal | Evidence and next move |
|---|---|---|
| 2fa04f51 Phone node page | **done** | `/phone` on dashboard main (78566c2, PR #4); proof tests in `npm test`. Next: one real-phone pass by the owner. |
| c5f059ee Hi-Link ingest | **done** | 5c4a89ddd: catalog.json with 541 Hi-Link rows, `docs/hardware/hi-link.md`, status "ingested". |
| 1df9fcf8 Local hosting on placement | **re-scope** | Slice 1 merged (a98d83778: roster import, memory budget, gated LAN exposure, Orpheus role). Remaining: failover to a second node, voice latency tolerance. Owner picks the node and tolerance. |
| 6d4b9390 MetaHarness flywheel | **re-scope, near done** | Scaffold, ceilings and anchors exist; scripts in `scripts/metaharness/`. Run `flywheel-score-eval.sh`, record the score, set a numeric floor for "toward 100". |
| c980b664 Infineon ingest | **re-scope** | 12 rows staged in `docs/hardware/infineon-sensor-ingest.json`, never merged into catalog.json; the hold is gone. New acceptance: merge rows, status "ingested" with a count. |
| 2b908cb9 ADR-108 install locally | **re-scope** | P1–P3 shipped on both sides (89d416c38, 2d87bbd7d, 483fbadab, cfbc950e4; dashboard #14, #17). P4 (configure / `setup.toml`) open. Close at P3 and file P4, or reduce acceptance to P4. |
| 3373be4b Edge pad bring-up | **blocked (owner)** | Boot, flip, touch, renderer, inventory, PTT (d922c037f); `leaf_listen_addr` + mesh identity merged (8702f285e). Remaining: "[mesh] connected" and a rendered DisplayText. Blocked on the gitignored `wifi_secrets.rs` and reconnecting the pad. |
| 444c7964 Leaf track | **re-scope** | Keys, signing, discovery, replay committed (e74fb3834, 839f5f015). No certified join on silicon. New acceptance: one real join plus replay after a parent restart. Depends on 3373be4b. |
| 0dc06c23 Phase 4 sandbox / nested WeftOS | **re-scope (split)** | Native path + Wasmtime driver committed (6a98e3927, e428eaced; feature off). Remaining: real user-daemon run, Linux-container acceptance (DinD), review, gate. No macOS Seatbelt driver found (shallow grep). Split into (a) native + Wasmtime acceptance, (b) container + Seatbelt drivers. |
| 3c65a1d0 Fleet manager Network tab | **close for P0–P4** | P0–P4 shipped (676242981 … 3b4f5da46, 9c6557410, 6ddf7b080). P5 gated actions "not started (only if wanted)". Open a P5 card only if wanted. |
| 397c3253 Spatial workspace MVP | **close as moved (owner confirm)** | Engine moved to the separate weftos-spacial-workspace repo (69b92aab3). Live acceptance depends on RuView data; R1/R2 (64b101a2, d887af62) remain. |
| 7f2ea05d eikon deploy | **re-scope** | Package and renderer exist (b63b6e8b8, d638ecaf1). Not deployed into ~/.claude, ~/.grok or Codex; no per-host test run. Needs owner approval to write home config dirs. |
| 0746a0ae Team bus: Codex + WeftOS | **re-scope (split)** | WeftOS as a `weft` command host done (ada997f33). Codex adapter not found; depends on upstream ruflo#3512; overlaps TM5. Split WeftOS half (done after a local demo) / Codex half. |
| 29023c6c Evidence-based ticket steward | **re-scope (umbrella)** | `scripts/ticket-steward.mjs` + test (74de99201). Keep as the contract card; acceptance = rollout doc + resume-after-failure test. Overlaps 3f6c1f33 and c08d3eae. |
| c08d3eae Keep Shasta subscription current | **close / fold** | A recurring operation; publisher exists (Shasta 4fd87fd); no schedule installed. Fold into 3f6c1f33, or file "schedule the Shasta publisher". |
| a64bbafd ECG/ToF exports to loopback | **release, raise priority** | No work: sen0213-ecg still defaults to 0.0.0.0:8046 and sen0628-tof to 0.0.0.0:8047 in every weftos-cogs checkout; tests assert the old default. Privacy default for vital-sign and position data — recommend high priority (weftos-cogs repo, feature branch). |

## Duplicates and overlaps
- 29023c6c, 3f6c1f33 (review) and c08d3eae all cover the steward / publisher contract.
- 2ea1922d (LD1040C guide) and 2b6fe600 (LD6002 guide) use the same mechanism (related, not duplicates).
- 3373be4b and 444c7964 share the hardware-join acceptance; keep both, sequenced.

## Questions for the owner
1. Which second node and what voice latency tolerance for 1df9fcf8?
2. Is fleet manager P5 (gated actions) wanted?
3. Does the weftos-spacial-workspace repo own 397c3253?
4. Approve rendering eikon globally into ~/.claude, ~/.grok and Codex?
5. When will the edge pad Wi-Fi secrets file be placed (and the pad reconnected)?
6. Schedule the Shasta publisher, or drop c08d3eae?
