#!/usr/bin/env bash
# NUCLEO-N657X0-Q (STM32N657, Cortex-M55) harness.
#
# macOS USB stalls the board's STLINK-V3EC (libusb "pipe is stalled"), so the
# probe is passed through OrbStack and probe-rs runs in a linux/arm64 container
# built from scripts/n6/Dockerfile.
#
#   scripts/n6.sh attach        pass the ST-LINK through to OrbStack
#   scripts/n6.sh detach        give it back to macOS
#   scripts/n6.sh probe         list probes and read the core CPUID
#   scripts/n6.sh smoke         end-to-end check (probe, CPUID, RAM, code runs)
#   scripts/n6.sh clock [secs]  measure the CPU clock on HSI and HSE against host time
#   scripts/n6.sh dw3000-id     read the DEV_ID of a DWM3000EVB shield on the Arduino header
#   scripts/n6.sh leaf          WeftOS leaf smoke: RVF segment + leaf-types CBOR on the M55,
#                               segment validated on the host with rvf-wire
#   scripts/n6.sh node [secs]   embassy node: HSE clocks, Ethernet DHCP, UDP 47006 announce;
#                               streams its defmt log and listens for the announce on this host
#   scripts/n6.sh run <elf>     load a RAM-linked ELF into AXISRAM and start it
#   scripts/n6.sh shell         interactive shell with probe-rs available
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
N6_DIR="$REPO/scripts/n6"
# Tag by Dockerfile content so edits rebuild the image.
IMAGE="weftos-n6-probe:$(shasum -a 256 "$N6_DIR/Dockerfile" | cut -c1-12)"
CHIP="STM32N657"
STLINK_VIDPID="0483:3754"

die() { echo "n6: $*" >&2; exit 1; }

stlink_id() { orb usb list 2>/dev/null | awk -v v="$STLINK_VIDPID" '$2 == v { print $1; exit }'; }

cmd_attach() {
  local id; id="$(stlink_id)"
  [[ -n "$id" ]] || die "no ST-LINK ($STLINK_VIDPID) visible to OrbStack; check the USB cable"
  if orb usb list | awk -v i="$id" '$1 == i' | grep -q attached; then
    echo "n6: ST-LINK $id already attached"
  else
    orb usb attach "$id"
  fi
}

cmd_detach() {
  local id; id="$(stlink_id)"
  [[ -n "$id" ]] || die "no ST-LINK visible"
  orb usb detach "$id"
}

ensure_image() {
  docker image inspect "$IMAGE" >/dev/null 2>&1 \
    || docker build -q -t "$IMAGE" "$N6_DIR" >/dev/null
}

# Run a script inside the probe container with the USB bus and repo files mounted.
in_container() {
  ensure_image
  cmd_attach >/dev/null
  docker run --rm -i --privileged \
    -v /dev/bus/usb:/dev/bus/usb \
    -v "$N6_DIR:/work/n6:ro" \
    ${N6_EXTRA_MOUNT:+-v "$N6_EXTRA_MOUNT"} \
    -e CHIP="$CHIP" -e N6_SECS="${N6_SECS:-}" \
    "$IMAGE" bash -euo pipefail -s
}

cmd_probe() {
  in_container <<'EOF'
probe-rs list
echo "CPUID: $(probe-rs read --chip "$CHIP" b32 0xE000ED00 1 | awk '{print $2}')"
EOF
}

cmd_smoke() {
  in_container <<'EOF'
. /work/n6/lib.sh
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; exit 1; }

# 1. The probe opens.
probe-rs list | grep -q "STLink V3" && pass "ST-LINK V3 opens" || fail "no ST-LINK"

# 2. The core answers: CPUID part number 0xD22 is Cortex-M55.
cpuid=$(rd 0xE000ED00)
[[ $(( (0x$cpuid >> 4) & 0xFFF )) -eq $((0xD22)) ]] \
  && pass "CPUID $cpuid (Cortex-M55)" || fail "CPUID $cpuid is not a Cortex-M55"

