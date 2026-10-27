#!/usr/bin/env bash
# One-click VPS deployment script for weft (clawft).
#
# Usage:
#   ./scripts/deploy/vps-deploy.sh [OPTIONS]
#
# Options:
#   --image IMAGE       Docker image to deploy (default: ghcr.io/weftos/weftos:latest)
#   --port PORT         Host port for the gateway (default: 8080)
#   --name NAME         Container name (default: weft)
#   --config DIR        Host directory for config (default: ~/.clawft)
#   --restart POLICY    Restart policy (default: unless-stopped)
#   --pull              Pull latest image before deploying
#   --health-timeout S  Seconds to wait for /api/health (default: 30)
#   --no-health         Skip health probe (no rollback; legacy behaviour)
#   --help              Show this help message
#
# Health-probe rollback (WEFT-456):
#   If a container with the same name already exists, it is stopped and
#   renamed to <name>-previous (not removed). The new container is started
#   under the original name and probed with GET /api/health (same shape as
#   WEFT-550 post-publish smoke). On success the previous container is
#   removed. On failure the new container is torn down and the previous
#   one is restored (renamed back + started).
#
# Prerequisites:
#   - Docker installed and running
#   - User in docker group (or run with sudo)
#   - curl available for the health probe (unless --no-health)
#
# Manual smoke (rollback path) — also documented in docs/deployment/docker.md:
#   ./scripts/deploy/vps-deploy.sh --name weft-smoke --port 18080
#   curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18080/api/health  # 200
#   docker pull alpine:3.21 && docker tag alpine:3.21 weft-smoke-unhealthy:local
#   ./scripts/deploy/vps-deploy.sh --name weft-smoke --port 18080 \
#       --image weft-smoke-unhealthy:local --health-timeout 8 || true
#   # Expect: new container removed, prior weft-smoke restored, /api/health 200
#   curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18080/api/health
#   docker rm -f weft-smoke weft-smoke-previous 2>/dev/null || true

set -euo pipefail

# Defaults
IMAGE="ghcr.io/weftos/weftos:latest"
PORT="8080"
CONTAINER_NAME="weft"
CONFIG_DIR="$HOME/.clawft"
RESTART_POLICY="unless-stopped"
PULL=false
HEALTH_TIMEOUT=30
SKIP_HEALTH=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --image)           IMAGE="$2";           shift 2 ;;
        --port)            PORT="$2";            shift 2 ;;
        --name)            CONTAINER_NAME="$2";  shift 2 ;;
        --config)          CONFIG_DIR="$2";      shift 2 ;;
        --restart)         RESTART_POLICY="$2";  shift 2 ;;
        --pull)            PULL=true;            shift   ;;
        --health-timeout)  HEALTH_TIMEOUT="$2";  shift 2 ;;
        --no-health)       SKIP_HEALTH=true;     shift   ;;
        --help)
            # Print comment header (lines after shebang until set -euo)
            awk 'NR==1{next} /^set -euo pipefail$/{exit} {sub(/^# ?/,""); print}' "$0"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

PREVIOUS_NAME="${CONTAINER_NAME}-previous"
HAD_PREVIOUS=false

echo "=== weft VPS deployment ==="
echo "Image:     $IMAGE"
echo "Port:      $PORT"
echo "Container: $CONTAINER_NAME"
echo "Config:    $CONFIG_DIR"
echo "Restart:   $RESTART_POLICY"
if [ "$SKIP_HEALTH" = true ]; then
    echo "Health:    skipped (--no-health)"
else
    echo "Health:    GET /api/health (timeout ${HEALTH_TIMEOUT}s, WEFT-456 rollback)"
fi
echo ""

# Check Docker
if ! command -v docker &>/dev/null; then
    echo "ERROR: Docker is not installed. Install Docker first:" >&2
    echo "  curl -fsSL https://get.docker.com | sh" >&2
    exit 1
fi

if [ "$SKIP_HEALTH" = false ] && ! command -v curl &>/dev/null; then
    echo "ERROR: curl is required for the health probe (or pass --no-health)." >&2
    exit 1
fi

# Ensure config directory exists
mkdir -p "$CONFIG_DIR"

# Pull latest image if requested
if [ "$PULL" = true ]; then
    echo "Pulling latest image..."
    docker pull "$IMAGE"
fi

container_exists() {
    docker ps -a --format '{{.Names}}' | grep -qw "$1"
}

container_running() {
    docker ps --format '{{.Names}}' | grep -qw "$1"
}

# Keep the prior container around until the new one is healthy (WEFT-456).
# Stale *-previous leftovers from a crashed earlier attempt are discarded.
if container_exists "$PREVIOUS_NAME"; then
    echo "Removing stale rollback container '$PREVIOUS_NAME'..."
    docker rm -f "$PREVIOUS_NAME" 2>/dev/null || true
