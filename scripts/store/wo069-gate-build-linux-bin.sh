#!/usr/bin/env bash
# WO-069 lane G2, gate "block 3": build the Linux x86-64 test binary s2_follower_half in a weftos-ci container, then fetch it.
# Offline preparation: nothing here runs unless the owner has said go (WO069_OWNER_GO_BLOCK3=1). `plan` and --dry-run run nothing.
set -euo pipefail
export GATE_NAME=wo069-gate-build-linux-bin
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-build-linux-bin.sh [--dry-run] [--ref REF] [--no-wait] [plan|push|build|wait|fetch|clean|elf-check FILE]

Builds the follower half of crates/weftos-store/tests/s2_follower_half.rs for Linux x86-64 on the weftos-ci host (rootless podman,
image localhost/tapestry/build:1.95-ci-wasm, the only sanctioned way code reaches that host: scratch/ci/ci-push.sh, ci-run.sh, ci-wait.sh),
then copies it to the notes volume. Its output is WO069_GATE_FH_LINUX_BIN for wo069-gate-item1-two-machines.sh --follower=library.

  plan       (default) print every command, the in-container command, time, disk and the runtime question. Runs nothing.
  push       ci-push.sh wo069-fh <repo> <ref>: a git bundle over ssh to ~/ci-jobs/wo069-fh/src. --ref REF (default: HEAD of
             w2/wo069-worktree-mirror); the commit is printed.
  build      ci-run.sh the targeted cargo test --no-run --locked, locate the ELF test executable, copy it to /target/wo069-fh/ with its
             sha256 and `file` output, then ci-wait.sh (a foreground poll every 30 s; --no-wait returns after the start, then use `wait`).
  wait       ci-wait.sh wo069-fh build (the second half of `build --no-wait`).
  fetch      copy the binary and its sha256 to $WO069_GATE_BIN_DIR (no default) as
             s2_follower_half.linux-x86_64, verify the sha256, check it is an x86-64 ELF, print the export line.
  clean      remove ~/ci-jobs/wo069-fh on the host and verify it is gone. Run it when you are done (the host has about 60 GB free).
  elf-check  local only: is FILE an x86-64 ELF? (what fetch uses; no remote call)
  --dry-run  print each command as DRY: ..., make no ssh, scp or network call.

Every remote subcommand (push, build, wait, fetch, clean) REFUSES unless WO069_OWNER_GO_BLOCK3=1: block 3 waits for the owner's go (it
builds on the shared CI host). The production host (PG) is refused. WO069_GATE_CI_HOST (default $WO069_XPC_HOST, required) is used by fetch and clean; ci-push.sh and ci-run.sh hard-code that host, so push and build refuse any other host.
Environment: WO069_GATE_CI_DIR (the directory holding ci-push.sh, ci-run.sh, ci-wait.sh; required for push, build, wait), WO069_GATE_BIN_DIR (absolute, required for fetch), WO069_XPC_HOST.
EOF
}

