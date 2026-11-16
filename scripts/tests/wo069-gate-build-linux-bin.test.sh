#!/usr/bin/env bash
# Test for scripts/store/wo069-gate-build-linux-bin.sh (WO-069 lane G2, block 3). Fake ssh, scp, rsync, podman and curl on PATH (and fake
# ci-push/ci-run/ci-wait in WO069_GATE_CI_DIR) record any call and exit 99; every --help, plan and --dry-run run must record none; every remote
# subcommand without WO069_OWNER_GO_BLOCK3=1 must refuse (rc 3) and record none; the production host is refused. No script is run for real.
# shellcheck disable=SC2034,SC2154  # variables are read inside the eval strings of ok()
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
S="$REPO/scripts/store/wo069-gate-build-linux-bin.sh"
mkdir -p "$REPO/target"; T=$(mktemp -d "$REPO/target/wo069-g2-test.XXXXXX"); trap 'rm -rf "$T"' EXIT
pass=0; fail=0
ok() { if eval "$2"; then pass=$((pass + 1)); else fail=$((fail + 1)); echo "FAIL: $1"; fi; }

mkdir -p "$T/fakebin" "$T/ci" "$T/bin" "$T/fakehome"
for t in ssh scp curl podman rsync; do printf '#!/bin/sh\necho "$0 $*" >>"%s/touched"\nexit 99\n' "$T" >"$T/fakebin/$t"; chmod +x "$T/fakebin/$t"; done
for t in ci-push.sh ci-run.sh ci-wait.sh; do printf '#!/bin/sh\necho "$0 $*" >>"%s/touched"\nexit 99\n' "$T" >"$T/ci/$t"; chmod +x "$T/ci/$t"; done
export PATH="$T/fakebin:$PATH" HOME="$T/fakehome" WO069_GATE_CI_DIR="$T/ci" WO069_GATE_BIN_DIR="$T/bin"
unset WO069_OWNER_GO_BLOCK3 WO069_GATE_CI_HOST
touched() { [ -e "$T/touched" ]; }
reset() { rm -f "$T/touched"; }

ok "script exists, under 500 lines, bash -n" '[ -f "$S" ] && [ "$(wc -l <"$S")" -lt 500 ] && bash -n "$S"'
ok "this test is under 500 lines" '[ "$(wc -l <"$0")" -lt 500 ]'
if command -v shellcheck >/dev/null 2>&1; then ok "shellcheck -S warning clean" 'shellcheck -S warning "$S" "$0"'; else echo "(shellcheck not installed: skipped)"; fi

# --help and plan: exit 0, usage printed, nothing touched
reset; out=$("$S" --help 2>&1); rc=$?
ok "--help exits 0 and prints usage" '[ $rc = 0 ] && echo "$out" | grep -qi "^usage"'
ok "--help touched nothing" '! touched'
reset; out=$("$S" plan 2>&1); rc=$?
ok "plan exits 0" '[ $rc = 0 ]'
ok "plan records zero calls" '! touched'
reset; out_default=$("$S" 2>&1); rc=$?
ok "no argument is plan (exit 0, zero calls)" '[ $rc = 0 ] && ! touched && echo "$out_default" | grep -q "block 3 plan"'
for want in 'ci-push.sh wo069-fh' 'ci-run.sh wo069-fh build weftos' 'cargo test -p weftos-store --features rustfs --release --test s2_follower_half --no-run --locked' 'ci-wait.sh wo069-fh build' \
            'scp ' 'rm -rf ~/ci-jobs/wo069-fh' 'WO069_GATE_FH_LINUX_BIN' 'glibc' 'Debian trixie' 'estimate, not measured' '/target/wo069-fh/s2_follower_half' 's2_follower_half.linux-x86_64'; do
    ok "plan mentions: $want" 'echo "$out" | grep -qF -- "$want"'
done

# every subcommand with --dry-run and the go unset: exit 0, zero calls, prints DRY lines
for sub in push build "build --no-wait" wait fetch clean; do
    reset; out=$("$S" --dry-run $sub 2>&1); rc=$?
    ok "--dry-run $sub exits 0" '[ $rc = 0 ]'
    ok "--dry-run $sub records zero calls" '! touched'
    ok "--dry-run $sub prints a DRY line or a plan line" 'echo "$out" | grep -Eq "^(DRY:|build:|push:|started)"'
    reset; out=$("$S" $sub --dry-run 2>&1); rc=$?
    ok "$sub --dry-run (flag after) exits 0, zero calls" '[ $rc = 0 ] && ! touched'
