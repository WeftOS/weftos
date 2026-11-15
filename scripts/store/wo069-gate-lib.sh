#!/usr/bin/env bash
# Sourced by the wo069-gate-*.sh scripts (WO-069 lane F2, ADR-126 section 4a gate evidence). Never run by itself.
# Provides: dry-run aware command runner, the PG refusal, the owner-go refusal, credential-file checks, a clock-offset
# probe and a raw SigV4 S3 call (curl --config, so a key never appears on a command line).
# Nothing here starts a service. Nothing here knows a credential value except inside gate_s3raw's 0600 temp file.

GATE_DRY=${GATE_DRY:-0}
# shellcheck disable=SC2034  # used by the scripts that source this file
GATE_REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

gate_say()  { printf '%s\n' "$*"; }
gate_warn() { printf '%s\n' "$*" >&2; }
gate_die()  { printf '%s: %s\n' "${GATE_NAME:-wo069-gate}" "$*" >&2; exit "${GATE_RC:-1}"; }
gate_now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

# gate_run cmd args...: run it, or in dry-run print it (quoted) and return 0. Remote commands are printed as the ssh line.
gate_run() {
    if [ "$GATE_DRY" = 1 ]; then
        local a q="'\\''"; printf 'DRY:'
        for a in "$@"; do if [[ $a =~ ^[A-Za-z0-9_@%+=:,./~-]+$ ]]; then printf ' %s' "$a"; else printf " '%s'" "${a//\'/$q}"; fi; done
        printf '\n'; return 0
    fi
    "$@"
}
# gate_note text: a line only printed in dry-run (what a step would check or write, with no command to show).
gate_note() { if [ "$GATE_DRY" = 1 ]; then printf 'DRY: # %s\n' "$*"; fi; }

# The production store: its address, name and port. A drill target that matches is refused, as wo069-drills-lib.sh does.
GATE_PG_RE='100\.90\.170\.87|photo-gallery|:9489|(^|[@./:_-])pg($|[./:_-])'
gate_refuse_pg() { # <what> <value>
    if printf '%s' "$2" | grep -Eiq "$GATE_PG_RE"; then
        GATE_RC=4 gate_die "refusing: $1 '$2' is the production store host (PG, photo-gallery). Gate drills run only against a throwaway server on a scratch port, never PG."
    fi
}
# Every endpoint in an env file is checked, not just the one a script was told about.
gate_refuse_pg_env() { # <env file>
    local ep; [ "$GATE_DRY" = 1 ] && [ ! -r "$1" ] && return 0
    ep=$(gate_env_get "$1" MESH_S3_ENDPOINT) || true
    gate_refuse_pg "endpoint in $1" "$ep"
}

# Owner hold, round-3 question 3: a follower DAEMON on aepod-xpc (the installed weaver there is 0.8.4 with no `store` command, and
# the CI rule says a live check in a container never uses a store-capable weaver).
gate_require_go_q3() { # <what needs it>
    if [ "${WO069_OWNER_GO_Q3:-}" != 1 ]; then
        GATE_RC=3 gate_die "refusing: $1 needs the owner's go on round-3 question 3 (a store-capable weaver and a follower daemon on aepod-xpc; CI rule: no store-capable weaver in a container). Set WO069_OWNER_GO_Q3=1 only after the owner says go. The decision-free alternative is --follower=library."
    fi
}

