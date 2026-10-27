#!/usr/bin/env bash
# Build a local Docker image for weft (clawft CLI).
#
# Wraps the canonical self-contained multi-stage Dockerfile (WEFT-594):
# compiles static musl `weft` from the workspace inside the builder
# stage — no GitHub Release asset download. Intended for local smoke
# testing; release-docker.yml publishes multi-arch images to GHCR on
# native amd64 + arm64 runners (no QEMU).
#
# Image name + tag are aligned with docker-compose.yml and the
# release-docker.yml workflow (WEFT-441 / WEFT-450 / WEFT-594):
#   ghcr.io/weftos/weftos:<tag>
#
# The host architecture is auto-detected and passed to buildx so the
# resulting image targets the local machine. Cross-arch / multi-arch
# builds are out of scope here — those go through release-docker.yml.
#
# Usage:
#   ./scripts/build/docker-build.sh [--tag TAG] [--version VERSION]
#                                   [--image NAME] [--platform PLATFORM]
#                                   [--push]
#
# Examples:
#   ./scripts/build/docker-build.sh --tag dev --version dev
#   ./scripts/build/docker-build.sh --tag v0.7.0 --version 0.7.0 --push
#   ./scripts/build/docker-build.sh --platform linux/arm64 --version 0.7.0

set -euo pipefail

# --- Colors ---
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# --- Helpers ---
info()  { printf "${CYAN}[INFO]${NC}  %s\n" "$*"; }
ok()    { printf "${GREEN}[OK]${NC}    %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${NC}  %s\n" "$*"; }
err()   { printf "${RED}[ERROR]${NC} %s\n" "$*" >&2; }

# Default image / repo name. Kept in lockstep with docker-compose.yml
# and release-docker.yml's $IMAGE_NAME.
DEFAULT_IMAGE_REPO="ghcr.io/weftos/weftos"

usage() {
    cat <<EOF
Build a local Docker image for weft (clawft CLI).

Usage: docker-build.sh [options]

Options:
  --tag <tag>           Docker image tag (default: latest).
  --version <ver>       Release version baked into the image as the
                        VERSION build-arg. Required when --tag is not a
                        SemVer tag like v0.7.0; otherwise derived by
                        stripping the leading 'v'.
  --image <name>        Override the image repository name.
                        Default: ${DEFAULT_IMAGE_REPO}
  --platform <plat>     buildx --platform value (default: auto-detected
                        from the host: linux/amd64 or linux/arm64).
  --push                Push the image to the registry after building.
                        Requires prior \`docker login\`.
  --help                Show this help message.

Examples:
  docker-build.sh --version 0.6.19
  docker-build.sh --tag v0.7.0 --version 0.7.0 --push
  docker-build.sh --platform linux/arm64 --version 0.7.0
EOF
    exit 0
}

# --- Parse arguments ---
IMAGE_TAG="latest"
IMAGE_REPO="${DEFAULT_IMAGE_REPO}"
VERSION=""
PLATFORM=""
PUSH=false

while [ $# -gt 0 ]; do
    case "$1" in
        --tag)
            [ $# -ge 2 ] || { err "--tag requires a value"; exit 1; }
            IMAGE_TAG="$2"; shift 2 ;;
        --version)
            [ $# -ge 2 ] || { err "--version requires a value"; exit 1; }
            VERSION="$2"; shift 2 ;;
        --image)
            [ $# -ge 2 ] || { err "--image requires a value"; exit 1; }
            IMAGE_REPO="$2"; shift 2 ;;
        --platform)
            [ $# -ge 2 ] || { err "--platform requires a value"; exit 1; }
            PLATFORM="$2"; shift 2 ;;
        --push)  PUSH=true; shift ;;
        --help|-h) usage ;;
        *) err "Unknown option: $1"; printf "\n"; usage ;;
    esac
done

# --- Derive VERSION from tag when possible ---
if [ -z "$VERSION" ]; then
    case "$IMAGE_TAG" in
        v*.*.*) VERSION="${IMAGE_TAG#v}" ;;
        *) err "--version is required when --tag is not v<semver> (got '${IMAGE_TAG}')"
           exit 1 ;;
    esac
fi

# --- Auto-detect host platform ---
if [ -z "$PLATFORM" ]; then
    HOST_ARCH=$(uname -m)
    case "$HOST_ARCH" in
        x86_64|amd64) PLATFORM="linux/amd64" ;;
        aarch64|arm64) PLATFORM="linux/arm64" ;;
        *) err "Unsupported host arch: ${HOST_ARCH}. Pass --platform explicitly."
           exit 1 ;;
    esac
fi

IMAGE_NAME="${IMAGE_REPO}:${IMAGE_TAG}"

# --- Resolve workspace root ---
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

info "Workspace root: $WORKSPACE_ROOT"
info "Image:          $IMAGE_NAME"
info "Version arg:    $VERSION"
info "Platform:       $PLATFORM"

# --- Check prerequisites ---
if ! command -v docker >/dev/null 2>&1; then
    err "'docker' is not installed or not in PATH."
    exit 1
fi

if ! docker buildx version >/dev/null 2>&1; then
    err "'docker buildx' is not available — required for --platform builds."
    exit 1
fi

# --- Build via buildx ---
info "Building Docker image with buildx..."

BUILDX_OUTPUT="--load"
if [ "$PUSH" = true ]; then
    BUILDX_OUTPUT="--push"
fi

# shellcheck disable=SC2086
docker buildx build \
    --platform "$PLATFORM" \
    --build-arg "VERSION=${VERSION}" \
    -t "$IMAGE_NAME" \
    -f "$WORKSPACE_ROOT/Dockerfile" \
    $BUILDX_OUTPUT \
    "$WORKSPACE_ROOT"

ok "Docker image built: $IMAGE_NAME"

# --- Validate image size (only when loaded into local docker) ---
# Self-contained multi-stage images land ~30–40 MB (Alpine + static weft);
# the old download-based layout was ~15 MB. Bound is soft ceiling for bloat.
if [ "$PUSH" != true ]; then
    MAX_IMAGE_SIZE_MB=50
    IMAGE_SIZE_BYTES=$(docker image inspect "$IMAGE_NAME" --format='{{.Size}}' 2>/dev/null)
    IMAGE_SIZE_MB=$(echo "scale=2; $IMAGE_SIZE_BYTES / 1048576" | bc)

    info "Image size: ${IMAGE_SIZE_MB} MB"

    OVER_LIMIT=$(echo "$IMAGE_SIZE_MB > $MAX_IMAGE_SIZE_MB" | bc -l)
    if [ "$OVER_LIMIT" -eq 1 ]; then
        err "Image exceeds ${MAX_IMAGE_SIZE_MB} MB limit (${IMAGE_SIZE_MB} MB)."
        err "Check binary size and Dockerfile for unnecessary layers."
        exit 1
    fi

    ok "Image size within ${MAX_IMAGE_SIZE_MB} MB limit."

    printf "\n"
    info "Image details:"
    docker image ls "$IMAGE_NAME"
fi

printf "\n"
ok "Docker build complete: $IMAGE_NAME"