LANE=wo069-fh; JOB=build; DEFAULT_HOST=${WO069_XPC_HOST:-}
HOST=${WO069_GATE_CI_HOST:-$DEFAULT_HOST}
CI_DIR=${WO069_GATE_CI_DIR:-<set WO069_GATE_CI_DIR>}
BIN_DIR=${WO069_GATE_BIN_DIR:-<set WO069_GATE_BIN_DIR>}
BIN_NAME=s2_follower_half.linux-x86_64
REMOTE_OUT='ci-jobs/wo069-fh/target/wo069-fh'   # CARGO_TARGET_DIR=/target in the container is ~/ci-jobs/<lane>/target on the host (ci README)
IMAGE=localhost/tapestry/build:1.95-ci-wasm
SUB=plan; REF=w2/wo069-worktree-mirror; WAIT=1; ARG=""
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        --ref) REF=${2:?--ref needs a value}; shift 2 ;;
        --no-wait) WAIT=0; shift ;;
        plan|push|build|wait|fetch|clean) SUB=$1; shift ;;
        elf-check) SUB=$1; ARG=${2:-}; shift $(( $# > 1 ? 2 : 1 )) ;;
        *) gate_die "unknown argument $1 (see --help)" ;;
    esac
done

# The in-container command: one line (ci-run.sh quotes it once through ssh). Runs in /src with CARGO_TARGET_DIR=/target.
CARGO_CMD='cargo test -p weftos-store --features rustfs --release --test s2_follower_half --no-run --locked'
LOCATE='D=/target/release/deps; B=; for f in $(ls -t $D/s2_follower_half-* 2>/dev/null); do case $f in *.d) continue;; esac; if [ -x $f ] && [ "$(od -An -tx1 -N4 $f | tr -d " \n")" = 7f454c46 ]; then B=$f; break; fi; done; test -n "$B"'
STAGE='O=/target/wo069-fh; rm -rf $O; mkdir -p $O; cp $B $O/s2_follower_half; cd $O; sha256sum s2_follower_half > s2_follower_half.sha256; if command -v file >/dev/null 2>&1; then file s2_follower_half > s2_follower_half.file; else echo "file(1) absent; e_machine bytes: $(od -An -tx1 -j18 -N2 s2_follower_half)" > s2_follower_half.file; fi; cat s2_follower_half.sha256 s2_follower_half.file'
BUILD_CMD="set -e; $CARGO_CMD; $LOCATE; $STAGE"

require_go() { # <what>
    [ "$GATE_DRY" = 1 ] && return 0
    [ "${WO069_OWNER_GO_BLOCK3:-}" = 1 ] || GATE_RC=3 gate_die "refusing: $1 touches the shared weftos-ci host, and block 3 of the gate run plan waits for the owner's go. Set WO069_OWNER_GO_BLOCK3=1 only after the owner says go. (--dry-run and plan print the commands and run nothing.)"
}
check_host() { # <needs the hard-coded ci host: 0|1>
    [ -n "$HOST" ] || gate_die "set WO069_XPC_HOST (or WO069_GATE_CI_HOST): the ssh target of the CI host"
    gate_refuse_pg "CI host" "$HOST"
    if [ "$1" = 1 ] && [ "$HOST" != "$DEFAULT_HOST" ]; then
        gate_die "push and build go through ci-push.sh and ci-run.sh, which hard-code $DEFAULT_HOST; refusing host '$HOST'"
    fi
}
ci() { local s=$1; shift; [ "$GATE_DRY" = 1 ] || [ -x "$CI_DIR/$s" ] || gate_die "missing $CI_DIR/$s (WO069_GATE_CI_DIR)"; gate_run "$CI_DIR/$s" "$@"; }
resolve_ref() { # prints the commit; in dry-run an unresolvable ref is shown, not fatal
    local sha
    if sha=$(git -C "$GATE_REPO" rev-parse --verify "$REF^{commit}" 2>/dev/null); then printf '%s' "$sha"
    elif [ "$GATE_DRY" = 1 ]; then printf '<%s: not found in this repo>' "$REF"
    else gate_die "ref '$REF' not found in $GATE_REPO"; fi
}
elf_x86_64() { # <file>: ELF magic, 64-bit class, little-endian e_machine 0x3e
    [ -s "$1" ] && [ "$(od -An -tx1 -N5 "$1" | tr -d ' \n')" = 7f454c4602 ] && [ "$(od -An -tx1 -j18 -N2 "$1" | tr -d ' \n')" = 3e00 ]
}

cmd_push() {
    check_host 1; require_go push
    local sha; sha=$(resolve_ref); gate_say "push: lane $LANE, ref $REF, commit $sha (HEAD of that ref in $GATE_REPO)"
    ci ci-push.sh "$LANE" "$GATE_REPO" "$sha"
}
cmd_build() {
    check_host 1; require_go build
    gate_say "build: lane $LANE job $JOB on the host; in-container command (run in /src, CARGO_TARGET_DIR=/target):"; gate_say "  $BUILD_CMD"
    ci ci-run.sh "$LANE" "$JOB" weftos "$BUILD_CMD"
    if [ "$WAIT" = 1 ]; then cmd_wait; else gate_say "started; finish with: $0 wait"; fi
}
cmd_wait() {
    check_host 0; require_go wait
    if [ "$GATE_DRY" = 1 ]; then ci ci-wait.sh "$LANE" "$JOB"; return 0; fi
    local out; out=$("$CI_DIR/ci-wait.sh" "$LANE" "$JOB") || true
    printf '%s\n' "$out"
    # ci-wait prints the job's status line first ("exit <code>..."); anything but exit 0 is a failed build, whatever ci-wait's own rc.
    printf '%s\n' "$out" | head -1 | grep -Eq '^exit[^0-9]*0([^0-9]|$)' || gate_die "the build job did not report exit 0 (first line: '$(printf '%s' "$out" | head -1)'); log in $CI_DIR/logs/$LANE/$JOB.log"
}
cmd_fetch() {
    check_host 0; require_go fetch
    case "$BIN_DIR" in /*) ;; *) gate_die "WO069_GATE_BIN_DIR '$BIN_DIR' must be an absolute path on the notes volume" ;; esac
    ! gate_bad_place "$BIN_DIR" || gate_die "WO069_GATE_BIN_DIR '$BIN_DIR' is under /tmp or the home directory; use the notes volume"
    local tmp="$BIN_DIR/.fetch.$$" want got
    gate_run mkdir -p "$BIN_DIR"; gate_run mkdir -p "$tmp"
    gate_run scp -q -o BatchMode=yes "$HOST:ci-jobs/$LANE/target/wo069-fh/s2_follower_half" "$HOST:ci-jobs/$LANE/target/wo069-fh/s2_follower_half.sha256" "$HOST:ci-jobs/$LANE/target/wo069-fh/s2_follower_half.file" "$tmp/"
    if [ "$GATE_DRY" = 1 ]; then
        gate_note "verify: sha256 of $tmp/s2_follower_half equals the first field of s2_follower_half.sha256 (written in the container); refuse otherwise"
        gate_note "verify: x86-64 ELF (magic 7f454c46 02, e_machine 3e 00); print the host's file(1) line"
        gate_note "then: mv to $BIN_DIR/$BIN_NAME, chmod 755, rm -rf $tmp, print the export line"; return 0
    fi
    want=$(cut -d' ' -f1 <"$tmp/s2_follower_half.sha256"); got=$(gate_sha256 "$tmp/s2_follower_half")
    [ -n "$want" ] && [ "$want" = "$got" ] || { rm -rf "$tmp"; gate_die "sha256 mismatch: container wrote '$want', fetched file is '$got'"; }
    elf_x86_64 "$tmp/s2_follower_half" || { rm -rf "$tmp"; gate_die "fetched file is not an x86-64 ELF executable"; }
    mv "$tmp/s2_follower_half" "$BIN_DIR/$BIN_NAME"; chmod 755 "$BIN_DIR/$BIN_NAME"
    gate_say "container file(1): $(cat "$tmp/s2_follower_half.file")"; rm -rf "$tmp"
    gate_say "sha256 $got (matches the container's), x86-64 ELF, $(wc -c <"$BIN_DIR/$BIN_NAME" | tr -d ' ') bytes"
    gate_say "export WO069_GATE_FH_LINUX_BIN=$BIN_DIR/$BIN_NAME"
}
cmd_clean() {
    check_host 0; require_go clean
    gate_run ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST" "rm -rf ~/ci-jobs/$LANE; if test -e ~/ci-jobs/$LANE; then echo STILL-THERE; exit 1; else echo gone; fi"
    gate_note "verify the output is 'gone'; the local bundle was removed by ci-push.sh; logs stay in $CI_DIR/logs/$LANE"
}

cmd_plan() {
    GATE_DRY=1; check_host 0
    local sha; sha=$(resolve_ref)
    cat <<EOF
== block 3 plan: Linux x86-64 build of the follower-half test binary (nothing below runs; nothing was touched)
host        $HOST   lane $LANE   image $IMAGE (ci default)   ref $REF -> $sha
source      git bundle over ssh to ~/ci-jobs/$LANE/src (ci-push.sh); jobs run in rootless podman as CARGO_TARGET_DIR=/target ( = ~/ci-jobs/$LANE/target )
slots       one of the six job slots (8 CPUs, 20 GB each by default); cargo home /cargo is shared per repo kind and may need crates.io from xpc
time        estimate, not measured: 8 to 15 minutes on a cold target dir (release, lto = true, codegen-units = 1 for the test binary)
disk        estimate, not measured: 2 to 4 GB under ~/ci-jobs/$LANE/target, the staged binary 10 to 40 MB; the host has about 60 GB free
cargo       $CARGO_CMD
              --locked: Cargo.lock is committed (git ls-files lists it) and 'cargo metadata --offline --locked' resolves cleanly here;
              the CI recipe's own examples use --locked. The test target is auto-discovered (crates/weftos-store/tests/s2_follower_half.rs), it
              is gated #![cfg(feature = "rustfs")], and --no-run builds the executable without running it (it needs a live RustFS).
output      /target/wo069-fh/s2_follower_half  (+ .sha256, .file) = ~/${REMOTE_OUT} on the host; fetched to $BIN_DIR/$BIN_NAME
runtime     The binary is dynamically linked against glibc of the build image (Debian trixie) and reqwest uses rustls (no libssl). It must run
            where that glibc exists: in the SAME image on the CI host under podman, which is what wo069-gate-item1-two-machines.sh
            --follower=library does (it mounts the host dir at /rfs and runs /rfs/s2_follower_half in $IMAGE). It was NOT built to run on the
            WSL distro's own userland or a Mac. At run time it needs only the follower env file (read-only key) and network reach to the
            throwaway RustFS (--network host on xpc); no weaver, no daemon. glibc and ELF compatibility are checked only by a real run.
commands (the owner's go is WO069_OWNER_GO_BLOCK3=1, in the environment of each):
EOF
    WAIT=1; cmd_push; cmd_build; cmd_fetch; cmd_clean
    printf 'order      push, build (or build --no-wait, then wait), fetch, clean. Then: export WO069_GATE_FH_LINUX_BIN=%s/%s\n' "$BIN_DIR" "$BIN_NAME"
}

case "$SUB" in
    plan) cmd_plan ;;
    push) cmd_push ;; build) cmd_build ;; wait) cmd_wait ;; fetch) cmd_fetch ;; clean) cmd_clean ;;
    elf-check) [ -n "$ARG" ] || gate_die "elf-check needs a file"; if elf_x86_64 "$ARG"; then gate_say "ok: $ARG is an x86-64 ELF"; else gate_die "$ARG is not an x86-64 ELF"; fi ;;
esac
