#!/bin/bash
# Board-side sampler for the NPU soak. Appends one CSV row every PERIOD seconds:
#   time, MemAvailable, MemFree, geniex process count and summed RSS, cog RSS,
#   input power (INA232 hwmon power1_input, uW), hottest thermal zone, load1,
#   oom_kill counter from /proc/vmstat.
# Reads sysfs and /proc only; it opens no device. Run it on the Ventuno:
#   OUT=/data/build/npu-soak/monitor.csv PERIOD=10 npu-soak-monitor.sh &
OUT=${OUT:-/data/build/npu-soak/monitor.csv}
PERIOD=${PERIOD:-10}
mkdir -p "$(dirname "$OUT")"
hw=""
for d in /sys/class/hwmon/hwmon*; do
  [ "$(cat "$d/name" 2>/dev/null)" = ina232 ] && hw=$d && break
done
[ -f "$OUT" ] || echo "ts,mem_avail_mib,mem_free_mib,geniex_n,geniex_rss_mib,cog_rss_mib,power_uw,temp_max_mc,load1,oom_kill" > "$OUT"
rss_sum() { # rss_sum <process name>
  local n=0 kb=0 p v
  for p in $(pgrep -x "$1"); do
    v=$(awk '/^VmRSS:/{print $2}' "/proc/$p/status" 2>/dev/null)
    [ -n "$v" ] && kb=$((kb + v)) && n=$((n + 1))
  done
  echo "$n $((kb / 1024))"
}
while true; do
  avail=$(awk '/^MemAvailable:/{print int($2/1024)}' /proc/meminfo)
  free=$(awk '/^MemFree:/{print int($2/1024)}' /proc/meminfo)
  read -r gn grss <<< "$(rss_sum geniex)"
  read -r _ crss <<< "$(rss_sum cog-npu-infer)"
  pw=$([ -n "$hw" ] && cat "$hw/power1_input" 2>/dev/null || echo "")
  tmax=$(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | sort -n | tail -1)
  load=$(cut -d' ' -f1 /proc/loadavg)
  oom=$(awk '/^oom_kill /{print $2}' /proc/vmstat)
  echo "$(date -u +%FT%TZ),$avail,$free,$gn,$grss,$crss,$pw,$tmax,$load,$oom" >> "$OUT"
  sleep "$PERIOD"
done
