#!/usr/bin/env bash
# Hardware-in-the-loop helper for the edge pad's chief-of-staff console
# (ADR-117 / ADR-118). Written so the owner or Grok can run it when the pad
# is reconnected; `rehearse` runs the same flow against the host-side pad
# emulator so every step can be checked first without hardware.
#
#   scripts/edgepad-console-hil.sh prepare    provision + enrol the pad's leaf identity
#   scripts/edgepad-console-hil.sh check      is the daemon listening, is the pad connected
#   scripts/edgepad-console-hil.sh run        draw the console on the pad (cos-console-host --leaf)
#   scripts/edgepad-console-hil.sh rehearse   the same, against the emulator (no hardware)
#
# Full walk-through: docs/guides/edgepad-console-hil.md
#
# Environment:
#   WEAVER             weaver binary (default: weaver)
#   EDGEPAD_HIL_DIR    where keys and the leaf identity live, mode 0700
#                      (default: ~/.weftos/edgepad-hil). Never inside the repo.
#   COS_HOST           cos-console-host binary
#                      (default: crates/weftos-cos-console/target/release/cos-console-host)
#   LEAF_EMULATOR      leaf-emulator binary, for `rehearse`
#                      (default: crates/weftos-display-client/target/debug/leaf-emulator)
#   COS_AGENT          the chief-of-staff agent's API (default http://127.0.0.1:18300)
#
# Nothing here flashes a device or touches USB. Flashing is a separate,
# deliberate step (see the guide).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEAVER="${WEAVER:-weaver}"
DIR="${EDGEPAD_HIL_DIR:-$HOME/.weftos/edgepad-hil}"
COS_HOST="${COS_HOST:-$ROOT/crates/weftos-cos-console/target/release/cos-console-host}"
LEAF_EMULATOR="${LEAF_EMULATOR:-$ROOT/crates/weftos-display-client/target/debug/leaf-emulator}"
COS_AGENT="${COS_AGENT:-http://127.0.0.1:18300}"
PROV="$DIR/leaf-prov"

say()  { printf '%s\n' "$*"; }
ok()   { printf '  ok    %s\n' "$*"; }
bad()  { printf '  FAIL  %s\n' "$*" >&2; FAILED=1; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
FAILED=0

need() { command -v "$1" >/dev/null 2>&1 || [ -x "$1" ] || die "$1 not found (set ${2:-the matching variable})"; }

# "key: value" line of `weaver kernel status` / `weaver mesh identity`.
field() { sed -n "s/^$1:[[:space:]]*//p" | head -1; }

leaf_id() {
  local dir="${1:-$PROV}"
  [ -f "$dir/leaf.json" ] || die "no leaf identity yet; run: $0 prepare"
  sed -n 's/.*"node_id"[[:space:]]*:[[:space:]]*"\([0-9a-f]\{32\}\)".*/\1/p' "$dir/leaf.json" | head -1
}

# Issue a leaf certificate into $1 (an empty path) under the machine's user key.
provision_into() {
  "$WEAVER" leaf provision --parent-key "$(cat "$DIR/user-key-path")" --mesh-pubkey "$DIR/node.pub" \
    --parent-scope "$(cat "$DIR/scope")" --out-dir "$1"
}

# Install the certificate in $1 in the daemon's registry (idempotent).
enrol_from() {
  local rt registry id; rt="$(runtime_dir)"; id="$(leaf_id "$1")"
  [ -n "$rt" ] || die "cannot find the daemon's runtime dir; start the daemon or set WEFTOS_RUNTIME_DIR"
  registry="$rt/leaf/registry"
  if [ ! -d "$registry" ]; then
    mkdir -p -m 700 "$rt/leaf" "$registry"
    say "  created $registry. The daemon installs the leaf gate only if this"
    say "  directory exists when it boots, so restart it once now:"
    say "      $WEAVER kernel restart   (or however this machine runs its daemon)"
  fi
  if [ -f "$registry/$id.json" ]; then
    ok "leaf $id already enrolled"
  else
    "$WEAVER" leaf enroll --public "$1/leaf.json" --registry-dir "$registry" \
      --parent-pubkey "$DIR/user.pub" --mesh-pubkey "$DIR/node.pub" --parent-scope "$(cat "$DIR/scope")"
  fi
}

runtime_dir() {
  if [ -n "${WEFTOS_RUNTIME_DIR:-}" ]; then printf '%s' "$WEFTOS_RUNTIME_DIR"; return; fi
  "$WEAVER" kernel status 2>/dev/null | field Runtime
}

cmd_prepare() {
  need "$WEAVER" WEAVER
  mkdir -p "$DIR"; chmod 700 "$DIR"
  if [ -d "$PROV" ]; then
    say "Leaf identity already provisioned: $(leaf_id)"
    say "(delete $PROV only if you mean to re-provision; the pad image embeds this identity)"
  else
    say "1/4  public identities of this machine"
    "$WEAVER" mesh identity --out "$DIR/node.pub" >/dev/null
    local user_out; user_out="$("$WEAVER" mesh identity --user --out "$DIR/user.pub")"
    local scope key_file
    scope="$(printf '%s\n' "$user_out" | field scope)"
    key_file="$(printf '%s\n' "$user_out" | field 'key file' | sed 's/ (public half only)//')"
    [ -n "$scope" ] && [ -f "$key_file" ] || die "could not read the user scope or key file from: $user_out"
    ok "scope $scope"

    say "2/4  provision the pad's leaf identity (no hardware is touched)"
    printf '%s' "$scope" > "$DIR/scope"
    printf '%s' "$key_file" > "$DIR/user-key-path"
    provision_into "$PROV"
  fi

  say "3/4  enrol it with this daemon"
  enrol_from "$PROV"

  say "4/4  what is left"
  cat <<EOF
  a. The daemon must listen for leaves on the LAN. In its weave.toml:

         [kernel.mesh]
         enabled = true
         listen_addr = "127.0.0.1:9489"
         leaf_listen_addr = "<this Mac's LAN IP>"     # the pad cannot reach loopback

     then restart the daemon and run: $0 check
  b. Build the pad image with this identity (does not flash):

         EDGEPAD_PROVISION_DIR=$PROV scripts/build.sh edgepad-fw

  c. Flash target/edgepad-fw/edgepad-merged.bin (README of
     crates/clawft-edge-pad-idf: "Flashing"), then: $0 check, $0 run
EOF
}

cmd_check() {
  need "$WEAVER" WEAVER
  local id status; id="$(leaf_id)"
  say "Checking leaf $id"
  status="$("$WEAVER" kernel status 2>&1)" || { bad "daemon not reachable: $status"; exit 1; }
  ok "daemon is up"
  if printf '%s\n' "$status" | grep -q '^Leaf bind:'; then
    ok "$(printf '%s\n' "$status" | grep '^Leaf bind:')"
    case "$(printf '%s\n' "$status" | grep '^Leaf bind:')" in
      *127.0.0.1*|*localhost*) bad "leaf listeners are on loopback; the pad cannot reach them (set leaf_listen_addr)";;
    esac
  else
    bad "no 'Leaf bind:' line: leaf admission is off. Needs [kernel.mesh] enabled, a persisted node key, and the dir $(runtime_dir)/leaf/registry present at boot"
  fi
  [ -f "$(runtime_dir)/leaf/registry/$id.json" ] && ok "leaf is enrolled" || bad "leaf is not enrolled (run: $0 prepare)"
  if "$WEAVER" leaf status --leaf-id "$id" 2>/dev/null | grep -q '^connected:  yes'; then
    ok "the pad is connected and subscribed to its push topic"
  else
    bad "the pad has not subscribed yet (is it powered, on the Mac's Wi-Fi, flashed with this identity? serial log: '[mesh] subscribed')"
  fi
  [ "$FAILED" = 0 ] || exit 1
}

