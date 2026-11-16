#!/usr/bin/env bash
# Test for the WO-069 gate scripts (scripts/store/wo069-gate-*): every script answers --help and --dry-run with exit 0 and touches
# nothing (a fake ssh, scp, curl, podman, nc, rsync and caffeinate on PATH record any call and fail it); the PG refusal; the owner-go
# (WO069_OWNER_GO_Q3) refusal; the fh step of the drills harness and the follower-half test file exist and are under 500 lines; the item 3 statistics script prints no path on a synthetic tree. No script is run for real.
# shellcheck disable=SC2034,SC2010  # variables are read inside the eval strings of ok()
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
D="$REPO/scripts/store"
mkdir -p "$REPO/target"; T=$(mktemp -d "$REPO/target/wo069-gate-test.XXXXXX"); trap 'rm -rf "$T"' EXIT
pass=0; fail=0
ok() { if eval "$2"; then pass=$((pass + 1)); else fail=$((fail + 1)); echo "FAIL: $1"; fi; }

# fake tools that must never be called
mkdir -p "$T/fakebin" "$T/fakehome" "$T/cred" "$T/state"
for t in ssh scp curl podman nc rsync caffeinate rustfs weaver; do printf '#!/bin/sh\necho "$0 $*" >>"%s/touched"\nexit 99\n' "$T" >"$T/fakebin/$t"; chmod +x "$T/fakebin/$t"; done
export PATH="$T/fakebin:$PATH" HOME="$T/fakehome"
printf 'MESH_S3_ENDPOINT=http://127.0.0.1:19431\nMESH_S3_ACCESS_KEY=a\nMESH_S3_SECRET_KEY=b\n' >"$T/ok.env"; chmod 600 "$T/ok.env"
printf 'MESH_S3_ENDPOINT=http://100.90.170.87:9489\nMESH_S3_ACCESS_KEY=a\nMESH_S3_SECRET_KEY=b\n' >"$T/pg.env"; chmod 600 "$T/pg.env"
export WO069_GATE_CRED_DIR="$T/cred" WO069_GATE_WRITER_ENV="$T/ok.env" WO069_GATE_FOLLOWER_ENV="$T/ok.env" WO069_GATE_STATE="$T/state" WEFTOS_STORE_TEST_RUSTFS_BIN="$T/fakebin/rustfs" WEFTOS_E2E_WEAVER="$T/fakebin/weaver"
unset WO069_OWNER_GO_Q3
before=$(ls "$REPO/target" | grep -c 'wo069-gate' || true)

scripts=(remote-rustfs item1-two-machines item3-tree-stats item4-leftover item5-failures item6-followers item8-writers block4-xpc)
for n in "${scripts[@]}"; do
    f="$D/wo069-gate-$n.sh"
    ok "$n: file exists, under 500 lines" '[ -f "$f" ] && [ "$(wc -l <"$f")" -lt 500 ]'
    ok "$n: bash -n" 'bash -n "$f"'
    out=$("$f" --help 2>&1); rc=$?
    ok "$n: --help exits 0 and prints usage" '[ $rc = 0 ] && echo "$out" | grep -qi "^usage"'
done
ok "lib and runbook: bash -n, under 500 lines" 'bash -n "$D/wo069-gate-lib.sh" && [ "$(wc -l <"$D/wo069-gate-lib.sh")" -lt 500 ] && [ "$(wc -l <"$REPO/docs/store/wo069-gate-runbook.md")" -lt 500 ]'
ok "follower-half test and the drills harness (fh step): present, under 500 lines, bash -n" '[ -f "$REPO/crates/weftos-store/tests/s2_follower_half.rs" ] && [ "$(wc -l <"$REPO/crates/weftos-store/tests/s2_follower_half.rs")" -lt 500 ] && bash -n "$D/wo069-drills.sh" && bash -n "$D/wo069-drills-lib.sh" && [ "$(wc -l <"$D/wo069-drills-lib.sh")" -lt 500 ] && grep -q "^fh_drill()" "$D/wo069-drills-lib.sh" && grep -q "want fh" "$D/wo069-drills.sh"'
ok "hold proxy parses" 'python3 -c "import ast,sys; ast.parse(open(sys.argv[1]).read())" "$D/wo069-gate-hold-proxy.py"'
if command -v shellcheck >/dev/null 2>&1; then ok "shellcheck -S warning is clean on every gate script and this test" 'shellcheck -S warning "$D"/wo069-gate-*.sh "$0"'; else echo "(shellcheck not installed: skipped)"; fi

