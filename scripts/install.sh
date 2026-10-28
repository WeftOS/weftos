#!/bin/sh
# WeftOS Universal Installer / Updater
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh
#
# Installs or updates: weft, weaver, weftos
# Detects platform automatically.
# Idempotent — safe to run multiple times.
#
# Provenance verification (WEFT-451):
#   By default, every downloaded archive is verified against the
#   sigstore attestation that cargo-dist publishes alongside the
#   release. Verification requires the GitHub CLI (`gh`) to be
#   installed and on $PATH. Without `gh`, the installer prints a
#   warning and continues; pass `--no-verify` (or set
#   WEFTOS_NO_VERIFY=1) to skip the check explicitly.
#
# Flags:
#   --verify       Force attestation verification (default).
#                  Aborts the install if `gh` is missing.
#   --no-verify    Skip attestation verification entirely.

set -eu

REPO="WeftOS/weftos"
INSTALL_DIR="${WEFTOS_INSTALL_DIR:-/usr/local/bin}"
BINS="clawft-cli clawft-weave weftos"
BIN_NAMES="weft weaver weftos"

# Verify mode: default | force | skip
# - default: verify if `gh` is available, warn otherwise
# - force:   `gh` must be present; abort if missing
# - skip:    never verify
VERIFY_MODE="default"
if [ "${WEFTOS_NO_VERIFY:-0}" = "1" ]; then
    VERIFY_MODE="skip"
fi

# Allow CLI flags to override env-driven default.
for arg in "$@"; do
    case "$arg" in
        --verify)    VERIFY_MODE="force" ;;
        --no-verify) VERIFY_MODE="skip" ;;
        --help|-h)
            sed -n '1,30p' "$0"
            exit 0
            ;;
    esac
done

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info() { printf "${CYAN}→${NC} %s\n" "$1"; }
ok()   { printf "${GREEN}✓${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}!${NC} %s\n" "$1"; }
err()  { printf "${RED}✗${NC} %s\n" "$1" >&2; exit 1; }

# Verify a downloaded asset against its sigstore attestation.
# Returns 0 on success, non-zero on hard failure (and exits if VERIFY_MODE=force).
verify_attestation() {
    asset_path="$1"
    asset_name="$2"

    if [ "$VERIFY_MODE" = "skip" ]; then
        return 0
    fi

    if ! command -v gh >/dev/null 2>&1; then
        if [ "$VERIFY_MODE" = "force" ]; then
            err "gh CLI not found but --verify was requested. Install gh from https://cli.github.com/ or pass --no-verify."
        fi
        warn "gh CLI not found — skipping attestation check for $asset_name (pass --no-verify to silence)."
        return 0
    fi

    if gh attestation verify "$asset_path" --repo "$REPO" >/dev/null 2>&1; then
        ok "Attestation verified: $asset_name"
        return 0
    fi

    if [ "$VERIFY_MODE" = "force" ]; then
        err "Attestation verification FAILED for $asset_name. Refusing to install — possible tampered download."
    fi
    warn "Attestation verification failed for $asset_name (continuing because not in --verify mode)."
    return 1
}

# Detect platform
detect_triple() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    ARCH=$(uname -m)

    case "$OS" in
        linux)
            case "$ARCH" in
                x86_64|amd64)
                    # Check musl vs glibc
                    if ldd --version 2>&1 | grep -qi musl; then
                        echo "x86_64-unknown-linux-musl"
                    else
                        echo "x86_64-unknown-linux-gnu"
                    fi
                    ;;
                aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
                *) err "Unsupported architecture: $ARCH" ;;
            esac
            ;;
        darwin)
            case "$ARCH" in
                x86_64|amd64) echo "x86_64-apple-darwin" ;;
                aarch64|arm64) echo "aarch64-apple-darwin" ;;
                *) err "Unsupported architecture: $ARCH" ;;
            esac
            ;;
        *) err "Unsupported OS: $OS" ;;
    esac
}

# Get latest version from GitHub
get_latest_version() {
    curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' \
        | head -1 \
        | sed 's/.*"v\([^"]*\)".*/\1/'
}

