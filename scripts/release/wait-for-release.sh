#!/usr/bin/env bash
# Wait until the GitHub Release for a tag exists, following the `Release`
# workflow (release.yml) run for the same commit instead of a fixed clock.
#
# release.yml creates the Release only in its final `host` job, after the
# whole cargo-dist matrix. That matrix takes 57-66 min in practice (and far
# longer when a runner queues), so a fixed 60 min poll raced it and lost
# (0eff3101). This waits as long as the Release run is alive and fails
# immediately, with the reason, when it ends without producing the Release.
#
# Usage: wait-for-release.sh <tag> <commit-sha> [max-seconds]
# Needs: GH_TOKEN, GH_REPO (or a git checkout), `actions: read` + contents read.
# Exit: 0 release exists; 1 release run ended without it; 2 timed out;
#       64 bad arguments.
set -uo pipefail

TAG="${1:-}"; SHA="${2:-}"; MAX="${3:-20700}"   # default 5h45m < 6h job cap
if [ -z "$TAG" ] || [ -z "$SHA" ] || ! [[ "$MAX" =~ ^[0-9]+$ ]]; then
  echo "usage: wait-for-release.sh <tag> <commit-sha> [max-seconds]" >&2
  exit 64
fi
INTERVAL="${WAIT_INTERVAL:-30}"
GRACE="${WAIT_NO_RUN_GRACE:-600}"   # time allowed for the Release run to appear

start=$(date +%s)
while :; do
  if gh release view "$TAG" >/dev/null 2>&1; then
    echo "Release $TAG found"
    exit 0
  fi

  # Newest Release-workflow run for this exact commit.
  run=$(gh run list --workflow release.yml --commit "$SHA" --limit 1 \
          --json databaseId,status,conclusion \
          --jq '.[0] | "\(.databaseId) \(.status) \(.conclusion)"' 2>/dev/null || true)
  elapsed=$(( $(date +%s) - start ))

  if [ -z "$run" ] || [ "$run" = "null null null" ]; then
    if [ "$elapsed" -ge "$GRACE" ]; then
      echo "::error::No Release workflow run for $SHA after ${GRACE}s; release $TAG will not appear."
      exit 1
    fi
    echo "No Release run for $SHA yet (${elapsed}s)"
  else
    read -r id status conclusion <<<"$run"
    if [ "$status" = "completed" ]; then
      # Re-check once: the release may have landed between the two calls.
      if gh release view "$TAG" >/dev/null 2>&1; then
        echo "Release $TAG found"
        exit 0
      fi
      echo "::error::Release run $id ended '$conclusion' without creating release $TAG."
      exit 1
    fi
    echo "Release run $id is $status; waiting for release $TAG (${elapsed}s)"
  fi

  if [ "$elapsed" -ge "$MAX" ]; then
    echo "::error::Release $TAG still absent after ${MAX}s."
    exit 2
  fi
  sleep "$INTERVAL"
done
