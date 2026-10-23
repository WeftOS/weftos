#!/usr/bin/env bash
# Post-publish / local smoke for the weft gateway image (WEFT-550).
# Usage: scripts/release/docker-smoke.sh <image> [host-port]
# Starts the image's default `gateway`, publishes the API port, and requires
# GET /api/health -> 200 within 30s. Always removes its container.
set -uo pipefail

IMAGE="${1:?usage: docker-smoke.sh <image> [host-port]}"
PORT="${2:-8080}"
NAME="weft-release-smoke"
BODY="$(mktemp)"

cleanup() {
  docker stop "$NAME" >/dev/null 2>&1 || true
  docker rm   "$NAME" >/dev/null 2>&1 || true
  rm -f "$BODY"
}
trap cleanup EXIT
docker rm -f "$NAME" >/dev/null 2>&1 || true

docker run -d --name "$NAME" -p "${PORT}:8080" "$IMAGE" gateway

# Fail fast if the container crashes during the first 3 seconds.
sleep 3
if ! docker ps --filter "name=$NAME" --format "{{.Names}}" | grep -q "$NAME"; then
  echo "::error::Image exited within 3 seconds."
  docker logs "$NAME" || true
  exit 1
fi

# Bounded /api/health probe (30s budget).
deadline=$(( $(date +%s) + 30 ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  code=$(curl -s -o "$BODY" -w '%{http_code}' --max-time 3 \
           "http://127.0.0.1:${PORT}/api/health" || echo "000")
  if [ "$code" = "200" ]; then
    echo "Image /api/health returned 200."
    cat "$BODY"; echo
    exit 0
  fi
  echo "  /api/health returned ${code}, retrying..."
  sleep 2
done
echo "::error::Image /api/health never returned 200 within 30s."
docker logs "$NAME" || true
exit 1
