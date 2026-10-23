#!/bin/sh
# Container entrypoint for the weft image.
#
# `weft gateway` serves /api/health as daemon reachability (ADR-102 D1: 503
# "degraded" without a kernel), so a gateway container needs a kernel beside
# it. For `gateway`, start the kernel daemon first and wait for it to listen;
# every other subcommand is passed straight to weft unchanged.
set -eu

if [ "${1:-}" = "gateway" ] && [ "${WEFT_NO_EMBEDDED_KERNEL:-}" != "1" ]; then
  /usr/local/bin/weaver kernel start --foreground &
  KERNEL_PID=$!
  i=0
  until /usr/local/bin/weaver kernel status >/dev/null 2>&1; do
    i=$((i + 1))
    if ! kill -0 "$KERNEL_PID" 2>/dev/null; then
      echo "entrypoint: kernel daemon exited during startup" >&2
      exit 1
    fi
    if [ "$i" -ge 60 ]; then
      echo "entrypoint: kernel daemon not ready after 30s" >&2
      exit 1
    fi
    sleep 0.5
  done
fi

exec /usr/local/bin/weft "$@"
