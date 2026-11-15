#!/usr/bin/env bash
# Audit the egui GUI crate for raw color literals that drift from
# docs/DESIGN.md §2 (the Tokens struct in
# crates/clawft-gui-egui/src/theming.rs).
#
# Rule:
#   D-TK01 : Color32::from_rgb / from_rgba_unmultiplied calls outside
#            theming.rs are flagged. Existing offenders are tracked
#            against a baseline file; the count can decrease (good)
#            but never increase (regression).
#
# Usage:
#   audit-theme.sh                             # report all offenders
#   audit-theme.sh --baseline <path>           # compare against baseline
#   audit-theme.sh [repo-root]                 # alternate root
#
# Exit 0 if (no offenders) OR (baseline supplied and count <= baseline).
# Non-zero if count > baseline OR baseline missing when requested.

set -euo pipefail

ROOT=""
BASELINE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --baseline) BASELINE="$2"; shift 2 ;;
    -h|--help)
      sed -n '3,20p' "$0"; exit 0 ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) ROOT="$1"; shift ;;
  esac
done

ROOT="${ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
GUI="$ROOT/crates/clawft-gui-egui/src"

if [[ ! -d "$GUI" ]]; then
  echo "no GUI crate at $GUI" >&2; exit 2
fi

# Find all color literals outside theming (theming.rs or the theming/ module dir)
offenders=$(grep -rnE 'Color32::from_(rgb|rgba_unmultiplied)\(' "$GUI" \
  --include="*.rs" \
  | grep -v -e "theming.rs" -e "/theming/" \
  || true)

if [[ -z "$offenders" ]]; then
  echo "clean: no color drift outside theming.rs"
  exit 0
fi

count=$(echo "$offenders" | wc -l | tr -d ' ')

if [[ -n "$BASELINE" ]]; then
  if [[ ! -f "$BASELINE" ]]; then
    echo "baseline file not found: $BASELINE" >&2
    exit 2
  fi
  # Pull the recorded count from the baseline trailer line:
  # `D-TK01: 246 color literal(s) outside theming.rs`
  baseline_count=$(grep -E 'D-TK01: [0-9]+ color literal' "$BASELINE" \
    | sed -E 's/.*D-TK01: ([0-9]+) color literal.*/\1/' \
    | head -1 \
    || echo 0)
  baseline_count=${baseline_count:-0}

  if [[ "$count" -gt "$baseline_count" ]]; then
    echo "$offenders"
    echo ""
    echo "D-TK01 REGRESSION: $count offenders, baseline allowed $baseline_count."
    echo "  add the new offending color to crates/clawft-gui-egui/src/theming.rs Tokens"
    echo "  or update the baseline at $BASELINE if you've graduated existing offenders."
    exit 1
  fi
  if [[ "$count" -lt "$baseline_count" ]]; then
    echo "D-TK01 ratchet ratcheted: $count offenders (baseline was $baseline_count)."
    echo "  consider updating the baseline at $BASELINE so future regressions are caught."
    exit 0
  fi
  echo "D-TK01 holds at baseline: $count offenders."
  exit 0
fi

# No baseline: just report the count as info; this is informational only,
# not a build break. Consumers that want a hard gate must pass --baseline.
echo "$offenders"
echo ""
echo "D-TK01: $count color literal(s) outside theming.rs"
echo "  see docs/DESIGN.md §2 — every color belongs in Tokens"
echo "  (pass --baseline <path> to ratchet against a recorded count)"
exit 0
