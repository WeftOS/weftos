# WEFT-594 result — release Docker image strategy

**Status:** decided + aligned  
**Branch:** `wave0b/weft-594-docker-strategy`  
**Date:** 2026-07-30  
**Blocked-by:** WEFT-593 (DONE — cargo-dist plan matrix no longer emptied by secret-scan)

## Decision

**Self-contained multi-stage Dockerfile + native multi-arch runners (no QEMU).**

| Candidate | Verdict | Rationale |
|-----------|---------|-----------|
| Download musl tarball from GitHub Release into Alpine | Rejected | Couples image tag to cargo-dist binary publish. Failed when the plan matrix was empty (v0.6.21 / WEFT-593). Even after WEFT-593, download-coupling remains a latent failure mode. |
| Self-contained + single QEMU multi-arch job | Rejected | arm64 Rust compile under QEMU is impractically slow for CI. |
| Self-contained + **native** amd64/arm64 jobs + manifest merge | **Chosen** | Compile from the tag on `ubuntu-latest` and `ubuntu-24.04-arm`; push digests; `docker buildx imagetools create` for `latest` / `vX.Y.Z`. No QEMU for the compile step. |
| Single-arch only (`linux/amd64`) | Rejected | Drops first-class arm64 (Apple Silicon hosts, ARM cloud). |

Dockerfile was already self-contained (commit `7ba56cd2` / multi-stage
`rust:1.93-alpine` → `alpine:3.21`). WEFT-594 is the **strategy lock + CI/docs
alignment**, not a second Dockerfile rewrite.

## What shipped

| Area | Change |
|------|--------|
| `.github/workflows/release-docker.yml` | Native matrix (`ubuntu-latest` / `ubuntu-24.04-arm`); drop `setup-qemu-action`; push-by-digest; merge job; keep WEFT-550 `/api/health` smoke. Still gated on Release success for orchestration, **not** for binary assets. |
| `docs/deployment/docker.md` | Rewrite for self-contained strategy; strategy table; CI steps; OrbStack + Apple container CLI note; legacy download appendix. |
| `docs/deployment/release.md` | Docker leg documents self-contained + native multi-arch. |
| `docs/src/content/docs/weftos/guides/deployment-docker.mdx` | Intro + multi-arch section no longer describe download/cargo-chef/Debian. |
| `scripts/build/docker-build.sh` | Comments match self-contained build; size ceiling 20→50 MB. |
| `crates/clawft-kernel/Dockerfile.alpine` | Comments no longer claim root Dockerfile downloads releases. |

## Acceptance criteria

| Criterion | Status |
|-----------|--------|
| Decide download vs self-contained (cross/native, not QEMU) vs single-arch | **Done** — self-contained + native multi-arch |
| ghcr image builds + WEFT-550 `/api/health` smoke | Workflow still runs smoke after merge; full end-to-end needs next tag Release (or `workflow_dispatch` on a tag). Local image path unchanged (`docker build` / `docker-build.sh`). |

## Local runtimes (same OCI image)

Published multi-arch OCI image `ghcr.io/weftos/weftos:*` is runtime-
agnostic:

- **Docker Desktop** / **OrbStack** — `docker pull` / `docker run` (arm64 layer
  natively on Apple Silicon).
- **Apple container CLI** — same GHCR tags; pulls the `linux/arm64` manifest
  entry. No separate Apple-only image.

## Out of scope / follow-ups

- Cross-compile-in-Dockerfile (single amd64 runner → aarch64-musl via zig/cross)
  is a valid fallback if arm runners ever unavailable; not required today.
- Full rewrite of remaining stale MDX paths (e.g. `/root/.clawft` in older
  examples) — partial update only; operator source of truth is
  `docs/deployment/docker.md`.
- Actual GHCR publish verification waits on the next successful tag Release
  after this branch merges to the integration line.

## Files

- `.github/workflows/release-docker.yml`
- `docs/deployment/docker.md`
- `docs/deployment/release.md`
- `docs/src/content/docs/weftos/guides/deployment-docker.mdx`
- `scripts/build/docker-build.sh`
- `crates/clawft-kernel/Dockerfile.alpine`
- `docs/plans/wave-0b-WEFT-594-result.md` — this file