gate_mode() { stat -f '%Lp' "$1" 2>/dev/null || stat -c '%a' "$1"; }
gate_under() { case "$1" in "$2"|"$2"/*) return 0 ;; esac; return 1; }
gate_bad_place() { # nothing in /tmp or the home directory
    local p=$1 d
    for d in /tmp /private/tmp /var/tmp /var/folders "${HOME:-/nonexistent-home}"; do gate_under "$p" "$d" && return 0; done; return 1
}
gate_secret_ok() { # <file>: exists, mode 0600, not under /tmp or $HOME
    if [ "$GATE_DRY" = 1 ]; then gate_note "check $1 exists, mode 0600, outside /tmp and \$HOME"; return 0; fi
    [ -f "$1" ] || gate_die "credential file $1 does not exist"
    [ "$(gate_mode "$1")" = 600 ] || gate_die "credential file $1 is not mode 0600"
    ! gate_bad_place "$1" || gate_die "credential file $1 is under /tmp or the home directory"
}
gate_cred_dir_ok() { # <dir>: absolute, outside /tmp and $HOME; created 0700 by the caller when missing
    case "$1" in /*) ;; *) gate_die "WO069_GATE_CRED_DIR must be an absolute path (on the notes volume), got '$1'" ;; esac
    ! gate_bad_place "$1" || gate_die "WO069_GATE_CRED_DIR '$1' is under /tmp or the home directory; use the notes volume"
}
gate_sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }
gate_env_get() { sed -n "s/^$2=//p" "$1" | head -1; }   # <file> <KEY>: prints the value; callers never echo it

# --- throwaway local principals (the same isolation as wo069-e2e-lib.sh's `as`): own HOME, runtime dir, state and agent label
gate_gitconfig() { printf '[user]\n\tname = gate\n\temail = gate@example.invalid\n[init]\n\tdefaultBranch = master\n' >"$1/gitconfig"; }
gate_gx() { # <work> <role> <project> <weaver> cmd...
    local w=$1 role=$2 proj=$3 weaver=$4; shift 4
    env HOME="$w/home-$role" WEFTOS_RUNTIME_DIR="$w/run-$role" WEFTOS_AGENT="$role" XDG_STATE_HOME="$w/xdg-$role" WTS_STATE_DIR="$w/xdg-$role/wts" \
        WEAVER_PROFILE=user WTS_STRICT=1 WTS_WEAVER="$weaver" WTS_PROJECT="$proj" GIT_CONFIG_GLOBAL="$w/gitconfig" GIT_CONFIG_NOSYSTEM=1 AGF_TARGET_REF=master "$@"
}
# gate_daemon_up <work> <role> <project> <weaver> <store env file> [VAR=val ...]: a user daemon on a throwaway runtime dir; sets GATE_PID
gate_daemon_up() {
    local w=$1 role=$2 proj=$3 weaver=$4 envf=$5; shift 5
    mkdir -p "$w/home-$role/.weftos" "$w/run-$role"
    printf '[kernel.governance]\noutside_project = "allow_all"\n' >"$w/home-$role/.weftos/weave.toml"
    rm -f "$w/run-$role/kernel.lock"
    env HOME="$w/home-$role" WEFTOS_RUNTIME_DIR="$w/run-$role" WEFTOS_STORE_RUSTFS_ENV="$envf" "$@" "$weaver" kernel start --foreground --profile user >>"$w/daemon-$role.out" 2>&1 &
    GATE_PID=$!
    for _ in $(seq 1 120); do gate_gx "$w" "$role" "$proj" "$weaver" "$weaver" store ls >/dev/null 2>&1 && return 0; kill -0 "$GATE_PID" 2>/dev/null || return 1; sleep 0.5; done
    return 1
}
gate_descendants() { local c; for c in $(pgrep -P "$1" 2>/dev/null); do echo "$c"; gate_descendants "$c"; done; }
gate_kill_tree() { local p kids; kids=$(gate_descendants "$1"); kill -9 "$1" 2>/dev/null || true; for p in $kids; do kill -9 "$p" 2>/dev/null || true; done; }  # <pid>: it and everything under it

# gate_clock_offset <ssh target>: remote minus local, in ms, from one ssh round trip (accuracy about rtt/2). Prints "offset_ms rtt_ms".
gate_clock_offset() {
    local t0 r t1
    if [ "$GATE_DRY" = 1 ]; then gate_run ssh -o BatchMode=yes "$1" 'date +%s%3N'; return 0; fi
    t0=$(gate_now_ms); r=$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$1" 'date +%s%3N'); t1=$(gate_now_ms)
    printf '%s %s\n' "$(( r - (t0 + t1) / 2 ))" "$(( t1 - t0 ))"
}

# gate_s3raw <env file> <METHOD> <path after the host, with leading /> [body file]
# One SigV4-signed request with the key in the env file, through `curl --config` (key in a 0600 temp file under $GATE_TMP, removed
# at once, never on the command line). Prints the HTTP status; the response body goes to $GATE_OUT (default /dev/null).
gate_s3raw() {
    local env=$1 method=$2 path=$3 body=${4:-} ep ak sk rg cfg hash code
    [ -n "${GATE_TMP:-}" ] || gate_die "internal: GATE_TMP unset"
    ep=$(gate_env_get "$env" MESH_S3_ENDPOINT); ak=$(gate_env_get "$env" MESH_S3_ACCESS_KEY); sk=$(gate_env_get "$env" MESH_S3_SECRET_KEY)
    rg=$(gate_env_get "$env" MESH_S3_REGION); rg=${rg:-us-east-1}
    if [ -n "$body" ]; then hash=$(gate_sha256 "$body")
    else hash=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855; fi
    cfg=$(umask 077; mktemp "$GATE_TMP/s3cfg.XXXXXX")
    {
        printf 'silent\nshow-error\nrequest = "%s"\nurl = "%s%s"\nuser = "%s:%s"\naws-sigv4 = "aws:amz:%s:s3"\n' "$method" "$ep" "$path" "$ak" "$sk" "$rg"
        printf 'header = "x-amz-content-sha256: %s"\noutput = "%s"\nwrite-out = "%%{http_code}"\n' "$hash" "${GATE_OUT:-/dev/null}"
        [ -z "$body" ] || printf 'data-binary = "@%s"\n' "$body"
    } >"$cfg"
    code=$(curl --config "$cfg" 2>/dev/null || true); rm -f "$cfg"
    printf '%s' "${code:-000}"
}

# --- the second-host probe (runs on the second host, fed over ssh stdin: `{ cat this-lib; echo 'gate_probe_X ...'; } | ssh host bash -s`)
# It needs bash, curl >= 7.75, sha256sum and python3 (or date +%s%3N). It reads one 0600 env file (the follower's read-only key).
# gate_probe_poll <follower env> <project bucket> <trials-file-of-ready-markers> ...: see wo069-gate-item1-two-machines.sh
gate_probe_poll() { # <follower env> <bucket> <key> <interval_ms> <max_ms> : prints "SEEN <epoch_ms> <sha256 of body>" at each change; stops at max_ms
    local env=$1 bucket=$2 key=$3 iv=$4 max=$5 last="" t0 now h
    GATE_TMP=${GATE_TMP:-$(dirname "$env")/tmp}; mkdir -p "$GATE_TMP"; chmod 700 "$GATE_TMP"
    t0=$(date +%s%3N)
    while :; do
        GATE_OUT="$GATE_TMP/poll.body" gate_s3raw "$env" GET "/$bucket/$key" >/dev/null
        h=$(gate_sha256 "$GATE_TMP/poll.body"); now=$(date +%s%3N)
        if [ "$h" != "$last" ]; then printf 'SEEN %s %s\n' "$now" "$h"; last=$h; fi
        [ $(( now - t0 )) -lt "$max" ] || break
        sleep "$(awk "BEGIN{print $iv/1000}")"
    done
}
gate_probe_refuse() { # <follower env> <project bucket> <worktree bucket>: eight write attempts, every one must be 403
    local env=$1 pb=$2 wb=$3 n=0 bad=0 c body
    GATE_TMP=${GATE_TMP:-$(dirname "$env")/tmp}; mkdir -p "$GATE_TMP"; chmod 700 "$GATE_TMP"
    body="$GATE_TMP/forged.json"; printf '{}' >"$body"
    try() { c=$(gate_s3raw "$env" "$1" "$2" "${3:-}"); n=$((n + 1)); printf 'WRITE %s %s %s -> %s\n' "$n" "$1" "$4" "$c"; case "$c" in 403) ;; *) bad=$((bad + 1)) ;; esac; }
    try PUT "/$pb/worktrees/forged.json" "$body" "registry-entry"
    try PUT "/$pb/worktrees/_digest.json" "$body" "digest"
    try PUT "/$wb/.wt/handoff.json" "$body" "handoff-record"
    try PUT "/$wb/.wt/manifest.json" "$body" "manifest"
    try PUT "/$wb/forged-object" "$body" "tree-object"
    try DELETE "/$pb/worktrees/forged.json" "" "delete-registry-entry"
    try DELETE "/$wb/.wt/manifest.json" "" "delete-manifest"
    try PUT "/wo069-forged-bucket-$$" "" "create-bucket"
    rm -f "$body"; printf 'REFUSED %s of %s (non-403: %s)\n' "$((n - bad))" "$n" "$bad"; [ "$bad" = 0 ]
}
