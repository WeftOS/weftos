#!/usr/bin/env bash
# check-wasm-bindgen-pin.sh — fail when a wasm-bindgen-cli install in CI or a
# container build is not pinned to the wasm-bindgen version in Cargo.lock.
#
# The CLI and the wasm-bindgen crate must match exactly ("schema version"
# mismatch otherwise; v0.8.5-rc.1 Browser WASM run 43452932538). An unpinned
# `cargo install wasm-bindgen-cli` installs latest and drifts the same way, so
# it fails too. Scans .github/workflows/*.yml, scripts/build.sh and any
# Containerfile / Dockerfile / *.containerfile in the repo.
#
# Usage: scripts/check-wasm-bindgen-pin.sh [ROOT]   (ROOT defaults to repo root)
set -uo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
LOCK="$ROOT/Cargo.lock"
[ -f "$LOCK" ] || { echo "check-wasm-bindgen-pin: no $LOCK" >&2; exit 2; }

want="$(awk '/^name = "wasm-bindgen"$/ {getline; gsub(/[^0-9.]/, "", $0); print; exit}' "$LOCK")"
[ -n "$want" ] || { echo "check-wasm-bindgen-pin: wasm-bindgen not in Cargo.lock" >&2; exit 2; }

files=()
while IFS= read -r f; do files+=("$f"); done < <(
    { ls "$ROOT"/.github/workflows/*.yml "$ROOT"/scripts/build.sh 2>/dev/null
      find "$ROOT" \( -path '*/target' -o -path '*/node_modules' -o -path '*/.git' \) -prune -o \
          -type f \( -name 'Containerfile*' -o -name 'Dockerfile*' -o -name '*.containerfile' \) -print
    } | sort -u)

bad=0
for f in "${files[@]}"; do
    while IFS=: read -r n line; do
        # Only actual installs: skip comments and prose mentioning the tool.
        case "$line" in *cargo*install*|*cargo*binstall*) ;; *) continue ;; esac
        stripped="${line#"${line%%[![:space:]]*}"}"
        case "$stripped" in '#'*|echo*|info*|skip*) continue ;; esac
        got="$(printf '%s\n' "$line" | sed -nE 's/.*wasm-bindgen-cli(@|=| +--version[ =]+| +)v?([0-9]+\.[0-9]+\.[0-9]+).*/\2/p; s/.*--version[ =]+v?([0-9]+\.[0-9]+\.[0-9]+).*wasm-bindgen-cli.*/\1/p' | head -n1)"
        rel="${f#"$ROOT"/}"
        if [ -z "$got" ]; then
            echo "FAIL $rel:$n: wasm-bindgen-cli install is unpinned (want $want): $stripped"; bad=1
        elif [ "$got" != "$want" ]; then
            echo "FAIL $rel:$n: wasm-bindgen-cli $got != Cargo.lock wasm-bindgen $want"; bad=1
        fi
    done < <(grep -nE 'wasm-bindgen-cli' "$f" 2>/dev/null)
done

if [ "$bad" = 0 ]; then echo "ok: all wasm-bindgen-cli pins match Cargo.lock ($want)"; fi
exit "$bad"
