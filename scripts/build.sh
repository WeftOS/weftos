#!/usr/bin/env bash
# Unified build script for ClawFT workspace.
# Wraps cargo, wasm, and UI builds behind simple subcommands.
set -euo pipefail

# ── Colors ───────────────────────────────────────────────────────────
RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
YELLOW=$'\033[1;33m'
CYAN=$'\033[0;36m'
BOLD=$'\033[1m'
NC=$'\033[0m'

# ── Resolve workspace root ──────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

# ── Defaults ─────────────────────────────────────────────────────────
PROFILE=""
# wasm-project: which half to build (guest | runner | all)
WASM_PROJECT_PART=""
FEATURES=""
VERBOSE=false
DRY_RUN=false
FORCE=false
DEBUG=false
NO_FAIL_FAST=false
PREFIX=""
SERVE_PORT=""
WASM_PANEL_MAX_RAW_KB=""
WASM_PANEL_MAX_GZ_KB=""
BENCH_CRATE=""
BENCH_NAME=""
CLEAN_STALE_DAYS=""
TEST_PACKAGES=()
TEST_FILTER=""
# WEFT-460: optional gate step — cargo-dist host-triple rehearsal
WITH_RELEASE_DRY_RUN=false
# agents-catalog: verify agents/catalog.json is up to date instead of writing it
AGENTS_CATALOG_CHECK=false
COMMAND=""
# cogs-conformance: everything after the subcommand is passed through
COGS_ARGS=()
# cogs-launcher: --linux-arm64 builds the launcher in an arm64 Linux container
LAUNCHER_LINUX=false
# test-pi: everything after the subcommand is passed through to scripts/pi/pi_lane.py
PI_ARGS=()
# licence-cross: which targets to build (default: armv7 and aarch64)
LICENCE_TARGETS=()

# ── Reporting helpers ────────────────────────────────────────────────
pass()  { printf "  ${GREEN}PASS${NC}  %s\n" "$*"; }
fail()  { printf "  ${RED}FAIL${NC}  %s\n" "$*"; }
skip()  { printf "  ${YELLOW}SKIP${NC}  %s\n" "$*"; }
info()  { printf "  ${CYAN}INFO${NC}  %s\n" "$*"; }
header(){ printf "\n${BOLD}── %s${NC}\n" "$*"; }

# ── Timer ────────────────────────────────────────────────────────────
TIMER_START=0
timer_start() { TIMER_START=$(date +%s); }
timer_end() {
    local elapsed=$(( $(date +%s) - TIMER_START ))
    local min=$((elapsed / 60))
    local sec=$((elapsed % 60))
    if [ "$min" -gt 0 ]; then
        printf "  ${CYAN}TIME${NC}  %dm %ds\n" "$min" "$sec"
    else
        printf "  ${CYAN}TIME${NC}  %ds\n" "$sec"
    fi
}

# ── Force-clean a package before rebuild ───────────────────────────
force_clean_pkg() {
    local pkg="$1"
    if [ "$FORCE" = true ]; then
        info "Forcing rebuild (cleaning $pkg)"
        cargo clean -p "$pkg" 2>/dev/null || true
    fi
}

# ── Run a command (respects --verbose and --dry-run) ─────────────────
run_cmd() {
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   %s\n" "$*"
        return 0
    fi
    if [ "$VERBOSE" = true ]; then
        "$@"
    else
        "$@" 2>&1 | tail -5
    fi
}

# ── Target check ────────────────────────────────────────────────────
check_target_installed() {
    local target="$1"
    if ! rustup target list --installed 2>/dev/null | grep -q "$target"; then
        printf "  ${YELLOW}WARN${NC}  Target %s not installed. Run: rustup target add %s\n" "$target" "$target"
        return 1
    fi
    return 0
}

# ── Size reporting ──────────────────────────────────────────────────
report_binary_size() {
    local file="$1" label="${2:-Binary}"
    if [ -f "$file" ]; then
        local bytes
        bytes=$(wc -c < "$file")
        local kb=$((bytes / 1024))
        if [ "$kb" -ge 1024 ]; then
            local mb
            mb=$(echo "scale=2; $bytes / 1048576" | bc 2>/dev/null || echo "$((kb / 1024))")
            printf "  ${CYAN}SIZE${NC}  %s: %s MB (%s bytes)\n" "$label" "$mb" "$bytes"
        elif [ "$kb" -gt 0 ]; then
            printf "  ${CYAN}SIZE${NC}  %s: %s KB (%s bytes)\n" "$label" "$kb" "$bytes"
        else
            printf "  ${CYAN}SIZE${NC}  %s: %s bytes\n" "$label" "$bytes"
        fi
    fi
}

# ── Feature flag builder ────────────────────────────────────────────
cargo_features_args() {
    if [ -n "$FEATURES" ]; then
        echo "--features $FEATURES"
    fi
}

# ── Subcommands ─────────────────────────────────────────────────────

cmd_native() {
    local profile="${PROFILE:-release}"
    header "Building native CLI binary (profile: $profile)"
    force_clean_pkg clawft-cli
    timer_start
    local args=(cargo build --bin weft --bin weaver)
    if [ "$profile" = "release" ] || [ "$profile" = "release-wasm" ]; then
        args+=(--profile "$profile")
    fi
    [ -n "$FEATURES" ] && args+=(--features "$FEATURES")
    run_cmd "${args[@]}"
    timer_end
    if [ "$profile" = "release" ]; then
        report_binary_size "target/release/weft" "Native binary (weft)"
        report_binary_size "target/release/weaver" "Native binary (weaver)"
    elif [ "$profile" = "release-wasm" ]; then
        report_binary_size "target/release-wasm/weft" "Native binary (weft)"
        report_binary_size "target/release-wasm/weaver" "Native binary (weaver)"
    else
        report_binary_size "target/debug/weft" "Native binary (weft)"
        report_binary_size "target/debug/weaver" "Native binary (weaver)"
    fi
}

cmd_native_debug() {
    header "Building native CLI binary (debug)"
    force_clean_pkg clawft-cli
    timer_start
    local args=(cargo build --bin weft --bin weaver)
    [ -n "$FEATURES" ] && args+=(--features "$FEATURES")
    run_cmd "${args[@]}"
    timer_end
    report_binary_size "target/debug/weft" "Native binary (weft, debug)"
    report_binary_size "target/debug/weaver" "Native binary (weave, debug)"
}

# ADR-118 C3: the chief-of-staff agent host. `--features mic` adds cpal
# push-to-talk capture; `--features mic,onnx` adds real parakeet STT.
cmd_cos() {
    header "Building chief-of-staff host (weftos-cos, debug)"
    timer_start
    local args=(cargo build -p weftos-cos --bin weftos-cos)
    [ -n "$FEATURES" ] && args+=(--features "$FEATURES")
    run_cmd "${args[@]}" || { timer_end; return 1; }
    timer_end
    report_binary_size "target/debug/weftos-cos" "Chief-of-staff host (weftos-cos, debug)"
    # A featured build is copied out of target/: any later `cargo test` or
    # gate run rebuilds target/debug/weftos-cos with default features at the
    # same path, which silently drops mic/onnx (STT) from whatever runs it.
    if [ -n "$FEATURES" ] && [ "$DRY_RUN" != true ]; then
        local stable="${WEFTOS_COS_BIN:-$HOME/.weftos/bin/weftos-cos}"
        mkdir -p "$(dirname "$stable")"
        install -m 0755 "$ROOT/target/debug/weftos-cos" "$stable.tmp" && mv -f "$stable.tmp" "$stable"
        printf "  Installed featured build (%s) to %s\n" "$FEATURES" "$stable"
    fi
}

# ── Build stamp env ─────────────────────────────────────────────────
# Export GIT_SHA / GIT_DIRTY / BUILD_TS so the CLI + weaver build.rs bake
# an authoritative, freshly-timestamped provenance stamp into the binary
# (see crates/clawft-cli/build.rs and crates/clawft-weave/build.rs). A
# fresh BUILD_TS each call also forces `cargo` to re-run those build
# scripts (rerun-if-env-changed=BUILD_TS), so `install` always re-stamps.
export_build_stamp() {
    local sha dirty ts
    sha="$(git rev-parse --short=8 HEAD 2>/dev/null || echo unknown)"
    if [ -n "$(git status --porcelain 2>/dev/null)" ]; then dirty=1; else dirty=0; fi
    ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    export GIT_SHA="$sha"
    export GIT_DIRTY="$dirty"
    export BUILD_TS="$ts"
    # Feature list rides the version stamp (WEFT-656): `--version` becomes
    # `0.6.20 (<sha> <ts>) [voice-onnx]`, so the install guard below can
    # refuse a build that silently drops a cargo feature (e.g. diskann —
    # whose absence degrades the vector backend to a brute-force stub) that
    # the installed binary was built with.
    export BUILD_FEATURES="$FEATURES"
    info "Build stamp: ${sha}$([ "$dirty" = 1 ] && echo '-dirty') @ ${ts}${FEATURES:+ [$FEATURES]}"
}

# List a binary's top-level subcommands, one per line, sorted. Best-effort:
# empty output if the binary is missing or won't run (e.g. an unsigned
# stale copy macOS SIGKILLs). Parses the clap `Commands:` help block.
probe_subcommands() {
    local bin="$1"
    [ -x "$bin" ] || return 0
    "$bin" --help 2>/dev/null \
        | sed -n '/^Commands:/,/^$/p' \
        | grep -E '^[[:space:]]+[a-z]' \
        | awk '{print $1}' \
        | sort -u
}

# Guard against silently stripping features on reinstall (WEFT-643).
#
# If the currently-installed binary answers to subcommands the freshly-built
# one does not, this install would drop a feature the user relies on — the
# exact `voice`-subcommand regression that motivated this guard. Refuse
# (non-zero) so the caller aborts, unless --force. Fails open when the old
# binary can't be probed (nothing to compare).
check_feature_regression() {
    local name="$1" new_bin="$2" installed="$3"
    [ -x "$installed" ] || return 0
    local old_cmds new_cmds dropped
    old_cmds="$(probe_subcommands "$installed")"
    new_cmds="$(probe_subcommands "$new_bin")"
    [ -z "$old_cmds" ] && return 0
    dropped="$(comm -23 <(printf '%s\n' "$old_cmds") <(printf '%s\n' "$new_cmds"))"
    if [ -n "$dropped" ]; then
        fail "install would DROP $name subcommand(s): $(echo $dropped | tr '\n' ' ')"
        info "the installed $name exposes features this build omits."
        info "re-run with the matching features (e.g. --features voice-onnx),"
        info "or pass --force to install the reduced build anyway."
        return 1
    fi
    return 0
}

# Cargo-feature regression guard (WEFT-656): compare the INSTALLED binary's
# `--version` feature suffix (`0.6.20 (<sha> <ts>) [voice-onnx,diskann]`,
# baked by build.rs from BUILD_FEATURES) against this invocation's $FEATURES.
# Refuse (non-zero) when the installed binary carries a feature this build
# was invoked without — the exact silent-stub trap the subcommand probe
# can't see. Fails open when the installed binary is missing, won't run, or
# predates the suffix (no brackets = nothing to compare).
check_version_feature_regression() {
    local name="$1" installed="$2"
    [ -x "$installed" ] || return 0
    local ver installed_feats
    ver="$("$installed" --version 2>/dev/null)" || return 0
    installed_feats="$(printf '%s' "$ver" | sed -n 's/.*\[\(.*\)\].*/\1/p')"
    [ -z "$installed_feats" ] && return 0
    local missing=""
    local feat
    for feat in $(printf '%s' "$installed_feats" | tr ',' ' '); do
        case ",$FEATURES," in
            *",$feat,"*) ;;
            *) missing="$missing $feat" ;;
        esac
    done
    if [ -n "$missing" ]; then
        fail "install would DROP $name cargo feature(s):$missing"
        info "the installed $name was built with [--features $installed_feats];"
        info "re-run with the same features, or pass --force to downgrade anyway."
        return 1
    fi
    return 0
}

# Atomically install a freshly-built binary into the destination directory.
#
# Copies to a temp file in the destination directory, then `mv` (same-fs
# rename) swaps it into place — an atomic replace that also sidesteps the
# ETXTBSY "text file busy" failure of overwriting a running binary.
install_binary() {
    local name="$1" src="$2" dst_dir="$3"
    local dst="$dst_dir/$name"
    if [ ! -f "$src" ]; then
        fail "built binary missing: $src"
        return 1
    fi
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   install %s -> %s (atomic)\n" "$src" "$dst"
        return 0
    fi
    local tmp="${dst}.new.$$"
    if cp "$src" "$tmp" && chmod +x "$tmp" && mv -f "$tmp" "$dst"; then
        # macOS SIGKILLs (Killed: 9) a freshly-replaced binary in ~/.cargo/bin
        # — even with identical bytes and an otherwise-valid signature — until
        # its code signature is refreshed against the new inode. Force an
        # ad-hoc re-sign so the reinstalled binary actually runs; without this
        # every reinstall produces a phantom "binary won't run". (WEFT-643)
        if [ "$(uname -s)" = "Darwin" ] && command -v codesign >/dev/null 2>&1; then
            if codesign --force -s - "$dst" 2>/dev/null; then
                pass "installed $name -> $dst (re-signed ad-hoc)"
            else
                skip "installed $name -> $dst (ad-hoc re-sign failed — may not run)"
            fi
        else
            pass "installed $name -> $dst"
        fi
    else
        rm -f "$tmp" 2>/dev/null || true
        fail "failed to install $name to $dst"
        return 1
    fi
}

# Build weft + weaver and install both to ~/.cargo/bin (WEFT installer DX).
#
# Fixes the stale-binary trap: a divergent installed binary silently drifts
# from a fresh build. `install` rebuilds both bins with a fresh provenance
# stamp and atomically replaces the installed copies, then prints the
# installed versions so the stamp is visible. Release by default;
# `install --debug` uses the debug profile for the fast-iteration path.
#
# Honors --features exactly like `native` / `native-debug`. This machine's
# working configuration is `--features voice-onnx` (the `voice` subcommand
# stack); installing without it strips voice, so a feature-regression guard
# refuses to drop subcommands the installed binary already exposes unless
# --force is given.
#
# --prefix DIR installs into DIR instead of ~/.cargo/bin — use it to verify
# an install without disturbing the user's live binary.
cmd_install() {
    local profile
    if [ "$DEBUG" = true ]; then profile="debug"; else profile="release"; fi
    local bindir="${PREFIX:-${CARGO_HOME:-$HOME/.cargo}/bin}"
    header "Installing weft + weaver to $bindir (profile: $profile)"

    export_build_stamp

    timer_start
    local args=(cargo build --bin weft --bin weaver)
    [ "$profile" = "release" ] && args+=(--profile release)
    [ -n "$FEATURES" ] && args+=(--features "$FEATURES")
    run_cmd "${args[@]}"
    timer_end

    local srcdir="target/$profile"

    # Feature-regression guard: never silently strip subcommands the
    # installed binary already has (the `voice` incident). --force overrides.
    if [ "$FORCE" != true ] && [ "$DRY_RUN" != true ]; then
        local regressed=0
        check_feature_regression weft   "$srcdir/weft"   "$bindir/weft"   || regressed=1
        check_feature_regression weaver "$srcdir/weaver" "$bindir/weaver" || regressed=1
        # Cargo-feature guard (WEFT-656): the subcommand probe can't see
        # features that change BEHAVIOR without adding subcommands (diskann:
        # its absence silently degrades the vector backend to a brute-force
        # stub). The installed binary's `--version` carries a `[features]`
        # suffix (baked by build.rs from BUILD_FEATURES); refuse to replace
        # it with a build that drops any of those features.
        check_version_feature_regression weft   "$bindir/weft"   || regressed=1
        check_version_feature_regression weaver "$bindir/weaver" || regressed=1
        if [ "$regressed" -gt 0 ]; then
            fail "aborting install to avoid a feature downgrade (use --force to override)"
            return 1
        fi
    fi

    if [ "$DRY_RUN" != true ]; then
        mkdir -p "$bindir"
    fi

    local failed=0
    install_binary weft   "$srcdir/weft"   "$bindir" || failed=$((failed + 1))
    install_binary weaver "$srcdir/weaver" "$bindir" || failed=$((failed + 1))

    if [ "$failed" -gt 0 ]; then
        fail "$failed binary install(s) failed"
        return 1
    fi

    if [ "$DRY_RUN" != true ]; then
        header "Installed versions"
        info "weft:   $("$bindir/weft" --version 2>/dev/null || echo '??')"
        info "weaver: $("$bindir/weaver" --version 2>/dev/null || echo '??')"
        info "If a daemon is running, restart it: weaver kernel restart"
    fi
    pass "install complete"
}

# Build the native egui GUI binary (`weft-gui-egui`). Lives in
# `crates/clawft-gui-egui/` and gates the native main loop behind the
# `native` feature so the wasm target excludes eframe's window code
# (see Cargo.toml `[[bin]]` `required-features = ["native"]`). The
# wasm bundle (used by the VSCode panel) is built via `cmd_browser`.
cmd_gui_egui() {
    local profile="${PROFILE:-release}"
    header "Building native egui GUI binary weft-gui-egui (profile: $profile)"
    force_clean_pkg clawft-gui-egui
    timer_start
    local feat="native"
    if [ -n "$FEATURES" ]; then
        feat="native,$FEATURES"
    fi
    local args=(cargo build -p clawft-gui-egui --bin weft-gui-egui --features "$feat")
    if [ "$profile" = "release" ] || [ "$profile" = "release-wasm" ]; then
        args+=(--profile "$profile")
    fi
    run_cmd "${args[@]}"
    timer_end
    if [ "$profile" = "release" ]; then
        report_binary_size "target/release/weft-gui-egui" "weft-gui-egui (release)"
    elif [ "$profile" = "release-wasm" ]; then
        report_binary_size "target/release-wasm/weft-gui-egui" "weft-gui-egui (release-wasm)"
    else
        report_binary_size "target/debug/weft-gui-egui" "weft-gui-egui (debug)"
    fi
}

# Trajectory Atlas egui render stress test (docs/plans/trajectory-atlas.md).
# Opens a window, runs scripted scenes on synthetic data with vsync off, writes
# a Markdown frame-time report (ATLAS_BENCH_OUT, default target/atlas-bench.md)
# and exits. ATLAS_BENCH_FRAMES sets timed frames per scene (default 300).
cmd_atlas_bench() {
    header "Trajectory Atlas egui stress test"
    timer_start
    run_cmd cargo run --release -p clawft-gui-egui --example atlas_stress --features native
    timer_end
}

cmd_wasi() {
    local profile="${PROFILE:-release-wasm}"
    header "Building WASM for WASI (wasm32-wasip2, profile: $profile)"
    if ! check_target_installed wasm32-wasip2; then return 1; fi
    force_clean_pkg clawft-wasm
    timer_start
    local args=(cargo build --target wasm32-wasip2 --profile "$profile" -p clawft-wasm)
    [ -n "$FEATURES" ] && args+=(--features "$FEATURES")
    run_cmd "${args[@]}"
    timer_end
    report_binary_size "target/wasm32-wasip2/${profile}/clawft_wasm.wasm" "WASI WASM"
}

# ADR-103 D9 Wasmtime project kernel. The guest is a core wasm32-wasip1 module
# (not the wasip2 library above) and the runner is the native Wasmtime process the
# user daemon launches; the daemon itself never links Wasmtime. Debug builds by
# default; `--profile release` for release. Never installs targets.
wasm_project_paths() {
    local dir="${CARGO_TARGET_DIR:-$ROOT/target}" sub=debug
    [ "$PROFILE" = release ] && sub=release
    WASM_PROJECT_GUEST="$dir/wasm32-wasip1/$sub/weftos-project-guest.wasm"
    WASM_PROJECT_RUNNER="$dir/$sub/weftos-wasm-project-runner"
}

cmd_wasm_project() {
    local part="${WASM_PROJECT_PART:-all}" rel=()
    [ "$PROFILE" = release ] && rel=(--release)
    case "$part" in guest|runner|all) ;; *) fail "wasm-project: unknown part '$part' (guest|runner|all)"; return 1 ;; esac
    wasm_project_paths
    if [ "$part" != runner ]; then
        header "Building the Wasmtime project guest (wasm32-wasip1)"
        if ! check_target_installed wasm32-wasip1; then return 1; fi
        timer_start
        run_cmd cargo build ${rel[@]+"${rel[@]}"} -p clawft-wasm --no-default-features --features project-kernel \
            --bin weftos-project-guest --target wasm32-wasip1
        timer_end
        report_binary_size "$WASM_PROJECT_GUEST" "project guest"
    fi
    if [ "$part" != guest ]; then
        header "Building the Wasmtime project runner (native)"
        timer_start
        run_cmd cargo build ${rel[@]+"${rel[@]}"} -p clawft-wasm-host --features project-kernel \
            --bin weftos-wasm-project-runner
        timer_end
        report_binary_size "$WASM_PROJECT_RUNNER" "project runner"
    fi
}