dry() { # <label> <expected rc> cmd...: dry run must exit with that rc and not touch anything
    local label=$1 want=$2; shift 2
    out=$("$@" 2>&1); rc=$?
    ok "dry-run $label exits $want" '[ $rc = '"$want"' ]'
}
HOST=aepod@aepod-xpc
for c in up mint-keys status down; do dry "remote-rustfs $c" 0 "$D/wo069-gate-remote-rustfs.sh" $c $HOST --dry-run; ok "remote-rustfs $c: prints DRY lines" 'echo "$out" | grep -q "^DRY:"'; done
up_out=$("$D/wo069-gate-remote-rustfs.sh" up $HOST --dry-run 2>&1); down_out=$("$D/wo069-gate-remote-rustfs.sh" down $HOST --dry-run 2>&1)
ok "remote-rustfs up: --pull=never, loopback port, ssh forward" 'echo "$up_out" | grep -q -- "--pull=never" && echo "$up_out" | grep -q "RUSTFS_ADDRESS=127.0.0.1" && echo "$up_out" | grep -q -- "-L 127.0.0.1:"'
ok "remote-rustfs down: removes container and dir and checks them" 'echo "$down_out" | grep -q "podman rm -f" && echo "$down_out" | grep -q "container exists"'
dry "item1 library" 0 "$D/wo069-gate-item1-two-machines.sh" --follower=library --dry-run
ok "item1 library: shows the clock probe and the second-host poll" 'echo "$out" | grep -q "date +%s%3N" && echo "$out" | grep -q "gate_probe_poll"'
ok "item1 library: shows the follower-half test on both machines (Linux binary copy, podman run in the CI image, writer half with its round, expected head)" 'echo "$out" | grep -q "^DRY: scp .*s2_follower_half" && echo "$out" | grep -q "podman run --rm --pull=never" && echo "$out" | grep -q "s2_follower_half follower_half --exact --ignored" && echo "$out" | grep -q "WEFTOS_FH_ROUND=2" && echo "$out" | grep -q "WEFTOS_FH_EXPECT_HEAD=h2" && echo "$out" | grep -q "writer_half --exact --ignored" && echo "$out" | grep -q "cargo test -p weftos-store --features rustfs --release --test s2_follower_half --no-run"'
ok "item1 library: names the clock re-measure and the latency formula" 'echo "$out" | grep -q "re-measure the clock offset" && echo "$out" | grep -q "seen_ms - offset - published_ms"'
ok "item1 library: the env files are named by path only (no key value in the output)" '! echo "$out" | grep -q "MESH_S3_SECRET_KEY=" && echo "$out" | grep -q "WEFTOS_FH_FOLLOWER_ENV=/rfs/follower.env"'
dry "item1 daemon without the owner go" 3 "$D/wo069-gate-item1-two-machines.sh" --follower=daemon --dry-run
ok "item1 daemon refusal names owner question 3" 'echo "$out" | grep -q "question 3"'
WO069_OWNER_GO_Q3=1 dry "item1 daemon with WO069_OWNER_GO_Q3=1" 0 "$D/wo069-gate-item1-two-machines.sh" --follower=daemon --dry-run
dry "item3" 0 "$D/wo069-gate-item3-tree-stats.sh" --dry-run "$T"
for c in start check finish; do dry "item4 $c" 0 "$D/wo069-gate-item4-leftover.sh" $c --dry-run; done
dry "item4 second-host merger without the owner go" 3 "$D/wo069-gate-item4-leftover.sh" start --merger=second-host --dry-run
for c in writer-kill merger-kill merger-check refresh-kill; do dry "item5 $c" 0 "$D/wo069-gate-item5-failures.sh" $c --dry-run; done
export WO069_GATE_STATE="$REPO/target/b4t"   # block 4's state dir must be short (daemon unix sockets); a dry run never creates it
dry "block4 without the owner go" 3 "$D/wo069-gate-block4-xpc.sh" setup --dry-run
for c in setup kill-start claim-merge handoff-land takeover-check versions stop all; do WO069_OWNER_GO_Q3=1 dry "block4 $c" 0 "$D/wo069-gate-block4-xpc.sh" $c --dry-run; done
b4=$(WO069_OWNER_GO_Q3=1 "$D/wo069-gate-block4-xpc.sh" all --dry-run 2>&1; WO069_OWNER_GO_Q3=1 "$D/wo069-gate-block4-xpc.sh" stop --dry-run 2>&1)
ok "block4: containers only under wo069-h2- names, --pull=never, the merger key mounted alone (never rustfs.env)" '! echo "$b4" | grep -E "podman (run|kill|rm|exec)" | grep -vq "wo069-h2-\|\$N" && echo "$b4" | grep -q -- "--pull=never" && ! echo "$b4" | grep -q "rustfs.env" && echo "$b4" | grep -q "merger.env:/key/merger.env:ro"'
ok "block4: the kill is podman kill of its own container after the proxy holds the claim PUT" 'echo "$b4" | grep -q "hold handoff.json" && echo "$b4" | grep -q "podman kill wo069-h2-mk"'
WO069_OWNER_GO_Q3=1 dry "block4 second host = PG" 4 "$D/wo069-gate-block4-xpc.sh" setup --second-host photo-gallery --dry-run
ok "block4: no state dir created by the dry runs" '[ ! -e "$REPO/target/b4t" ]'; export WO069_GATE_STATE="$T/state"
dry "item6" 0 "$D/wo069-gate-item6-followers.sh" --dry-run
dry "item6 a2 (libfaketime writer) without the owner go" 3 "$D/wo069-gate-item6-followers.sh" --only a2 --dry-run
printf '# hosts\nmac local /opt/none/weaver\nxpc %s\npg mathew@100.90.170.87\n' "$HOST" >"$T/hosts"
dry "item8" 0 "$D/wo069-gate-item8-writers.sh" --dry-run "$T/hosts"
ok "item8: a local target runs no ssh" 'echo "$out" | grep -q "DRY: /opt/none/weaver --version"'
ok "item8: PG is skipped by default and says so" 'echo "$out" | grep -q "skipped: PG is production"'