# Get current installed version
get_current_version() {
    if command -v weaver >/dev/null 2>&1; then
        weaver version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1
    elif command -v weft >/dev/null 2>&1; then
        weft --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1
    else
        echo "none"
    fi
}

main() {
    echo ""
    printf "${CYAN}WeftOS Installer${NC}\n"
    echo "════════════════"
    echo ""

    TRIPLE=$(detect_triple)
    info "Platform: $TRIPLE"
    case "$VERIFY_MODE" in
        force)   info "Provenance: gh attestation verify (required)" ;;
        skip)    info "Provenance: skipped (--no-verify)" ;;
        default) info "Provenance: gh attestation verify (best-effort; pass --verify to require)" ;;
    esac

    LATEST=$(get_latest_version)
    if [ -z "$LATEST" ]; then
        err "Failed to fetch latest version from GitHub"
    fi
    info "Latest version: v$LATEST"

    CURRENT=$(get_current_version)
    if [ "$CURRENT" = "$LATEST" ]; then
        ok "Already up to date (v$LATEST)"
        echo ""
        return 0
    elif [ "$CURRENT" = "none" ]; then
        info "Fresh install"
    else
        info "Updating: v$CURRENT → v$LATEST"
    fi

    # Stop kernel if running
    if command -v weaver >/dev/null 2>&1; then
        if weaver kernel status >/dev/null 2>&1; then
            info "Stopping running kernel..."
            weaver kernel stop 2>/dev/null || true
            RESTART_KERNEL=1
        fi
    fi

    echo ""

    # Download and install each binary
    set -- clawft-cli clawft-weave weftos
    set_names="weft weaver weftos"
    i=1
    for asset_prefix in "$@"; do
        bin_name=$(echo "$set_names" | cut -d' ' -f$i)
        asset="${asset_prefix}-${TRIPLE}.tar.gz"
        url="https://github.com/$REPO/releases/download/v${LATEST}/${asset}"

        info "Downloading $bin_name..."
        tmpdir=$(mktemp -d)
        if curl -fsSL -o "$tmpdir/$asset" "$url" 2>/dev/null; then
            # Sigstore attestation check (WEFT-451). In default mode we
            # proceed on missing `gh`; in --verify mode we abort on any
            # verification failure inside verify_attestation.
            verify_attestation "$tmpdir/$asset" "$asset" || true
            tar xzf "$tmpdir/$asset" --strip-components=1 -C "$tmpdir"
            if [ -f "$tmpdir/$bin_name" ]; then
                chmod +x "$tmpdir/$bin_name"
                if cp "$tmpdir/$bin_name" "$INSTALL_DIR/$bin_name" 2>/dev/null; then
                    ok "$bin_name installed to $INSTALL_DIR/$bin_name"
                else
                    warn "Permission denied, trying sudo..."
                    sudo cp "$tmpdir/$bin_name" "$INSTALL_DIR/$bin_name"
                    ok "$bin_name installed to $INSTALL_DIR/$bin_name"
                fi
            else
                warn "$bin_name not found in archive, skipping"
            fi
        else
            warn "$asset not available for this platform, skipping"
        fi
        rm -rf "$tmpdir"
        i=$((i + 1))
    done

    echo ""

    # Restart kernel if it was running
    if [ "${RESTART_KERNEL:-0}" = "1" ]; then
        info "Restarting kernel..."
        weaver kernel start 2>/dev/null || true
    fi

    # Verify
    echo ""
    if command -v weaver >/dev/null 2>&1; then
        ok "$(weaver version 2>/dev/null || echo "weaver v$LATEST")"
    fi
    if command -v weft >/dev/null 2>&1; then
        ok "$(weft --version 2>/dev/null || echo "weft v$LATEST")"
    fi

    echo ""
    ok "WeftOS v$LATEST installed successfully"
    echo ""
    echo "  Getting started:"
    echo "    weaver kernel start    # Start the kernel"
    echo "    weft assess init       # Initialize a project"
    echo "    weft assess            # Run an assessment"
    echo ""
    echo "  Update anytime:"
    echo "    weaver update          # or re-run this script"
    echo ""
}

main
