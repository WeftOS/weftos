#!/usr/bin/env bash
# Colour snapshot of the egui GUI surfaces (tessellation level, no pixels).
#
#   scripts/gui-colour-snapshot.sh <out-dir>
#   scripts/gui-colour-compare.py <base-out-dir> <tip-out-dir>
#
# Runs every `colsnap_*` test of clawft-gui-egui with GUI_SNAP_OUT=<out-dir>, so
# each surface writes <surface>.txt: every tessellated vertex (position, uv,
# rgba) plus the text strings. Run it once at the commit to compare against and
# once at the commit under test, then diff the two directories with the compare
# script. See docs/development/gui-colour-snapshot.md.
#
# Per the build rules this belongs on a CI lane (weftos-ci), not on the Mac.
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: $0 <out-dir>" >&2
    exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$1"
OUT="$(cd "$1" && pwd)"
rm -f "$OUT"/*.txt

# A private data dir keeps the Agent Workspace layout file out of the run.
XDG="$(mktemp -d)"
trap 'rm -rf "$XDG"' EXIT

GUI_SNAP_OUT="$OUT" XDG_DATA_HOME="$XDG" \
    "$ROOT/scripts/build.sh" test clawft-gui-egui --no-fail-fast --filter colsnap_

echo "snapshot written: $(ls "$OUT"/*.txt | wc -l) surfaces in $OUT"