# 3. AXISRAM write, read back, restore.
scratch=0x341C0000
orig=$(rd $scratch)
probe-rs write --chip "$CHIP" b32 $scratch 0xA5C3F00D
got=$(rd $scratch)
probe-rs write --chip "$CHIP" b32 $scratch 0x$orig
[[ $got == a5c3f00d ]] && pass "AXISRAM write/readback at $scratch (restored 0x$orig)" \
                       || fail "AXISRAM readback $got"

# 4. Code runs: assemble, load into AXISRAM, start it, watch the counter advance.
cd /tmp
arm-none-eabi-as -march=armv8.1-m.main -mthumb -o smoke.o /work/n6/smoke/smoke.S
arm-none-eabi-ld -T /work/n6/smoke/smoke.ld -o smoke.elf smoke.o
counter=0x$(arm-none-eabi-nm smoke.elf | awk '$3 == "counter" {print $1}')
load_and_start smoke.elf >/tmp/load.log   # not piped: VECTORS must reach this shell
sed 's/^/      /' /tmp/load.log
want=$(printf '%08x' "$(arm-none-eabi-nm smoke.elf | awk '$3 == "_estack" { print "0x" $1 }')")
[[ $(rd "$VECTORS") == "$want" ]] && pass "image loaded: vector table at $VECTORS matches the ELF" \
                                  || fail "vector table at $VECTORS reads $(rd "$VECTORS"), expected $want"
a=$(rd $counter); sleep 1; b=$(rd $counter)
if [[ $((0x$b)) -gt $((0x$a)) ]]; then
  pass "program runs from AXISRAM: counter @$counter 0x$a -> 0x$b"
else
  sed 's/^/      /' /tmp/gdbsrv.log | tail -15
  fail "counter @$counter did not advance (0x$a -> 0x$b)"
fi
echo "smoke: all checks passed"
EOF
}

cmd_clock() {
  local secs="${1:-60}"
  [[ "$secs" =~ ^[0-9]+$ ]] || die "usage: n6.sh clock [seconds-per-source]"
  N6_SECS="$secs" in_container <<'EOF'
. /work/n6/lib.sh
cd /tmp
arm-none-eabi-as -march=armv8.1-m.main -mthumb -o smoke.o /work/n6/smoke/smoke.S
arm-none-eabi-ld -T /work/n6/smoke/smoke.ld -o smoke.elf smoke.o
load_and_start smoke.elf >/dev/null   # keep the core running so CYCCNT counts
python /work/n6/clock.py "$N6_SECS" 2> >(grep -v -i "disk devices by id" >&2)
EOF
}

cmd_dw3000_id() {
  in_container <<'EOF'
python /work/n6/dw3000_id.py 2> >(grep -v -i "disk devices by id" >&2)
EOF
}

cmd_leaf() {
  "$REPO/scripts/build.sh" n6-leaf >/dev/null || die "scripts/build.sh n6-leaf failed"
  local out; out="$(mktemp -d)"
  cp "$REPO/crates/weftos-n6-leaf/target/thumbv8m.main-none-eabihf/release/weftos-n6-leaf" "$out/app.elf"
  N6_EXTRA_MOUNT="$out:/work/out" in_container <<'EOF'
. /work/n6/lib.sh
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; exit 1; }
elf=/work/out/app.elf
sym() { arm-none-eabi-nm "$elf" | awk -v s="$1" '$3 == s { print "0x" $1 }'; }
res=$(sym N6_RESULT); seg=$(sym N6_SEGMENT)
words() { probe-rs read --chip "$CHIP" b32 "$1" "$2" | tr ' ' '\n' | grep -E '^[0-9a-f]{8}$'; }

load_and_start "$elf" >/tmp/load.log
sed 's/^/      /' /tmp/load.log
for _ in 1 2 3 4 5 6; do
  mapfile -t r < <(words "$res" 5)
  [[ ${r[0]:-} == 57454654 ]] && break
  sleep 0.5
