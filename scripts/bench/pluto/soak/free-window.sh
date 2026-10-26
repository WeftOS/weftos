#!/bin/bash
# Tests that need the Pluto to themselves (the soak is paused): backends, the FIR path, loopback
# reset on taking ownership, and short captures for the replay fixtures. Receive only; the one
# loopback test sets the chip's digital loopback at -89.75 dB TX attenuation, as agreed.
cd /data/build/pluto-soak
B=/data/build/pluto-cog/target/release/cog-sdr-pluto-rx
OUT=/data/build/pluto-soak/free-window; mkdir -p $OUT
U=ip:192.168.2.1
show() { python3 -c "
import sys,json
d=json.loads(sys.stdin.read())
if d.get('status')=='no_source': print('no_source',d.get('error')); sys.exit()
s=d['samples']; c=d['counters']
print('backend:',d['backend'])
print('band:',d['band']['centre_hz'],d['band']['rate_sps'],'hw rate',d['hw']['rate_sps'],'bw',d['hw']['bw_hz'],'gain',d['hw']['gain_db'],d['hw']['gain_mode'])
print('samples: delivered',s['delivered'],'expected',s['expected'],'lost',s['lost'],'pending',s['pending_unconfirmed'],'gaps',c['gap_events'],'zero_blocks',c['zero_blocks'],'dead',c['dead_receive_path'])
print('safe:',d['safe_state'])
print('temps:',d['hw']['temp_phy_c'],d['hw']['temp_zynq_c'],'power dBFS',round(d['power_dbfs'],1))
"; }
echo "### 1. backend auto (measures ip: and usb:)"
$B --once --window 3 --no-store 2>$OUT/auto.err | tee $OUT/auto.json | show; grep backend $OUT/auto.err
echo "### 2. backend usb:"
$B --once --window 3 --backend usb 2>/dev/null | tee $OUT/usb.json | show
echo "### 3. low rates through the FIR (1 MS/s dec 4, 2 MS/s dec 2, 600 kS/s)"
for r in 2e6 1e6 6e5; do echo "-- rate $r"; $B --once --window 3 --uri $U --rate $r --centre-hz 433.92e6 2>/dev/null | show; done
echo "### 4. loopback reset on taking ownership"
iio_attr -u $U -D ad9361-phy loopback 1 >/dev/null; echo "loopback set to: $(iio_attr -u $U -D ad9361-phy loopback)"
$B --once --window 2 --uri $U 2>/dev/null | show
echo "loopback now: $(iio_attr -u $U -D ad9361-phy loopback)"
echo "### 5. gain modes"
$B --once --window 2 --uri $U --gain 20 2>/dev/null | show | grep -E "band|power"
$B --once --window 2 --uri $U --gain 60 2>/dev/null | show | grep -E "band|power"
echo "### 6. fixtures: one 65536-sample read each, receive only"
mkdir -p $OUT/fx
cap() { name=$1; shift; $B --once --window 1 --uri $U "$@" --record $OUT/fx/$name.full 2>/dev/null >/dev/null; head -c 262176 $OUT/fx/$name.full > $OUT/fx/$name.iq; rm -f $OUT/fx/$name.full; ls -la $OUT/fx/$name.iq; }
cap adsb-1090-2M4 --centre-hz 1.09e9 --rate 2.4e6 --gain 50
cap ism-433-2M --centre-hz 433.92e6 --rate 2e6 --gain 50
cap fm-98-5M --centre-hz 98e6 --rate 5e6 --gain 40
echo "### 7. state after"
iio_attr -u $U -D ad9361-phy loopback; iio_attr -u $U -o -c ad9361-phy voltage0 hardwaregain; iio_attr -u $U -c ad9361-phy voltage0 filter_fir_en
