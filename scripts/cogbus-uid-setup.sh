#!/bin/sh
# Owner-run host setup for per-cog uids on the cogbus (ADR-122 gaps 2 and 3).
#
# The daemon never changes the host. This script does, once, and can undo it:
#
#   apply   create the `cogbus` group (if absent), check that the uid range is
#           unused, write <runtime-dir>/cogbus-isolation.json (owner-only), and
#           record what it did in /etc/weftos/cogbus-uid.manifest
#   undo    remove exactly what the manifest says apply created (the group only
#           if apply created it and nothing is a member), then the manifest
#   status  show what is in place and what is missing; changes nothing
#
# Nothing is done unless --yes is given: the default is a dry run that prints
# every command. Cogs run under numeric uids from the range; no passwd entries
# are created. Restart the machine service afterwards so it re-reads the file.
#
# Usage:
#   scripts/cogbus-uid-setup.sh --runtime-dir DIR [--first-uid N] [--count N]
#                               [--group NAME] [--require] [--yes] apply|undo|status
#
# DIR is the machine service's runtime dir (where cogbus.sock lives). It, and
# every parent, must be traversable by the cog uids (o+x) for the socket's
# group permission to be reachable; status reports it, apply does not change it.

set -eu

RUNTIME_DIR=""
FIRST=60000
COUNT=1000
GROUP="cogbus"
REQUIRE=false
YES=false
MANIFEST="/etc/weftos/cogbus-uid.manifest"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }
run() {
    if $YES; then say "+ $*"; "$@"; else say "[dry run] $*"; fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --runtime-dir) RUNTIME_DIR="$2"; shift 2 ;;
        --first-uid) FIRST="$2"; shift 2 ;;
        --count) COUNT="$2"; shift 2 ;;
        --group) GROUP="$2"; shift 2 ;;
        --require) REQUIRE=true; shift ;;
        --yes) YES=true; shift ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        apply|undo|status) CMD="$1"; shift ;;
        *) die "unknown argument: $1" ;;
    esac
done
[ -n "${CMD:-}" ] || die "say apply, undo or status (see --help)"
[ "$(uname -s)" = "Linux" ] || die "per-cog uids are Linux only"
case "$FIRST$COUNT" in *[!0-9]*) die "--first-uid and --count are numbers" ;; esac
[ "$FIRST" -ge 1000 ] || die "--first-uid must be at least 1000"
[ "$COUNT" -ge 1 ] && [ "$COUNT" -le 65536 ] || die "--count must be 1..65536"
LAST=$((FIRST + COUNT - 1))
case "$GROUP" in ""|*[!A-Za-z0-9_-]*) die "--group must be a plain name" ;; esac
CONF="$RUNTIME_DIR/cogbus-isolation.json"

need_root() { if $YES && [ "$(id -u)" -ne 0 ]; then die "run as root (sudo) with --yes"; fi; }

range_free() {
    getent passwd | awk -F: -v a="$FIRST" -v b="$LAST" '$3>=a && $3<=b {print $1" ("$3")"}'
}

do_status() {
    if getent group "$GROUP" >/dev/null; then say "group $GROUP: present (gid $(getent group "$GROUP" | cut -d: -f3))"; else say "group $GROUP: MISSING"; fi
    used=$(range_free)
    if [ -n "$used" ]; then say "uid range $FIRST..$LAST: IN USE by $used"; else say "uid range $FIRST..$LAST: free of accounts"; fi
    if [ -n "$RUNTIME_DIR" ]; then
        if [ -f "$CONF" ]; then say "config: $CONF present"; else say "config: $CONF MISSING"; fi
        d="$RUNTIME_DIR"
        while [ "$d" != "/" ] && [ -n "$d" ]; do
            if [ -d "$d" ] && [ "$(stat -c %A "$d" | cut -c10)" != "x" ] && [ "$(stat -c %A "$d" | cut -c10)" != "t" ]; then
                say "traverse: $d is not o+x; cog uids cannot reach the socket through it"
            fi
            d=$(dirname "$d")
        done
    fi
    if [ -f "$MANIFEST" ]; then say "manifest: $MANIFEST"; sed 's/^/  /' "$MANIFEST"; else say "manifest: none (apply has not run)"; fi
}

do_apply() {
    [ -n "$RUNTIME_DIR" ] || die "--runtime-dir is required"
    [ -d "$RUNTIME_DIR" ] || die "$RUNTIME_DIR is not a directory"
    need_root
    used=$(range_free)
    [ -z "$used" ] || die "uid range $FIRST..$LAST already has accounts: $used"
    [ ! -f "$MANIFEST" ] || die "$MANIFEST exists: run undo first"
    created_group=no
    if ! getent group "$GROUP" >/dev/null; then
        run groupadd --system "$GROUP"
        created_group=yes
    fi
    created_dir=no
    [ -d "$(dirname "$MANIFEST")" ] || created_dir=yes
    run mkdir -p "$(dirname "$MANIFEST")"
    tmp=$(mktemp)
    printf '{\n  "require_cog_uid": %s,\n  "uid_range": [%s, %s],\n  "group": "%s"\n}\n' "$REQUIRE" "$FIRST" "$LAST" "$GROUP" > "$tmp"
    run install -m 0600 -o root -g root "$tmp" "$CONF"
    rm -f "$tmp"
    if $YES; then
        {
            echo "created_group=$created_group"
            echo "created_dir=$created_dir"
            echo "group=$GROUP"
            echo "config=$CONF"
            echo "uid_range=$FIRST-$LAST"
        } > "$MANIFEST"
        chmod 0600 "$MANIFEST"
    else
        say "[dry run] write $MANIFEST (created_group=$created_group, config=$CONF, range $FIRST-$LAST)"
    fi
    say "restart the machine service to pick this up; 'weaver cogbus list' then shows per-cog-uid"
}

do_undo() {
    [ -f "$MANIFEST" ] || die "no $MANIFEST: nothing this script created"
    need_root
    created_group=$(sed -n 's/^created_group=//p' "$MANIFEST")
    group=$(sed -n 's/^group=//p' "$MANIFEST")
    config=$(sed -n 's/^config=//p' "$MANIFEST")
    created_dir=$(sed -n 's/^created_dir=//p' "$MANIFEST")
    [ -z "$config" ] || run rm -f "$config"
    if [ "$created_group" = "yes" ] && getent group "$group" >/dev/null; then
        members=$(getent group "$group" | cut -d: -f4)
        if [ -n "$members" ]; then
            say "keeping group $group: it has members ($members)"
        else
            run groupdel "$group"
        fi
    fi
    run rm -f "$MANIFEST"
    if [ "$created_dir" = "yes" ]; then run rmdir "$(dirname "$MANIFEST")"; fi
    say "restart the machine service: cogs return to the shared uid"
}

case "$CMD" in
    status) do_status ;;
    apply) do_apply ;;
    undo) do_undo ;;
esac