done
[[ ${r[0]:-} == 57454654 ]] || fail "N6_RESULT magic ${r[0]:-none} (firmware did not finish)"
(( (16#${r[1]} & 3) == 3 )) || fail "status 0x${r[1]} (bit0 RVF segment, bit1 leaf-types CBOR)"
pass "M55 built a $((16#${r[2]}))-byte RVF segment and round-tripped a $((16#${r[3]}))-byte leaf-types CBOR announce"
hb1=$((16#${r[4]})); sleep 0.5; hb2=$((16#$(words "$res" 5 | tail -1)))
(( hb2 > hb1 )) && pass "firmware alive: heartbeat $hb1 -> $hb2" || fail "heartbeat stuck at $hb1"
len=$((16#${r[2]}))
probe-rs read --chip "$CHIP" b8 "$seg" "$len" | tr ' ' '\n' | grep -E '^[0-9a-f]{2}$' | tr -d '\n' \
  | python3 -c 'import sys,binascii; sys.stdout.buffer.write(binascii.unhexlify(sys.stdin.read()))' >/work/out/segment.bin
EOF
  "$REPO/crates/weftos-n6-leaf-hostcheck/target/release/weftos-n6-leaf-hostcheck" "$out/segment.bin"
}

cmd_node() {
  local secs="${1:-40}"
  [[ "$secs" =~ ^[0-9]+$ ]] || die "usage: n6.sh node [seconds]"
  "$REPO/scripts/build.sh" n6-leaf >/dev/null || die "scripts/build.sh n6-leaf failed"
  local elf="$REPO/crates/weftos-n6-node/target/thumbv8m.main-none-eabihf/release/weftos-n6-node"
  # Listen on this host for the node's CBOR announce (UDP broadcast, port 47006).
  python3 - "$secs" <<'PY' &
import socket, sys, time
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("", 47006)); s.settimeout(1.0)
end, n = time.time() + int(sys.argv[1]) + 10, 0
while time.time() < end:
    try:
        data, addr = s.recvfrom(2048)
    except socket.timeout:
        continue
    n += 1
    print(f"host: announce from {addr[0]}:{addr[1]}, {len(data)} bytes CBOR, n6-node={b'n6-node' in data}", flush=True)
print(f"host: {n} announce(s) received on UDP 47006" if n else "host: no announce received on UDP 47006", flush=True)
PY
  local listener=$!
  N6_SECS="$secs" N6_EXTRA_MOUNT="$elf:/work/app.elf:ro" in_container <<'EOF'
. /work/n6/lib.sh
load_and_start /work/app.elf
timeout "$N6_SECS" probe-rs attach --chip "$CHIP" /work/app.elf 2>&1 | grep -v -E 'SIGTERM|Exited by user' || true
EOF
  wait "$listener"
}

cmd_run() {
  local elf="${1:-}"
  [[ -f "$elf" ]] || die "usage: n6.sh run <ram-linked.elf>"
  local abs; abs="$(cd "$(dirname "$elf")" && pwd)/$(basename "$elf")"
  N6_EXTRA_MOUNT="$abs:/work/app.elf:ro" in_container <<'EOF'
. /work/n6/lib.sh
load_and_start /work/app.elf
EOF
}

cmd_shell() {
  ensure_image
  cmd_attach >/dev/null
  docker run --rm -it --privileged -v /dev/bus/usb:/dev/bus/usb \
    -v "$N6_DIR:/work/n6:ro" -e CHIP="$CHIP" "$IMAGE" bash
}

case "${1:-}" in
  attach) cmd_attach ;;
  detach) cmd_detach ;;
  probe)  cmd_probe ;;
  smoke)  cmd_smoke ;;
  clock)  shift; cmd_clock "$@" ;;
  dw3000-id) cmd_dw3000_id ;;
  leaf)   cmd_leaf ;;
  node)   shift; cmd_node "$@" ;;
  run)    shift; cmd_run "$@" ;;
  shell)  cmd_shell ;;
  *) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
