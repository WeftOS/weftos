#!/bin/bash
# Ventuno Q NPU model suite: ~/llm eval packs over the GenieX OpenAI endpoint (SSH tunnel :18181).
# Usage: VENQ_HOST=user@board MODELS_LIST="m1 m2" [PACKS_LIST=...] suite.sh <log>
cd ~/llm || exit 1
EP=http://127.0.0.1:18181/v1
LOG=${1:?log}
read -r -a MODELS <<< "${MODELS_LIST:?}"
read -r -a PACKS <<< "${PACKS_LIST:-speed instruct math code tool_use long_context}"
temps() { ssh "${VENQ_HOST:-arduino@ventunoq}" 'paste -sd" " <(for z in /sys/class/thermal/thermal_zone*/temp; do echo $(( $(cat $z)/1000 )); done | sort -n | tail -3)'; }
for m in "${MODELS[@]}"; do
  echo "=== $m start $(date -u +%H:%M:%SZ) temps(C)=$(temps)" | tee -a "$LOG"
  for p in "${PACKS[@]}"; do
    bin/eval "$m" --pack "$p" --endpoint "$EP" 2>&1 | grep -E "^\[eval\] .*(accuracy|tok/s)" | tee -a "$LOG"
  done
  echo "=== $m end $(date -u +%H:%M:%SZ) temps(C)=$(temps)" | tee -a "$LOG"
done
echo SUITE-DONE | tee -a "$LOG"
