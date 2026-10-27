# WeaveLogic Web Properties — Deploy Origins

Pin-down of where each ADR-015 web property is built and deployed from, so
operators of **this** repo (`WeftOS/weftos`, formerly clawft) know what
is in-tree vs out-of-tree.

Source of the open question: `.planning/reviews/0.7.0-release-gate/14-deployment-release.md`
(“Is `assess.weavelogic.ai` actually deployed?”). Closed for docs by **WEFT-474**.

Architectural intent: [ADR-015 — Three-Property Web Architecture](../adr/adr-015-three-property-web.md).
Strategy detail: [Web Presence Strategy](../weftos/web-presence-strategy.md).

## Status summary (verified 2026-07-31)

| Property | Public URL | DNS / live? | Deploy origin | Hosted on |
|----------|------------|-------------|---------------|-----------|
| Marketing | https://weavelogic.ai | Yes (A → Google Frontend) | Sibling repo [`weave-logic-ai/weavelogic.ai`](https://github.com/weave-logic-ai/weavelogic.ai) (private) | GCP (Next.js; `server: Google Frontend`) |
| Docs / platform | https://weftos.weavelogic.ai | Yes (CNAME → Vercel) | **This repo** — Fumadocs under `docs/src/` | Vercel |
| Assessment product | https://assess.weavelogic.ai | **No — NXDOMAIN** | Sibling project `agentic_ai_assessor` (not this repo; see below) | **Not deployed** |

None of the GitHub Actions under `.github/workflows/` in **weftos** target
`assess.weavelogic.ai`. That is expected: the assessment app is intentionally
out of this monorepo.

---

## 1. weavelogic.ai (marketing)

| Attribute | Value |
|-----------|--------|
| Role | Buyer-facing marketing + product landing |
| Repo | https://github.com/weave-logic-ai/weavelogic.ai (private, org-internal) |
| Historical workspace path | `/claw/root/weavelogic/projects/weavelogic.ai/` |
| Live check (2026-07-31) | HTTP 200; Next.js; Google Frontend |
| CI in weftos? | No |

**Interim assessment intake:** The marketing site serves an “AI Maturity
Assessment” flow at **https://weavelogic.ai/assess**. That path is part of the
marketing property, **not** the standalone assessor app. Do not treat
`/assess` as proof that `assess.weavelogic.ai` is up.

---

## 2. weftos.weavelogic.ai (docs / platform)

| Attribute | Value |
|-----------|--------|
| Role | WeftOS platform documentation (Fumadocs) |
| Repo | **This repo** — https://github.com/WeftOS/weftos |
| In-tree source | `docs/src/` |
| Live check (2026-07-31) | HTTP 200; `server: Vercel`; CNAME `*.vercel-dns-016.com` |
| Related artifacts | `install.sh` published at https://weftos.weavelogic.ai/install.sh (see [install.md](./install.md)) |

Release and install runbooks for **WeftOS binaries / Docker / WASM** remain
under this directory (`release.md`, `docker.md`, `wasm.md`, `install.md`).
They do not deploy the marketing or assessor properties.

---

## 3. assess.weavelogic.ai (assessment product) — WEFT-474

### Confirmed status: **not deployed**

As of **2026-07-31**:

- `host assess.weavelogic.ai` → **NXDOMAIN** (no A/AAAA/CNAME).
- `curl https://assess.weavelogic.ai` → cannot resolve host.
- No workflow, Dockerfile, or deploy script in **weftos** references this host.
- ADR-015 still names the property as a **planned** third surface; it is not
  dead. No ADR amendment required — only this origin pin-down.

### Deploy origin (sibling project, not weftos)

| Attribute | Value |
|-----------|--------|
| Project name | `agentic_ai_assessor` |
| Role | Assessment product app: intake, scoring, reports, admin |
| Historical workspace path | `/claw/root/weavelogic/projects/agentic_ai_assessor/` |
| Product BRD (historical path) | `…/agentic_ai_assessor/docs/BUSINESS_REQUIREMENTS.md` |
| Cross-refs in this repo | ADR-015; `docs/weftos/web-presence-strategy.md`; `.planning/sparc/weftos/0.1/10-sprint-plan.md` (W8) |
| GitHub under `weave-logic-ai` | **No repo named `agentic_ai_assessor` listed** in the org inventory checked 2026-07-31 — treat as sibling / offline workspace until published under the org |
| CI in weftos? | **No** — do not add assessor deploy jobs here |

Planned stack (strategy doc, not production reality yet):

| Layer | Planned hosting |
|-------|-----------------|
| Frontend (Next.js) | Vercel |
| API / backend (Express + AI / discovery workloads) | GCP Cloud Run |
| DNS | `assess.weavelogic.ai` CNAME → assessor hosting (not yet configured) |

See strategy §7: [Web Presence Strategy — Cross-Property Technical Architecture](../weftos/web-presence-strategy.md#71-dns-configuration).

### What operators of weftos should do

1. **Do not** look for assessor deploy pipelines in this repository.
2. **Do** link buyers to https://weavelogic.ai/assess for the current public
   intake until the standalone property is live.
3. When `agentic_ai_assessor` is published under `weave-logic-ai` and DNS is
   cut over, update the status table in this file and (optionally) add a short
   “live” note under ADR-015 consequences — still no need to fold deploy CI
   into weftos.

### Acceptance map (WEFT-474)

| Criterion | Result |
|-----------|--------|
| Status confirmed (deployed / not / from-where) | **Not deployed** (NXDOMAIN). Origin: sibling `agentic_ai_assessor`, not weftos. |
| Origin documented in release runbook or `docs/deployment/` | This file. |
| Sibling link or ADR-015 amend if dead | Sibling project + marketing interim URL documented; property is planned, **not** dead → ADR-015 left as Accepted. |

---

## Related

- [ADR-015](../adr/adr-015-three-property-web.md)
- [Web presence strategy](../weftos/web-presence-strategy.md)
- [Deployment SOPs — WeaveLogic properties appendix](../guides/weftos-deployment-sops.md) (clawft + weavelogic.ai SOP paths)
- [Release process (WeftOS tag ship)](./release.md)