# Build both halves, then drive them with the signing-parent protocol fixture
# (scripts/test-wasm-project.py; needs python3 `cryptography`). All state lives
# in a fresh, short temp directory; HOME is never touched.
cmd_test_wasm_project() {
    # The runner compiles the whole guest on every start: a release build keeps
    # the fixture to minutes instead of the better part of an hour.
    [ -n "$PROFILE" ] || PROFILE=release
    WASM_PROJECT_PART=all
    cmd_wasm_project || return 1
    header "Wasmtime project lifecycle fixture"
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   python3 scripts/test-wasm-project.py --runner <runner> --guest <guest> --state-root <fresh dir>\n"
        return 0
    fi
    python3 -c 'import cryptography' 2>/dev/null || { fail "python3 'cryptography' is required"; return 1; }
    local root
    root="$(mktemp -d "${TMPDIR:-/tmp}/wp.XXXXXX")" || return 1
    timer_start
    python3 "$ROOT/scripts/test-wasm-project.py" --runner "$WASM_PROJECT_RUNNER" --guest "$WASM_PROJECT_GUEST" \
        --state-root "$root/s" --startup-timeout 60 || { rm -rf "$root"; fail "wasm project lifecycle fixture"; return 1; }
    rm -rf "$root"
    timer_end
}

cmd_browser() {
    local profile="${PROFILE:-release-wasm}"
    header "Building WASM for browser (wasm32-unknown-unknown, profile: $profile)"
    if ! check_target_installed wasm32-unknown-unknown; then return 1; fi
    force_clean_pkg clawft-wasm
    timer_start
    local args=(cargo build --target wasm32-unknown-unknown -p clawft-wasm --no-default-features --features browser)
    args+=(--profile "$profile")
    # Append extra features if provided (comma-separated with browser)
    if [ -n "$FEATURES" ]; then
        # browser is already set; append user features
        args[-1]="browser,$FEATURES"
    fi
    run_cmd "${args[@]}"
    timer_end

    local wasm_file="target/wasm32-unknown-unknown/${profile}/clawft_wasm.wasm"
    report_binary_size "$wasm_file" "Browser WASM (raw)"

    # Run wasm-bindgen to generate JS glue into www/pkg/ so the test
    # harness can be served directly from www/ at the root URL.
    local pkg_dir="$ROOT/crates/clawft-wasm/www/pkg"
    if command -v wasm-bindgen >/dev/null 2>&1; then
        info "Running wasm-bindgen → $pkg_dir"
        run_cmd wasm-bindgen "$wasm_file" \
            --out-dir "$pkg_dir" \
            --target web \
            --no-typescript
        report_binary_size "$pkg_dir/clawft_wasm_bg.wasm" "Browser WASM (bindgen)"
        pass "pkg/ ready — run: scripts/build.sh serve"
    else
        skip "wasm-bindgen CLI not found — pkg/ not generated"
        info "Install with: cargo install wasm-bindgen-cli"
    fi
}

# Companion apps for sensor cogs (ADR-104): crates/weftos-<name>-scope, binary weft-<name>-scope.
cmd_scope() {
    local name="${1:-}"
    [ -n "$name" ] && [ -d "$ROOT/crates/weftos-${name}-scope" ] || { fail "usage: scripts/build.sh scope <name>  (crates/weftos-<name>-scope; e.g. ecg, tof)"; return 1; }
    local profile="${PROFILE:-release}"
    header "Building weft-${name}-scope (native egui companion, profile: $profile)"
    timer_start
    run_cmd cargo build -p "weftos-${name}-scope" --bin "weft-${name}-scope" --profile "$profile"
    timer_end
    local dir="$profile"; [ "$profile" = "dev" ] && dir="debug"
    report_binary_size "target/${dir}/weft-${name}-scope" "weft-${name}-scope"
    pass "run: SEED_HOST=169.254.42.1 target/${dir}/weft-${name}-scope"
}

cmd_scope_web() {
    local name="${1:-}"
    [ -n "$name" ] && [ -d "$ROOT/crates/weftos-${name}-scope" ] || { fail "usage: scripts/build.sh scope-web <name>"; return 1; }
    local profile="${PROFILE:-release-wasm}"
    header "Building weft-${name}-scope for the browser (wasm32-unknown-unknown, profile: $profile)"
    if ! check_target_installed wasm32-unknown-unknown; then return 1; fi
    timer_start
    run_cmd cargo build --target wasm32-unknown-unknown -p "weftos-${name}-scope" --lib --profile "$profile"
    timer_end
    local wasm_file="target/wasm32-unknown-unknown/${profile}/weftos_${name}_scope.wasm"
    report_binary_size "$wasm_file" "${name}-scope WASM (raw)"
    local pkg_dir="$ROOT/crates/weftos-${name}-scope/www/pkg"
    if command -v wasm-bindgen >/dev/null 2>&1; then
        info "Running wasm-bindgen → $pkg_dir"
        run_cmd wasm-bindgen "$wasm_file" --out-dir "$pkg_dir" --target web --no-typescript
        report_binary_size "$pkg_dir/weftos_${name}_scope_bg.wasm" "${name}-scope WASM (bindgen)"
        pass "pkg/ ready — serve crates/weftos-${name}-scope/www over http and open /?seed=<seed-ip>"
    else
        skip "wasm-bindgen CLI not found — pkg/ not generated"
        info "Install with: cargo install wasm-bindgen-cli"
    fi
}

cmd_guide_web() {
    local profile="${PROFILE:-release-wasm}"
    header "Building weftos-guide-view for the browser (wasm32-unknown-unknown, profile: $profile)"
    if ! check_target_installed wasm32-unknown-unknown; then return 1; fi
    timer_start
    run_cmd cargo build --target wasm32-unknown-unknown -p weftos-guide-view --lib --profile "$profile"
    timer_end
    local wasm_file="target/wasm32-unknown-unknown/${profile}/weftos_guide_view.wasm"
    report_binary_size "$wasm_file" "guide-view WASM (raw)"
    local pkg_dir="$ROOT/crates/weftos-guide-view/www/pkg"
    if command -v wasm-bindgen >/dev/null 2>&1; then
        info "Running wasm-bindgen → $pkg_dir"
        run_cmd wasm-bindgen "$wasm_file" --out-dir "$pkg_dir" --target web --no-typescript
        report_binary_size "$pkg_dir/weftos_guide_view_bg.wasm" "guide-view WASM (bindgen)"
        pass "pkg/ ready — Sensor Explorer loads /guide-view/weftos_guide_view.js"
    else
        skip "wasm-bindgen CLI not found — pkg/ not generated"
        info "Install with: cargo install wasm-bindgen-cli"
    fi
}

cmd_manager() {
    local profile="${PROFILE:-release}"
    header "Building Weave Manager (appliance console, native, profile: $profile)"
    timer_start
    run_cmd cargo build -p weftos-cog-manager --bin weave-manager --profile "$profile"
    timer_end
    local dir="$profile"; [ "$profile" = "dev" ] && dir="debug"
    report_binary_size "target/${dir}/weave-manager" "weave-manager"
    pass "run: target/${dir}/weave-manager   (WEFTOS_HOST overrides the first address; unset tries this machine)"
}

cmd_manager_web() {
    local profile="${PROFILE:-release-wasm}"
    header "Building weft-cog-manager for the browser (wasm32-unknown-unknown, profile: $profile)"
    if ! check_target_installed wasm32-unknown-unknown; then return 1; fi
    timer_start
    run_cmd cargo build --target wasm32-unknown-unknown -p weftos-cog-manager --lib --profile "$profile"
    timer_end
    local wasm_file="target/wasm32-unknown-unknown/${profile}/weftos_cog_manager.wasm"
    report_binary_size "$wasm_file" "cog-manager WASM (raw)"
    local pkg_dir="$ROOT/crates/weftos-cog-manager/www/pkg"
    if command -v wasm-bindgen >/dev/null 2>&1; then
        info "Running wasm-bindgen → $pkg_dir"
        run_cmd wasm-bindgen "$wasm_file" --out-dir "$pkg_dir" --target web --no-typescript
        report_binary_size "$pkg_dir/weftos_cog_manager_bg.wasm" "cog-manager WASM (bindgen)"
        pass "pkg/ ready — serve crates/weftos-cog-manager/www and open /?host=http://<ip>:9480"
    else
        skip "wasm-bindgen CLI not found — pkg/ not generated"
        info "Install with: cargo install wasm-bindgen-cli"
    fi
}

cmd_ui() {
    header "Building React frontend (tsc + vite)"
    if [ ! -d "$ROOT/clawft-ui" ] || [ ! -f "$ROOT/clawft-ui/package.json" ]; then
        skip "clawft-ui/ directory not found — skipping"
        return 0
    fi
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cd clawft-ui && npm run build\n"
    else
        (cd "$ROOT/clawft-ui" && npm run build)
    fi
    timer_end
    if [ -d "$ROOT/clawft-ui/dist" ]; then
        local size
        size=$(du -sh "$ROOT/clawft-ui/dist" 2>/dev/null | cut -f1)
        printf "  ${CYAN}SIZE${NC}  UI bundle: %s\n" "$size"
    fi
}

cmd_ui_docker() {
    header "Building clawft-ui Docker image (multi-stage)"
    if [ ! -f "$ROOT/clawft-ui/Dockerfile" ]; then
        skip "clawft-ui/Dockerfile not found — skipping"
        return 0
    fi
    if ! command -v docker >/dev/null 2>&1; then
        fail "docker not installed; install Docker Engine to build the UI image"
        return 1
    fi
    local tag="${CLAWFT_UI_DOCKER_TAG:-clawft-ui:dev}"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   docker build -f clawft-ui/Dockerfile -t %s .\n" "$tag"
    else
        (cd "$ROOT" && docker build -f clawft-ui/Dockerfile -t "$tag" .)
        local size
        size=$(docker image inspect "$tag" --format='{{.Size}}' 2>/dev/null)
        if [ -n "$size" ]; then
            local mb=$((size / 1024 / 1024))
            printf "  ${CYAN}SIZE${NC}  %s image: %d MB\n" "$tag" "$mb"
        fi
    fi
    timer_end
}

cmd_docker_smoke() {
    header "Docker release smoke (build root Dockerfile, run gateway, probe /api/health)"
    if ! command -v docker >/dev/null 2>&1; then
        fail "docker not installed"
        return 1
    fi
    local tag="${WEFT_DOCKER_TAG:-weft:smoke}"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   docker build -t %s . && scripts/release/docker-smoke.sh %s\n" "$tag" "$tag"
    else
        (cd "$ROOT" && docker build -t "$tag" .) || return 1
        "$ROOT/scripts/release/docker-smoke.sh" "$tag" "${WEFT_SMOKE_PORT:-8080}" || return 1
    fi
    timer_end
}

cmd_ui_e2e() {
    header "Running clawft-ui Playwright E2E suite"
    if [ ! -d "$ROOT/clawft-ui/tests" ]; then
        skip "clawft-ui/tests/ not found — Playwright suite not scaffolded"
        return 0
    fi
    if [ ! -d "$ROOT/clawft-ui/node_modules" ]; then
        printf "  ${CYAN}INFO${NC}  installing clawft-ui dependencies\n"
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   cd clawft-ui && npm ci\n"
        else
            (cd "$ROOT/clawft-ui" && npm ci --no-audit --no-fund)
        fi
    fi
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cd clawft-ui && npx playwright install --with-deps chromium && npx playwright test\n"
    else
        (cd "$ROOT/clawft-ui" \
            && npx playwright install --with-deps chromium \
            && npx playwright test)
    fi
    timer_end
}

cmd_releases_mdx() {
    header "Regenerating docs releases.mdx from CHANGELOG.md"
    if [ ! -x "$ROOT/scripts/build-releases-mdx.sh" ]; then
        fail "scripts/build-releases-mdx.sh not found or not executable"
        return 1
    fi
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   scripts/build-releases-mdx.sh\n"
    else
        "$ROOT/scripts/build-releases-mdx.sh"
    fi
    timer_end
}

cmd_all() {
    header "Building everything"
    local failed=0
    cmd_native  || failed=$((failed + 1))
    cmd_wasi    || failed=$((failed + 1))
    cmd_browser || failed=$((failed + 1))
    cmd_ui      || failed=$((failed + 1))
    echo ""
    if [ "$failed" -gt 0 ]; then
        fail "$failed build(s) failed"
        return 1
    else
        pass "All builds succeeded"
    fi
}

# Run the workspace test suite. Prefers cargo-nextest: it runs each test in
# its own process, which eliminates the parallel-test-isolation flake class
# (tests sharing process-global env vars, statics, or the global tracing
# subscriber) and is faster than libtest across the whole workspace. nextest
# does not run doctests, so those get a separate `cargo test --doc` pass.
# Falls back to plain `cargo test` when cargo-nextest isn't installed
# (install: `curl -LsSf https://get.nexte.st/latest/mac | tar zxf - -C ~/.cargo/bin`).
#
# --no-fail-fast runs the whole suite even after a failure — needed for an
# honest full-suite verdict on a dev box where a known-environmental failure
# (e.g. the clawft-rpc no-daemon tests while a daemon is running) would
# otherwise fail-fast and mask the remaining tests.
# Tests link the non-test kernel lib, which resolves runtime files (cluster
# peers, apps, revoked hosts, node key) through RuntimePaths::resolve().
# Point that at a throwaway dir unless the caller already chose one, so a
# test run never touches a real project's .weftos/runtime or ~/.clawft.
#
# One shared dir for every test binary let binaries trample each other's
# files and hid the legacy-adoption path (a set WEFTOS_RUNTIME_DIR disables
# it), so each test process (nextest) or test binary (cargo test) gets its
# own dir under WEFTOS_TEST_RUNTIME_ROOT through scripts/test-runtime-wrap.sh.
# A caller-chosen WEFTOS_RUNTIME_DIR still wins and is left alone.
TEST_RUNTIME_CLEANUP=""
cleanup_test_runtime() {
    [ -n "$TEST_RUNTIME_CLEANUP" ] && rm -rf "$TEST_RUNTIME_CLEANUP"
    return 0
}
# One EXIT trap for the whole script (a second `trap ... EXIT` would replace it).
trap cleanup_test_runtime EXIT

isolate_test_runtime() {
    [ -n "${WEFTOS_RUNTIME_DIR:-}" ] && return 0
    NEXTEST_CONFIG=(--config-file "$ROOT/config/nextest.toml")
    if [ -z "${WEFTOS_TEST_RUNTIME_ROOT:-}" ]; then
        WEFTOS_TEST_RUNTIME_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/weftos-test-runtime.XXXXXX")"
        export WEFTOS_TEST_RUNTIME_ROOT
        TEST_RUNTIME_CLEANUP="$WEFTOS_TEST_RUNTIME_ROOT"
    fi
    # `cargo test` runs each test binary through the wrapper as its runner.
    export "$(cargo_runner_var)=$ROOT/scripts/test-runtime-wrap.sh"
}

# Name of the cargo `target.<host>.runner` env var.
cargo_runner_var() {
    local triple
    triple="$(rustc -vV | sed -n 's/^host: //p')"
    printf 'CARGO_TARGET_%s_RUNNER' "$(printf '%s' "$triple" | tr 'a-z-' 'A-Z_')"
}

# Doctests run through rustdoc, which ignores the cargo runner: give them one
# throwaway dir too (a caller-chosen WEFTOS_RUNTIME_DIR still wins).
doctest_runtime() {
    if [ -z "${WEFTOS_RUNTIME_DIR:-}" ] && [ -n "${WEFTOS_TEST_RUNTIME_ROOT:-}" ]; then
        mkdir -p "$WEFTOS_TEST_RUNTIME_ROOT/doctests"
        WEFTOS_RUNTIME_DIR="$WEFTOS_TEST_RUNTIME_ROOT/doctests" "$@"
    else
        "$@"
    fi
}

# nextest config: per-test runtime dirs (see config/nextest.toml). Set by
# isolate_test_runtime only when it made a root: the wrapper refuses to run
# without one, so a caller-pinned WEFTOS_RUNTIME_DIR runs without it.
NEXTEST_CONFIG=()

# ── Standalone crates ───────────────────────────────────────────────
# The leaf crates (weftos-leaf-scene/-renderer/-sim, weftos-display-client,
# …) carry their own `[workspace]` table and sit in the root `exclude`, so
# `-p <crate>` from the workspace root cannot see them. `check|clippy|test
# <pkg>` detects such a package and runs cargo inside its directory.
STANDALONE_PACKAGES=()

standalone_crate_dir() {
    local dir="$ROOT/crates/$1"
    if [ -f "$dir/Cargo.toml" ] && grep -q '^\[workspace\]' "$dir/Cargo.toml"; then
        printf '%s' "$dir"
    fi
}

# Move standalone names out of TEST_PACKAGES into STANDALONE_PACKAGES.
split_standalone_packages() {
    STANDALONE_PACKAGES=()
    local rest=() pkg
    for pkg in ${TEST_PACKAGES[@]+"${TEST_PACKAGES[@]}"}; do
        if [ -n "$(standalone_crate_dir "$pkg")" ]; then
            STANDALONE_PACKAGES+=("$pkg")
        else
            rest+=("$pkg")
        fi
    done
    TEST_PACKAGES=(${rest[@]+"${rest[@]}"})
}

# run_standalone <check|clippy|test> — one cargo run per standalone crate.
# Returns 0 when there were none, so callers fall through to the
# workspace path for whatever is left in TEST_PACKAGES.
run_standalone() {
    local verb="$1" pkg dir rc=0
    local feat=()
    [ -n "$FEATURES" ] && feat=(--features "$FEATURES")
    # Standalone crates with native deps (SDL2 in weftos-leaf-sim) link against
    # Homebrew libraries on macOS, which the linker does not search by default.
    if [ "$(uname -s)" = Darwin ] && [ -d /opt/homebrew/lib ]; then
        export LIBRARY_PATH="/opt/homebrew/lib${LIBRARY_PATH:+:$LIBRARY_PATH}"
    fi
    for pkg in ${STANDALONE_PACKAGES[@]+"${STANDALONE_PACKAGES[@]}"}; do
        dir="$(standalone_crate_dir "$pkg")"
        header "Running cargo $verb in crates/$pkg (standalone crate)${FEATURES:+ --features $FEATURES}"
        timer_start
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   (cd crates/%s && cargo %s --all-targets%s)\n" "$pkg" "$verb" "${FEATURES:+ --features $FEATURES}"
        else
            case "$verb" in
                check)  (cd "$dir" && cargo check --all-targets ${feat[@]+"${feat[@]}"}) || rc=$? ;;
                clippy) (cd "$dir" && cargo clippy --all-targets ${feat[@]+"${feat[@]}"} -- -D warnings) || rc=$? ;;
                test)   (cd "$dir" && cargo test ${feat[@]+"${feat[@]}"}) || rc=$? ;;
            esac
            if [ $rc -eq 0 ]; then pass "$verb crates/$pkg"; else fail "$verb crates/$pkg"; fi
        fi
        timer_end
        [ $rc -ne 0 ] && return $rc
    done
    return 0
}

# Doctest scope: `cargo test --doc` errors ("no library targets found") for a
# bin-only package, so a scoped run keeps only packages with a lib-like target.
# Prints the `-p <pkg>` args; prints nothing when no named package qualifies.
# Refuses (non-zero) if cargo metadata cannot be read, rather than guessing.
doc_scope_args() {
    local meta
    meta=$(cargo metadata --no-deps --format-version 1 2>/dev/null) || return 1
    printf '%s' "$meta" | python3 -c '
import json, sys
want = set(sys.argv[1:])
libs = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
for p in json.load(sys.stdin)["packages"]:
    if p["name"] in want and any(set(t["kind"]) & libs for t in p["targets"]):
        print("-p"); print(p["name"])
' "$@"
}