done
reset; out=$("$S" --dry-run push --ref HEAD 2>&1); rc=$?
ok "--dry-run push --ref HEAD resolves a commit" '[ $rc = 0 ] && echo "$out" | grep -Eq "commit [0-9a-f]{40}" && ! touched'
reset; out=$("$S" --dry-run push --ref no-such-ref-xyz 2>&1); rc=$?
ok "--dry-run push with an unknown ref shows it, no call" '[ $rc = 0 ] && echo "$out" | grep -q "not found" && ! touched'
reset; out=$("$S" push --ref no-such-ref-xyz 2>&1); rc=$?
ok "push (real) refuses before the ref is even looked up" '[ $rc = 3 ] && ! touched'

# the owner go: no go, no --dry-run -> non-zero (3), a message naming block 3, and no recorded call
for sub in push build "build --no-wait" wait fetch clean; do
    reset; out=$("$S" $sub 2>&1); rc=$?
    ok "$sub without WO069_OWNER_GO_BLOCK3 refuses with rc 3" '[ $rc = 3 ]'
    ok "$sub refusal names block 3 and the owner" 'echo "$out" | grep -qi "block 3" && echo "$out" | grep -qi "owner"'
    ok "$sub refusal records zero calls" '! touched'
    reset; out=$(WO069_OWNER_GO_BLOCK3=0 "$S" $sub 2>&1); rc=$?
    ok "$sub with WO069_OWNER_GO_BLOCK3=0 still refuses" '[ $rc = 3 ] && ! touched'
done

# the production host
for h in photo-gallery aepod@photo-gallery 100.90.170.87 root@100.90.170.87:22 pg; do
    for sub in "" "--dry-run push" "--dry-run fetch" "--dry-run clean"; do
        reset; out=$(WO069_GATE_CI_HOST=$h "$S" $sub 2>&1); rc=$?
        ok "host '$h' ${sub:-plan} refused (rc 4)" '[ $rc = 4 ] && echo "$out" | grep -qi "production" && ! touched'
    done
done
reset; out=$(WO069_OWNER_GO_BLOCK3=1 WO069_GATE_CI_HOST=photo-gallery "$S" clean 2>&1); rc=$?
ok "production host refused even with the go set" '[ $rc = 4 ] && ! touched'
reset; out=$(WO069_OWNER_GO_BLOCK3=1 WO069_GATE_CI_HOST=other@somewhere "$S" build 2>&1); rc=$?
ok "build refuses a host the ci scripts do not use" '[ $rc != 0 ] && echo "$out" | grep -q "hard-code" && ! touched'

# the destination must be on the notes volume
reset; out=$(WO069_GATE_BIN_DIR=/tmp/x "$S" --dry-run fetch 2>&1); rc=$?
ok "fetch refuses a /tmp destination" '[ $rc != 0 ] && ! touched'
reset; out=$(WO069_GATE_BIN_DIR="$HOME/bin" "$S" --dry-run fetch 2>&1); rc=$?
ok "fetch refuses a home-directory destination" '[ $rc != 0 ] && ! touched'

# elf-check is local only: synthetic headers
printf '\177ELF\002\001\001\000\000\000\000\000\000\000\000\000\003\000\076\000rest' >"$T/x86.elf"
printf '\177ELF\002\001\001\000\000\000\000\000\000\000\000\000\003\000\267\000rest' >"$T/arm.elf"
printf 'MZ not an elf' >"$T/win.exe"; : >"$T/empty"
reset
ok "elf-check accepts an x86-64 ELF header" '"$S" elf-check "$T/x86.elf" >/dev/null 2>&1'
ok "elf-check rejects aarch64, a PE file, an empty file and a missing file" '! "$S" elf-check "$T/arm.elf" >/dev/null 2>&1 && ! "$S" elf-check "$T/win.exe" >/dev/null 2>&1 && ! "$S" elf-check "$T/empty" >/dev/null 2>&1 && ! "$S" elf-check "$T/none" >/dev/null 2>&1'
ok "elf-check made no remote call" '! touched'

# unknown argument
out=$("$S" --bogus 2>&1); rc=$?
ok "unknown argument is an error" '[ $rc != 0 ]'

echo "passed $pass, failed $fail"
[ "$fail" = 0 ]
