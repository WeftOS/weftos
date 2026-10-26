#!/bin/bash
# Run on the Ventuno Q with the cogs built in /data/build/pluto-cog and the throwaway broker
# (pluto_demo.rs, an example of weftos-cogbus; it needs `sdr.v1` added to the registered schema ids
# of clawft-types until WeftOS registers it) built in /data/build/npu-infer/target-weftos. Receive only.
# Pluto soak: phase 1 = one hour of 2.4 GHz at 5 MS/s; phase 2 = a band plan cycling 2.4 GHz,
# 1090 MHz and 433/915 MHz for $P2_SECS. Receive only. Results go to /data/build/pluto-soak/run-*/.
set -u
D=/data/build/pluto-soak
RUN=$D/run-$(date +%Y%m%d-%H%M%S)
mkdir -p $RUN $D/bin
cp /data/build/pluto-cog/target/release/cog-sdr-pluto-rx $D/bin/cog-sdr-pluto-rx
[ -x /data/build/pluto-cog/target/release/cog-spectrum ] && cp /data/build/pluto-cog/target/release/cog-spectrum $D/bin/cog-spectrum
echo $RUN > $D/current-run
P1_SECS=${P1_SECS:-3600}
P2_SECS=${P2_SECS:-21600}
SOCK=$RUN/cogbus.sock
RXTOK=$(printf 'r%.0s' $(seq 64))
SPTOK=$(printf 's%.0s' $(seq 64))
HW=/sys/class/hwmon/hwmon4

# board-side logger: INA232 power, bus voltage, current, load, cog CPU and memory, Pluto on the bus
( while true; do
    ts=$(date +%s)
    p=$(cat $HW/power1_input 2>/dev/null); v=$(cat $HW/in1_input 2>/dev/null); c=$(cat $HW/curr1_input 2>/dev/null)
    usb=$(lsusb 2>/dev/null | grep -c "0456:b673")
    ping1=$(ping -c1 -W1 192.168.2.1 >/dev/null 2>&1 && echo 1 || echo 0)
    load=$(cut -d" " -f1 /proc/loadavg)
    cog=$(ps -C cog-sdr-pluto-rx -o %cpu=,rss= 2>/dev/null | head -1 | tr -s " ")
    spc=$(ps -C cog-spectrum -o %cpu= 2>/dev/null | head -1 | tr -d " ")
    cam=$(pgrep -fc "v4l2src|ffmpeg.*video|gst-launch" 2>/dev/null)
    echo "$ts,$p,$v,$c,$usb,$ping1,$load,\"$cog\",$spc,$cam" >> $RUN/env.csv
    sleep 10
  done ) &
LOGGER=$!
echo "ts,power_uW,bus_mV,current_mA,pluto_usb,pluto_ping,load1,cog_cpu_rss,spectrum_cpu,camera_procs" > $RUN/env.csv.hdr

# the throwaway broker with the independent IQ sink, for the whole run
RX_TOKEN=$RXTOK SPEC_TOKEN=$SPTOK RUN_SECS=$((P1_SECS+P2_SECS+120)) \
  /data/build/npu-infer/target-weftos/release/examples/pluto_demo $SOCK > $RUN/broker.log 2>&1 &
BROKER=$!
sleep 3

run_cog() { # name secs plan
  local name=$1 secs=$2 plan=$3
  echo "$(date +%s) start $name plan=$plan secs=$secs" >> $RUN/events.log
  WEFTOS_COGBUS=$SOCK WEFTOS_COGBUS_TOKEN=$RXTOK timeout -s INT $secs $D/bin/cog-sdr-pluto-rx \
    --uri ip:192.168.2.1 --no-store --interval 10 --band-plan "$plan" --api-bind 127.0.0.1:8063 \
    > $RUN/$name-cog.out 2> $RUN/$name-cog.err
  echo "$(date +%s) end $name rc=$?" >> $RUN/events.log
}

if [ -x $D/bin/cog-spectrum ]; then
  PRINT_SECS=60 WEFTOS_COGBUS=$SOCK WEFTOS_COGBUS_TOKEN=$SPTOK $D/bin/cog-spectrum --no-store --interval 60 --api-bind 127.0.0.1:8064 > $RUN/spectrum.out 2> $RUN/spectrum.err &
  SPEC=$!
fi
[ "$P1_SECS" -gt 0 ] && run_cog phase1 $P1_SECS "2437e6:5e6:1000"
run_cog phase2 $P2_SECS "2437e6:5e6:8000,1090e6:2.4e6:6000,433.92e6:2e6:5000,915e6:2e6:5000"
[ -n "${SPEC:-}" ] && kill -INT $SPEC 2>/dev/null
sleep 2
kill $LOGGER $BROKER 2>/dev/null
echo "$(date +%s) all done" >> $RUN/events.log
touch $RUN/DONE
