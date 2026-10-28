# Docker Deployment

> **Public site:** [Fumadocs — Docker Deployment](../src/content/docs/weftos/guides/deployment-docker.mdx)
> (WEFT-468 dual surface — see [README.md](./README.md)).

WeftOS ships a multi-arch Docker image based on `alpine:3.21`. The image is
**self-contained**: a multi-stage build compiles a static musl `weft` binary
and `weaver` from the repository tag (or local checkout), then copies them
into a minimal Alpine runtime. There is no download of cargo-dist / GitHub Release assets at
image-build time — the image tag is not coupled to whether platform binaries
published successfully (WEFT-594 decision; see also WEFT-593).

> **Registry move (2026-10-27):** images now publish to `ghcr.io/weftos/weftos`.
> Images published before the move stay under `ghcr.io/weave-logic-ai/weftos`.

Compressed size is typically ~30–40 MB; supported architectures are
`linux/amd64` and `linux/arm64`.

## Strategy (WEFT-594)

| Option | Chosen? | Why |
|--------|---------|-----|
| Download release musl tarball into Alpine | No | Couples image tag to cargo-dist binary publish; broke when the plan matrix was empty (v0.6.21 / WEFT-593). |
| Self-contained multi-stage + QEMU multi-arch | No | arm64 Rust compile under QEMU is impractically slow in CI. |
| Self-contained multi-stage + **native** multi-arch | **Yes** | Compile from source on `ubuntu-latest` (amd64) and `ubuntu-24.04-arm` (arm64); merge digests into one OCI manifest. No QEMU. |
| Single-arch only | No | arm64 is first-class (Apple Silicon hosts, ARM cloud). |

CI: `.github/workflows/release-docker.yml` — per-arch digests, then
`docker buildx imagetools create` for `latest` and `vX.Y.Z`.