fi

if container_exists "$CONTAINER_NAME"; then
    echo "Preserving existing container as '$PREVIOUS_NAME'..."
    docker stop "$CONTAINER_NAME" 2>/dev/null || true
    docker rename "$CONTAINER_NAME" "$PREVIOUS_NAME"
    HAD_PREVIOUS=true
fi

# Tear down a failed deploy and restore the previous container when present.
rollback() {
    local reason="${1:-health probe failed}"
    echo "" >&2
    echo "ERROR: Deploy failed ($reason). Rolling back..." >&2

    if container_exists "$CONTAINER_NAME"; then
        echo "  Removing failed container '$CONTAINER_NAME'..." >&2
        docker logs --tail 50 "$CONTAINER_NAME" 2>/dev/null || true
        docker stop "$CONTAINER_NAME" 2>/dev/null || true
        docker rm "$CONTAINER_NAME" 2>/dev/null || true
    fi

    if [ "$HAD_PREVIOUS" = true ] && container_exists "$PREVIOUS_NAME"; then
        echo "  Restoring previous container as '$CONTAINER_NAME'..." >&2
        docker rename "$PREVIOUS_NAME" "$CONTAINER_NAME"
        docker start "$CONTAINER_NAME"
        echo "  Rollback complete — previous container is running." >&2
    else
        echo "  No previous container to restore." >&2
    fi
    return 1
}

# Run the new container under the canonical name.
echo "Starting weft gateway..."
if ! docker run -d \
    --name "$CONTAINER_NAME" \
    --restart "$RESTART_POLICY" \
    -p "${PORT}:8080" \
    -v "${CONFIG_DIR}:/home/weft/.clawft" \
    "$IMAGE" gateway; then
    rollback "docker run failed" || exit 1
    exit 1
fi

if [ "$SKIP_HEALTH" = true ]; then
    if [ "$HAD_PREVIOUS" = true ] && container_exists "$PREVIOUS_NAME"; then
        echo "Health skipped — removing previous container '$PREVIOUS_NAME'..."
        docker rm -f "$PREVIOUS_NAME" 2>/dev/null || true
    fi
    echo ""
    echo "weft gateway is running on port $PORT (health probe skipped)"
    echo "  Container: $CONTAINER_NAME"
    echo "  Config:    $CONFIG_DIR"
    exit 0
fi

# Fail fast if the container crashes during the first 3 seconds (WEFT-550 shape).
echo "Checking container stayed up (3s)..."
sleep 3
if ! container_running "$CONTAINER_NAME"; then
    rollback "container exited within 3 seconds" || exit 1
    exit 1
fi

# Bounded GET /api/health probe (same shape as release-docker smoke, WEFT-550).
echo "Probing GET http://127.0.0.1:${PORT}/api/health (timeout ${HEALTH_TIMEOUT}s)..."
deadline=$(( $(date +%s) + HEALTH_TIMEOUT ))
health_ok=false
while [ "$(date +%s)" -lt "$deadline" ]; do
    if ! container_running "$CONTAINER_NAME"; then
        rollback "container exited during health probe" || exit 1
        exit 1
    fi
    code=$(curl -s -o /tmp/weft-vps-health.body -w '%{http_code}' \
        --max-time 3 "http://127.0.0.1:${PORT}/api/health" || echo "000")
    if [ "$code" = "200" ]; then
        echo "  /api/health returned 200."
        if [ -f /tmp/weft-vps-health.body ]; then
            cat /tmp/weft-vps-health.body
            echo
        fi
        health_ok=true
        break
    fi
    echo "  /api/health returned ${code}, retrying..."
    sleep 2
done

if [ "$health_ok" != true ]; then
    rollback "/api/health never returned 200 within ${HEALTH_TIMEOUT}s" || exit 1
    exit 1
fi

# Health passed — drop the previous container.
if [ "$HAD_PREVIOUS" = true ] && container_exists "$PREVIOUS_NAME"; then
    echo "Health OK — removing previous container '$PREVIOUS_NAME'..."
    docker rm -f "$PREVIOUS_NAME" 2>/dev/null || true
fi

echo ""
echo "weft gateway is running on port $PORT"
echo "  Container: $CONTAINER_NAME"
echo "  Config:    $CONFIG_DIR"
echo "  Health:    GET /api/health → 200"
echo ""
echo "Useful commands:"
echo "  docker logs -f $CONTAINER_NAME    # View logs"
echo "  docker stop $CONTAINER_NAME       # Stop"
echo "  docker restart $CONTAINER_NAME    # Restart"
echo "  curl -sS http://127.0.0.1:${PORT}/api/health"
