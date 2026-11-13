#!/usr/bin/env bash
# Dry evaluation of the release image tag logic (WO-199).
# Run: bash tests/scripts/test_docker_tags.sh
# Prints the tags the legacy logic and the fixed logic produce for each
# version, and fails if a prerelease would move :latest or a stable would not.
set -uo pipefail
S="$(cd "$(dirname "$0")/../../scripts/release" && pwd)/docker-tags.sh"
fail=0
check() { # version expect_latest(yes|no)
  local v="$1" want="$2" got
  printf '%-18s legacy: %-22s fixed: %-22s' "$v" \
    "$("$S" "$v" --legacy | paste -sd, -)" "$("$S" "$v" | paste -sd, -)"
  if "$S" "$v" | grep -qx latest; then got=yes; else got=no; fi
  if [ "$got" = "$want" ]; then echo " ok"; else echo " FAIL (latest=$got, want $want)"; fail=1; fi
}
check v0.8.5 yes
check 0.8.5 yes
check v10.20.30 yes
check v0.8.5-rc.1 no
check v0.8.5-rc.15 no
check v0.9.0-alpha.2 no
check v0.9.0-beta no
check v0.8.5+build.7 no
if "$S" "main" >/dev/null 2>&1; then echo "non-version accepted: FAIL"; fail=1; else echo "non-version rejected: ok"; fi
exit $fail
