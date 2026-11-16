#!/usr/bin/env bash
# WO-069 lane F2: a THROWAWAY RustFS container on a second host, reached through one ssh port forward.
# Modelled on the lane I run of 2026-11-15 (wo069-4a-drills-20261114.md, "Lane I addendum").
set -euo pipefail
export GATE_NAME=wo069-gate-remote-rustfs
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-remote-rustfs.sh up|mint-keys|status|down <ssh-target> [--dry-run]
       wo069-gate-remote-rustfs.sh --help

  up         start the container on <ssh-target> (podman, --pull=never, loopback scratch port, 4 CPUs, 4 GiB), generate a random
             root key into a 0600 file, open one ssh local forward 127.0.0.1:$WO069_GATE_FWD_PORT -> remote loopback port.
  mint-keys  mint two scoped keys on that server through the admin API: writer (s3:* on every bucket, no admin) and follower
             (read-only), as 0600 env files; copy the follower's to the remote host (0600); probe both (the follower must get 403 on a PUT).
  status     container, port, forward, credential files (modes only; no value is ever printed).
  down       remove container, data, remote dir, forward and every credential file, and VERIFY each is gone (exit 1 if any is not).

Environment (every one has the default shown; WO069_GATE_CRED_DIR has none and is required):
  WO069_GATE_CRED_DIR     absolute local dir for the 0600 credential files; on the notes volume, never /tmp or $HOME
  WO069_GATE_RFS_NAME     wo069-gate-rustfs         container and file-name prefix
  WO069_GATE_RFS_IMAGE    docker.io/rustfs/rustfs:1.0.4   must already be local on the host (the script never pulls)
  WO069_GATE_RFS_PORT     19431                     remote loopback port of the server
  WO069_GATE_FWD_PORT     19431                     local loopback port of the ssh forward
  WO069_GATE_REMOTE_DIR   ~/ci-jobs/wo069-gate-rfs  data, env files and tmp on the host (removed by down)
Refuses PG (100.90.170.87, photo-gallery, :9489) as the target and in any env file. Nothing is printed that is a credential.
--dry-run prints every local and remote command and runs none of them.
EOF
}
CMD=""; HOST=""
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        up|mint-keys|status|down) CMD=$1; shift ;;
        -*) gate_die "unknown option $1 (see --help)" ;;
        *) [ -z "$HOST" ] || gate_die "one host only"; HOST=$1; shift ;;
    esac
