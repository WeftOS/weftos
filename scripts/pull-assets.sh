#!/usr/bin/env bash
# Pull CDN assets (WASM + KB) into docs/src/public/ for local development.
#
# Usage:
#   scripts/pull-assets.sh              # pull from GitHub Releases (cdn-assets tag)
#   scripts/pull-assets.sh --local      # copy from local build artifacts
#   scripts/pull-assets.sh --sha <sha>  # pull a SHA-stamped snapshot (WEFT-454)
#
# The docs site .gitignore excludes public/wasm/ and public/kb/.
# In production these are served from NEXT_PUBLIC_CDN_URL.
#
# SHA snapshots: every Docs Assets Publish run also uploads
#   clawft_wasm-{sha}.wasm / clawft_wasm-{sha}.js / weftos-docs-{sha}.rvf
# See docs/deployment/cdn.md and scripts/release/cdn-snapshot.sh.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

WASM_DIR="$ROOT/docs/src/public/wasm"
KB_DIR="$ROOT/docs/src/public/kb"

MODE="remote"
SHA=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --local)
            MODE="local"
            shift
            ;;
        --sha)
            SHA="${2:-}"
            [[ -n "$SHA" ]] || { echo "error: --sha requires a value" >&2; exit 1; }
            shift 2
            ;;
        -h|--help)
            sed -n '2,16p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "error: unknown arg: $1 (try --local | --sha <sha>)" >&2
            exit 1
            ;;
    esac
done

mkdir -p "$WASM_DIR" "$KB_DIR"

if [[ "$MODE" == "local" ]]; then
    if [[ -n "$SHA" ]]; then
        echo "error: --local and --sha are mutually exclusive" >&2
        exit 1
    fi
    echo "Copying from local build artifacts..."

    # WASM from bindgen output. scripts/build.sh browser writes www/pkg/;
    # older wasm-pack builds used crates/clawft-wasm/pkg/.
    PKG_DIR=""
    for candidate in \
        "$ROOT/crates/clawft-wasm/www/pkg" \
        "$ROOT/crates/clawft-wasm/pkg"
    do
        if [[ -f "$candidate/clawft_wasm_bg.wasm" ]]; then
            PKG_DIR="$candidate"
            break
        fi
    done
    if [[ -n "$PKG_DIR" ]]; then
        cp "$PKG_DIR/clawft_wasm_bg.wasm" "$WASM_DIR/"
        cp "$PKG_DIR/clawft_wasm.js" "$WASM_DIR/"
        echo "  WASM: copied from $PKG_DIR"
    else
        echo "  WASM: not found in www/pkg or pkg — run scripts/build.sh browser first"
        exit 1
    fi

    # KB from build-kb output or worktree
    if [[ -f "$ROOT/docs/src/public/kb/weftos-docs.rvf" ]]; then
        echo "  KB: already exists"
    else
        echo "  KB: running build-kb..."
        "$ROOT/scripts/build-kb.sh"
    fi
else
    REPO="${GITHUB_REPOSITORY:-WeftOS/weftos}"
    TAG="cdn-assets"
    BASE="https://github.com/$REPO/releases/download/$TAG"

    if [[ -n "$SHA" ]]; then
        # Normalize to 12-char short when a full SHA is passed
        SHA_SHORT=$(echo "$SHA" | tr '[:upper:]' '[:lower:]' | cut -c1-12)
        echo "Pulling SHA snapshot from GitHub Releases ($REPO@$TAG sha=$SHA_SHORT)..."

        echo "  Downloading clawft_wasm-${SHA_SHORT}.wasm → clawft_wasm_bg.wasm..."
        curl -fsSL "$BASE/clawft_wasm-${SHA_SHORT}.wasm" -o "$WASM_DIR/clawft_wasm_bg.wasm"

        echo "  Downloading clawft_wasm-${SHA_SHORT}.js → clawft_wasm.js..."
        curl -fsSL "$BASE/clawft_wasm-${SHA_SHORT}.js" -o "$WASM_DIR/clawft_wasm.js"

        echo "  Downloading weftos-docs-${SHA_SHORT}.rvf → weftos-docs.rvf..."
        curl -fsSL "$BASE/weftos-docs-${SHA_SHORT}.rvf" -o "$KB_DIR/weftos-docs.rvf"

        # Record which snapshot we pinned locally
        cat >"$WASM_DIR/CDN_SHA" <<EOF
$SHA_SHORT
EOF
        echo "  Pinned local assets to snapshot $SHA_SHORT (wrote wasm/CDN_SHA)"
    else
        echo "Pulling assets from GitHub Releases ($REPO@$TAG)..."

        echo "  Downloading clawft_wasm_bg.wasm..."
        curl -fsSL "$BASE/clawft_wasm_bg.wasm" -o "$WASM_DIR/clawft_wasm_bg.wasm"

        echo "  Downloading clawft_wasm.js..."
        curl -fsSL "$BASE/clawft_wasm.js" -o "$WASM_DIR/clawft_wasm.js"

        echo "  Downloading weftos-docs.rvf..."
        curl -fsSL "$BASE/weftos-docs.rvf" -o "$KB_DIR/weftos-docs.rvf"

        # Best-effort: record current rolling SHA from manifest if present
        if curl -fsSL "$BASE/cdn-manifest.json" -o "$WASM_DIR/cdn-manifest.json" 2>/dev/null; then
            echo "  Downloaded cdn-manifest.json (current rolling pointer)"
        fi
    fi
fi

echo ""
echo "Assets ready:"
ls -lh "$WASM_DIR/"
ls -lh "$KB_DIR/"