workspace_test() {
    isolate_test_runtime
    local extra=()
    local filter=() cfilter=()
    # `--filter <substr>` narrows to tests whose name contains <substr>.
    if [ -n "${TEST_FILTER:-}" ]; then
        filter=("$TEST_FILTER")
        cfilter=(-- "$TEST_FILTER")
    fi
    [ "$NO_FAIL_FAST" = true ] && extra+=(--no-fail-fast)
    # Honor `--features <f>` so feature-gated adapters (matrix, email, …)
    # are compiled and tested (WEFT-159).
    [ -n "$FEATURES" ] && extra+=(--features "$FEATURES")
    # `scripts/build.sh test <pkg>…` scopes to the named packages; no
    # packages means the whole workspace (the historical behavior).
    local scope=(--workspace)
    if [ ${#TEST_PACKAGES[@]} -gt 0 ]; then
        scope=()
        local pkg
        for pkg in "${TEST_PACKAGES[@]}"; do
            scope+=(-p "$pkg")
        done
    fi
    # ${arr[@]+…} guard: macOS bash 3.2 + `set -u` errors on expanding an
    # empty array without it.
    run_doctests() {
        if [ ${#TEST_PACKAGES[@]} -eq 0 ]; then
            doctest_runtime cargo test "${scope[@]}" --doc ${extra[@]+"${extra[@]}"}
            return
        fi
        local doc=() line
        local out
        out=$(doc_scope_args "${TEST_PACKAGES[@]}") || { echo "cannot read cargo metadata for doctest scope" >&2; return 1; }
        while IFS= read -r line; do [ -n "$line" ] && doc+=("$line"); done <<< "$out"
        if [ ${#doc[@]} -eq 0 ]; then
            echo "no library targets in ${TEST_PACKAGES[*]}; skipping doctests"
            return 0
        fi
        doctest_runtime cargo test "${doc[@]}" --doc ${extra[@]+"${extra[@]}"}
    }
    if command -v cargo-nextest >/dev/null 2>&1; then
        env -u "$(cargo_runner_var)" cargo nextest run ${NEXTEST_CONFIG[@]+"${NEXTEST_CONFIG[@]}"} "${scope[@]}" ${extra[@]+"${extra[@]}"} ${filter[@]+"${filter[@]}"} \
            && { [ ${#filter[@]} -gt 0 ] || run_doctests; }
    else
        cargo test "${scope[@]}" ${extra[@]+"${extra[@]}"} ${cfilter[@]+"${cfilter[@]}"}
    fi
}

cmd_test() {
    split_standalone_packages
    if [ ${#STANDALONE_PACKAGES[@]} -gt 0 ]; then
        run_standalone test || return $?
        [ ${#TEST_PACKAGES[@]} -eq 0 ] && return 0
    fi
    local scope_desc="--workspace"
    [ ${#TEST_PACKAGES[@]} -gt 0 ] && scope_desc="${TEST_PACKAGES[*]}"
    if command -v cargo-nextest >/dev/null 2>&1; then
        header "Running cargo nextest run ($scope_desc) (+ doctests)"
    else
        header "Running cargo test ($scope_desc) (cargo-nextest not installed)"
    fi
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   workspace test (nextest if available, else cargo test)\n"
    else
        # Always show full output — tail -5 hides test results
        workspace_test 2>&1
    fi
    timer_end
}

# Browser regression suite (WEFT-388 / M5-A).
#
# Builds + runs `crates/clawft-wasm/tests/browser_pipeline.rs` under
# headless Chrome via `wasm-pack test`. Requires:
#   - wasm-pack             (rustup component or cargo-installed)
#   - chromedriver matching the installed Chrome
#   - Chrome / Chromium     (linux: google-chrome; macOS: /Applications/.../Google Chrome)
# CI installs all three via the `wasm-browser-test` job in
# `.github/workflows/pr-gates.yml`.
#
# Override the browser via `--features` if you want firefox: this
# script defaults to chrome.
cmd_test_browser() {
    header "Running browser WASM regression suite (wasm-pack --headless --chrome)"
    if ! command -v wasm-pack >/dev/null 2>&1; then
        fail "wasm-pack not found — install via: cargo install wasm-pack"
        return 1
    fi
    if ! check_target_installed wasm32-unknown-unknown; then return 1; fi
    timer_start
    # Default suite is browser (entry-point contracts). Pass FEATURES=browser-opfs
    # to also exercise OPFS FS (WEFT-13 / browser_opfs.rs), env
    # (WEFT-14 / browser_env_persist.rs), and conversation history
    # (WEFT-399 / browser_history_persist.rs) persistence.
    local feat="browser"
    if [ -n "$FEATURES" ]; then
        feat="browser,$FEATURES"
    fi
    local args=(wasm-pack test --headless --chrome crates/clawft-wasm
                --no-default-features --features "$feat"
                --test browser_pipeline)
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   %s\n" "${args[*]}"
        if echo "$feat" | grep -q 'browser-opfs'; then
            printf "  ${YELLOW}DRY${NC}   … --test browser_opfs\n"
            printf "  ${YELLOW}DRY${NC}   … --test browser_env_persist\n"
            printf "  ${YELLOW}DRY${NC}   … --test browser_history_persist\n"
        fi
        timer_end
        return 0
    fi
    # Always show full output — tail -5 hides per-test results from the runner.
    "${args[@]}" 2>&1
    local rc=$?
    if [ "$rc" -eq 0 ] && echo "$feat" | grep -q 'browser-opfs'; then
        info "Running WEFT-13 OPFS filesystem persistence suite"
        wasm-pack test --headless --chrome crates/clawft-wasm \
            --no-default-features --features "$feat" \
            --test browser_opfs 2>&1
        rc=$?
        if [ "$rc" -eq 0 ]; then
            info "Running WEFT-14 BrowserEnvironment OPFS persistence suite"
            wasm-pack test --headless --chrome crates/clawft-wasm \
                --no-default-features --features "$feat" \
                --test browser_env_persist 2>&1
            rc=$?
        fi
        if [ "$rc" -eq 0 ]; then
            info "Running WEFT-399 OPFS conversation history + config suite"
            wasm-pack test --headless --chrome crates/clawft-wasm \
                --no-default-features --features "$feat" \
                --test browser_history_persist 2>&1
            rc=$?
        fi
    fi
    timer_end
    return "$rc"
}

# Browser WASM bundle-size gate (WEFT-389 / M5-A).
#
# Runs `scripts/bench/check-bundle-size.sh` against the post-bindgen
# bundle. Default thresholds (raw 1600 KB / gz 600 KB) live in the
# script and are documented in `docs/architecture/wasm-bundle-size.md`.
# Override via `--features` style if you ever need to override here:
#   scripts/build.sh bundle-size 1500 550
cmd_bundle_size() {
    header "Browser WASM bundle-size gate"
    local pkg="$ROOT/crates/clawft-wasm/www/pkg/clawft_wasm_bg.wasm"
    if [ ! -f "$pkg" ]; then
        info "pkg/ not found — running scripts/build.sh browser first"
        cmd_browser || return 1
    fi
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   scripts/bench/check-bundle-size.sh\n"
        timer_end
        return 0
    fi
    bash "$ROOT/scripts/bench/check-bundle-size.sh" "$pkg" "$@"
    local rc=$?
    timer_end
    return "$rc"
}

# VSCode dev-panel wasm bundle (WEFT-484 / M6-B).
#
# Promotes `extensions/vscode-weft-panel/scripts/build-wasm.sh` into a
# first-class scripts/build.sh subcommand so the panel build pipeline
# is reachable from the same place as every other build target. The
# inner script handles:
#   - wasm-pack (preferred) or cargo + wasm-bindgen fallback
#   - wasm-opt -Oz (WEFT-246) with the rustc 1.93 feature flag union
#   - emission to extensions/vscode-weft-panel/webview/wasm/
#
# This wrapper additionally re-uses scripts/bench/check-bundle-size.sh
# (the same gate WEFT-389 uses for the clawft-wasm browser bundle) to
# enforce the documented panel budget. The clawft_gui_egui bundle is
# distinct from the clawft-wasm bundle and rides a separate budget;
# defaults are wider here because the panel ships eframe + egui_extras.
#
#   raw budget:  4500 KB  (WEFT-577 — restored WEFT-484 raw ceiling)
#   gz budget:   1600 KB  (step toward 1500; measured ~1576 KB gzip-9)
#
# History:
#   - WEFT-484 set 4500/1500.
#   - M7+M7b feature wave grew the bundle to ~7.3 MB raw / ~3.4 MB gz;
#     the gate was raised to 7600/3500 so ship wasn't blocked.
#   - WEFT-577 (this pass) reclaimed raw under 4500 and gz to ~1576 via
#     loader feature cuts (drop resvg/http/gif/webp), native-only
#     egui_demo_lib, release-wasm opt-level=z + wasm-opt -Oz, Latin
#     font subsets (no emoji pack), and splash-logo quantisation.
#     Residual to full 1500 KB gz is documented in
#     docs/architecture/wasm-bundle-size.md and the wave-0l result.
#
# Override by passing positional args: scripts/build.sh wasm-panel 4500 1600
cmd_wasm_panel() {
    header "Building VSCode dev-panel wasm bundle"
    local inner="$ROOT/extensions/vscode-weft-panel/scripts/build-wasm.sh"
    if [ ! -x "$inner" ]; then
        fail "inner build script missing or not executable: $inner"
        return 1
    fi
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   %s\n" "$inner"
        printf "  ${YELLOW}DRY${NC}   scripts/bench/check-bundle-size.sh <panel-bundle>\n"
        timer_end
        return 0
    fi
    if ! "$inner"; then
        fail "panel wasm build failed"
        timer_end
        return 1
    fi
    timer_end

    local bundle="$ROOT/extensions/vscode-weft-panel/webview/wasm/clawft_gui_egui_bg.wasm"
    if [ ! -f "$bundle" ]; then
        fail "expected bundle missing: $bundle"
        return 1
    fi
    report_binary_size "$bundle" "Panel WASM (post-opt)"

    # Size gate. Re-uses the clawft-wasm bundle gate against panel-specific
    # thresholds. Override via positional args:
    #   scripts/build.sh wasm-panel <max-raw-kb> <max-gz-kb>
    local max_raw_kb="${1:-}"
    local max_gz_kb="${2:-}"
    [ -z "$max_raw_kb" ] && max_raw_kb=4500
    [ -z "$max_gz_kb" ] && max_gz_kb=1600
    info "Panel size gate: raw≤${max_raw_kb}KB, gz≤${max_gz_kb}KB"
    if ! bash "$ROOT/scripts/bench/check-bundle-size.sh" \
            "$bundle" "$max_raw_kb" "$max_gz_kb"; then
        fail "panel bundle exceeds size budget"
        return 1
    fi
    pass "Panel wasm bundle ready at extensions/vscode-weft-panel/webview/wasm/"
}

# WEFT-114: clawft-kernel on wasm32-unknown-unknown with mesh (and every other
# default feature) off. Catches non-browser code that creeps into the default
# feature set and would break browser/WASM consumers. Equivalent CI hard gate:
#   .github/workflows/pr-gates.yml → wasm-kernel-no-mesh
check_kernel_wasm_no_mesh() {
    cargo check -p clawft-kernel --target wasm32-unknown-unknown --no-default-features
}

# WEFT-504: `ecc` must not compile on wasm32-unknown-unknown (blake3 /
# vector-memory / BVH are native). Expect cargo check to fail; success is a
# gate failure. Logs suppressed unless VERBOSE=1.
check_kernel_ecc_rejected_on_wasm() {
    local log
    log="$(mktemp -t weft-ecc-wasm.XXXXXX)"
    # shellcheck disable=SC2064
    trap "rm -f '$log'" RETURN
    set +e
    cargo check -p clawft-kernel \
        --target wasm32-unknown-unknown \
        --no-default-features \
        --features ecc \
        >"$log" 2>&1
    local rc=$?
    set -e
    if [ "$rc" -eq 0 ]; then
        fail "ecc unexpectedly compiled on wasm32-unknown-unknown (WEFT-504)"
        if [ "${VERBOSE:-0}" = "1" ]; then
            cat "$log"
        fi
        return 1
    fi
    # Prefer our compile_error message; also accept dep-level getrandom failures.
    if grep -q 'WEFT-504\|ecc.*not supported\|getrandom\|could not compile' "$log"; then
        pass "ecc correctly rejected on wasm32-unknown-unknown (WEFT-504)"
        return 0
    fi
    fail "ecc wasm check failed for an unexpected reason (WEFT-504); re-run with VERBOSE=1"
    if [ "${VERBOSE:-0}" = "1" ]; then
        cat "$log"
    fi
    return 1
}

# P3-K0: the mesh role must build without exochain/cluster/tilezero/ecc so a
# machine mesh service can depend on the kernel's mesh modules alone.
cmd_check_mesh_only() {
    header "Running cargo check -p clawft-kernel --no-default-features --features native,mesh"
    timer_start
    run_cmd cargo check -p clawft-kernel --no-default-features --features native,mesh
    timer_end
}

# The cog conformance launcher's feature set (scripts/pi/pi_plan.py
# launcher_cargo_args): workload-runtime without mesh/ecc/exochain, so code
# that names the licence module must stay behind its cfg.
cmd_check_launcher() {
    header "Running cargo check -p clawft-kernel --no-default-features --features workload-runtime --example cog_adapter_run"
    timer_start
    run_cmd cargo check -p clawft-kernel --no-default-features --features workload-runtime --example cog_adapter_run
    timer_end
}

# build-kb: the docs knowledge-base generator (tools/build-kb, its own
# workspace and lockfile; used by .github/workflows/docs-assets.yml).
cmd_kb() {
    header "Building tools/build-kb (release, --locked)"
    timer_start
    run_cmd cargo build --release --locked --manifest-path tools/build-kb/Cargo.toml
    timer_end
}

# fuzz-check: the clawft-core fuzz crate is outside the workspace and has its
# own lockfile; --locked fails when that lock goes stale.
cmd_fuzz_check() {
    header "Running cargo check --locked for crates/clawft-core/fuzz"
    timer_start
    run_cmd cargo check --locked --manifest-path crates/clawft-core/fuzz/Cargo.toml
    timer_end
}

# Doc drift: the weaver/weft commands and [kernel.mesh] keys in the verified
# deployment docs must exist in target/debug/{weaver,weft}
# (scripts/check-doc-commands.sh). Not part of `gate`.
cmd_check_doc_commands() {
    header "Checking documented weaver/weft commands against the built CLI"
    timer_start
    run_cmd scripts/check-doc-commands.sh --self-test
    run_cmd scripts/check-doc-commands.sh
    timer_end
}

# P3-X: the machine mesh service lane: the mesh crates' tests, the Phase 3
# end-to-end test (the real service plus two user daemons as the current user,
# tempdirs only; scripts/dev/mesh-p3-e2e.sh), the no-owned-state gate and the
# mesh-only kernel build. Nothing here needs root or touches /var or /etc.
cmd_test_mesh_service() {
    header "Running the machine mesh service lane (mesh crates, Phase 3 e2e, no-owned-state, mesh-only build)"
    timer_start
    isolate_test_runtime
    run_cmd cargo test -p clawft-mesh-local -p clawft-mesh-service
    # The shipped binary runs the process smoke (argument parsing, default
    # paths, --health-listen off, adopted key, real quarantine, restart).
    run_cmd cargo build -p clawft-weave --bin weaver
    local target_dir
    target_dir="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
    run_cmd env "WEAVER_BIN=${target_dir:-$ROOT/target}/debug/weaver" scripts/dev/mesh-p3-e2e.sh
    cmd_check_mesh_no_owned_state
    cmd_check_mesh_only
    timer_end
}

# P3-S: compile (not run) the tests of the named packages. The lead session
# runs the tests; this is the fast loop for writing them.
cmd_check_tests() {
    if [ ${#TEST_PACKAGES[@]} -eq 0 ]; then
        fail "check-tests needs at least one package: scripts/build.sh check-tests <package>…"
        return 1
    fi
    local scope=() pkg
    for pkg in "${TEST_PACKAGES[@]}"; do scope+=(-p "$pkg"); done
    header "Running cargo check ${scope[*]} --tests"
    timer_start
    run_cmd cargo check "${scope[@]}" --tests
    timer_end
}

# P3-S "must not own": the machine mesh service links the kernel only for its
# mesh modules. It must never pull in the chain, resource tree, RVF runtime,
# the governance tile, the operator CLI, or the kernel features that bring
# them (exochain, cluster, tilezero, ecc).
cmd_check_mesh_no_owned_state() {
    header "Asserting clawft-mesh-service owns no chain, governance or operator-CLI state"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cargo tree -p clawft-mesh-service -e normal,features\n"
        timer_end
        return 0
    fi
    local tree rc=0
    tree="$(cargo tree -p clawft-mesh-service -e normal,features 2>&1)" || rc=$?
    if [ "$rc" -ne 0 ]; then
        fail "cargo tree failed for clawft-mesh-service"
        printf '%s\n' "$tree" | tail -20
        return 1
    fi
    local hits
    hits="$(printf '%s\n' "$tree" \
        | grep -E '(exo-resource-tree|rvf-runtime|cognitum-gate-tilezero|clawft-weave) v[0-9]|clawft-kernel feature "(exochain|cluster|tilezero|ecc)"' \
        || true)"
    if [ -n "$hits" ]; then
        fail "clawft-mesh-service must not own state, but its dependency tree contains:"
        printf '%s\n' "$hits" | sort -u | sed 's/^/        /'
        return 1
    fi
    # Direct dependencies are an allowlist: a new one needs a deliberate edit here.
    local allowed=" clawft-mesh-local clawft-kernel clawft-types serde serde_json ed25519-dalek sha2 thiserror rand toml async-trait tracing tokio libc "
    local direct name extra=""
    direct="$(cargo tree -p clawft-mesh-service -e normal --depth 1 --prefix none 2>&1 | awk 'NR>1 {print $1}' | sort -u)"
    for name in $direct; do
        case "$allowed" in *" $name "*) ;; *) extra="$extra $name" ;; esac
    done
    if [ -n "$extra" ]; then
        fail "clawft-mesh-service has direct dependencies outside the allowlist:$extra (edit cmd_check_mesh_no_owned_state deliberately)"
        return 1
    fi
    pass "clawft-mesh-service dependency tree is free of exochain, resource tree, RVF runtime, tilezero and clawft-weave; direct deps match the allowlist"
    timer_end
}

# ADR-106 phase 1c: the daemon trusts the service-stamped `deliver` origin only
# because its client passed the real peer-credential check. The client's
# `testing` feature injects a fake server credential, so no non-test build of
# the daemon may enable it (dev-dependencies are not part of `-e normal`).
cmd_check_daemon_no_mesh_testing() {
    header "Asserting the daemon's dependency graph does not enable clawft-mesh-local's testing feature"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cargo tree -p clawft-weave -e normal,features -i clawft-mesh-local\n"
        timer_end
        return 0
    fi
    # Default features, every feature, and none: a shipped feature set must not enable it.
    local flags tree rc hits
    for flags in "" "--all-features" "--no-default-features"; do
        rc=0
        # shellcheck disable=SC2086
        tree="$(cargo tree -p clawft-weave $flags -e normal,features -i clawft-mesh-local 2>&1)" || rc=$?
        if [ "$rc" -ne 0 ]; then
            fail "cargo tree ${flags:-(default features)} failed for clawft-weave"
            printf '%s\n' "$tree" | tail -20
            return 1
        fi
        # The inverted tree must still reach the crate, or the check proves nothing.
        case "$tree" in *"clawft-mesh-local v"*) ;; *)
            fail "clawft-mesh-local is not in clawft-weave's dependency graph (${flags:-default}); update cmd_check_daemon_no_mesh_testing"
            return 1 ;;
        esac
        hits="$(printf '%s\n' "$tree" | grep -E 'clawft-mesh-local feature "testing"' || true)"
        if [ -n "$hits" ]; then
            fail "the daemon's dependency graph (${flags:-default features}) enables clawft-mesh-local's testing feature (it fakes the service peer check):"
            printf '%s\n' "$tree" | sed 's/^/        /'
            return 1
        fi
    done
    pass "clawft-mesh-local's testing feature is not enabled in the daemon's dependency graph"
    timer_end
}

cmd_check() {
    # `check <pkg>…` scopes to the named packages (fast loop for new crates);
    # the kernel wasm gates below only run for the whole-workspace check.
    split_standalone_packages
    if [ ${#STANDALONE_PACKAGES[@]} -gt 0 ]; then
        run_standalone check || return $?
        [ ${#TEST_PACKAGES[@]} -eq 0 ] && return 0
    fi
    if [ ${#TEST_PACKAGES[@]} -gt 0 ]; then
        local scope=() pkg
        for pkg in "${TEST_PACKAGES[@]}"; do scope+=(-p "$pkg"); done
        header "Running cargo check ${scope[*]}${FEATURES:+ --features $FEATURES}"
        timer_start
        if [ -n "$FEATURES" ]; then
            run_cmd cargo check "${scope[@]}" --features "$FEATURES"
        else
            run_cmd cargo check "${scope[@]}"
        fi
        timer_end
        return 0
    fi
    header "Running cargo check --workspace${FEATURES:+ --features $FEATURES}"
    timer_start
    if [ -n "$FEATURES" ]; then
        run_cmd cargo check --workspace --features "$FEATURES"
    else
        run_cmd cargo check --workspace
    fi
    timer_end

    cmd_check_launcher

    # WEFT-114: hard local twin of the PR-gates wasm-kernel-no-mesh job.
    # Skipped only when the target is not installed (contributors without the
    # browser WASM toolchain); CI always has the target.
    if check_target_installed wasm32-unknown-unknown; then
        header "Running cargo check -p clawft-kernel --target wasm32-unknown-unknown --no-default-features (no mesh)"
        timer_start
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   cargo check -p clawft-kernel --target wasm32-unknown-unknown --no-default-features\n"
        else
            check_kernel_wasm_no_mesh
        fi
        timer_end

        # WEFT-504: assert `ecc` is rejected on wasm32-unknown-unknown
        # (compile_error in clawft-kernel + native-only deps).
        header "Asserting ecc is rejected on wasm32-unknown-unknown (WEFT-504)"
        timer_start
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   cargo check -p clawft-kernel --target wasm32-unknown-unknown --no-default-features --features ecc  (expect fail)\n"
        else
            check_kernel_ecc_rejected_on_wasm
        fi
        timer_end
    else
        skip "wasm32-unknown-unknown not installed — skip kernel no-mesh WASM check (WEFT-114)"
        info "Install with: rustup target add wasm32-unknown-unknown"
    fi
}

cmd_clippy() {
    # `clippy <pkg>…` scopes to the named packages; none means the workspace.
    split_standalone_packages
    if [ ${#STANDALONE_PACKAGES[@]} -gt 0 ]; then
        run_standalone clippy || return $?
        [ ${#TEST_PACKAGES[@]} -eq 0 ] && return 0
    fi
    local scope=(--workspace)
    if [ ${#TEST_PACKAGES[@]} -gt 0 ]; then
        scope=()
        local pkg
        for pkg in "${TEST_PACKAGES[@]}"; do scope+=(-p "$pkg"); done
        # Scoped runs lint only the named packages, not their workspace deps.
        scope+=(--no-deps)
    fi
    header "Running clippy (warnings as errors) ${scope[*]}${FEATURES:+ --features $FEATURES}"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cargo clippy --all-targets %s%s -- -D warnings\n" \
            "${scope[*]}" "${FEATURES:+ --features $FEATURES}"
    else
        # Always show full output — tail -5 hides warnings
        if [ -n "$FEATURES" ]; then
            cargo clippy --keep-going --all-targets "${scope[@]}" --features "$FEATURES" -- -D warnings 2>&1
        else
            cargo clippy --keep-going --all-targets "${scope[@]}" -- -D warnings 2>&1
        fi
    fi
    timer_end
}

# Workspace clippy plus the feature-gated lanes a bare --workspace run skips.
cmd_clippy_gate() {
    cargo clippy --keep-going --all-targets --workspace -- -D warnings || return $?
    cargo clippy --all-targets -p clawft-voice-onnx --features onnx -- -D warnings
}

cmd_bench() {
    local crate="$1" name="$2"
    if [ -z "$crate" ] || [ -z "$name" ]; then
        fail "usage: scripts/build.sh bench <crate> <bench-name> [--features f]"
        return 1
    fi
    header "Running cargo bench -p $crate --bench $name${FEATURES:+ --features $FEATURES}"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cargo bench -p %s --bench %s%s\n" \
            "$crate" "$name" "${FEATURES:+ --features $FEATURES}"
    else
        # Benches print their own report (e.g. vector_backend_bench emits
        # JSON lines + a human table) — don't route through run_cmd, which
        # tails build logs to 5 lines and would truncate that report.
        if [ -n "$FEATURES" ]; then
            cargo bench -p "$crate" --bench "$name" --features "$FEATURES"
        else
            cargo bench -p "$crate" --bench "$name"
        fi
    fi
    timer_end
}

cmd_clean() {
    header "Cleaning build artifacts"
    run_cmd cargo clean
    if [ -d "$ROOT/clawft-ui/dist" ]; then
        info "Removing clawft-ui/dist"
        rm -rf "$ROOT/clawft-ui/dist"
    fi
    if [ -d "$ROOT/crates/clawft-wasm/www/pkg" ]; then
        info "Removing crates/clawft-wasm/www/pkg"
        rm -rf "$ROOT/crates/clawft-wasm/www/pkg"
    fi
    pass "Clean complete"
}

# Prune orphaned dev-profile incremental caches (WEFT: target-dir growth).
#
# Every dependency bump, feature-flag flip, or rustc update mints a fresh
# incremental hash directory and abandons the previous one. Cargo never
# garbage-collects these, so `target/debug/incremental` grows without bound on
# a long-lived dev box — measured 2026-07-24 at 128G across 2,952 dirs, of
# which only 213 belonged to the most recent build session.
#
# Pruning by mtime is safe: an incremental cache is pure derived state. The
# worst case for deleting a live one is a slower next compile, never a wrong
# build. Default threshold is 7 days; override with `--days N`.
#
# Space is reported as a df delta rather than a du sum: cargo hardlinks and
# APFS-clones blocks between deps/ and incremental/, so per-directory du
# double-counts shared extents (du said 117G for a set that freed less).
cmd_clean_stale() {
    local days="${CLEAN_STALE_DAYS:-7}"
    local incr="$ROOT/target/debug/incremental"
    header "Pruning incremental caches older than ${days}d"

    if [ ! -d "$incr" ]; then
        info "No target/debug/incremental — nothing to prune"
        return 0
    fi

    local total stale
    total=$(find "$incr" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
    stale=$(find "$incr" -mindepth 1 -maxdepth 1 -type d -mtime "+$days" | wc -l | tr -d ' ')
    info "$stale of $total cache dirs older than ${days}d"

    if [ "$stale" -eq 0 ]; then
        pass "Nothing to prune"
        return 0
    fi

    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   rm -rf %s stale dirs under target/debug/incremental\n" "$stale"
        return 0
    fi

    local before after
    before=$(df -k "$ROOT" | tail -1 | awk '{print $4}')
    timer_start
    # -n 200 keeps the argv under ARG_MAX when thousands of dirs are stale.
    find "$incr" -mindepth 1 -maxdepth 1 -type d -mtime "+$days" -print0 \
        | xargs -0 -n 200 rm -rf
    timer_end
    after=$(df -k "$ROOT" | tail -1 | awk '{print $4}')

    awk -v b="$before" -v a="$after" \
        'BEGIN { printf "  \033[0;36mINFO\033[0m  Reclaimed %.1f GB (free: %.1f -> %.1f GB)\n", \
                 (a-b)/1048576, b/1048576, a/1048576 }'
    pass "Pruned $stale stale incremental caches"
}

cmd_serve() {
    local port="${1:-8080}"
    local www_dir="$ROOT/crates/clawft-wasm/www"
    header "Serving browser test harness on http://localhost:$port"
    if [ ! -d "$www_dir/pkg" ]; then
        fail "www/pkg/ not found — run 'scripts/build.sh browser' first"
        return 1
    fi

    # Generate .env-keys.json from detected environment variables.
    # Keys are served locally only — never committed (gitignored).
    local keys_file="$www_dir/.env-keys.json"
    local found=0
    printf '{' > "$keys_file"
    local first=true
    for pair in \
        "OPENROUTER_API_KEY:openrouter" \
        "ANTHROPIC_API_KEY:anthropic" \
        "OPENAI_API_KEY:openai" \
        "DEEPSEEK_API_KEY:deepseek" \
        "GROQ_API_KEY:groq" \
        "GOOGLE_GEMINI_API_KEY:gemini" \
        "XAI_API_KEY:xai"; do
        local env_var="${pair%%:*}"
        local provider="${pair##*:}"
        local val="${!env_var:-}"
        if [ -n "$val" ]; then
            if [ "$first" = true ]; then first=false; else printf ',' >> "$keys_file"; fi
            printf '"%s":"%s"' "$provider" "$val" >> "$keys_file"
            info "Detected $env_var → providers.$provider"
            found=$((found + 1))
        fi
    done
    printf '}' >> "$keys_file"

    if [ "$found" -gt 0 ]; then
        pass "$found API key(s) injected into .env-keys.json (local only)"
    else
        info "No API keys detected in environment — textarea defaults will be used"
    fi

    # Clean up keys file on exit (Ctrl+C or normal stop).
    trap 'rm -f "$keys_file" 2>/dev/null; exit 0' INT TERM

    info "Open http://localhost:$port in your browser"
    info "API requests proxied via /proxy/ (avoids CORS)"
    python3 "$SCRIPT_DIR/dev_server.py" "$port" "$www_dir"
    rm -f "$keys_file" 2>/dev/null
}

# ── cargo-audit ignore list ─────────────────────────────────────────
# Advisories that are known and tracked as 0.8.x followups. Each
# `--ignore` carries the WEFT-N tracker so the next reviewer can map
# the ID back to the followup. See:
#   .planning/reviews/0.7.0-release-gate/audit-findings/cargo-audit-cold-run-2026-04-28.md
#
# When a followup lands, drop the matching IDs from this array.
#
# WEFT-551 — DONE (wave0c): wasmtime/wasmtime-wasi 33 → 45.0.3; ignores removed.
# wasmtime 48.0.5 + Rust 1.95 — DONE: RUSTSEC-2026-0222/0269/0314/0316 ignores removed.
# WEFT-552 — DONE (wave0c): rustls-webpki via ruvector-core 2.3 + pin 0.103.13; ignores removed.
# WEFT-553 — partial (wave0i): cleared serial / instant / rustls-pemfile / rand;
#            residual bincode + paste need upstream (ruvector/hnsw_rs, tokenizers/egui_dock).
CARGO_AUDIT_IGNORES=(
    # WEFT-553 residual — unmaintained, blocked on upstream
    --ignore RUSTSEC-2024-0436   # paste via tokenizers / egui_dock / macro_rules_attribute
    --ignore RUSTSEC-2025-0141   # bincode via ruvector-* / hnsw_rs
    # 0.8.0 residual — tracked post-tag; do not expand silently without Plane note
    --ignore RUSTSEC-2026-0194   # quick-xml quadratic attrs (transitive); need >=0.41
    --ignore RUSTSEC-2026-0195   # quick-xml NsReader DoS (transitive); need >=0.41
    --ignore RUSTSEC-2026-0221   # event-listener unsound (transitive async stack)
    --ignore RUSTSEC-2023-0086   # lexical-core unsound (transitive)
    --ignore RUSTSEC-2024-0449   # unsafe-libyaml unmaintained, via deprecated serde_yaml (board card: migrate off serde_yaml)
    # 2026-10 residuals, justified in docs/security/cargo-audit-residual.md
    --ignore RUSTSEC-2023-0071   # rsa Marvin timing: no fix; jsonwebtoken rust_crypto only verifies RS256 with public keys
    --ignore RUSTSEC-2026-0367   # zip 1.1.4 via candle-core (optional `candle` feature): extract() never called
    --ignore RUSTSEC-2026-0368   # zip 1.1.4 (same)
    --ignore RUSTSEC-2026-0369   # zip 1.1.4 (same)
    --ignore RUSTSEC-2026-0374   # basic-toml unmaintained via askama_derive <- uniffi_bindgen (build-time, trusted input)
    # spin 0.9.8 yanked via multer→axum; no non-yanked path without multer bump —
    # skip yanked check for 0.8.0 cut (still report via separate note)
    --no-yanked
)

cmd_audit() {
    header "Running cargo audit (with 0.7.0 ignore-list)"
    if ! command -v cargo-audit >/dev/null 2>&1; then
        fail "cargo-audit not installed — run: cargo install --locked cargo-audit"
        return 1
    fi
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cargo audit %s\n" "${CARGO_AUDIT_IGNORES[*]}"
    else
        # Fail on any new vulnerability or warning that is NOT in the ignore-list.
        cargo audit --deny warnings "${CARGO_AUDIT_IGNORES[@]}" 2>&1
    fi
    timer_end
}

# ── npm audit (WEFT-598) ────────────────────────────────────────────
# Audit npm lockfiles for critical/high. Moderates under the ruflo /
# @claude-flow/cli pin (OpenTelemetry chain) are accepted residual risk
# and do not fail the gate. See docs/security/npm-audit-residual.md.
#
# Directories audited when present with package-lock.json:
#   clawft-ui, . (root), docs/src
#
# Env:
#   NPM_AUDIT_LEVEL  severity floor for fail (default: high)
#                    values: critical | high | moderate | low | info
#   NPM_AUDIT_SOFT=1 soft mode: print findings, never fail

# Per-advisory residual allowlist for the npm audit gate. Each entry is
# "<lockfile label> <GHSA id> <expiry YYYY-MM-DD>". Only the named advisory in
# the named lockfile is tolerated, and only until the expiry date; any other
# >=level advisory still fails. Rationale: docs/security/npm-audit-residual.md.
NPM_AUDIT_ALLOW=(
    # braces <=3.0.3 stack-exhaustion DoS; no patched release exists (latest is
    # 3.0.3). Dev tooling only: agentic-flow -> http-proxy-middleware -> micromatch.
    "root GHSA-vfj7-8cjw-p6xm 2026-12-31"
)

# Print the >=level advisories in an `npm audit --json` document that are not
# covered by an unexpired NPM_AUDIT_ALLOW entry for this label. Empty output
# means everything at or above the floor is allowlisted.
npm_audit_unallowed() {
    local label="$1" level="$2" json="$3" today allow
    today=$(date +%Y-%m-%d)
    allow=$(printf '%s;' "${NPM_AUDIT_ALLOW[@]}")
    printf '%s' "$json" | NPM_ALLOW="$allow" LABEL="$label" LEVEL="$level" TODAY="$today" node -e '
        const rank={info:0,low:1,moderate:2,high:3,critical:4};
        let d=""; process.stdin.on("data",c=>d+=c); process.stdin.on("end",()=>{
          let j;
          try { j=JSON.parse(d); } catch(e) { console.log("parse-error"); return; }
          // Fail closed: an error document or one without a vulnerabilities map
          // proves nothing about the lockfile.
          if (!j||j.error||typeof j.vulnerabilities!=="object"||j.vulnerabilities===null) {
            console.log("parse-error"); return;
          }
          const floor=rank[process.env.LEVEL]??3;
          const allow=new Map();
          for (const e of (process.env.NPM_ALLOW||"").split(";").filter(Boolean)) {
            const f=e.split(" ");
            const ok=f.length===3 && /^\d{4}-\d{2}-\d{2}$/.test(f[2]) &&
              !Number.isNaN(Date.parse(f[2]+"T00:00:00Z")) &&
              new Date(f[2]+"T00:00:00Z").toISOString().slice(0,10)===f[2];
            if (!ok) { console.log("bad-allow-entry \""+e+"\""); continue; }
            if (f[0]===process.env.LABEL && f[2]>=process.env.TODAY) allow.set(f[1],f[2]);
          }
          const seen=new Set();
          for (const v of Object.values(j.vulnerabilities)) {
            for (const via of v.via||[]) {
              if (typeof via==="string"||(rank[via.severity]??4)<floor) continue;
              const id=(via.url||"").split("/").pop();
              if (allow.has(id)||seen.has(id)) continue;
              seen.add(id);
              console.log(id+" "+via.name+" ("+via.severity+")");
            }
          }
        });'
}

npm_audit_one() {
    local dir="$1"
    local label="${2:-$dir}"
    local level="${NPM_AUDIT_LEVEL:-high}"
    local audit_json audit_rc summary

    if [ ! -f "$dir/package-lock.json" ]; then
        info "skip $label — no package-lock.json"
        return 0
    fi
    if [ ! -f "$dir/package.json" ]; then
        info "skip $label — no package.json"
        return 0
    fi

    # npm audit exits 1 when findings exist at/above audit-level.
    set +e
    audit_json=$(cd "$dir" && npm audit --json --audit-level="$level" 2>/dev/null)
    audit_rc=$?
    set -e

    summary=$(printf '%s' "$audit_json" | node -e '
        let d=""; process.stdin.on("data",c=>d+=c); process.stdin.on("end",()=>{
          try {
            const j=JSON.parse(d);
            const v=j.metadata&&j.metadata.vulnerabilities||{};
            console.log(
              "crit="+ (v.critical||0) +
              " high="+ (v.high||0) +
              " mod="+ (v.moderate||0) +
              " low="+ (v.low||0) +
              " total="+ (v.total||0)
            );
          } catch(e) { console.log("parse-error"); }
        });
    ' 2>/dev/null || echo "parse-error")

    if [ "$audit_rc" -eq 0 ]; then
        pass "$label npm audit ($summary) — no ≥$level"
        return 0
    fi

    # Non-zero: either findings at/above level, or npm error.
    if printf '%s' "$audit_json" | grep -q '"vulnerabilities"'; then
        if [ "${NPM_AUDIT_SOFT:-0}" = "1" ]; then
            skip "$label npm audit ($summary) ≥$level present (soft)"
            return 0
        fi
        local unallowed
        unallowed=$(npm_audit_unallowed "$label" "$level" "$audit_json" 2>/dev/null || echo "parse-error")
        if [ -z "$unallowed" ]; then
            pass "$label npm audit ($summary) — ≥$level only allowlisted residual (docs/security/npm-audit-residual.md)"
            return 0
        fi
        info "non-allowlisted ≥$level advisories:"
        printf '%s\n' "$unallowed"
        fail "$label npm audit ($summary) — ≥$level present"
        # Print human report for the failure path (truncated).
        (cd "$dir" && npm audit --audit-level="$level" 2>&1 | tail -40) || true
        return 1
    fi

    # npm audit infrastructure failure — soft skip so missing npm never
    # blocks a Rust-only gate run.
    skip "$label npm audit unavailable (npm error)"
    return 0
}

cmd_npm_audit() {
    header "Running npm audit (critical/high gate — WEFT-598)"
    timer_start
    local level="${NPM_AUDIT_LEVEL:-high}"
    local failed=0

    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   npm audit --audit-level=%s (clawft-ui, root, docs/src)\n" "$level"
        timer_end
        return 0
    fi

    if ! command -v npm >/dev/null 2>&1; then
        skip "npm not installed"
        timer_end
        return 0
    fi
    if ! command -v node >/dev/null 2>&1; then
        skip "node not installed (needed to parse npm audit JSON)"
        timer_end
        return 0
    fi

    info "audit-level=$level soft=${NPM_AUDIT_SOFT:-0}"

    # Primary: clawft-ui (product UI) and root (ruflo pin / agent tooling)
    npm_audit_one "$ROOT/clawft-ui" "clawft-ui" || failed=$((failed + 1))
    npm_audit_one "$ROOT" "root" || failed=$((failed + 1))
    # Secondary product/docs surfaces when present
    npm_audit_one "$ROOT/docs/src" "docs/src" || failed=$((failed + 1))

    timer_end
    if [ "$failed" -gt 0 ]; then
        return 1
    fi
    return 0
}

# ── Agent Directory (AD-1/AD-2, WEFT agent-directory design) ───────────
# Team spec v1 (ADR-112 TM1): seeded failing fixtures must each be refused.
agents_team_tests() {
    node --test "$ROOT/scripts/agents-team-validate.test.mjs" "$ROOT/scripts/agents-validate.test.mjs"
}

# Gate check 17: package + team validation, then the team-spec and package-rule fixtures.
agents_validate_gate_impl() {
    node "$ROOT/scripts/agents-validate.mjs" --quiet || return 1
    agents_team_tests
}

cmd_agents_validate() {
    header "Validating agents/ packages (AD-1)"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   node scripts/agents-validate.mjs\n"
        timer_end
        return 0
    fi
    node "$ROOT/scripts/agents-validate.mjs"
    local rc=$?
    if [ $rc -eq 0 ]; then
        agents_team_tests
        rc=$?
    fi
    timer_end
    return $rc
}

cmd_agents_catalog() {
    header "agents/catalog.json (AD-1)"
    timer_start
    local args=()
    if [ "$AGENTS_CATALOG_CHECK" = true ]; then
        args+=(--check)
    fi
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   node scripts/agents-catalog.mjs %s\n" "${args[*]:-}"
        timer_end
        return 0
    fi
    node "$ROOT/scripts/agents-catalog.mjs" ${args[@]+"${args[@]}"}
    local rc=$?
    timer_end
    return $rc
}

cmd_agents_leak_check() {
    header "agents/ confidentiality leak check (AD-2)"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   scripts/agents-leak-check.sh\n"
        timer_end
        return 0
    fi
    bash "$ROOT/scripts/agents-leak-check.sh"
    local rc=$?
    timer_end
    return $rc
}

# ── Cog conformance harness (mesh-placement-08, ADR-100) ──────────────
cmd_cogs_conformance() {
    header "Cog conformance harness (scripts/cogs)"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   python3 scripts/cogs/conformance.py %s\n" "${COGS_ARGS[*]:-}"
        timer_end
        return 0
    fi
    local rc=0
    python3 "$ROOT/scripts/cogs/conformance.py" "${COGS_ARGS[@]}" || rc=$?
    timer_end
    return $rc
}

# ── Cog adapter launcher (mesh-placement-09) ──────────────────────────
# Builds examples/cog_adapter_run.rs, which the conformance harness runs as
# a plan `launcher` (cogs-conformance --launcher) so cogs go through the
# WorkloadRuntime adapters. --linux-arm64 builds it inside an arm64 Rust
# container (image: $COG_LAUNCHER_BUILDER) for a Linux ARM node or an
# aarch64 container standing in for one; output under target/linux-arm64/.
cmd_cogs_launcher() {
    header "Cog adapter launcher (examples/cog_adapter_run)"
    timer_start
    local feats=(--no-default-features --features workload-runtime)
    local rc=0
    if [ "$LAUNCHER_LINUX" = true ]; then
        local image="${COG_LAUNCHER_BUILDER:-rust:1-bookworm}"
        local out="$ROOT/target/linux-arm64"
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   docker run --platform linux/arm64 %s cargo build -p clawft-kernel %s --example cog_adapter_run\n" "$image" "${feats[*]}"
            timer_end
            return 0
        fi
        mkdir -p "$out/cargo-registry"
        docker run --rm --platform linux/arm64 \
            -v "$ROOT":/src:ro -v "$out":/target \
            -v "$out/cargo-registry":/usr/local/cargo/registry \
            -w /src -e CARGO_TARGET_DIR=/target "$image" \
            cargo build -p clawft-kernel "${feats[@]}" --example cog_adapter_run || rc=$?
        [ $rc -eq 0 ] && pass "launcher: $out/debug/examples/cog_adapter_run"
    else
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   cargo build -p clawft-kernel %s --example cog_adapter_run\n" "${feats[*]}"
            timer_end
            return 0
        fi
        cargo build -p clawft-kernel "${feats[@]}" --example cog_adapter_run || rc=$?
        [ $rc -eq 0 ] && pass "launcher: $ROOT/target/debug/examples/cog_adapter_run"
    fi
    timer_end
    return $rc
}

# ── weft-licence cross-build (ADR-106 phase 2) ────────────────────────
# Cross-builds the Seed-side `weft-licence` binary (with the https registry
# reader) for armv7-unknown-linux-gnueabihf (the Seed userland is armhf) and
# aarch64-unknown-linux-gnu inside the WeaveLogic cogs cross image
# (weavelogic-cogs-cross:1.97.1, built by cogs/scripts/cross-build.sh from
# scripts/cross/Dockerfile in the private cogs repo, so the toolchain is the
# one the cogs themselves use). Offline: crates come from the host's cargo
# cache (mounted read-only; sources are unpacked inside the container, so the
# host registry is never written), nothing is downloaded. Skips with a message when docker or the
# image is missing. Output: target/licence-cross/<triple>/release/weft-licence
# (stripped copy beside it as weft-licence.stripped) and a size report.
# Shared by licence-cross and cog-host-cross: cross-build one workspace binary for the Seed
# targets. Args: <package> <bin> <extra cargo args or ""> <output dir>.
cross_build_bin() {
    local pkg="$1" bin="$2" extra="$3" out="$4"
    header "$bin cross-build (Seed: armv7 + aarch64)"
    local image="${LICENCE_CROSS_IMAGE:-weavelogic-cogs-cross:1.97.1}"
    local targets=("${LICENCE_TARGETS[@]+"${LICENCE_TARGETS[@]}"}")
    [ ${#targets[@]} -gt 0 ] || targets=(armv7-unknown-linux-gnueabihf aarch64-unknown-linux-gnu)
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   docker run %s cargo build --locked --offline --release -p %s %s --target %s\n" "$image" "$pkg" "$extra" "${targets[*]}"
        return 0
    fi
    if ! command -v docker >/dev/null 2>&1 || ! docker image inspect "$image" >/dev/null 2>&1; then
        skip "cross toolchain not available (need docker and image $image; build it with the cogs repo scripts/cross-build.sh)"
        return 0
    fi
    [ -d "$HOME/.cargo/registry/cache" ] || { fail "no host cargo registry at ~/.cargo/registry (offline build needs it)"; return 1; }
    # Make sure every crate is in the host cache first. This is offline: it
    # fails (and says what is missing) instead of downloading.
    local ft
    for ft in "${targets[@]}"; do
        cargo fetch --locked --offline --target "$ft" >/dev/null 2>&1 \
            || { fail "host cargo cache is missing crates for $ft; run 'cargo fetch --locked --target $ft' once with network, then retry"; return 1; }
    done
    timer_start
    local rc=0 t strip_bin
    mkdir -p "$out/cargo-src"
    for t in "${targets[@]}"; do
        case "$t" in
            armv7-unknown-linux-gnueabihf) strip_bin=arm-linux-gnueabihf-strip ;;
            aarch64-unknown-linux-gnu)     strip_bin=aarch64-linux-gnu-strip ;;
            *) fail "unknown target $t"; return 1 ;;
        esac
        info "building $bin for $t"
        docker run --rm --user "$(id -u):$(id -g)" \
            -v "$ROOT":/src:ro -v "$out":/target \
            -v "$HOME/.cargo/registry/cache":/cargo-home/registry/cache:ro \
            -v "$HOME/.cargo/registry/index":/cargo-home/registry/index:ro \
            -v "$out/cargo-src":/cargo-home/registry/src \
            -w /src -e HOME=/tmp -e CARGO_HOME=/cargo-home \
            -e CARGO_TARGET_DIR=/target -e RUSTUP_TOOLCHAIN=1.97.1 -e CARGO_NET_OFFLINE=true \
            -e CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
            -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
            "$image" bash -euo pipefail -c '
                cargo build --locked --offline --release -p '"$pkg"' '"$extra"' --target '"$t"'
                cp /target/'"$t"'/release/'"$bin"' /target/'"$t"'/release/'"$bin"'.stripped
                '"$strip_bin"' /target/'"$t"'/release/'"$bin"'.stripped
            ' || { fail "cross-build failed for $t"; rc=1; continue; }
        local raw="$out/$t/release/$bin" st="$out/$t/release/$bin.stripped"
        pass "$t: $(wc -c < "$raw" | tr -d ' ') bytes, stripped $(wc -c < "$st" | tr -d ' ') bytes ($st)"
    done
    timer_end
    return $rc
}

# Phone parent-node app (docs/plans/phone-parent-node.md, P0): UniFFI Kotlin bindings from
# weftos-node-core, the Rust core for arm64-v8a via cargo-ndk, then a debug APK.
# Installs nothing: it lists exactly what is missing. Output: the APK path and size.
cmd_android() {
    header "Android phone node (weftos-node-core + apps/android-phone-node)"
    local app="$ROOT/apps/android-phone-node" tdir="${CARGO_TARGET_DIR:-$ROOT/target}"
    local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
    local ndk="${ANDROID_NDK_HOME:-}" jhome="${JAVA_HOME:-}" gradle_cmd=""
    [ -n "$ndk" ] || ndk="$(ls -d "$sdk"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
    [ -n "$jhome" ] || jhome="$(/usr/libexec/java_home -v 17 2>/dev/null || true)"
    if [ -x "$app/gradlew" ]; then gradle_cmd="$app/gradlew"; elif command -v gradle >/dev/null 2>&1; then gradle_cmd="gradle"; fi

    local missing=()
    [ -d "$sdk/platforms/android-35" ] || missing+=("Android SDK platform 35 under $sdk (Android Studio > SDK Manager > SDK Platforms)")
    [ -n "$ndk" ] && [ -d "$ndk" ] || missing+=("Android NDK (side by side) under $sdk/ndk (SDK Manager > SDK Tools > NDK (Side by side)), or set ANDROID_NDK_HOME")
    command -v cargo-ndk >/dev/null 2>&1 || missing+=("cargo-ndk: cargo install cargo-ndk --locked")
    rustup target list --installed 2>/dev/null | grep -qx aarch64-linux-android || missing+=("Rust target: rustup target add aarch64-linux-android")
    [ -n "$jhome" ] && [ -x "$jhome/bin/java" ] || missing+=("JDK 17 (e.g. brew install --cask temurin@17), or set JAVA_HOME")
    [ -n "$gradle_cmd" ] || missing+=("Gradle 8.9 (brew install gradle; the app has no wrapper jar yet), or add a gradlew to apps/android-phone-node")

    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cargo build -p weftos-node-core --features bindgen-cli; uniffi-bindgen generate --library ... --language kotlin\n"
        printf "  ${YELLOW}DRY${NC}   cargo ndk -t arm64-v8a -o apps/android-phone-node/app/src/main/jniLibs build --release -p weftos-node-core\n"
        printf "  ${YELLOW}DRY${NC}   gradle -p apps/android-phone-node :app:testDebugUnitTest :app:assembleDebug\n"
        for m in ${missing[@]+"${missing[@]}"}; do info "missing: $m"; done
        return 0
    fi

    timer_start
    # 1. Kotlin bindings from the host build (library mode; no NDK needed).
    local ext="so"; [ "$(uname -s)" = Darwin ] && ext="dylib"
    cargo build -p weftos-node-core --features bindgen-cli || { fail "host build of weftos-node-core failed"; return 1; }
    rm -rf "$app/app/src/main/generated-uniffi"
    cargo run -q -p weftos-node-core --features bindgen-cli --bin uniffi-bindgen -- generate \
        --library "$tdir/debug/libweftos_node_core.$ext" --language kotlin \
        --out-dir "$app/app/src/main/generated-uniffi" \
        || { fail "uniffi Kotlin binding generation failed"; return 1; }
    pass "Kotlin bindings: apps/android-phone-node/app/src/main/generated-uniffi"

    if [ ${#missing[@]} -gt 0 ]; then
        fail "cannot build the APK; missing (nothing was installed):"
        for m in "${missing[@]}"; do printf "         - %s\n" "$m"; done
        return 1
    fi

    # 2. Rust core for arm64-v8a.
    export ANDROID_HOME="$sdk" ANDROID_NDK_HOME="$ndk" JAVA_HOME="$jhome"
    cargo ndk -t arm64-v8a -o "$app/app/src/main/jniLibs" build --release -p weftos-node-core \
        || { fail "cargo-ndk build failed"; return 1; }
    pass "libweftos_node_core.so (arm64-v8a)"

    # 3. Debug APK.
    "$gradle_cmd" -p "$app" :app:testDebugUnitTest :app:assembleDebug \
        || { fail "gradle testDebugUnitTest/assembleDebug failed"; return 1; }
    pass "JVM unit tests"
    local apk
    apk="$(ls "$app"/app/build/outputs/apk/debug/*.apk 2>/dev/null | head -1 || true)"
    [ -n "$apk" ] || { fail "no APK produced"; return 1; }
    pass "APK: $apk ($(wc -c < "$apk" | tr -d ' ') bytes)"
    timer_end
}

# Glasses agent (docs/plans/phone-parent-node.md, P0): com.weftos.glasses for the Mentra Live
# (Android 11, arm64). Same shape as `android`: UniFFI Kotlin bindings, the Rust core via
# cargo-ndk, then Gradle. Also checks the committed cue WAVs against scripts/gen_cues.py and runs
# the JVM unit tests. Installs nothing (no adb): hand the APK to whoever owns the glasses.
cmd_glasses() {
    header "Glasses agent (weftos-node-core + apps/android-glasses-node)"
    local app="$ROOT/apps/android-glasses-node" tdir="${CARGO_TARGET_DIR:-$ROOT/target}"
    local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
    local ndk="${ANDROID_NDK_HOME:-}" jhome="${JAVA_HOME:-}" gradle_cmd=""
    [ -n "$ndk" ] || ndk="$(ls -d "$sdk"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
    [ -n "$jhome" ] || jhome="$(/usr/libexec/java_home -v 17 2>/dev/null || true)"
    [ -n "$jhome" ] && [ -x "$jhome/bin/java" ] || jhome="/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home"
    if [ -x "$app/gradlew" ]; then gradle_cmd="$app/gradlew"; elif command -v gradle >/dev/null 2>&1; then gradle_cmd="gradle"; fi

    local missing=()
    [ -d "$sdk/platforms/android-35" ] || missing+=("Android SDK platform 35 under $sdk (Android Studio > SDK Manager > SDK Platforms)")
    [ -n "$ndk" ] && [ -d "$ndk" ] || missing+=("Android NDK (side by side) under $sdk/ndk, or set ANDROID_NDK_HOME")
    command -v cargo-ndk >/dev/null 2>&1 || missing+=("cargo-ndk: cargo install cargo-ndk --locked")
    rustup target list --installed 2>/dev/null | grep -qx aarch64-linux-android || missing+=("Rust target: rustup target add aarch64-linux-android")
    [ -n "$jhome" ] && [ -x "$jhome/bin/java" ] || missing+=("JDK 17 (e.g. brew install --cask temurin@17), or set JAVA_HOME")
    [ -n "$gradle_cmd" ] || missing+=("Gradle (brew install gradle; the app has no wrapper jar), or add a gradlew to apps/android-glasses-node")
    command -v python3 >/dev/null 2>&1 || missing+=("python3 (cue asset check)")

    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   python3 apps/android-glasses-node/scripts/gen_cues.py --check\n"
        printf "  ${YELLOW}DRY${NC}   cargo build -p weftos-node-core --features bindgen-cli; uniffi-bindgen generate --library ... --language kotlin\n"
        printf "  ${YELLOW}DRY${NC}   cargo ndk -t arm64-v8a -o apps/android-glasses-node/app/src/main/jniLibs build --release -p weftos-node-core\n"
        printf "  ${YELLOW}DRY${NC}   gradle -p apps/android-glasses-node :app:testDebugUnitTest :app:assembleDebug\n"
        for m in ${missing[@]+"${missing[@]}"}; do info "missing: $m"; done
        return 0
    fi

    timer_start
    if command -v python3 >/dev/null 2>&1; then
        python3 "$app/scripts/gen_cues.py" --check || { fail "cue assets are stale; run python3 apps/android-glasses-node/scripts/gen_cues.py"; return 1; }
        pass "cue assets match the tone definitions"
    fi
    local ext="so"; [ "$(uname -s)" = Darwin ] && ext="dylib"
    cargo build -p weftos-node-core --features bindgen-cli || { fail "host build of weftos-node-core failed"; return 1; }
    rm -rf "$app/app/src/main/generated-uniffi"
    cargo run -q -p weftos-node-core --features bindgen-cli --bin uniffi-bindgen -- generate \
        --library "$tdir/debug/libweftos_node_core.$ext" --language kotlin \
        --out-dir "$app/app/src/main/generated-uniffi" \
        || { fail "uniffi Kotlin binding generation failed"; return 1; }
    pass "Kotlin bindings: apps/android-glasses-node/app/src/main/generated-uniffi"

    if [ ${#missing[@]} -gt 0 ]; then
        fail "cannot build the APK; missing (nothing was installed):"
        for m in "${missing[@]}"; do printf "         - %s\n" "$m"; done
        return 1
    fi

    export ANDROID_HOME="$sdk" ANDROID_NDK_HOME="$ndk" JAVA_HOME="$jhome"
    cargo ndk -t arm64-v8a -o "$app/app/src/main/jniLibs" build --release -p weftos-node-core \
        || { fail "cargo-ndk build failed"; return 1; }
    pass "libweftos_node_core.so (arm64-v8a)"

    "$gradle_cmd" -p "$app" :app:testDebugUnitTest :app:assembleDebug \
        || { fail "gradle testDebugUnitTest/assembleDebug failed"; return 1; }
    pass "JVM unit tests"
    local apk
    apk="$(ls "$app"/app/build/outputs/apk/debug/*.apk 2>/dev/null | head -1 || true)"
    [ -n "$apk" ] || { fail "no APK produced"; return 1; }
    pass "APK: $apk ($(wc -c < "$apk" | tr -d ' ') bytes)"
    timer_end
}

# Builds weft-cog-host natively for x86_64 Linux servers (e.g. photo-gallery) inside the
# pinned rust image under linux/amd64 emulation, offline from the host cargo cache.
# Output: target/cog-host-x86/release/weft-cog-host(.stripped).
cmd_cog_host_x86() {
    header "weft-cog-host build (x86_64-unknown-linux-gnu)"
    local image="${COG_HOST_X86_IMAGE:-rust:1.95-bookworm}" out="$ROOT/target/cog-host-x86"
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   docker run --platform linux/amd64 %s cargo build --locked --offline --release -p weftos-cog-host\n" "$image"
        return 0
    fi
    if ! command -v docker >/dev/null 2>&1 || ! docker image inspect "$image" >/dev/null 2>&1; then
        skip "need docker and the $image image (docker pull --platform linux/amd64 $image)"
        return 0
    fi
    [ -d "$HOME/.cargo/registry/cache" ] || { fail "no host cargo registry at ~/.cargo/registry (offline build needs it)"; return 1; }
    cargo fetch --locked --offline --target x86_64-unknown-linux-gnu >/dev/null 2>&1 \
        || { fail "host cargo cache is missing crates for x86_64-unknown-linux-gnu; run 'cargo fetch --locked --target x86_64-unknown-linux-gnu' once with network, then retry"; return 1; }
    timer_start
    mkdir -p "$out/cargo-src"
    docker run --rm --platform linux/amd64 --user "$(id -u):$(id -g)" \
        -v "$ROOT":/src:ro -v "$out":/target \
        -v "$HOME/.cargo/registry/cache":/cargo-home/registry/cache:ro \
        -v "$HOME/.cargo/registry/index":/cargo-home/registry/index:ro \
        -v "$out/cargo-src":/cargo-home/registry/src \
        -w /src -e HOME=/tmp -e CARGO_HOME=/cargo-home -e CARGO_TARGET_DIR=/target \
        -e CARGO_NET_OFFLINE=true -e RUSTUP_TOOLCHAIN=1.95 \
        "$image" bash -euo pipefail -c '
            export PATH=/usr/local/cargo/bin:$PATH RUSTUP_HOME=/usr/local/rustup
            cargo build --locked --offline --release -p weftos-cog-host
            cp /target/release/weft-cog-host /target/release/weft-cog-host.stripped
            strip /target/release/weft-cog-host.stripped
        ' || { fail "x86_64 build failed"; timer_end; return 1; }
    local st="$out/release/weft-cog-host.stripped"
    pass "x86_64: stripped $(wc -c < "$st" | tr -d ' ') bytes ($st)"
    timer_end
}

# Windows compile check of every cargo-dist app (weft, weaver, weftos, the egui GUI)
# for x86_64-pc-windows-gnu with mingw-w64. The release builds x86_64-pc-windows-msvc,
# which macOS cannot compile (ring needs MSVC), but cfg(unix)-only code breaks both the
# same way; this catches it before a tag. Returns 0 pass, 1 fail, 2 skipped.
cmd_windows_check_impl() {
    if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 \
        || ! rustup target list --installed 2>/dev/null | grep -qx x86_64-pc-windows-gnu; then
        return 2
    fi
    CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar \
        cargo check --release --target x86_64-pc-windows-gnu \
        -p clawft-cli -p clawft-gui-egui -p clawft-weave -p weftos --bins
}

cmd_windows_check() {
    header "Windows compile check (x86_64-pc-windows-gnu, cargo-dist apps)"
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cargo check --release --target x86_64-pc-windows-gnu -p clawft-cli -p clawft-gui-egui -p clawft-weave -p weftos --bins\n"
        return 0
    fi
    timer_start
    local rc=0
    cmd_windows_check_impl || rc=$?
    case $rc in
        0) pass "Windows compile check" ;;
        2) skip "needs mingw-w64 (brew install mingw-w64) and rustup target add x86_64-pc-windows-gnu" ;;
        *) fail "Windows compile check" ;;
    esac
    timer_end
    [ $rc -eq 1 ] && return 1 || return 0
}

cmd_licence_cross() {
    cross_build_bin weft-licence weft-licence "--features net" "$ROOT/target/licence-cross"
}

# Cross-builds weft-cog-host (the appliance supervisor) for the Seed. Output:
# target/cog-host-cross/<triple>/release/weft-cog-host(.stripped).
cmd_cog_host_cross() {
    cross_build_bin weftos-cog-host weft-cog-host "" "$ROOT/target/cog-host-cross"
}

# weft-licence uid isolation check (ADR-106 phase 2 acceptance): on Linux, a
# different user (standing in for a cog's uid) cannot read or list the grant
# key written by `weft-licence init` under the service user. Runs the aarch64
# binary from `licence-cross` in the cogs cross image, as real users.
cmd_licence_uid_check() {
    header "weft-licence key isolation between users (Linux container)"
    local image="${LICENCE_CROSS_IMAGE:-weavelogic-cogs-cross:1.97.1}"
    local bin="$ROOT/target/licence-cross/aarch64-unknown-linux-gnu/release/weft-licence"
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   docker run %s (init as weft-licence, read as another user)\n" "$image"
        return 0
    fi
    if ! command -v docker >/dev/null 2>&1 || ! docker image inspect "$image" >/dev/null 2>&1; then
        skip "docker or image $image not available"
        return 0
    fi
    [ -x "$bin" ] || { fail "no aarch64 binary; run: scripts/build.sh licence-cross aarch64"; return 1; }
    # The aarch64 binary needs an arm64 container: native on an arm64 host,
    # otherwise only with qemu/binfmt. Skip cleanly when neither is there.
    case "$(uname -m)" in
        arm64|aarch64) ;;
        *) if ! docker run --rm --platform linux/arm64 "$image" true >/dev/null 2>&1; then
               skip "this host is not arm64 and has no qemu for linux/arm64 containers"
               return 0
           fi ;;
    esac
    docker run --rm --platform linux/arm64 -v "$bin":/usr/local/bin/weft-licence:ro "$image" bash -euo pipefail -c '
        useradd --system --no-create-home weft-licence
        useradd --no-create-home cog1
        install -d -m 0700 -o weft-licence -g weft-licence /var/lib/weft-licence
        su -s /bin/bash weft-licence -c "weft-licence --state-dir /var/lib/weft-licence init" | sed "s/^/  svc: /"
        stat -c "  key mode %a owner %U" /var/lib/weft-licence/grant.key
        stat -c "  dir mode %a owner %U" /var/lib/weft-licence
        if su -s /bin/bash cog1 -c "cat /var/lib/weft-licence/grant.key" 2>/dev/null; then echo "  FAIL: another user read the key"; exit 1; fi
        if su -s /bin/bash cog1 -c "ls /var/lib/weft-licence" 2>/dev/null; then echo "  FAIL: another user listed the state dir"; exit 1; fi
        echo "  another user can neither read nor list: ok"
        if su -s /bin/bash weft-licence -c "weft-licence --state-dir /var/lib/weft-licence init" 2>&1; then echo "  FAIL: init overwrote"; exit 1; fi
        echo "  second init refused: ok"
    '
}

# ── Real Pi 5 test lane (mesh-placement-fu-pi-test-lane) ────────────
# Cross-builds aarch64 test binaries in an arm64 Debian container, runs them
# on the Pi named by WEFTOS_PI_HOST (skips when unset) with an isolated HOME
# and WEFTOS_RUNTIME_DIR, plus the native adapter live test and the cog
# conformance harness in remote mode. See docs/cogs/test-pi.md.
cmd_test_pi() {
    header "ARM tests on the real Pi 5 (scripts/pi/pi_lane.py)"
    timer_start
    local rc=0
    # ${arr[@]+…} guard: bare `test-pi` (the full lane) leaves PI_ARGS empty,
    # which macOS bash 3.2 + `set -u` rejects as unbound.
    python3 -u "$ROOT/scripts/pi/pi_lane.py" ${PI_ARGS[@]+"${PI_ARGS[@]}"} || rc=$?
    timer_end
    return $rc
}

# ── STM32N6 leaf firmware + host validator (standalone crates) ───────
cmd_n6_leaf() {
    header "STM32N6 leaf firmware + host RVF validator"
    timer_start
    local rc=0 v=""
    [ "$VERBOSE" = true ] && v="--verbose"
    rustup target list --installed | grep -qx thumbv8m.main-none-eabihf \
        || rustup target add thumbv8m.main-none-eabihf || rc=$?
    # Each crate has its own [workspace]; build inside it so the firmware's
    # .cargo/config.toml (thumbv8m target) applies only to the firmware.
    [ $rc -eq 0 ] && (cd "$ROOT/crates/weftos-n6-leaf" && cargo build --release $v) || rc=$?
    [ $rc -eq 0 ] && (cd "$ROOT/crates/weftos-n6-leaf-hostcheck" && cargo build --release $v) || rc=$?
    [ $rc -eq 0 ] && (cd "$ROOT/crates/weftos-n6-node" && cargo build --release $v) || rc=$?
    timer_end
    return $rc
}

# ADR-117 real-leaf display path, no hardware: the session protocol and its
# leaf-side state machine (weftos-leaf-scene), the display client and the
# host-side pad emulator (weftos-display-client --features wire-leaf), and the
# daemon side: the endpoint tests and the end-to-end test (real daemon dispatch,
# real certified-leaf listener and discovery, emulated pad, display client and
# console host). Clippy runs with warnings as errors on every touched crate.
cmd_leaf_display() {
    header "leaf-display: session protocol, pad emulator, daemon endpoint, e2e"
    timer_start
    local rc=0 v=""
    [ "$VERBOSE" = true ] && v="--verbose"
    (cd "$ROOT/crates/weftos-leaf-scene" \
        && cargo test $v \
        && cargo clippy --all-targets $v -- -D warnings) || rc=$?
    [ $rc -eq 0 ] && (cd "$ROOT/crates/weftos-display-client" \
        && cargo test --features wire-leaf $v \
        && cargo clippy --all-targets --features wire-leaf $v -- -D warnings) || rc=$?
    [ $rc -eq 0 ] && (cd "$ROOT/crates/weftos-cos-console" \
        && cargo build --bin cos-console-host $v \
        && cargo clippy --all-targets $v -- -D warnings) || rc=$?
    if [ $rc -eq 0 ]; then
        isolate_test_runtime
        (cd "$ROOT" \
            && cargo test -p clawft-weave --test display_endpoint --test display_e2e $v \
            && cargo clippy -p clawft-weave --all-targets $v -- -D warnings) || rc=$?
    fi
    timer_end
    return $rc
}

# The edge pad firmware (crates/clawft-edge-pad-idf), built in a throwaway
# container from the cached ESP-IDF image (no host Xtensa toolchain exists).
# The source is copied in; the image's cached IDF tree and target dir are
# kept so the build is incremental. Nothing is flashed.
#
#   EDGEPAD_IMAGE            image (default weftos-leaf-build:idf532-cache)
#   EDGEPAD_PROVISION_DIR    dir holding leaf.seed + leaf.cbor from
#                            `weaver leaf provision`; without it the image
#                            boots the display but never joins the mesh
#   EDGEPAD_PARENT_HINT      optional ip:port of the daemon's discovery socket;
#                            the pad probes it unicast before broadcasting.
#                            Else a one-line parent_hint file in the provision dir.
#   EDGEPAD_WIFI_SECRETS     path to the owner's wifi_secrets.rs (gitignored);
#                            without it the image's own copy, else the
#                            example, is used
# Output: target/edgepad-fw/{clawft-edge-pad-idf.elf,edgepad-merged.bin}
cmd_edgepad_fw() {
    header "edge pad firmware build (xtensa-esp32s3-espidf, container)"
    local image="${EDGEPAD_IMAGE:-weftos-leaf-build:idf532-cache}" out="$ROOT/target/edgepad-fw"
    local prov="${EDGEPAD_PROVISION_DIR:-}" wifi="${EDGEPAD_WIFI_SECRETS:-}"
    local mounts=() envs=()
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   docker run --rm %s: copy crates, cargo build --release, espflash save-image --merge\n" "$image"
        return 0
    fi
    if ! command -v docker >/dev/null 2>&1 || ! docker image inspect "$image" >/dev/null 2>&1; then
        skip "need docker and the $image image (see docs/guides/edgepad-console-hil.md)"
        return 0
    fi
    if [ -n "$prov" ]; then
        [ -f "$prov/leaf.seed" ] && [ -f "$prov/leaf.cbor" ] || { fail "$prov must hold leaf.seed and leaf.cbor"; return 1; }
        mounts+=(-v "$prov":/provision:ro)
        envs+=(-e WEFTOS_LEAF_SEED_FILE=/provision/leaf.seed -e WEFTOS_LEAF_CERT_FILE=/provision/leaf.cbor)
    fi
    # Optional unicast parent hint (ip:port of the daemon's discovery socket):
    # EDGEPAD_PARENT_HINT, else a one-line parent_hint file in the provision dir.
    local hint="${EDGEPAD_PARENT_HINT:-}"
    [ -z "$hint" ] && [ -n "$prov" ] && [ -f "$prov/parent_hint" ] && hint="$(tr -d '[:space:]' < "$prov/parent_hint")"
    if [ -n "$hint" ]; then
        [[ "$hint" =~ ^[0-9.]+:[0-9]+$ || "$hint" =~ ^\[[0-9a-fA-F:]+\]:[0-9]+$ ]] || { fail "EDGEPAD_PARENT_HINT must be ip:port (example 192.168.10.100:9490)"; return 1; }
        envs+=(-e WEFTOS_LEAF_PARENT_HINT="$hint")
    fi
    if [ -n "$wifi" ]; then
        [ -f "$wifi" ] || { fail "EDGEPAD_WIFI_SECRETS $wifi does not exist"; return 1; }
        mounts+=(-v "$wifi":/provision-wifi/wifi_secrets.rs:ro)
    fi
    timer_start
    mkdir -p "$out"
    docker run --rm -v "$ROOT/crates":/source:ro -v "$out":/out ${mounts[@]+"${mounts[@]}"} ${envs[@]+"${envs[@]}"} \
        --entrypoint bash "$image" -lc '
            set -euo pipefail
            source ~/export-esp.sh
            cd /home/esp/build/crates
            keep_wifi=""
            [ -f clawft-edge-pad-idf/src/wifi_secrets.rs ] && { cp clawft-edge-pad-idf/src/wifi_secrets.rs /tmp/wifi_secrets.rs; keep_wifi=1; }
            for c in clawft-edge-pad-idf weftos-leaf-types weftos-leaf-renderer weftos-leaf-scene; do
                mkdir -p "$c"; rm -rf "$c/src"
                tar -C "/source/$c" --exclude=./target --exclude=./.embuild --exclude=./build -cf - . | tar -C "$c" -xf -
            done
            cd clawft-edge-pad-idf
            if [ -f /provision-wifi/wifi_secrets.rs ]; then cp /provision-wifi/wifi_secrets.rs src/wifi_secrets.rs
            elif [ -n "$keep_wifi" ]; then cp /tmp/wifi_secrets.rs src/wifi_secrets.rs
            else cp src/wifi_secrets.rs.example src/wifi_secrets.rs; echo "WARNING: building with the example wifi_secrets.rs"; fi
            cargo build --release
            elf=target/xtensa-esp32s3-espidf/release/clawft-edge-pad-idf
            cp "$elf" /out/clawft-edge-pad-idf.elf
            boot=$(ls -d target/xtensa-esp32s3-espidf/release/build/esp-idf-sys-*/out/build/bootloader/bootloader.bin | head -1)
            espflash save-image --chip esp32s3 --flash-size 4mb --merge --bootloader "$boot" \
                --partition-table partitions.csv "$elf" /out/edgepad-merged.bin
        ' || { fail "edge pad firmware build failed"; timer_end; return 1; }
    pass "firmware: $(wc -c < "$out/edgepad-merged.bin" | tr -d ' ') byte merged image ($out/edgepad-merged.bin; not flashed)"
    timer_end
}

# Relay the hosted dashboard's phone page to the local chief of staff. Runs the
# installed featured binary (scripts/build.sh cos --features mic,onnx installs it);
# override with WEFTOS_COS_BIN. Extra arguments (--once, --dry-run, ...) pass through.
cmd_cos_bridge() {
    header "weftos-cos bridge: dashboard relay for the chief of staff"
    local bin="${WEFTOS_COS_BIN:-$HOME/.weftos/bin/weftos-cos}"
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   %s bridge\n" "$bin"
        return 0
    fi
    [ -x "$bin" ] || { printf "  ${RED}FAIL${NC}  %s not found; run scripts/build.sh cos --features mic,onnx or set WEFTOS_COS_BIN\n" "$bin"; return 1; }
    exec "$bin" bridge
}

# ── Chief-of-staff console, ADR-118 C2 (standalone crate) ─────────────
# ADR-118 C3: the owner's one command for the chief of staff. Builds the agent
# host with push-to-talk and local STT (mic,onnx), then runs it in the
# foreground: starts the local model server if it is down (and stops it again
# on exit), warms the model, speaks voice replies. The console window is the
# second command: scripts/build.sh cos-host, then cos-console-host --window.
cmd_cos_up() {
    header "weftos-cos up: build (mic,onnx) and serve with the local model"
    local saved="$FEATURES"
    FEATURES="mic,onnx"
    cmd_cos || { FEATURES="$saved"; return 1; }
    FEATURES="$saved"
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   ${WEFTOS_COS_BIN:-$HOME/.weftos/bin/weftos-cos} serve --start-model --mic\n"
        return 0
    fi
    printf "  Console window (second terminal): scripts/build.sh cos-host && crates/weftos-cos-console/target/release/cos-console-host --window\n"
    exec "${WEFTOS_COS_BIN:-$HOME/.weftos/bin/weftos-cos}" serve --start-model --mic
}

cmd_cos_console() {
    header "weftos-cos-console: build, test, clippy"
    timer_start
    local rc=0 v=""
    [ "$VERBOSE" = true ] && v="--verbose"
    # Own [workspace] like its leaf siblings; build inside the crate.
    (cd "$ROOT/crates/weftos-cos-console" \
        && cargo build --release $v \
        && cargo test $v \
        && cargo clippy --all-targets $v -- -D warnings) || rc=$?
    timer_end
    return $rc
}

# ADR-118 C3: the console host (window + agent + push-to-talk). Builds
# `cos-console-host` with the SDL2 window, runs the console crate's tests
# (host against a fake agent, headless), clippy with the window feature,
# the display-client tests (key sensors), and the agent's API tests
# (`POST /v1/cos/ptt`) in the workspace.
cmd_cos_host() {
    header "cos-console-host: build (window), test, clippy; weftos-cos API tests"
    timer_start
    local rc=0 v=""
    [ "$VERBOSE" = true ] && v="--verbose"
    (cd "$ROOT/crates/weftos-display-client" \
        && cargo test $v \
        && cargo clippy --all-targets --features window $v -- -D warnings) || rc=$?
    [ $rc -eq 0 ] && (cd "$ROOT/crates/weftos-cos-console" \
        && cargo build --release --features window --bin cos-console-host $v \
        && cargo test $v \
        && cargo clippy --all-targets --features window $v -- -D warnings) || rc=$?
    # Direct (not run_cmd): a failing test must fail the lane, and its
    # output must be visible.
    [ $rc -eq 0 ] && (cd "$ROOT" \
        && cargo test -p weftos-cos --test ptt_api --test api_contract $v \
        && cargo clippy -p weftos-cos --all-targets $v -- -D warnings) || rc=$?
    timer_end
    [ $rc -eq 0 ] && report_binary_size "crates/weftos-cos-console/target/release/cos-console-host" "Console host (cos-console-host, release)"
    return $rc
}

cmd_cos_console_demo() {
    header "weftos-cos-console: replay the scripted conversation to PNG key frames"
    timer_start
    local rc=0
    (cd "$ROOT/crates/weftos-cos-console" \
        && cargo run --release --bin cos-console-demo -- \
            --out "$ROOT/docs/design/cos-console") || rc=$?
    timer_end
    return $rc
}

# ── Gate check 13 helper: clawft-kernel diskann + bench feature matrix ──
check_kernel_diskann_and_bench_matrix() {
    # --tests included deliberately: cfg-gated test modules rot separately
    # from the lib (found live: stub-only cosine_distance tests failed to
    # compile under --features diskann while the lib checked clean).
    cargo check -p clawft-kernel --features diskann --tests --benches \
        && cargo check -p clawft-kernel --tests --benches
}

# ── Gate: full phase-gate checks ────────────────────────────────────
# WEFT-56 — fast pipeline regression pass (clawft-core pipeline::* +
# related integration tests). Faster than full workspace test; used as
# gate step 15 and as a standalone subcommand for local iteration.
#
# Filter: nextest expression `test(pipeline)` matches ~350 unit tests under
# pipeline:: plus related integration names (e.g. compress_pipeline).
# Typical runtime: <5s after compile; AC target <60s.
cmd_pipeline_pass_impl() {
    isolate_test_runtime
    if cargo nextest --version >/dev/null 2>&1; then
        env -u "$(cargo_runner_var)" cargo nextest run ${NEXTEST_CONFIG[@]+"${NEXTEST_CONFIG[@]}"} -p clawft-core -E 'test(pipeline)'
    else
        # Fallback when nextest is missing: cargo test path filter (slower,
        # less precise than nextest, but still package-scoped).
        cargo test -p clawft-core --lib pipeline
    fi
}

cmd_pipeline_pass() {
    header "Pipeline pass (clawft-core)"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   cargo nextest run -p clawft-core -E 'test(pipeline)'\n"
        timer_end
        return 0
    fi
    if cmd_pipeline_pass_impl; then
        pass "pipeline pass"
        timer_end
        return 0
    else
        fail "pipeline pass"
        timer_end
        return 1
    fi
}

# ── WEFT-460: cargo-dist host-triple release rehearsal ───────────────
# Reproduces the local leg of release.yml for the host triple so
# contributors catch packaging breakage before tag push. Not a substitute
# for the full multi-target CI matrix — only the host triple is built.
#
# Expected local archives (from [package.metadata.dist] dist=true crates):
#   clawft-cli-{triple}.tar.gz   (weft)
#   clawft-weave-{triple}.tar.gz (weaver)
#   weftos-{triple}.tar.gz       (weftos)
#   clawft-gui-egui-{triple}.tar.gz (weft-gui-egui)
# Artifacts land in target/distrib/. Requires cargo-dist (the `dist` CLI),
# pinned at [workspace.metadata.dist] cargo-dist-version.
#
# Usage:
#   scripts/build.sh release-dry-run
#   scripts/build.sh gate --with-release-dry-run   # optional gate step 20
#   GATE_RELEASE_DRY_RUN=1 scripts/build.sh gate

# Resolve the cargo-dist CLI binary (`dist` preferred; `cargo dist` fallback).
dist_cli() {
    if command -v dist >/dev/null 2>&1; then
        echo "dist"
        return 0
    fi
    if cargo dist --version >/dev/null 2>&1; then
        echo "cargo dist"
        return 0
    fi
    return 1
}

host_triple() {
    rustc -vV 2>/dev/null | sed -n 's/^host: //p'
}

# Core of release-dry-run. Returns 0 on success. Does not print the
# phase-gate style summary — callers own that.
cmd_release_dry_run_impl() {
    local host dist_bin dist_ver pinned_ver
    local manifest_tmp
    local expected_apps=(clawft-cli clawft-weave weftos clawft-gui-egui)
    local expected_bins_clawft_cli=weft
    local expected_bins_clawft_weave=weaver
    local expected_bins_weftos=weftos
    local expected_bins_clawft_gui_egui=weft-gui-egui
    # Real release archives are multi-MB stripped LTO binaries. Lies-mode
    # stubs (~11 KB) and empty files must not pass verification.
    local min_archive_bytes=102400
    local failures=0
    local app archive checksum path bytes bin_name listing

    host="$(host_triple)"
    if [ -z "$host" ]; then
        fail "could not determine host triple (rustc -vV)"
        return 1
    fi
    info "Host triple: $host"

    if ! dist_bin="$(dist_cli)"; then
        fail "cargo-dist not installed — run: cargo install --locked cargo-dist --version 0.31.0"
        fail "(or: curl --proto '=https' --tlsv1.2 -LsSf https://github.com/axodotdev/cargo-dist/releases/download/v0.31.0/cargo-dist-installer.sh | sh)"
        return 1
    fi
    dist_ver="$($dist_bin --version 2>/dev/null | head -1 || true)"
    info "Using: $dist_bin ($dist_ver)"

    # Soft pin check against workspace.metadata.dist cargo-dist-version.
    pinned_ver="$(
        sed -n 's/^cargo-dist-version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
            "$ROOT/Cargo.toml" 2>/dev/null | head -1
    )"
    if [ -n "$pinned_ver" ] && ! printf '%s\n' "$dist_ver" | grep -q "$pinned_ver"; then
        printf "  ${YELLOW}WARN${NC}  cargo-dist version mismatch: local reports '%s', Cargo.toml pins %s\n" \
            "$dist_ver" "$pinned_ver"
        info "Install the pin with: cargo install --locked cargo-dist --version $pinned_ver"
    fi

    # Host must be in the release matrix (else CI wouldn't build it either).
    if ! grep -E "\"${host}\"" "$ROOT/Cargo.toml" >/dev/null 2>&1; then
        fail "host triple $host is not listed in [workspace.metadata.dist] targets"
        fail "cargo-dist will not produce a release archive for this machine"
        return 1
    fi
    pass "host triple is in cargo-dist targets"

    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   %s build --artifacts=local --target %s --output-format=json\n" \
            "$dist_bin" "$host"
        printf "  ${YELLOW}DRY${NC}   verify target/distrib/{%s}-%s.tar.gz names + sizes\n" \
            "$(IFS=,; echo "${expected_apps[*]}")" "$host"
        return 0
    fi

    header "cargo-dist local build for $host"
    info "This is a full profile=dist (inherits release/LTO) build — expect several minutes"
    timer_start
    # BSD mktemp (macOS) requires the Xs at the end of the template.
    manifest_tmp="$(mktemp "${TMPDIR:-/tmp}/weft-release-dry-run.XXXXXX")" || {
        fail "mktemp failed"
        return 1
    }
    # stdout is the dist-manifest JSON; warnings/progress go to stderr.
    # shellcheck disable=SC2086
    if ! $dist_bin build --artifacts=local --target "$host" --output-format=json \
            >"$manifest_tmp"; then
        timer_end
        fail "dist build failed for target $host"
        rm -f "$manifest_tmp"
        return 1
    fi
    timer_end
    pass "dist build --artifacts=local --target $host"

    header "Verifying asset names + sizes (target/distrib/)"
    for app in "${expected_apps[@]}"; do
        archive="${app}-${host}.tar.gz"
        checksum="${archive}.sha256"
        path="$ROOT/target/distrib/$archive"

        if [ ! -f "$path" ]; then
            fail "missing archive: $archive"
            failures=$((failures + 1))
            continue
        fi

        bytes=$(wc -c < "$path" | tr -d ' ')
        if [ "$bytes" -lt "$min_archive_bytes" ]; then
            fail "$archive is only ${bytes} bytes (min ${min_archive_bytes}) — build may have produced a stub"
            failures=$((failures + 1))
            continue
        fi
        report_binary_size "$path" "$archive"
        pass "archive present: $archive"

        if [ ! -f "$ROOT/target/distrib/$checksum" ]; then
            fail "missing checksum: $checksum"
            failures=$((failures + 1))
        else
            pass "checksum present: $checksum"
        fi

        # Confirm the expected binary is inside the archive.
        case "$app" in
            clawft-cli)      bin_name="$expected_bins_clawft_cli" ;;
            clawft-weave)    bin_name="$expected_bins_clawft_weave" ;;
            weftos)          bin_name="$expected_bins_weftos" ;;
            clawft-gui-egui) bin_name="$expected_bins_clawft_gui_egui" ;;
            *)               bin_name="" ;;
        esac
        if [ -n "$bin_name" ]; then
            listing="$(tar -tzf "$path" 2>/dev/null || true)"
            if printf '%s\n' "$listing" | grep -E "(^|/)${bin_name}(\.exe)?$" >/dev/null 2>&1; then
                pass "archive contains binary: $bin_name"
            else
                fail "archive $archive does not contain expected binary '$bin_name'"
                failures=$((failures + 1))
            fi
            # LICENSE + README are include = [...] in workspace.metadata.dist
            if printf '%s\n' "$listing" | grep -E '(^|/)LICENSE$' >/dev/null 2>&1 \
                && printf '%s\n' "$listing" | grep -E '(^|/)README\.md$' >/dev/null 2>&1; then
                pass "archive contains LICENSE + README.md"
            else
                fail "archive $archive missing LICENSE and/or README.md"
                failures=$((failures + 1))
            fi
        fi
    done

    # Surface any extra host archives dist produced (informational).
    if [ -d "$ROOT/target/distrib" ]; then
        info "All host archives in target/distrib/:"
        # shellcheck disable=SC2012
        ls -1 "$ROOT/target/distrib"/*"${host}"*.tar.gz 2>/dev/null | while read -r f; do
            report_binary_size "$f" "$(basename "$f")"
        done || true
    fi

    rm -f "$manifest_tmp"

    if [ "$failures" -gt 0 ]; then
        fail "release-dry-run: $failures verification failure(s)"
        return 1
    fi
    pass "release-dry-run: all host assets verified"
    return 0
}

cmd_release_dry_run() {
    header "Release dry-run (cargo-dist host triple) — WEFT-460"
    timer_start
    if cmd_release_dry_run_impl; then
        timer_end
        return 0
    else
        timer_end
        return 1
    fi
}

cmd_gate() {
    # Honor GATE_RELEASE_DRY_RUN=1 even without the CLI flag.
    if [ "${GATE_RELEASE_DRY_RUN:-}" = "1" ] || [ "${GATE_RELEASE_DRY_RUN:-}" = "true" ]; then
        WITH_RELEASE_DRY_RUN=true
    fi
    local total=23
    if [ "$WITH_RELEASE_DRY_RUN" = true ]; then
        total=24
    fi
    header "Phase Gate — ${total} checks"
    local passed=0 failed=0 skipped=0

    run_gate_check() {
        local num="$1" label="$2"
        shift 2
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" "$num" "$total" "$label"
        timer_start
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   %s\n" "$*"
            passed=$((passed + 1))
        else
            local gate_log_dir="${GATE_LOG_DIR:-$ROOT/target/gate-logs}"
            mkdir -p "$gate_log_dir"
            local gate_log="$gate_log_dir/check-$(printf '%02d' "$num").log"
            if "$@" >"$gate_log" 2>&1; then
                pass "$label"
                passed=$((passed + 1))
            else
                fail "$label"
                failed=$((failed + 1))
                printf "  ${YELLOW}LOG${NC}   %s (last 40 lines below)\n" "$gate_log"
                tail -n 40 "$gate_log" | sed 's/^/        /'
            fi
        fi
        timer_end
    }

    run_gate_check_soft() {
        local num="$1" label="$2"
        shift 2
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" "$num" "$total" "$label"
        timer_start
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   %s\n" "$*"
            passed=$((passed + 1))
        elif "$@" >/dev/null 2>&1; then
            pass "$label"
            passed=$((passed + 1))
        else
            skip "$label (not yet available)"
            skipped=$((skipped + 1))
        fi
        timer_end
    }

    # 1. Workspace tests — nextest (per-test process isolation, kills the
    #    parallel-isolation flake class) + doctests when available, else cargo test
    run_gate_check 1 "workspace tests (nextest + doctests)" \
        workspace_test

    # 2. Release binaries (weft + weave)
    run_gate_check 2 "cargo build --release --bin weft --bin weaver" \
        cargo build --release --bin weft --bin weaver

    # 3. WASI WASM
    if check_target_installed wasm32-wasip2; then
        run_gate_check 3 "WASI WASM (wasm32-wasip2)" \
            cargo build --target wasm32-wasip2 --profile release-wasm -p clawft-wasm
    else
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 3 "$total" "WASI WASM (wasm32-wasip2)"
        skip "wasm32-wasip2 target not installed"
        skipped=$((skipped + 1))
    fi

    # 4–9. Browser WASM checks per crate
    local browser_crates=(clawft-types clawft-platform clawft-core clawft-llm clawft-tools clawft-wasm)
    local gate_num=4
    if check_target_installed wasm32-unknown-unknown; then
        for crate in "${browser_crates[@]}"; do
            # Hard check: all six compile for the browser now, so a break must fail the gate
            # rather than show as "not yet available".
            run_gate_check "$gate_num" "Browser WASM: $crate" \
                cargo check --target wasm32-unknown-unknown -p "$crate" --no-default-features --features browser
            gate_num=$((gate_num + 1))
        done
    else
        for crate in "${browser_crates[@]}"; do
            printf "\n${BOLD}[%2d/%d]${NC} %s\n" "$gate_num" "$total" "Browser WASM: $crate"
            skip "wasm32-unknown-unknown target not installed"
            skipped=$((skipped + 1))
            gate_num=$((gate_num + 1))
        done
    fi

    # 10. UI build
    if [ -d "$ROOT/clawft-ui" ] && [ -f "$ROOT/clawft-ui/package.json" ]; then
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 10 "$total" "UI build (tsc + vite)"
        timer_start
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   cd clawft-ui && npm run build\n"
            passed=$((passed + 1))
        elif (cd "$ROOT/clawft-ui" && npm run build) >/dev/null 2>&1; then
            pass "UI build"
            passed=$((passed + 1))
        else
            fail "UI build"
            failed=$((failed + 1))
        fi
        timer_end
    else
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 10 "$total" "UI build"
        skip "clawft-ui/ directory not found"
        skipped=$((skipped + 1))
    fi

    # 11. Voice feature
    run_gate_check_soft 11 "Voice feature (clawft-plugin)" \
        cargo check --features voice -p clawft-plugin

    # 12. cargo audit (deny warnings, with 0.7.0 ignore-list).
    # See CARGO_AUDIT_IGNORES + cmd_audit above. Soft check: if
    # cargo-audit isn't installed locally, skip rather than fail; CI
    # always installs it (see .github/workflows/pr-gates.yml). When
    # WEFT-551/552/553 land, drop the matching IDs from
    # CARGO_AUDIT_IGNORES so this check tightens.
    if command -v cargo-audit >/dev/null 2>&1; then
        run_gate_check 12 "cargo audit (deny warnings, 0.7.0 ignores)" \
            cargo audit --deny warnings "${CARGO_AUDIT_IGNORES[@]}"
    else
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 12 "$total" "cargo audit (deny warnings)"
        skip "cargo-audit not installed — run: cargo install --locked cargo-audit"
        skipped=$((skipped + 1))
    fi

    # 13. npm audit critical/high (WEFT-598). Hard fail when npm+node are
    # present and any audited lockfile has ≥high. Moderates under the
    # ruflo pin are residual — see docs/security/npm-audit-residual.md.
    # Soft locally when npm is missing; CI always has node.
    if command -v npm >/dev/null 2>&1 && command -v node >/dev/null 2>&1; then
        run_gate_check 13 "npm audit (critical/high — WEFT-598)" \
            cmd_npm_audit
    else
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 13 "$total" "npm audit (critical/high — WEFT-598)"
        skip "npm/node not installed"
        skipped=$((skipped + 1))
    fi

    # 14. diskann feature matrix (WEFT-656): the `diskann` cargo feature is
    # NOT in kernel defaults; without this compile check the real backend's
    # cfg-gated code can rot while every default build silently uses the
    # brute-force stub. (The default-features side is covered by check 1.)
    # Also compile-checks vector_backend_bench (WEFT-366) under both
    # feature states — `cargo check --workspace` (check 1 / cmd_check)
    # does not build [[bench]] targets by default, so without `--benches`
    # here the bench's diskann-gated arm could silently bit-rot.
    run_gate_check 14 "diskann + bench feature compile (clawft-kernel)" \
        check_kernel_diskann_and_bench_matrix

    # 15. WEFT-114: clawft-kernel wasm32-unknown-unknown with mesh OFF
    # (--no-default-features). Hard fail when the target is installed; skip
    # locally only if rustup target missing. CI always installs the target.
    if check_target_installed wasm32-unknown-unknown; then
        run_gate_check 15 "kernel WASM no-mesh (wasm32-unknown-unknown)" \
            check_kernel_wasm_no_mesh
    else
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 15 "$total" "kernel WASM no-mesh (wasm32-unknown-unknown)"
        skip "wasm32-unknown-unknown target not installed"
        skipped=$((skipped + 1))
    fi

    # 16. WEFT-56 — explicit pipeline-pass: focused clawft-core pipeline
    # regression (router/rate_limiter/transport/… unit + related). Runs in
    # a few seconds vs full workspace; does not replace check 1.
    run_gate_check 16 "pipeline pass (clawft-core test(pipeline))" \
        cmd_pipeline_pass_impl

    # 17-19. Agent Directory (AD-1/AD-2, docs/research/agent-directory/design.md):
    # every agents/<pkg>/weftos-package.yaml validates, agents/catalog.json is
    # up to date (checked, never written by the gate), and agents/ carries no
    # client-identifying content (the repo is public). Soft when node/npm is
    # missing locally; CI always has both.
    if command -v node >/dev/null 2>&1; then
        run_gate_check 17 "agents/ package + team spec validation (AD-1, ADR-112)" \
            agents_validate_gate_impl
        run_gate_check 18 "agents/catalog.json up to date (AD-1)" \
            node "$ROOT/scripts/agents-catalog.mjs" --check
    else
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 17 "$total" "agents/ package validation (AD-1)"
        skip "node not installed"
        skipped=$((skipped + 1))
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 18 "$total" "agents/catalog.json up to date (AD-1)"
        skip "node not installed"
        skipped=$((skipped + 1))
    fi
    run_gate_check 19 "agents/ confidentiality leak check (AD-2)" \
        bash "$ROOT/scripts/agents-leak-check.sh"

    # 20. P3-S "must not own": the machine mesh service owns no chain,
    # governance or operator-CLI state (dependency tree + direct-dep allowlist).
    run_gate_check 20 "mesh service owns no state (check-mesh-no-owned-state)" \
        cmd_check_mesh_no_owned_state

    # 21. ADR-106 1c: the daemon is never built with the mesh-local test seam.
    run_gate_check 21 "daemon does not enable clawft-mesh-local testing (check-daemon-no-mesh-testing)" \
        cmd_check_daemon_no_mesh_testing

    # 22. Workspace clippy, warnings as errors (same invocation as `clippy`).
    #     --all-targets lints tests, examples and benches too, so test code
    #     stays clean. Debt cleared in cards 8dfd7ed5 / 0f92a3ca and the
    #     all-targets sweep; this keeps it from returning.
    #     Feature-gated lanes (clawft-voice-onnx --features onnx) are not
    #     reached by a bare --workspace run, so they are linted here too.
    run_gate_check 22 "workspace clippy, all targets (warnings as errors)" \
        cmd_clippy_gate

    # 23. Windows compile check of the cargo-dist apps (mingw cross-check). Two release
    #     candidates failed on Windows after a green gate; this closes that gap.
    printf "\n${BOLD}[%2d/%d]${NC} %s\n" 23 "$total" "Windows compile check (cargo-dist apps)"
    timer_start
    if [ "$DRY_RUN" = true ]; then
        printf "  ${YELLOW}DRY${NC}   scripts/build.sh windows-check\n"
        passed=$((passed + 1))
    else
        local wrc=0
        cmd_windows_check_impl >"$ROOT/target/gate-logs/check-23.log" 2>&1 || wrc=$?
        case $wrc in
            0) pass "Windows compile check (cargo-dist apps)"; passed=$((passed + 1)) ;;
            2) skip "Windows compile check: needs mingw-w64 + rustup target x86_64-pc-windows-gnu"; skipped=$((skipped + 1)) ;;
            *) fail "Windows compile check (cargo-dist apps)"; printf "  LOG   %s\n" "$ROOT/target/gate-logs/check-23.log"; failed=$((failed + 1)) ;;
        esac
    fi
    timer_end

    # 24. WEFT-460 — optional cargo-dist host-triple release rehearsal.
    # Off by default (multi-minute LTO build). Enable with:
    #   scripts/build.sh gate --with-release-dry-run
    #   GATE_RELEASE_DRY_RUN=1 scripts/build.sh gate
    if [ "$WITH_RELEASE_DRY_RUN" = true ]; then
        printf "\n${BOLD}[%2d/%d]${NC} %s\n" 24 "$total" "release-dry-run (cargo-dist host triple)"
        timer_start
        if [ "$DRY_RUN" = true ]; then
            printf "  ${YELLOW}DRY${NC}   scripts/build.sh release-dry-run\n"
            passed=$((passed + 1))
        elif cmd_release_dry_run_impl; then
            pass "release-dry-run (cargo-dist host triple)"
            passed=$((passed + 1))
        else
            fail "release-dry-run (cargo-dist host triple)"
            failed=$((failed + 1))
        fi
        timer_end
    fi

    # Summary
    echo ""
    printf "${BOLD}═══════════════════════════════════════${NC}\n"
    printf "  ${GREEN}PASSED${NC}: %d  " "$passed"
    if [ "$failed" -gt 0 ]; then
        printf "${RED}FAILED${NC}: %d  " "$failed"
    else
        printf "FAILED: %d  " "$failed"
    fi
    if [ "$skipped" -gt 0 ]; then
        printf "${YELLOW}SKIPPED${NC}: %d" "$skipped"
    else
        printf "SKIPPED: %d" "$skipped"
    fi
    printf "  (total: %d)\n" "$total"
    printf "${BOLD}═══════════════════════════════════════${NC}\n"

    if [ "$failed" -gt 0 ]; then
        return 1
    fi
}

# ── Usage ────────────────────────────────────────────────────────────
usage() {
    cat <<EOF
${BOLD}Usage:${NC} scripts/build.sh <command> [options]

${BOLD}Commands:${NC}
  native          Build native CLI binary (release)
  native-debug    Build native CLI binary (debug, fast)
  cos             Build the chief-of-staff agent host (weftos-cos, debug; ADR-118 C3).
                  --features mic adds push-to-talk capture, --features mic,onnx real STT.
                  Run: target/debug/weftos-cos serve  (also: weft cos serve)
  cos-up          One command for the chief of staff: build weftos-cos (mic,onnx),
                  start the local model if it is down, serve with push-to-talk and
                  spoken replies (stops the model it started on exit).
  cos-bridge      Relay the dashboard phone page to the local chief of staff: runs
                  \${WEFTOS_COS_BIN:-~/.weftos/bin/weftos-cos} bridge (needs [dashboard]
                  in ~/.weftos/weave.toml; see docs/plans/cos-dashboard-bridge.md).
  install         Build weft + weaver and install both to ~/.cargo/bin
                  (atomic replace, ad-hoc re-sign on macOS, fresh git build
                  stamp, prints versions). Release by default; --debug for
                  the fast-iteration profile. Honors --features — this
                  machine's working config is --features voice-onnx.
                  Refuses to drop subcommands the installed binary has
                  (--force overrides). --prefix DIR installs elsewhere (for
                  verification). Restart the daemon afterward if running.
  gui-egui        Build native egui GUI binary (weft-gui-egui, requires --features native)
  wasi            Build WASM for WASI (wasm32-wasip2)
  browser         Build WASM for browser (wasm32-unknown-unknown)
  scope <name>    Build a sensor-cog companion app crates/weftos-<name>-scope (ecg, tof); ADR-104
  scope-web <name> Same for the browser (wasm + wasm-bindgen into crates/weftos-<name>-scope/www/pkg)
  guide-web       Browser popup for one sensor guide (wasm into crates/weftos-guide-view/www/pkg)
  ecg-scope[-web] Aliases for scope ecg / scope-web ecg
  ui              Build React frontend (tsc + vite)
  docker-smoke    Build the root Dockerfile image and run the release health smoke.
  ui-docker       Build the clawft-ui multi-stage Docker image (WEFT-317).
                  Override tag with CLAWFT_UI_DOCKER_TAG=...
  ui-e2e          Run the clawft-ui Playwright E2E suite (WEFT-314).
                  Installs npm deps + chromium on first run.
  releases-mdx    Regenerate docs/src/content/docs/weftos/vision/releases.mdx
                  from CHANGELOG.md (also runs as --check before commits)
  wasm-project [guest|runner|all]
                  ADR-103 D9 Wasmtime project kernel: the wasm32-wasip1 guest and the
                  native runner (clawft-wasm / clawft-wasm-host --features project-kernel).
                  --profile release for release; never installs targets.
  test-wasm-project
                  Build both halves, then run scripts/test-wasm-project.py (signing
                  parent protocol fixture; needs python3 cryptography) in a fresh dir.
  all             Build everything (native + wasi + browser + ui)
  test [pkg…] [--filter <substr>]
                  Run cargo test --workspace (or scoped: test clawft-channels …);
                  --filter (alias --test-filter) keeps only tests whose
                  name contains <substr>
  test-pi [crate…] [--filter <test>] [--live-native] [--cogs] [--full]
                  Run ARM tests on the real Raspberry Pi 5: cross-build aarch64
                  test binaries in an arm64 Debian container (image
                  rust:<toolchain>-bookworm, glibc no newer than the Pi's),
                  rsync them to ~/weftos-test-pi on the Pi, run them there
                  under env -i with an isolated HOME and WEFTOS_RUNTIME_DIR,
                  stream results, clean up. No args = full lane: clawft-kernel
                  + native adapter live test (anomaly-detect) + cog conformance
                  in ssh mode. Pi from WEFTOS_PI_HOST (skips when unset); never
                  touches ~/.clawft or weaver.service. See docs/cogs/test-pi.md
  n6-leaf         Build the STM32N6 firmware: the leaf smoke
                  (crates/weftos-n6-leaf) with its host RVF validator
                  (crates/weftos-n6-leaf-hostcheck), and the embassy node
                  (crates/weftos-n6-node). Run with scripts/n6.sh leaf / node.
  cos-console     Build, test and clippy the chief-of-staff console crate
                  (crates/weftos-cos-console, ADR-118 C2; own workspace).
  cos-console-demo
                  Replay the scripted console conversation in the leaf
                  simulator and write key frames to docs/design/cos-console/.
  leaf-display    ADR-117 real-leaf display path without hardware: session
                  protocol + pad emulator tests, the daemon endpoint tests and
                  the daemon + console host + emulated pad end-to-end test,
                  with clippy -D warnings on every touched crate.
  edgepad-fw      Build the edge pad firmware (crates/clawft-edge-pad-idf) in
                  a container from the cached ESP-IDF image; does not flash.
                  EDGEPAD_PROVISION_DIR / EDGEPAD_WIFI_SECRETS: see the lane.
  cos-host        Build cos-console-host (SDL2 window; ADR-118 C3), run the
                  console host tests against a fake agent, clippy, and the
                  agent's push-to-talk API tests. Run: see
                  docs/guides/chief-of-staff-console.md
  test-browser    Run browser WASM regression suite under headless Chrome
                  (WEFT-388 / M5-A). Requires wasm-pack + chromedriver.
  bundle-size     Gate browser WASM bundle (raw + gzip) against the
                  documented budget (WEFT-389 / M5-A).
                  See docs/architecture/wasm-bundle-size.md
  atlas-bench     egui render stress test for the Trajectory Atlas (writes target/atlas-bench.md)
  wasm-panel      Build the VSCode dev-panel wasm bundle (clawft-gui-egui)
                  via wasm-pack / cargo + wasm-bindgen + wasm-opt -Oz, then
                  gate against the panel size budget. (WEFT-484 / M6-B)
                  Override budget: scripts/build.sh wasm-panel <max-raw-kb> <max-gz-kb>
  check-mesh-only Run cargo check -p clawft-kernel --no-default-features --features native,mesh
  kb              Build tools/build-kb (docs knowledge-base generator, --locked)
  fuzz-check      cargo check --locked for crates/clawft-core/fuzz (own lockfile)
  check-launcher  Check clawft-kernel under the cog_adapter_run launcher's features
                  (workload-runtime only; also part of whole-workspace check)
  check-doc-commands
                  Doc drift check: weaver/weft commands and [kernel.mesh] keys in
                  docs/guides/agents.md and the deployment SOPs (SOP 1, SOP 3)
                  must exist in target/debug/{weaver,weft} (run native-debug first)
  test-mesh-service
                  Machine mesh service lane (P3): cargo test -p clawft-mesh-local
                  -p clawft-mesh-service, scripts/dev/mesh-p3-e2e.sh, the
                  no-owned-state gate and check-mesh-only. Current user, tempdirs only
  check-tests     Compile (not run) the tests of the named packages: check-tests <pkg>…
  check-daemon-no-mesh-testing  Fail if the daemon's dependency graph enables clawft-mesh-local's testing feature
  check-mesh-no-owned-state
                  Fail if \`cargo tree -p clawft-mesh-service -e normal,features\` contains
                  exo-resource-tree, rvf-runtime, cognitum-gate-tilezero, clawft-weave or
                  the kernel exochain/cluster/tilezero/ecc features (P3-S "must not own")
  check           Run cargo check --workspace (fast compile check), then
                  cargo check -p clawft-kernel --target wasm32-unknown-unknown
                  --no-default-features when the target is installed (WEFT-114:
                  mesh/default features off — blocks non-browser code creeping
                  into the kernel default feature set). Also asserts that
                  --features ecc fails on that target (WEFT-504). CI hard gate
                  twin: pr-gates.yml job wasm-kernel-no-mesh.
  clippy-onnx     Run clippy (warnings as errors) on clawft-voice-onnx with --features onnx
  clippy          Run clippy with warnings-as-errors
  audit           Run cargo audit with residual ignore-list (deny warnings).
                  Requires: cargo install --locked cargo-audit
                  Followups: WEFT-551/552 DONE; WEFT-553 residual paste+bincode
                  (see docs/plans/wave-0i-WEFT-553-result.md).
  npm-audit       Run npm audit on clawft-ui, root, docs/src lockfiles
                  (WEFT-598). Fails on critical/high by default
                  (NPM_AUDIT_LEVEL=high). Set NPM_AUDIT_SOFT=1 to report only.
                  Residual moderates under ruflo pin: docs/security/npm-audit-residual.md
  agents-validate Validate every agents/<pkg>/weftos-package.yaml (AD-1: required
                  files, AGENT.md/SKILL.md frontmatter, no per-package version
                  field, requires/dependencies resolve, references/ links
                  resolve, 300-line SKILL cap, skill-smell lint) and every
                  agents/teams/<team>/team.yaml. Legacy agents/ content with no
                  weftos-package.yaml is ignored by construction.
                  See docs/research/agent-directory/design.md
  agents-catalog  Regenerate agents/catalog.json from every validated package
                  (deterministic, sorted; version = workspace Cargo.toml).
                  Pass --check to verify it's up to date instead of writing it.
  agents-leak-check
                  Scan agents/ for client-identifying content (AD-2): client/org
                  names, client paths, roster entries, credential shapes. The
                  repo is public — see scripts/agents-leak-check.sh.
  cogs-conformance <sweep|probe|summarize|selftest> [opts]
                  Cog conformance harness (ADR-100, ADR-099): run cogs against
                  a fake ESP32 UDP feed + stub ingest on docker, apple-container,
                  native or a remote node over ssh; JSON results, baseline check,
                  perf.cog.cycle_ms. See docs/cogs/conformance-harness.md
  cogs-launcher [--linux-arm64]
                  Build the cog adapter launcher (examples/cog_adapter_run)
                  that cogs-conformance --launcher uses to run cogs through
                  the WorkloadRuntime adapters; --linux-arm64 builds it in an
                  arm64 Rust container (COG_LAUNCHER_BUILDER, default rust:1-bookworm).
  windows-check   Compile the cargo-dist apps for x86_64-pc-windows-gnu (mingw-w64) to catch
                  unix-only code before a release tag. Also gate check 23.
  android         Phone parent node: UniFFI Kotlin bindings + weftos-node-core for arm64-v8a (cargo-ndk)
                  + debug APK of apps/android-phone-node. Installs nothing; lists missing NDK/JDK/etc.
  glasses         Glasses agent (com.weftos.glasses, Mentra Live / Android 11): cue-asset check, UniFFI
                  Kotlin bindings, weftos-node-core for arm64-v8a (cargo-ndk), JVM unit tests, debug APK
                  of apps/android-glasses-node. Installs nothing; lists missing NDK/JDK/etc.
  cog-host-x86    Build weft-cog-host for x86_64 Linux servers (photo-gallery) in the pinned
                  rust image under linux/amd64, offline. Output under target/cog-host-x86/.
  cog-host-cross [armv7-unknown-linux-gnueabihf|aarch64-unknown-linux-gnu]...
                  Cross-build weft-cog-host for the Seed in the cogs cross image (same
                  offline mechanism as licence-cross). Output under target/cog-host-cross/.
  licence-cross [armv7-unknown-linux-gnueabihf|aarch64-unknown-linux-gnu]...
                  Cross-build weft-licence (ADR-106 phase 2, the Seed licence
                  proxy) with the https registry reader in the cogs cross image
                  (LICENCE_CROSS_IMAGE, default weavelogic-cogs-cross:1.97.1),
                  offline from the host cargo registry. Skips when docker or the
                  image is missing. Output under target/licence-cross/.
  licence-uid-check
                  Linux check that another user cannot read or list the
                  weft-licence grant key (runs the licence-cross aarch64
                  binary as real users in the cogs cross image).
  gate            Run full phase gate (22 checks, includes cargo audit +
                  npm audit critical/high / WEFT-598 +
                  kernel WASM no-mesh / WEFT-114 + pipeline pass / WEFT-56 +
                  agents/ validate + catalog + leak-check / AD-1 + AD-2).
                  Pass --with-release-dry-run (or GATE_RELEASE_DRY_RUN=1)
                  to add optional check 20: cargo-dist host-triple rehearsal.
  pipeline-pass   Fast clawft-core pipeline regression
                  (nextest -E 'test(pipeline)'; typically <5s). Also gate #15.
  release-dry-run Rehearse the cargo-dist release for the host triple only
                  (WEFT-460). Runs: dist build --artifacts=local --target <host>
                  then verifies asset names, sizes, and archive contents under
                  target/distrib/. Requires cargo-dist (pin: Cargo.toml
                  [workspace.metadata.dist] cargo-dist-version). Multi-minute
                  LTO build — not part of the default gate.
  bench <crate> <name>
                  Run a \`[[bench]] harness = false\` target (e.g.
                  scripts/build.sh bench clawft-kernel vector_backend_bench
                  --features diskann). Prints the bench's own report
                  in full (not truncated like other commands' output).
  serve [port]    Serve browser test harness (default: 8080)
  clean           Clean all build artifacts
  clean-stale     Prune orphaned dev-profile incremental caches (safe: pure
                  derived state; worst case is a slower next compile). Cargo
                  never GCs these, so target/debug/incremental grows without
                  bound. Default 7d threshold — override with --days

${BOLD}Options:${NC}
  --features <f>  Extra features to enable (e.g. --features voice,channels).
                  Notable: voice-onnx (native STT/TTS — this machine's working
                  config); diskann (real DiskANN vector backend — WITHOUT it a
                  diskann/hybrid vector config silently degrades to a brute-
                  force stub; the kernel warns at boot, vector.strict errors)
  --profile <p>   Cargo profile: debug, release, release-wasm (default varies)
  --force, -f     Force rebuild even if artifacts are up-to-date; for
                  install, override the feature-downgrade guard
  --debug         Use the debug profile (install command)
  --no-fail-fast  Keep running tests after a failure (test command) — full
                  verdict when a known-environmental failure would fail-fast
  --prefix <dir>  Install into <dir> instead of ~/.cargo/bin (install command)
  --days <n>      Age threshold in days (clean-stale command, default 7)
  --with-release-dry-run
                  (gate only) Also run release-dry-run as check 20 (WEFT-460).
                  Equivalent env: GATE_RELEASE_DRY_RUN=1
  --check         (agents-catalog command only) Verify agents/catalog.json is
                  up to date instead of writing it; exits non-zero if stale.
  --verbose       Show full cargo output
  --dry-run       Print commands without executing
  --help          Show this help

${BOLD}Examples:${NC}
  scripts/build.sh native                          # Release CLI binary
  scripts/build.sh install --features voice-onnx    # Install with voice (this machine)
  scripts/build.sh install --debug                  # Fast-iteration install
  scripts/build.sh install --prefix /tmp/wprefix    # Verify install off ~/.cargo/bin
  scripts/build.sh native --features voice          # CLI with voice
  scripts/build.sh gui-egui                         # Native egui GUI (release)
  scripts/build.sh gui-egui --profile debug         # Native egui GUI (debug)
  scripts/build.sh browser                          # Browser WASM
  scripts/build.sh gate                             # Full phase gate
  WEFTOS_PI_HOST=pi5 scripts/build.sh test-pi       # Full lane on the real Pi 5
  WEFTOS_PI_HOST=pi5 scripts/build.sh test-pi clawft-kernel --filter chain
  scripts/build.sh gate --with-release-dry-run      # Gate + cargo-dist host rehearsal
  scripts/build.sh release-dry-run                  # cargo-dist host-triple dry-run
  scripts/build.sh native --dry-run                 # Preview commands
  scripts/build.sh wasi --force                      # Force WASI rebuild
  scripts/build.sh browser && scripts/build.sh serve # Build + serve test harness
EOF
}

# ── Argument parsing ─────────────────────────────────────────────────
parse_args() {
    if [ $# -eq 0 ]; then
        usage
        exit 0
    fi

    COMMAND="$1"
    shift

    # cogs-conformance passes every remaining argument through to
    # scripts/cogs/conformance.py (sweep | probe | summarize | selftest).
    if [ "$COMMAND" = "cogs-conformance" ]; then
        while [ $# -gt 0 ]; do
            if [ "$1" = "--dry-run" ]; then DRY_RUN=true; else COGS_ARGS+=("$1"); fi
            shift
        done
        [ ${#COGS_ARGS[@]} -gt 0 ] || COGS_ARGS=(--help)
        return 0
    fi

    # test-pi passes every remaining argument (including --dry-run and
    # --help) through to scripts/pi/pi_lane.py.
    if [ "$COMMAND" = "test-pi" ]; then
        PI_ARGS=("$@")
        return 0
    fi

    if [ "$COMMAND" = "licence-cross" ] || [ "$COMMAND" = "cog-host-cross" ]; then
        while [ $# -gt 0 ]; do
            case "$1" in
                armv7|armv7-unknown-linux-gnueabihf) LICENCE_TARGETS+=(armv7-unknown-linux-gnueabihf) ;;
                aarch64|aarch64-unknown-linux-gnu) LICENCE_TARGETS+=(aarch64-unknown-linux-gnu) ;;
                --dry-run) DRY_RUN=true ;;
                *) echo "licence-cross: unknown option $1" >&2; exit 1 ;;
            esac
            shift
        done
        return 0
    fi

    if [ "$COMMAND" = "cogs-launcher" ]; then
        while [ $# -gt 0 ]; do
            case "$1" in
                --linux-arm64) LAUNCHER_LINUX=true ;;
                --dry-run) DRY_RUN=true ;;
                *) echo "cogs-launcher: unknown option $1" >&2; exit 1 ;;
            esac
            shift
        done
        return 0
    fi

    # Capture positional arg for serve command (port number)
    if [ "$COMMAND" = "serve" ] && [ $# -gt 0 ] && [[ "$1" =~ ^[0-9]+$ ]]; then
        SERVE_PORT="$1"
        shift
    fi

    # Capture positional args for test command (package scoping):
    #   scripts/build.sh test [<package>…]
    #   scripts/build.sh clippy [<package>…]
    #   scripts/build.sh check [<package>…]
    if [ "$COMMAND" = "test" ] || [ "$COMMAND" = "clippy" ] || [ "$COMMAND" = "check" ] || [ "$COMMAND" = "check-tests" ]; then
        while [ $# -gt 0 ] && [[ "$1" != --* ]]; do
            TEST_PACKAGES+=("$1")
            shift
        done
    fi

    # wasm-project [guest|runner|all]
    if [ "$COMMAND" = "wasm-project" ] && [ $# -gt 0 ] && [[ "$1" != --* ]]; then
        WASM_PROJECT_PART="$1"
        shift
    fi

    # Capture positional args for bench command:
    #   scripts/build.sh bench <crate> <bench-name> [--features f]
    if [ "$COMMAND" = "bench" ]; then
        if [ $# -gt 0 ] && [[ "$1" != --* ]]; then
            BENCH_CRATE="$1"
            shift
        fi
        if [ $# -gt 0 ] && [[ "$1" != --* ]]; then
            BENCH_NAME="$1"
            shift
        fi
    fi

    # scope / scope-web take the companion app name: scripts/build.sh scope <name>
    SCOPE_NAME=""
    if [ "$COMMAND" = "scope" ] || [ "$COMMAND" = "scope-web" ]; then
        if [ $# -gt 0 ] && [[ "$1" != -* ]]; then
            SCOPE_NAME="$1"
            shift
        fi
    fi

    # Capture optional positional budget overrides for wasm-panel:
    #   scripts/build.sh wasm-panel [<max-raw-kb> [<max-gz-kb>]]
    if [ "$COMMAND" = "wasm-panel" ]; then
        if [ $# -gt 0 ] && [[ "$1" =~ ^[0-9]+$ ]]; then
            WASM_PANEL_MAX_RAW_KB="$1"
            shift
            if [ $# -gt 0 ] && [[ "$1" =~ ^[0-9]+$ ]]; then
                WASM_PANEL_MAX_GZ_KB="$1"
                shift
            fi
        fi
    fi

    while [ $# -gt 0 ]; do
        case "$1" in
            --features)
                FEATURES="${2:?'--features requires a value'}"
                shift 2
                ;;
            --filter)
                TEST_FILTER="${2:?'--filter requires a value'}"
                shift 2
                ;;
            --profile)
                PROFILE="${2:?'--profile requires a value'}"
                shift 2
                ;;
            --test-filter)
                TEST_FILTER="${2:?'--test-filter requires a test-name substring'}"
                shift 2
                ;;
            --force|-f)
                FORCE=true
                shift
                ;;
            --debug)
                DEBUG=true
                shift
                ;;
            --no-fail-fast)
                NO_FAIL_FAST=true
                shift
                ;;
            --prefix)
                PREFIX="${2:?'--prefix requires a directory'}"
                shift 2
                ;;
            --days)
                CLEAN_STALE_DAYS="${2:?'--days requires a number'}"
                shift 2
                ;;
            --with-release-dry-run)
                WITH_RELEASE_DRY_RUN=true
                shift
                ;;
            --check)
                AGENTS_CATALOG_CHECK=true
                shift
                ;;
            --verbose)
                VERBOSE=true
                shift
                ;;
            --dry-run)
                DRY_RUN=true
                shift
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                printf "${RED}Unknown option: %s${NC}\n" "$1"
                usage
                exit 1
                ;;
        esac
    done
}

# ── Main ─────────────────────────────────────────────────────────────
main() {
    parse_args "$@"

    case "$COMMAND" in
        native)       cmd_native ;;
        native-debug) cmd_native_debug ;;
        cos)          cmd_cos ;;
        cos-up)       cmd_cos_up ;;
        cos-bridge)   cmd_cos_bridge ;;
        install)      cmd_install ;;
        gui-egui)     cmd_gui_egui ;;
        atlas-bench)  cmd_atlas_bench ;;
        wasi)         cmd_wasi ;;
        browser)      cmd_browser ;;
        scope)        cmd_scope "$SCOPE_NAME" ;;
        scope-web)    cmd_scope_web "$SCOPE_NAME" ;;
        guide-web)    cmd_guide_web ;;
        ecg-scope)    cmd_scope ecg ;;
        ecg-scope-web) cmd_scope_web ecg ;;
        sound-scope)  cmd_scope sound ;;
        sound-scope-web) cmd_scope_web sound ;;
        manager)      cmd_manager ;;
        manager-web)  cmd_manager_web ;;
        ui)           cmd_ui ;;
        ui-docker)    cmd_ui_docker ;;
        docker-smoke) cmd_docker_smoke ;;
        ui-e2e)       cmd_ui_e2e ;;
        releases-mdx) cmd_releases_mdx ;;
        all)          cmd_all ;;
        test)         cmd_test ;;
        test-browser) cmd_test_browser ;;
        bundle-size)  cmd_bundle_size ;;
        wasm-panel)   cmd_wasm_panel "${WASM_PANEL_MAX_RAW_KB:-}" "${WASM_PANEL_MAX_GZ_KB:-}" ;;
        check)        cmd_check ;;
        check-mesh-only) cmd_check_mesh_only ;;
        kb)           cmd_kb ;;
        fuzz-check)   cmd_fuzz_check ;;
        check-launcher) cmd_check_launcher ;;
        check-doc-commands) cmd_check_doc_commands ;;
        test-mesh-service) cmd_test_mesh_service ;;
        check-tests)  cmd_check_tests ;;
        check-mesh-no-owned-state) cmd_check_mesh_no_owned_state ;;
        check-daemon-no-mesh-testing) cmd_check_daemon_no_mesh_testing ;;
        clippy)       cmd_clippy ;;
        clippy-onnx)  cargo clippy --all-targets -p clawft-voice-onnx --features onnx -- -D warnings ;;
        audit)        cmd_audit ;;
        npm-audit)    cmd_npm_audit ;;
        agents-validate)    cmd_agents_validate ;;
        agents-catalog)     cmd_agents_catalog ;;
        agents-leak-check)  cmd_agents_leak_check ;;
        cogs-conformance)   cmd_cogs_conformance ;;
        cogs-launcher)      cmd_cogs_launcher ;;
        licence-cross)      cmd_licence_cross ;;
        cog-host-cross)     cmd_cog_host_cross ;;
        cog-host-x86)       cmd_cog_host_x86 ;;
        android)            cmd_android ;;
        glasses)            cmd_glasses ;;
        wasm-project)       cmd_wasm_project ;;
        test-wasm-project)  cmd_test_wasm_project ;;
        windows-check)      cmd_windows_check ;;
        licence-uid-check)  cmd_licence_uid_check ;;
        test-pi)            cmd_test_pi ;;
        n6-leaf)            cmd_n6_leaf ;;
        cos-console)        cmd_cos_console ;;
        cos-console-demo)   cmd_cos_console_demo ;;
        cos-host)           cmd_cos_host ;;
        leaf-display)       cmd_leaf_display ;;
        edgepad-fw)         cmd_edgepad_fw ;;
        gate)         cmd_gate ;;
        pipeline-pass) cmd_pipeline_pass ;;
        release-dry-run) cmd_release_dry_run ;;
        bench)        cmd_bench "$BENCH_CRATE" "$BENCH_NAME" ;;
        serve)        cmd_serve "$SERVE_PORT" ;;
        clean)        cmd_clean ;;
        clean-stale)  cmd_clean_stale ;;
        --help|-h)    usage ;;
        *)
            printf "${RED}Unknown command: %s${NC}\n" "$COMMAND"
            usage
            exit 1
            ;;
    esac
}

main "$@"
