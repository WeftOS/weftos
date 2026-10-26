# Dashboard portfolio goals — 2026-09

The WeftOS dashboard is the portfolio view and the native development board for
the OS. A project harness may also use a dashboard board from its original
repository with a credential scoped to that project. A subscribed project board
remains owned by its source project and is read-only in the portfolio. Use one
authority for each kind of work so a snapshot and a native ticket do not become
competing copies.

| Work | Write authority | Portfolio path |
|---|---|---|
| WeftOS OS and MetaHarness development | Dashboard WeftOS board | Native tickets and goals |
| Shasta software coordination | Dashboard Shasta board, from the Shasta repo | Native tickets and goals |
| Shasta trailer restoration | Shasta Postgres `/board` | Existing read-only subscription |
| BakeOS product work and goals | BakeOS OS board | 30 native Product Board tickets in a read-only subscription; goals pending |
| Sansone product work and goals | Sansone steward and approved OS paths | 835 Product Board tickets in a read-only subscription under the [export decision](sansone-board-subscription-proposal-2026-09.md); goals pending |
| FlipsOS cards and goals | FlipsOS native board | 102 redacted native tickets in a read-only subscription; goal migration and recurring sync pending |
| RuView and oil-rig demo work | Their dashboard project boards | Native project goals and tickets |

## Roles

- **Portfolio lead:** sets outcome and evidence, links tickets to goals, checks
  subscription freshness, and keeps the remote dashboard operational.
- **Project steward:** decides what source-board data may leave a project. For
  Sansone, the steward retains board write authority and controls restricted cards.
- **Harness implementer:** claims a native development ticket from the original
  repo, runs its checks, and posts a completion receipt.
- **Demo operator:** supplies physical hardware, room capture, and measured
  observations for demos. Software cannot substitute for these receipts.
- **Reviewer:** checks claim limits and provenance before a goal is marked
  complete or shown outside its project.

No target dates or named owners are inferred from the source documents. Set
those when the responsible project lead commits to a delivery window.

## Goals and first task chains

| Goal | Initial chain and evidence | Source |
|---|---|---|
| Project harnesses use scoped dashboard boards | Project-scoped credential RPCs → Shasta repo client → cross-project denial and board round trip → project goal API. Credential files remain host-local. | [Triple loop](../guides/agent-harness-triple-loop.md), Shasta `docs/weftos-dashboard-board.md` |
| Portfolio shows project-owned boards and goals | Keep Shasta's 60-item snapshot fresh → keep BakeOS's 30 native tickets, Sansone's 835 tickets, and FlipsOS's 102 redacted tickets current → complete FlipsOS goal storage and recurring sync. | [Triple loop](../guides/agent-harness-triple-loop.md), project board ADRs |
| Restore WeftOS baseline gate and research receipts | Existing gate ticket → fix baseline failures by owner → publish gate, score, and crosscut receipts on the board and nodes. | [Roadmap](../brain/01-roadmap-and-phases.md) |
| Deliver a measured RuView room demo | Connect gear and diagnose router-2 association → first real capture and dimensions → placement and training → first hardware-backed live frame. Recorded benchmark output stays labeled. | `the client demo repo's docs/CONTRACT.md`, `docs/LIVE-PATH-READINESS.md` |
| Complete the confidential oil-rig study | Draft the private paper from the existing abstract → test air-curtain, RFID portal, and deck-quality JSON at the collect gate. Cog packaging waits for a vendor home. This goal is excluded from public demo material. | `docs/handoff-oil-rig.md`, `docs/plans/cognitum-cogs-deck-twin.md` |
| Validate the Cardano score contract | Correct governance-counsel 5D drift → repeat sidecar on a stable circuit → record a graduation decision. The previous n=104 run did not show generator lift and does not justify promotion. | `.planning/symposiums/liber-de-ludo-aleae/deliverables/03-weftos-mapping.md`, `studies/2026-08-14-overnight-metaharness-sidecar.md` |
| Demonstrate an honest Urth region-to-query path | R1 live BVH region publish → R2 spatial RPC end to end → R3 dual-backend service → one licensed OSM/DEM pilot region and client view. Unobserved space remains unobserved; appearance does not mint metric geometry. | [Urth applicability](../research/spatial-intelligence-2026/urth-applicability.md), [Urth twin](../weftos/urth-digital-twin.md) |

The dashboard has records for these goals and linked first tasks. Shasta also
has a project-level integration goal and native ticket. These are planning
records, not evidence that the demo or spatial deliverables have shipped.

## Development order

1. Finish and verify scoped dashboard board access, the Shasta client, and
   credential UI. This unblocks project harnesses without changing source-board
   authority.
2. Add project-scoped goal reads and ticket-goal links to the harness API, then
   expose goal evidence and dependencies in the dashboard UI.
3. Maintain the live BakeOS, Sansone, and FlipsOS ticket subscriptions. Deploy
   the FlipsOS goal model and recurring source-side sync after review. Sansone's owner-only workspace
   may hold the full ticket metadata snapshot under the lead's decision; do not
   expose it in public demos or broader dashboard memberships.
4. Build from the ready evidence tasks: WeftOS baseline gate; RuView hardware
   and real-room path; oil-rig paper and sensor fixtures; Cardano stable-circuit
   evaluation; Urth R1–R3 before the region pilot.

The RuView and oil-rig source notes currently live in local project working
trees. Keep their project-specific evidence there; the dashboard stores
outcomes, task links, and safe summaries.
