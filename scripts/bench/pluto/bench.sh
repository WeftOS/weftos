#!/bin/bash
# Pluto extended bench: LO sweep, rate/throughput, health, digital-loopback TX.
# Usage: scripts/bench/pluto/bench.sh [out-dir]. PLUTO_URI defaults to usb:; libiio tools must be on PATH.
# TX stays at max attenuation (-89.75 dB) and loopback is digital (no RF path).
U=${PLUTO_URI:-usb:}; D=${1:-${TMPDIR:-/tmp}/pluto-bench}; mkdir -p "$D"
a() { iio_attr -u $U "$@" 2>&1; }
pw() { python3 -I -c "
import numpy as n,sys
x=n.fromfile(sys.argv[1],dtype='<i2').astype(n.float32)
if x.size<4: print('nodata'); sys.exit()
i,q=x[0::2],x[1::2]; z=i+1j*q; z-=z.mean()
p=n.mean(n.abs(z)**2); print('%.1f dBFS' % (10*n.log10(p/2048**2+1e-12)))" "$1"; }
peak() { python3 -I -c "
import numpy as n,sys
fs=float(sys.argv[2]); x=n.fromfile(sys.argv[1],dtype='<i2').astype(n.float32)
z=(x[0::2]+1j*x[1::2])[:65536]; z-=z.mean()
s=n.abs(n.fft.fftshift(n.fft.fft(z*n.hanning(z.size))))**2
f=n.fft.fftshift(n.fft.fftfreq(z.size,1/fs)); k=s.argmax()
print('peak %.1f kHz, %.1f dB above median' % (f[k]/1e3, 10*n.log10(s[k]/n.median(s))))" "$1" "$2"; }

echo "== health"
for c in temp0; do echo "xadc $c raw=$(a -c xadc $c raw) scale=$(a -c xadc $c scale) offset=$(a -c xadc $c offset)"; done
echo "ad9361 temp0 input=$(a -i -c ad9361-phy temp0 input)"
for c in voltage0 voltage1 voltage2; do echo "xadc $c raw=$(a -c xadc $c raw) scale=$(a -c xadc $c scale)"; done

echo "== LO sweep (fs 4 MS/s, gain manual 40 dB)"
a -i -c ad9361-phy voltage0 sampling_frequency 4000000 >/dev/null
a -i -c ad9361-phy voltage0 gain_control_mode manual >/dev/null
a -i -c ad9361-phy voltage0 hardwaregain 40 >/dev/null
for f in 325 433 868 915 1090 1575 1800 2140 2437 2600 3500 3800; do
  a -c ad9361-phy altvoltage0 frequency ${f}000000 >/dev/null
  got=$(a -c ad9361-phy altvoltage0 frequency)
  iio_readdev -u $U -b 65536 -s 262144 cf-ad9361-lpc voltage0 voltage1 > $D/lo$f.bin 2>/dev/null
  echo "$f MHz -> LO $got, rssi $(a -i -c ad9361-phy voltage0 rssi), $(pw $D/lo$f.bin)"
done
echo "== out-of-range requests (expect clamp or refusal)"
for f in 70 300 3900 6000; do
  r=$(a -c ad9361-phy altvoltage0 frequency ${f}000000)
  echo "$f MHz request -> $(echo "$r" | tail -1) ; LO now $(a -c ad9361-phy altvoltage0 frequency)"
done

echo "== rate / USB throughput (20 M samples per rate)"
a -c ad9361-phy altvoltage0 frequency 2437000000 >/dev/null
for fs in 2083333 5000000 10000000 20000000 30720000 61440000; do
  a -i -c ad9361-phy voltage0 sampling_frequency $fs >/dev/null
  t0=$(python3 -c 'import time;print(time.time())')
  iio_readdev -u $U -b 1048576 -s 20000000 cf-ad9361-lpc voltage0 voltage1 > $D/rate.bin 2>$D/rate.err; rc=$?
  t1=$(python3 -c 'import time;print(time.time())')
  echo "fs=$fs set=$(a -i -c ad9361-phy voltage0 sampling_frequency) rc=$rc bytes=$(wc -c < $D/rate.bin) wall=$(python3 -c "print(round($t1-$t0,2))")s eff=$(python3 -c "print(round(20e6/($t1-$t0)/1e6,2))")MS/s err=$(head -c 120 $D/rate.err)"
done

echo "== TX via digital loopback (TX atten -89.75 dB, DDS tone 200 kHz)"
a -i -c ad9361-phy voltage0 sampling_frequency 4000000 >/dev/null
a -o -c ad9361-phy voltage0 hardwaregain -89.75 >/dev/null
a -c ad9361-phy altvoltage1 frequency 2437000000 >/dev/null
a -D ad9361-phy loopback 1 >/dev/null
for c in altvoltage0 altvoltage1 altvoltage2 altvoltage3; do a -c cf-ad9361-dds-core-lpc $c raw 0 >/dev/null; done
a -c cf-ad9361-dds-core-lpc altvoltage0 frequency 200000 >/dev/null
a -c cf-ad9361-dds-core-lpc altvoltage0 scale 0.25 >/dev/null
a -c cf-ad9361-dds-core-lpc altvoltage1 frequency 200000 >/dev/null
a -c cf-ad9361-dds-core-lpc altvoltage1 phase 90000 >/dev/null
a -c cf-ad9361-dds-core-lpc altvoltage1 scale 0.25 >/dev/null
a -c cf-ad9361-dds-core-lpc altvoltage0 raw 1 >/dev/null
iio_readdev -u $U -b 65536 -s 262144 cf-ad9361-lpc voltage0 voltage1 > $D/loop.bin 2>/dev/null
echo "loopback on: $(pw $D/loop.bin); $(peak $D/loop.bin 4000000)"
a -c cf-ad9361-dds-core-lpc altvoltage0 raw 0 >/dev/null
a -D ad9361-phy loopback 0 >/dev/null
iio_readdev -u $U -b 65536 -s 262144 cf-ad9361-lpc voltage0 voltage1 > $D/noloop.bin 2>/dev/null
echo "loopback off: $(pw $D/noloop.bin)"
echo "tx gain now $(a -o -c ad9361-phy voltage0 hardwaregain), loopback $(a -D ad9361-phy loopback)"
echo "== health after"
echo "ad9361 temp0 input=$(a -i -c ad9361-phy temp0 input) xadc temp raw=$(a -c xadc temp0 raw)"
