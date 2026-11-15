#!/usr/bin/env bash
# Sourced by wo069-e2e-test.sh: throwaway RustFS + throwaway user daemon, step timing, teardown.
# Everything lives under $WORK (a mktemp dir under target/). Nothing here touches ~/.weftos, PG or a shared store.

now_ms() { perl -MTime::HiRes=time -e 'printf "%d", time()*1000'; }
rand_hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
free_port() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }

FAILS=0
# check <description> <shell string>: runs the string, records PASS/FAIL and its wall time. It reads $WORK/last.out
# (the stdout of the last `timed` command, stderr in last.err); its own output goes to chk.out, never to last.out.
check() {
    local t0 t1 res=PASS
    t0=$(now_ms); eval "$2" >"$WORK/chk.out" 2>&1 || res=FAIL
    t1=$(now_ms)
    printf '%s\t%s ms\t%s\n' "$res" "$((t1 - t0))" "$1" | tee -a "$WORK/steps.tsv"
    if [ "$res" = FAIL ]; then FAILS=$((FAILS + 1)); sed 's/^/    | /' "$WORK/chk.out" | head -12; fi
}
# timed <label> <shell string>: run and print the time only (measurements, not assertions).
timed() { local t0; t0=$(now_ms); eval "$2" >"$WORK/last.out" 2>"$WORK/last.err"; local rc=$?; printf 'TIME\t%s ms\t%s\n' "$(( $(now_ms) - t0 ))" "$1" | tee -a "$WORK/steps.tsv"; return $rc; }

rustfs_start() { # sets RPORT RPID; writes $WORK/live.env (0600)
    local access secret code up=""
    RPORT=$(free_port); access="e2e$(rand_hex 6)"; secret=$(rand_hex 20)
    mkdir -p "$WORK/rfs-data"
    env RUSTFS_ACCESS_KEY="$access" RUSTFS_SECRET_KEY="$secret" "$RUSTFS_BIN" server --address "127.0.0.1:$RPORT" "$WORK/rfs-data" >"$WORK/rustfs.log" 2>&1 &
    RPID=$!
    for _ in $(seq 1 60); do
        code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$RPORT/" || true)
        # 000 = not listening, 503 = listening but still starting (the first bucket call would fail)
        if [ "$code" != 000 ] && [ "$code" != 503 ]; then up=1; break; fi
        sleep 0.5
    done
    [ -n "$up" ] || return 1
    sleep 2
    ( umask 077; printf 'MESH_S3_ENDPOINT=http://127.0.0.1:%s\nMESH_S3_ACCESS_KEY=%s\nMESH_S3_SECRET_KEY=%s\n' "$RPORT" "$access" "$secret" >"$WORK/live.env" )
}

# Run a command with the isolated environment of one local principal (writer or follower): own agent label,
# own follow/registration state, the logging weaver wrapper, the throwaway runtime dir and a fake HOME.
as() { # <role> cmd...
    local role=$1; shift
    env HOME="$WORK/home" WEFTOS_RUNTIME_DIR="$WORK/run" WEFTOS_AGENT="$role" XDG_STATE_HOME="$WORK/xdg-$role" \
        WTS_STATE_DIR="$WORK/xdg-$role/wts" WEAVER_PROFILE=user WTS_STRICT=1 WTS_WEAVER="$WORK/bin/weaver" WRAP_LOG="$WORK/calls-$role.log" WTS_PROJECT="$PROJECT" \
        GIT_CONFIG_GLOBAL="$WORK/gitconfig" GIT_CONFIG_NOSYSTEM=1 AGF_TARGET_REF=master "$@"
}
calls() { grep -c "^store worktree $2" "$WORK/calls-$1.log" 2>/dev/null || true; }   # <role> <verb>

daemon_start() { # sets DPID
    mkdir -p "$WORK/home/.weftos" "$WORK/run" "$WORK/bin"
    # A user daemon is read-only outside a registered project (scope gate, ADR-103 D12). This run registers no project
    # (there is no real one to add), so the throwaway config lifts that for the throwaway daemon only.
    printf '[kernel.governance]\noutside_project = "allow_all"\n' >"$WORK/home/.weftos/weave.toml"
    printf '#!/bin/sh\necho "$*" >>"$WRAP_LOG"\nexec %s "$@"\n' "$WEAVER" >"$WORK/bin/weaver"; chmod +x "$WORK/bin/weaver"
    env HOME="$WORK/home" WEFTOS_RUNTIME_DIR="$WORK/run" WEFTOS_STORE_RUSTFS_ENV="$WORK/live.env" \
        "$WEAVER" kernel start --foreground --profile user >"$WORK/daemon.out" 2>&1 &
    DPID=$!
    for _ in $(seq 1 120); do
        kill -0 "$DPID" 2>/dev/null || { echo "daemon exited early:"; tail -15 "$WORK/daemon.out"; return 1; }
        as writer "$WEAVER" store ls >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    echo "daemon did not serve the store in 60 s"; tail -15 "$WORK/daemon.out"; return 1
}

descendants() { local c; for c in $(pgrep -P "$1" 2>/dev/null); do echo "$c"; descendants "$c"; done; }

teardown() { # kills only what this run started, removes $WORK
    trap - EXIT INT TERM
    local kids p
    [ -z "${DPID:-}" ] || { kids=$(descendants "$DPID"); kill "$DPID" 2>/dev/null; for p in $kids; do kill "$p" 2>/dev/null; done; wait "$DPID" 2>/dev/null; }
    [ -z "${RPID:-}" ] || { kill "$RPID" 2>/dev/null; wait "$RPID" 2>/dev/null; }
    sleep 1
    echo "teardown: still alive -> daemon:$(kill -0 "${DPID:-0}" 2>/dev/null && echo yes || echo no) rustfs:$(kill -0 "${RPID:-0}" 2>/dev/null && echo yes || echo no)"
    [ -n "${KEEP:-}" ] && { echo "KEEP set: $WORK left in place"; return; }
    [ -z "${E2E_LOG_DIR:-}" ] || cp "$WORK/steps.tsv" "$E2E_LOG_DIR/e2e-steps.tsv" 2>/dev/null || true
    rm -rf "$WORK"; echo "teardown: $WORK removed"
}