Local: `Dockerfile` at the repo root (same self-contained multi-stage).
Kernel-only variant: `crates/clawft-kernel/Dockerfile.alpine` (builds
`weaver`, not the full CLI).

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) 20.10 or later (with
  `buildx` for multi-arch builds), **or** a compatible OCI runtime (see
  [Local runtimes on macOS](#local-runtimes-on-macos) below).

## Quick Start

Pull the published image and run the gateway:

```bash
docker pull ghcr.io/weftos/weftos:latest
docker run --rm -it ghcr.io/weftos/weftos:latest --version
```

By default the entrypoint is `weft` and the default command is `gateway`,
so a bare `docker run ... weftos:latest` starts the gateway on port 8080.
For `gateway` the entrypoint (`scripts/docker/entrypoint.sh`) first starts
`weaver kernel start --foreground` in the background and waits up to 30
seconds for `weaver kernel status` to succeed, because the gateway reports
kernel reachability on `/api/health` (ADR-102 D1). Set
`WEFT_NO_EMBEDDED_KERNEL=1` to skip that. Any other command is passed to
`weft` unchanged; `weaver` is on the path for `docker exec`.

## Image Tags

| Tag         | Contents                                            |
|-------------|-----------------------------------------------------|
| `latest`    | Most recent release (rolls forward on every release)|
| `X.Y.Z`     | Pinned release, no leading `v` (e.g. `0.8.4`)       |

The workflow tags the image with the tag name minus its `v`
(`VERSION="${TAG#v}"` in `release-docker.yml`). It runs after any successful
`Release` run on a version tag and does not exclude release candidates, so
`latest` can point at a pre-release. Pin to a version in production.

## Configuration

The container runs as the unprivileged `weft` user with `$HOME=/home/weft`.
The canonical config directory inside the container is
`/home/weft/.clawft` -- mount your host config there. `/home/weft/.weftos`
holds kernel state and is declared as a second volume.

The image ships a default `config.json` that enables the auth-gated REST/WS
API on port 8080 so `weft gateway` starts out of the box. Only `/api/health`
and the token-bootstrap path are public; other routes require a Bearer token.

### Mounting Config

```bash
docker run --rm -it \
  -v "$HOME/.clawft:/home/weft/.clawft:ro" \
  ghcr.io/weftos/weftos:latest gateway
```

### Environment Variables

Pass API keys and runtime settings via environment variables. Common ones:

```bash
docker run --rm -it \
  -e OPENAI_API_KEY="sk-..." \
  -e CLAWFT_CONFIG="/home/weft/.clawft/config.json" \
  -e RUST_LOG=info \
  -v "$HOME/.clawft:/home/weft/.clawft:ro" \
  ghcr.io/weftos/weftos:latest gateway
```

### Persisting Workspace State

To persist sessions, memory, and skills across restarts, mount a writable
volume on the same path:

```bash
docker run --rm -it \
  -v weftos-data:/home/weft/.clawft \
  ghcr.io/weftos/weftos:latest gateway
```

## Docker Compose

A working compose definition ships under `scripts/deploy/docker-compose.yml`.
Run it directly:

```bash
docker compose -f scripts/deploy/docker-compose.yml up -d
docker compose -f scripts/deploy/docker-compose.yml logs -f
```

The shipped file uses a named volume (`weft-config`) for state and exposes
port 8080. Override the version with the `WEFT_VERSION` environment
variable:

```bash
WEFT_VERSION=0.8.4 docker compose -f scripts/deploy/docker-compose.yml up -d
```

Inline equivalent:

```yaml
services:
  weft:
    image: ghcr.io/weftos/weftos:${WEFT_VERSION:-latest}
    command: ["gateway"]
    restart: unless-stopped
    ports:
      - "8080:8080"
    volumes:
      - weft-config:/home/weft/.clawft
    environment:
      - RUST_LOG=info
      - OPENAI_API_KEY=${OPENAI_API_KEY}
    healthcheck:
      test: ["CMD", "weft", "status"]
      interval: 30s
      timeout: 5s
      start_period: 10s
      retries: 3

volumes:
  weft-config:
```

## VPS One-Click Deploy

`scripts/deploy/vps-deploy.sh` wraps `docker run` with sensible defaults
for a single-host VPS install:

```bash
./scripts/deploy/vps-deploy.sh --pull
```

Flags: `--image`, `--port`, `--name`, `--config`, `--restart`, `--pull`,
`--health-timeout`, `--no-health`.

### Health-probe rollback (WEFT-456)

On upgrade the script **does not** destroy the running container first.
It stops and renames it to `<name>-previous`, starts the new container
under the original name, then probes `GET /api/health` on the host port
(same shape as the WEFT-550 post-publish smoke: 3s crash check + bounded
HTTP 200 deadline, default 30s).

| Outcome | Action |
|---------|--------|
| Probe returns 200 | Previous container is removed. |
| Crash within 3s / probe timeout / non-200 | New container is removed; previous is renamed back and started. |

Pass `--no-health` only when you intentionally want the old stop-and-
replace behaviour (e.g. offline hosts without `curl`).

#### Manual smoke (rollback path)

```bash
# Known-good baseline
./scripts/deploy/vps-deploy.sh --name weft-smoke --port 18080
curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18080/api/health  # 200

# Force an unhealthy deploy (alpine has no `gateway` binary → exits)
docker pull alpine:3.21
docker tag alpine:3.21 weft-smoke-unhealthy:local
./scripts/deploy/vps-deploy.sh --name weft-smoke --port 18080 \
  --image weft-smoke-unhealthy:local --health-timeout 8 || true

# Previous container restored; health still 200
curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18080/api/health
docker rm -f weft-smoke weft-smoke-previous 2>/dev/null || true
```

## Health Checks

The image declares a `HEALTHCHECK` that runs `weft status` every 30
seconds. The same probe is shown in the compose example above. The release
pipeline additionally probes `GET /api/health` (WEFT-550) after publish.

To check health from the host:

```bash
docker inspect --format '{{.State.Health.Status}}' weft
docker exec weft weft status --detailed
curl -sS http://127.0.0.1:8080/api/health
```

## Building Locally

Build from the repo root (compiles inside Docker — no pre-built binary
required):

```bash
# Host arch only (fast iteration)
./scripts/build/docker-build.sh --tag dev --version dev

# Or plain Docker / buildx
docker build -t weft:dev --build-arg VERSION=dev .
```

Multi-arch locally requires a builder that can reach both platforms (on
Apple Silicon, buildx can produce `linux/arm64` natively and `linux/amd64`
via emulation — fine for one-off publishes, not what CI uses):

```bash
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --build-arg VERSION=0.8.4 \
  -t weftos:local \
  --load .
```

For a kernel-only image (`weaver`):

```bash
docker build -f crates/clawft-kernel/Dockerfile.alpine -t weft-kernel:dev .
```

## CI / Release Pipeline

The image is built and published by `.github/workflows/release-docker.yml`,
triggered when the `Release` workflow (cargo-dist) succeeds on a tag push,
or via `workflow_dispatch`. The workflow:

1. Gates on Release success (orchestration only — binaries are **not**
   downloaded into the image).
2. Checks out the release tag.
3. Builds **natively** on `ubuntu-latest` (`linux/amd64`) and
   `ubuntu-24.04-arm` (`linux/arm64`) — no QEMU for Rust compilation.
4. Pushes per-platform digests, then creates a multi-arch manifest tagged
   `latest` and `vX.Y.Z` on GHCR.
5. Runs the post-publish `/api/health` smoke (WEFT-550).

See `docs/deployment/release.md` for how Docker fits into the full release
graph.

## Local runtimes on macOS

The published artifact is a standard multi-arch OCI image. Any runtime that
speaks OCI/GHCR can consume the same tags:

| Runtime | Notes |
|---------|--------|
| **Docker Desktop** | Pulls the matching `linux/arm64` layer on Apple Silicon; `linux/amd64` via Rosetta/QEMU if forced with `--platform`. |
| **OrbStack** | Drop-in Docker-compatible engine; uses the same `docker pull/run` commands against `ghcr.io/weftos/weftos:*`. Prefer for lighter local resource use. |
| **Apple container CLI** | Apple's OCI-oriented tooling (`container pull` / `container run`) can pull the same GHCR multi-arch image and run the `linux/arm64` manifest entry on Apple Silicon — same image digest family as Docker/OrbStack, not a separate build. |

Example with Docker or OrbStack (identical CLI):

```bash
docker pull ghcr.io/weftos/weftos:latest
docker run --rm -p 8080:8080 ghcr.io/weftos/weftos:latest gateway
```

There is no separate "Apple Silicon image" — multi-arch is the product.

## Security Considerations

The image is small but not minimal -- it includes Alpine + musl libc +
`ca-certificates` + `tzdata`. Hardening recommendations:

- **Run as the built-in unprivileged user.** The image already declares
  `USER weft`. Don't override with `--user 0:0` unless you know why.
- **Mount config read-only.** Use `:ro` on the config bind mount; only
  the workspace volume needs to be writable.
- **Restrict egress.** For network-isolated deployments use a dedicated
  Docker network or `--network=none` for offline workflows.
- **Never bake secrets into the image.** Pass keys via environment
  variables, Docker secrets, or a sidecar secrets manager.
- **Pin tags in production.** `latest` is convenient for local dev; pin
  to `vX.Y.Z` in any deployment you don't want to silently roll forward.

## Troubleshooting

**Container exits immediately.** Inspect with:

```bash
docker run --rm -it \
  -v "$HOME/.clawft:/home/weft/.clawft:ro" \
  ghcr.io/weftos/weftos:latest status --detailed
```

Common causes: missing config, invalid JSON, an LLM provider env var
referenced by config but not set in the container's environment.

**Permission denied on config file.** The container runs as a non-root
`weft` user. Make sure the host config is readable by anyone:

```bash
chmod 644 ~/.clawft/config.json
```

**Cannot reach LLM API.** Verify DNS and outbound network access from
inside the container:

```bash
docker run --rm ghcr.io/weftos/weftos:latest agent -m "ping"
```

If running behind a corporate proxy, propagate the proxy env vars:

```bash
docker run --rm \
  -e HTTPS_PROXY="http://proxy:8080" \
  -e HTTP_PROXY="http://proxy:8080" \
  -e NO_PROXY="localhost,127.0.0.1" \
  ghcr.io/weftos/weftos:latest agent -m "ping"
```

---

## Appendix: Legacy Build Approaches (Historical)

Earlier releases used different image layouts. Kept only so operators
upgrading from old documentation can map history.

### Download-based Alpine (pre-self-contained, ~0.4.3–0.6.x)

The root Dockerfile downloaded a cargo-dist musl tarball for `VERSION`
from GitHub Releases and installed it into Alpine. Build was ~2 minutes
and produced a ~15 MB image, but the image tag was coupled to binary
publish success — empty cargo-dist matrices (WEFT-593) produced 404s at
image build. Replaced by the self-contained multi-stage Dockerfile
(WEFT-594).

### `FROM scratch` (pre-0.4.2, deprecated)

Sprint 11 / 12 images were built `FROM scratch`, copying a single
statically-linked musl binary into the empty filesystem (~5 MB compressed).
Dropped for lack of `ca-certificates`, `tzdata`, and a shell for ops.

### `cargo-chef` multi-stage on Debian (proposed, never shipped)

A K2-era proposal used `debian:bookworm-slim` + `cargo-chef`. Never
adopted: longer cold builds, larger runtime (~50 MB), and QEMU multi-arch
cost. The current Alpine self-contained layout is the shipping path.
