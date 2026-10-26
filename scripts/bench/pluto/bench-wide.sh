#!/bin/bash
# Pluto wide-range bench (AD9364 mode): 70-6000 MHz sweep with per-band noise floor and
# known-source checks (FM broadcast, ADS-B, 2.4/5.8 GHz Wi-Fi). Receive only.
# Usage: scripts/bench/pluto/bench-wide.sh [out-dir]. PLUTO_URI defaults to usb:.
# macOS strips DYLD_* for /bin/bash, so a source-built libiio is found here (LIBIIO_PREFIX).
P=${LIBIIO_PREFIX:-$HOME/.local/libiio}; [ -d "$P/lib" ] && export PATH="$P/bin:$PATH" DYLD_LIBRARY_PATH="$P/lib" LD_LIBRARY_PATH="$P/lib"
U=${PLUTO_URI:-usb:}; D=${1:-${TMPDIR:-/tmp}/pluto-bench-wide}; mkdir -p "$D"
a() { iio_attr -u $U "$@" 2>&1; }
cap() { iio_readdev -u $U -b 65536 -s ${2:-262144} cf-ad9361-lpc voltage0 voltage1 > "$1" 2>/dev/null; }
# floor = median FFT bin (dBFS/bin), peak = strongest bin above it, n_peaks = bins > floor+15 dB
stat() { python3 -I -c "
import numpy as n,sys
x=n.fromfile(sys.argv[1],dtype='<i2').astype(n.float32)
z=x[0::2]+1j*x[1::2]; z-=z.mean(); N=4096; m=z.size//N
s=n.mean(n.abs(n.fft.fft(z[:m*N].reshape(m,N)*n.hanning(N),axis=1))**2,axis=0)
s=n.fft.fftshift(s)/(N*2048.0)**2; db=10*n.log10(s+1e-20); fl=n.median(db)
print('floor %.1f dB/bin, peak +%.1f dB, bins>+15dB %d, rms %.1f dBFS' % (fl, db.max()-fl, int((db>fl+15).sum()), 10*n.log10(n.mean(n.abs(z)**2)/2048**2+1e-12)))" "$1"; }
pulses() { python3 -I -c "
import numpy as n,sys
x=n.fromfile(sys.argv[1],dtype='<i2').astype(n.float32)
m=n.abs(x[0::2]+1j*x[1::2]); med=n.median(m); hi=m>6*med
edges=int(n.sum(hi[1:]&~hi[:-1])); print('pulse edges >6x median: %d in %.2f s' % (edges, m.size/float(sys.argv[2])))" "$1" "$2"; }
setrx() { a -i -c ad9361-phy voltage0 sampling_frequency $1 >/dev/null; a -i -c ad9361-phy voltage0 rf_bandwidth $2 >/dev/null; }
tune() { a -c ad9361-phy altvoltage0 frequency $1 >/dev/null; sleep 0.05; a -c ad9361-phy altvoltage0 frequency; }

echo "== mode: $(iio_info -u $U 2>/dev/null | grep -o 'ad9361-phy,model: .*'), LO range $(a -o -c ad9361-phy altvoltage0 frequency_available)"
a -i -c ad9361-phy voltage0 gain_control_mode manual >/dev/null
a -i -c ad9361-phy voltage0 hardwaregain 50 >/dev/null
echo "== sweep (4 MS/s, 4 MHz BW, manual 50 dB)"
setrx 4000000 4000000
for f in 70 100 150 250 325 433 600 868 915 1090 1200 1575 1900 2140 2437 2600 3000 3500 3800 4200 4900 5200 5500 5800 6000; do
  got=$(tune ${f}000000); cap $D/w$f.bin
  printf "%5s MHz  LO %-11s %s\n" $f "$got" "$(stat $D/w$f.bin)"
done
echo "== limits"
for f in 60 6100; do echo "$f MHz -> $(a -c ad9361-phy altvoltage0 frequency ${f}000000 | tail -1)"; done
echo "== FM broadcast (10 MS/s, 10 MHz BW, centres 93 and 103 MHz)"
setrx 10000000 10000000; a -i -c ad9361-phy voltage0 hardwaregain 30 >/dev/null
for f in 93 103; do tune ${f}000000 >/dev/null; cap $D/fm$f.bin 1048576; echo "$f MHz: $(stat $D/fm$f.bin)"; done
echo "== ADS-B 1090 MHz (2.5 MS/s, 2 s, manual 60 dB)"
setrx 2500000 2000000; a -i -c ad9361-phy voltage0 hardwaregain 60 >/dev/null
tune 1090000000 >/dev/null; cap $D/adsb.bin 5000000; echo "$(pulses $D/adsb.bin 2500000); $(stat $D/adsb.bin)"
tune 1030000000 >/dev/null; cap $D/adsb-ref.bin 5000000; echo "1030 MHz reference: $(pulses $D/adsb-ref.bin 2500000)"
echo "== Wi-Fi (20 MS/s, 20 MHz BW, manual 30 dB; burst capture)"
setrx 20000000 20000000; a -i -c ad9361-phy voltage0 hardwaregain 30 >/dev/null
for f in 2412 2437 2462 5180 5240 5745 5785 5825; do tune ${f}000000 >/dev/null; cap $D/wifi$f.bin 1048576; echo "$f MHz: $(stat $D/wifi$f.bin)"; done
echo "== loopback TX at 5800 MHz (digital loopback, TX atten -89.75 dB)"
setrx 4000000 4000000
a -o -c ad9361-phy voltage0 hardwaregain -89.75 >/dev/null
a -c ad9361-phy altvoltage1 frequency 5800000000 >/dev/null; tune 5800000000 >/dev/null
a -D ad9361-phy loopback 1 >/dev/null
a -c cf-ad9361-dds-core-lpc altvoltage0 frequency 300000 >/dev/null; a -c cf-ad9361-dds-core-lpc altvoltage0 scale 0.25 >/dev/null
a -c cf-ad9361-dds-core-lpc altvoltage1 frequency 300000 >/dev/null; a -c cf-ad9361-dds-core-lpc altvoltage1 phase 90000 >/dev/null
a -c cf-ad9361-dds-core-lpc altvoltage1 scale 0.25 >/dev/null; a -c cf-ad9361-dds-core-lpc altvoltage0 raw 1 >/dev/null
cap $D/loop58.bin; echo "loopback: $(stat $D/loop58.bin)"
a -c cf-ad9361-dds-core-lpc altvoltage0 raw 0 >/dev/null; a -D ad9361-phy loopback 0 >/dev/null
echo "tx gain $(a -o -c ad9361-phy voltage0 hardwaregain), loopback $(a -D ad9361-phy loopback)"
echo "== health: ad9364 temp $(a -i -c ad9361-phy temp0 input) mC; xadc temp raw $(a -c xadc temp0 raw)"