done
[ -n "$CMD" ] && [ -n "$HOST" ] || { usage >&2; exit 2; }
gate_refuse_pg "target host" "$HOST"
NAME=${WO069_GATE_RFS_NAME:-wo069-gate-rustfs}; IMAGE=${WO069_GATE_RFS_IMAGE:-docker.io/rustfs/rustfs:1.0.4}
RPORT=${WO069_GATE_RFS_PORT:-19431}; FPORT=${WO069_GATE_FWD_PORT:-19431}; RDIR=${WO069_GATE_REMOTE_DIR:-'~/ci-jobs/wo069-gate-rfs'}
CRED=${WO069_GATE_CRED_DIR:-}; [ -n "$CRED" ] || gate_die "set WO069_GATE_CRED_DIR (absolute, on the notes volume, mode 0700)"
gate_cred_dir_ok "$CRED"
ROOT_ENV="$CRED/$NAME.root.env"; WRITER_ENV="$CRED/$NAME.writer.env"; FOLLOWER_ENV="$CRED/$NAME.follower.env"
SOCK="$CRED/s$(printf %s "$NAME" | cksum | cut -d" " -f1).sock"; STATE="$CRED/$NAME.state"
[ ${#SOCK} -lt 100 ] || gate_die "control socket path $SOCK is too long for ssh (limit about 104); use a shorter WO069_GATE_CRED_DIR"
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10)
rhex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
rsh() { gate_run "${SSH[@]}" "$HOST" "$1"; }   # one remote command line

env_file() { # <path> <endpoint> <access> <secret>: written 0600, values never echoed
    ( umask 077; printf 'MESH_S3_ENDPOINT=%s\nMESH_S3_ACCESS_KEY=%s\nMESH_S3_SECRET_KEY=%s\nMESH_S3_REGION=us-east-1\n' "$2" "$3" "$4" >"$1" )
}

do_up() {
    gate_say "== up: throwaway RustFS on $HOST (image $IMAGE, remote loopback :$RPORT, local forward :$FPORT)"
    gate_run mkdir -p -m 700 "$CRED"
    rsh "podman image exists $IMAGE || { echo 'image $IMAGE is not local on this host; this script never pulls'; exit 1; }"
    rsh "! podman container exists $NAME || { echo 'container $NAME already exists: run down first'; exit 1; }"
    rsh "! ss -ltn | grep -q ':$RPORT ' || { echo 'port $RPORT is in use on the host: pick another WO069_GATE_RFS_PORT'; exit 1; }"
    if [ "$GATE_DRY" = 1 ]; then
        gate_note "generate access and secret locally (random, not printed); write $ROOT_ENV mode 0600 with endpoint http://127.0.0.1:$FPORT"
        gate_note "send RUSTFS_ACCESS_KEY / RUSTFS_SECRET_KEY to $HOST:$RDIR/rustfs.env (mode 0600) through ssh stdin, never argv"
    else
        local ak sk; ak="gate$(rhex 6)"; sk=$(rhex 20)
        env_file "$ROOT_ENV" "http://127.0.0.1:$FPORT" "$ak" "$sk"
        printf 'RUSTFS_ACCESS_KEY=%s\nRUSTFS_SECRET_KEY=%s\n' "$ak" "$sk" | "${SSH[@]}" "$HOST" "umask 077; mkdir -p $RDIR/data $RDIR/tmp; cat >$RDIR/rustfs.env"
        ( umask 077; printf 'host=%s\nname=%s\nrport=%s\nfport=%s\nrdir=%s\nstarted=%s\n' "$HOST" "$NAME" "$RPORT" "$FPORT" "$RDIR" "$(date -u +%FT%TZ)" >"$STATE" )
    fi
    rsh "podman run -d --name $NAME --pull=never --network host --user 0 --cpus 4 --memory 4g --env-file $RDIR/rustfs.env -e RUSTFS_ADDRESS=127.0.0.1:$RPORT -e RUSTFS_CONSOLE_ENABLE=false -v $RDIR/data:/data $IMAGE >/dev/null"
    rsh "for i in \$(seq 1 120); do c=\$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:$RPORT/ || true); [ \"\$c\" != 000 ] && [ \"\$c\" != 503 ] && { echo up http \$c; exit 0; }; sleep 1; done; echo 'server did not come up in 120 s'; podman logs $NAME | tail -5; exit 1"
    gate_run "${SSH[@]}" -f -N -M -S "$SOCK" -o ExitOnForwardFailure=yes -L "127.0.0.1:$FPORT:127.0.0.1:$RPORT" "$HOST"
    gate_run curl -s -o /dev/null -w 'forward answers http %{http_code}\n' "http://127.0.0.1:$FPORT/"
    gate_say "up done. root env: $ROOT_ENV (0600). Next: mint-keys. Always finish with: $0 down $HOST"
}

do_mint() {
    gate_say "== mint-keys: scoped writer (s3:* on all buckets, no admin) and follower (read-only) through the admin API"
    GATE_TMP="$CRED/tmp"; gate_run mkdir -p -m 700 "$GATE_TMP"
    gate_secret_ok "$ROOT_ENV"; gate_refuse_pg_env "$ROOT_ENV"
    local wp='{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:*"],"Resource":["arn:aws:s3:::*"]}]}'
    local fp='{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:GetObject","s3:ListBucket","s3:GetBucketLocation","s3:ListAllMyBuckets","s3:GetBucketTagging"],"Resource":["arn:aws:s3:::*"]}]}'
    if [ "$GATE_DRY" = 1 ]; then
        gate_note "for writer and follower: random key id (gw/gf + 6 hex) and secret; PUT /rustfs/admin/v3/add-user?accessKey=ID body {secretKey,status}"
        gate_note "PUT /rustfs/admin/v3/add-canned-policy?name=p-ID (writer s3:* ; follower Get/List only), then PUT set-user-or-group-policy?policyName=p-ID&userOrGroup=ID&isGroup=false"
        gate_note "write $WRITER_ENV and $FOLLOWER_ENV (0600, endpoint via the forward); follower env copied to $HOST:$RDIR/follower.env (0600, endpoint http://127.0.0.1:$RPORT)"
        gate_note "probe: writer creates and deletes a probe bucket (must be 2xx); follower PUT of an object must be 403; neither may call the admin API successfully"
        return 0
    fi
    local role id sk body pol code ep="http://127.0.0.1:$FPORT"
    for role in writer follower; do
        id="g${role:0:1}$(rhex 6)"; sk=$(rhex 20); pol=$wp; [ "$role" = follower ] && pol=$fp
        body=$(umask 077; mktemp "$GATE_TMP/body.XXXXXX"); printf '{"secretKey":"%s","status":"enabled"}' "$sk" >"$body"
        code=$(gate_s3raw "$ROOT_ENV" PUT "/rustfs/admin/v3/add-user?accessKey=$id" "$body"); rm -f "$body"
        [ "${code:0:1}" = 2 ] || gate_die "add-user for the $role key answered http $code (the curl SigV4 admin call is unverified on this RustFS: see the runbook risk R-mint)"
        body=$(umask 077; mktemp "$GATE_TMP/body.XXXXXX"); printf '%s' "$pol" >"$body"
        code=$(gate_s3raw "$ROOT_ENV" PUT "/rustfs/admin/v3/add-canned-policy?name=p-$id" "$body"); rm -f "$body"
        [ "${code:0:1}" = 2 ] || gate_die "add-canned-policy for $role answered http $code"
        code=$(gate_s3raw "$ROOT_ENV" PUT "/rustfs/admin/v3/set-user-or-group-policy?policyName=p-$id&userOrGroup=$id&isGroup=false")
        [ "${code:0:1}" = 2 ] || gate_die "set-user-or-group-policy for $role answered http $code"
        env_file "$CRED/$NAME.$role.env" "$ep" "$id" "$sk"
    done
    # the follower's env for the second host: loopback of the host itself, copied through stdin, 0600
    sed "s#^MESH_S3_ENDPOINT=.*#MESH_S3_ENDPOINT=http://127.0.0.1:$RPORT#" "$FOLLOWER_ENV" | "${SSH[@]}" "$HOST" "umask 077; mkdir -p $RDIR/tmp; cat >$RDIR/follower.env"
    code=$(gate_s3raw "$WRITER_ENV" PUT "/wo069-probe-$$"); gate_say "probe writer create-bucket: http $code (want 2xx)"
    gate_s3raw "$WRITER_ENV" DELETE "/wo069-probe-$$" >/dev/null
    code=$(gate_s3raw "$FOLLOWER_ENV" PUT "/wo069-probe2-$$"); gate_say "probe follower create-bucket: http $code (want 403)"
    [ "$code" = 403 ] || gate_die "the follower key was not refused: do not run any drill with it"
    gate_say "mint-keys done: $WRITER_ENV and $FOLLOWER_ENV (0600)"
}

do_status() {
    gate_say "== status ($HOST, $NAME)"
    rsh "podman ps -a --filter name=$NAME --format '{{.Names}} {{.Status}}'; ss -ltn | grep ':$RPORT ' || echo 'port $RPORT not listening'; ls -ld $RDIR $RDIR/rustfs.env $RDIR/follower.env 2>&1 | awk '{print \$1, \$NF}'"
    gate_run "${SSH[@]}" -S "$SOCK" -O check "$HOST"
    local f; for f in "$ROOT_ENV" "$WRITER_ENV" "$FOLLOWER_ENV" "$STATE"; do
        if [ "$GATE_DRY" = 1 ]; then gate_note "stat mode of $f"; elif [ -e "$f" ]; then printf 'local %s mode %s\n' "$f" "$(gate_mode "$f")"; else printf 'local %s absent\n' "$f"; fi
    done
}

do_down() {
    gate_say "== down: remove everything this script made on $HOST and locally, then verify"
    rsh "podman rm -f -v $NAME >/dev/null 2>&1 || true; podman volume rm -f $NAME >/dev/null 2>&1 || true; rm -rf $RDIR"   # -v: an anonymous volume of this container goes with it
    gate_run "${SSH[@]}" -S "$SOCK" -O exit "$HOST" || true
    if [ "$GATE_DRY" = 1 ]; then
        gate_note "remove $ROOT_ENV $WRITER_ENV $FOLLOWER_ENV $STATE and $CRED/tmp; then check each is gone"
        rsh "! podman container exists $NAME && ! test -e $RDIR && ! ss -ltn | grep -q ':$RPORT '"
        return 0
    fi
    rm -f "$ROOT_ENV" "$WRITER_ENV" "$FOLLOWER_ENV" "$STATE"; rm -rf "$CRED/tmp"
    local bad=0
    chk() { if eval "$2" >/dev/null 2>&1; then printf 'PASS  %s\n' "$1"; else printf 'FAIL  %s\n' "$1"; bad=1; fi; }
    chk "container $NAME gone on $HOST" "${SSH[*]} $HOST '! podman container exists $NAME'"
    chk "remote dir $RDIR gone" "${SSH[*]} $HOST '! test -e $RDIR'"
    chk "remote port $RPORT closed" "${SSH[*]} $HOST '! ss -ltn | grep -q :$RPORT\\ '"
    chk "local forward port $FPORT closed" "! nc -z 127.0.0.1 $FPORT"
    chk "forward control socket gone" "! test -e '$SOCK'"
    chk "no credential file left under $CRED" "! ls '$CRED'/$NAME.* 2>/dev/null | grep -q ."
    [ "$bad" = 0 ] || gate_die "cleanup check failed: do not leave this host until every line above passes"
    gate_say "down verified."
}

case "$CMD" in up) do_up ;; mint-keys) do_mint ;; status) do_status ;; down) do_down ;; esac
