#!/usr/bin/env bash
# Image tags for a release version (WO-199).
# Usage: scripts/release/docker-tags.sh <version> [--legacy]
# Prints one tag per line. `latest` is printed only for a stable X.Y.Z
# version; any prerelease (rc, alpha, beta, ...) or build-metadata version
# gets its own version tag only, so an rc never moves :latest.
# --legacy reproduces the pre-fix behaviour (latest always), for the
# before/after comparison in tests/scripts/test_docker_tags.sh.
set -euo pipefail

VERSION="${1:?usage: docker-tags.sh <version> [--legacy]}"
VERSION="${VERSION#v}"
if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+[0-9A-Za-z.+_-]*$'; then
  echo "docker-tags.sh: '$VERSION' is not a release version" >&2
  exit 64
fi
echo "$VERSION"
if [ "${2:-}" = "--legacy" ] || printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "latest"
fi