# PG refusals (exit 4)
dry "remote-rustfs up photo-gallery" 4 "$D/wo069-gate-remote-rustfs.sh" up photo-gallery --dry-run
dry "remote-rustfs up mathew@100.90.170.87" 4 "$D/wo069-gate-remote-rustfs.sh" up mathew@100.90.170.87 --dry-run
dry "item1 second host = PG address" 4 "$D/wo069-gate-item1-two-machines.sh" --follower=library --second-host 100.90.170.87 --dry-run
ok "PG refusal says production" 'echo "$out" | grep -qi "production"'
WO069_GATE_WRITER_ENV="$T/pg.env" dry "item5 with a PG endpoint in the env file" 4 "$D/wo069-gate-item5-failures.sh" writer-kill --dry-run
WO069_GATE_WRITER_ENV="$T/pg.env" dry "item6 with a PG endpoint in the env file" 4 "$D/wo069-gate-item6-followers.sh" --dry-run
WO069_GATE_WRITER_ENV="$T/pg.env" dry "item1 with a PG endpoint in the env file" 4 "$D/wo069-gate-item1-two-machines.sh" --follower=library --dry-run
dry "remote-rustfs on aepod-xpc is not refused" 0 "$D/wo069-gate-remote-rustfs.sh" status "$HOST" --dry-run
dry "credential dir under /tmp is refused" 1 env WO069_GATE_CRED_DIR=/tmp/x "$D/wo069-gate-remote-rustfs.sh" up "$HOST" --dry-run

# item 3 on a synthetic tree: counts only, no path, no name
S="$T/tree"; mkdir -p "$S/ACME_Confidential/deep/deeper" "$S/big"
echo hello >"$S/ACME_Confidential/Customer_List.txt"; echo x >"$S/ACME_Confidential/deep/deeper/Secret_Plan.md"; echo y >"$S/root_file.txt"
head -c 17000000 /dev/zero >"$S/big/Huge_Dump.bin"; ln -s root_file.txt "$S/a_link"
stats=$("$D/wo069-gate-item3-tree-stats.sh" "$S" 2>&1); rc=$?
ok "item3: exits 0" '[ $rc = 0 ]'
ok "item3: file count 4, symlinks 1, files over 16 MiB 1, max depth 3" 'echo "$stats" | grep -qx "files 4" && echo "$stats" | grep -qx "symlinks 1" && echo "$stats" | grep -qx "files_over_16MiB 1" && echo "$stats" | grep -qx "max_depth 3"'
ok "item3: largest file is the 17 MB one" 'echo "$stats" | grep -qx "largest_bytes 17000000"'
ok "item3: no name, path or extension appears" '! echo "$stats" | grep -qiE "ACME|Confidential|Customer|Secret|Plan|Huge|Dump|root_file|a_link|deeper|\.txt|\.md|\.bin|$T"'
ok "item3: every line is a fixed label and one value" '! echo "$stats" | grep -vE "^[A-Za-z0-9_]+( [A-Za-z0-9_]+)? ([0-9]+|yes|NO)$"'
ok "item3: says the tree fits the file and size ceilings but not the per-file cap" 'echo "$stats" | grep -qx "fits_100000_files yes" && echo "$stats" | grep -qx "fits_16MiB_per_file NO"'
git -C "$S" init -q 2>/dev/null; git -C "$S" -c user.name=t -c user.email=t@e.invalid add -A 2>/dev/null
gstats=$("$D/wo069-gate-item3-tree-stats.sh" "$S" 2>&1); ok "item3 in git mode: same counts (untracked, not ignored)" 'echo "$gstats" | grep -qx "files 4"'

# nothing was touched
ok "no fake tool was called (no ssh, scp, curl, podman, nc, rsync, caffeinate, rustfs, weaver)" '[ ! -e "$T/touched" ]'
after=$(ls "$REPO/target" | grep -c 'wo069-gate' || true)
ok "no work directory was created under target/ by a dry run (only this test's own)" '[ "$after" = "$before" ]'
ok "no credential file or state file was created" '[ -z "$(find "$T/cred" "$T/state" -mindepth 1 | head -1)" ]'
ok "no file leaked into the repo" '[ -z "$(git -C "$REPO" status --porcelain | grep -v "^?? \(docs/store/\|scripts/store/wo069-gate\|scripts/tests/wo069-gate\)")" ]'
[ ! -e "$T/touched" ] || cat "$T/touched"
echo "wo069-gate-scripts tests: $pass passed, $fail failed"; [ "$fail" -eq 0 ]