cmd_run() {
  need "$COS_HOST" COS_HOST
  say "Console host -> leaf $(leaf_id), agent $COS_AGENT"
  exec "$COS_HOST" --leaf "$(leaf_id)" --agent "$COS_AGENT" "$@"
}

# The same flow with the emulated pad in place of the hardware: a throwaway
# rehearsal identity against the real daemon, so the daemon config, the
# registry and the console host can be checked first.
cmd_rehearse() {
  need "$LEAF_EMULATOR" LEAF_EMULATOR
  need "$COS_HOST" COS_HOST
  [ -f "$DIR/user-key-path" ] || die "run '$0 prepare' first"
  # Its own identity: the daemon keeps a replay floor per leaf, and a rehearsal
  # must not advance the real pad's.
  local reh="$DIR/leaf-rehearsal"
  [ -d "$reh" ] || provision_into "$reh"
  enrol_from "$reh"
  local id status disc
  id="$(leaf_id "$reh")"
  status="$("$WEAVER" kernel status 2>&1)" || die "daemon not reachable: $status"
  disc="$(printf '%s\n' "$status" | sed -n 's/.*discovery udp \([0-9.:]*\).*/\1/p' | head -1)"
  [ -n "$disc" ] || die "the daemon reports no leaf listener (see: $0 check)"
  say "Starting the emulated pad (leaf $id) via discovery at $disc"
  "$LEAF_EMULATOR" --seed "$reh/leaf.seed" --cert "$reh/leaf.cbor" --parent "discover:$disc" \
    --state "$DIR/emulator-journal.cbor" --headless &
  local pid=$!
  trap "kill $pid 2>/dev/null || true" EXIT
  for _ in $(seq 1 50); do
    "$WEAVER" leaf status --leaf-id "$id" 2>/dev/null | grep -q '^connected:  yes' && break
    sleep 0.2
  done
  "$WEAVER" leaf status --leaf-id "$id" 2>/dev/null | grep -q '^connected:  yes' || die "the emulated pad never subscribed"
  ok "emulated pad is subscribed"
  say "Running the console host for 5 s against it"
  "$COS_HOST" --leaf "$id" --agent "$COS_AGENT" --seconds 5
  ok "console host ran; the display session opened and closed cleanly"
}

case "${1:-}" in
  prepare)  shift; cmd_prepare "$@";;
  check)    shift; cmd_check "$@";;
  run)      shift; cmd_run "$@";;
  rehearse) shift; cmd_rehearse "$@";;
  *) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 2;;
esac
