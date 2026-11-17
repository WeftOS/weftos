#!/bin/bash
# Usage: run-job.sh <lane> <job> <repo-kind: tapestry|weftos> <shell command run in /src>
# Runs one job in a rootless podman container, at most $CI_SLOTS (default 6) heavy jobs at once (slot locks).
# Code: ~/ci-jobs/<lane>/src (from ci-push.sh). Target: ~/ci-jobs/<lane>/target. Log: ~/ci-jobs/<lane>/logs/<job>.log
set -u
LANE=$1 JOB=$2 KIND=$3; shift 3; CMD="$*"
J=$HOME/ci-jobs; L=$J/$LANE; mkdir -p $L/logs $L/target $L/home $J/cargo-home-$KIND
LOG=$L/logs/$JOB.log; ST=$L/logs/$JOB.status
IMG=${CI_IMAGE:-localhost/tapestry/build:1.95-ci-wasm}
echo running > $ST
while true; do
  for s in $(seq 1 ${CI_SLOTS:-6}); do
    exec {fd}>$J/slot$s.lock
    if flock -n $fd; then SLOT=$s; break 2; fi
    exec {fd}>&-
  done
  echo "waiting for a slot $(date -Is)" >> $LOG; sleep 20
done
echo "slot $SLOT start $(date -Is) cmd: $CMD" >> $LOG
L0=$(wc -l < $LOG)
S0=$(date +%s)
podman run --rm --init --userns=keep-id --cpus ${CI_CPUS:-8} --memory ${CI_MEM:-20g} --pids-limit 8192 \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e PROTOC=/usr/bin/protoc -e HOME=/home/ci -e CARGO_BUILD_JOBS=${CI_CPUS:-8} \
  ${CI_EXTRA_ARGS:-} \
  -v $L/src:/src -v $L/target:/target -v $J/cargo-home-$KIND:/cargo -v $L/home:/home/ci -w /src \
  $IMG bash -o pipefail -c "$CMD" >> $LOG 2>&1
RC=$?
# 2026-11-17 (backend:3): a job like `cargo test ...; echo RC $?` exits 0 even when cargo failed.
# Scan this run's part of the log for cargo/test failure markers; record a non-zero status if any appear.
NOTE=""
if [ $RC -eq 0 ]; then
  HIT=$(tail -n +$((L0+1)) $LOG | grep -m1 -E "could not compile|test result: FAILED|^error: test failed|^error: [0-9]+ targets? failed|^error: process didn.t exit successfully|^RC [A-Za-z0-9_.:-]+ [1-9][0-9]*$")
  if [ -n "$HIT" ]; then RC=1; NOTE=" (container exit 0, but the log shows a failure: $HIT)"; fi
fi
echo "exit $RC after $(( $(date +%s)-S0 ))s $(date -Is)$NOTE" >> $LOG
echo "exit $RC$NOTE" > $ST
exit $RC
