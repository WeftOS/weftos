#!/usr/bin/env bash
# WO-069 lane F2, ADR-126 section 4a gate item 8: which machines that write a worktree bucket run a build with the claim fence and the
# conditional manifest write. Read-only: it runs `weaver --version` and nothing else on each host.
set -euo pipefail
export GATE_NAME=wo069-gate-item8
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-item8-writers.sh [--min-commit SHA] [--dry-run] <hosts file>
       wo069-gate-item8-writers.sh [--min-commit SHA] [--dry-run] <label>=<ssh target>[=<weaver path>] ...
       wo069-gate-item8-writers.sh --help

Prints one row per host: label, weaver path, the `weaver --version` string, its build commit, and a verdict:
  OK       the build commit contains --min-commit (git merge-base --is-ancestor in this repo)
  OLDER    the build commit does not contain it: that machine voids the fence and the conditional manifest write for every bucket it writes
  UNKNOWN  the commit is not in this repo's object database, or the host has no weaver, or --min-commit was not given
Hosts file: one host per line, `label ssh-target [weaver path]` (default weaver path: weaver on the remote PATH), `#` comments allowed.
An ssh target of `local` runs the weaver at the given path on this machine, with no ssh (for the Mac binary the drills will use).
Include every machine that holds a write key for the project's worktree buckets, even a spare laptop: a stale daemon with the key counts.
--min-commit: the first commit that has the claim fence and the conditional manifest write (the fence atomicity lane and the D3 lane of
w2/wo069-worktree-mirror); WO069_GATE_MIN_COMMIT works too. No release has a version number for it yet.
It never calls `weaver kernel status`, never opens a daemon socket, and never reads a key. PG is production and is SKIPPED
by default and printed as such; --include-pg reads only its binary's version string (one ssh, `weaver --version`), and only if the owner
agrees that counts as "not touching" PG. A skipped PG row is not a pass: the gate still needs PG's answer from its operator.
EOF
}
MIN=${WO069_GATE_MIN_COMMIT:-}; INC_PG=0; SPECS=()
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        --min-commit) MIN=$2; shift 2 ;;
        --include-pg) INC_PG=1; shift ;;
        -*) gate_die "unknown option $1 (see --help)" ;;
        *) SPECS+=("$1"); shift ;;
    esac
done
[ ${#SPECS[@]} -gt 0 ] || { usage >&2; exit 2; }
ROWS=()
if [ ${#SPECS[@]} = 1 ] && [ -f "${SPECS[0]}" ]; then
    while IFS= read -r l; do l=${l%%#*}; [ -n "${l//[[:space:]]/}" ] || continue; ROWS+=("$l"); done <"${SPECS[0]}"
else
    for s in "${SPECS[@]}"; do ROWS+=("$(printf '%s' "$s" | tr '=' ' ')"); done
fi
[ -z "$MIN" ] || [ "$GATE_DRY" = 1 ] || git -C "$GATE_REPO" cat-file -e "$MIN^{commit}" 2>/dev/null || gate_die "--min-commit $MIN is not a commit in $GATE_REPO"
printf '%-14s %-28s %-52s %-10s %s\n' label weaver version commit verdict
older=0; unknown=0; skipped=0
for row in "${ROWS[@]}"; do
    read -r label target wpath <<<"$row"; wpath=${wpath:-weaver}
    if [ "$INC_PG" = 0 ] && printf '%s' "$target" | grep -Eiq "$GATE_PG_RE"; then printf '%-14s %s\n' "$label" "(skipped: PG is production; ask its operator, or rerun with --include-pg)"; skipped=$((skipped + 1)); continue; fi
    if [ "$GATE_DRY" = 1 ]; then if [ "$target" = local ]; then gate_run "$wpath" --version; else gate_run ssh -o BatchMode=yes -o ConnectTimeout=10 "$target" "$wpath --version"; fi; continue; fi
    if [ "$target" = local ]; then ver=$("$wpath" --version 2>/dev/null | head -1) || ver=""
    else ver=$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$target" "$wpath --version" 2>/dev/null | head -1) || ver=""; fi
    commit=$(printf '%s' "$ver" | sed -n 's/.*(\([0-9a-f]\{7,40\}\).*/\1/p')
    if [ -z "$ver" ]; then verdict=UNKNOWN; elif [ -z "$MIN" ] || [ -z "$commit" ]; then verdict=UNKNOWN
    elif ! git -C "$GATE_REPO" cat-file -e "$commit^{commit}" 2>/dev/null; then verdict=UNKNOWN
    elif git -C "$GATE_REPO" merge-base --is-ancestor "$MIN" "$commit" 2>/dev/null; then verdict=OK
    else verdict=OLDER; fi
    case $verdict in OLDER) older=$((older + 1)) ;; UNKNOWN) unknown=$((unknown + 1)) ;; esac
    printf '%-14s %-28s %-52s %-10s %s\n' "$label" "$wpath" "${ver:-no weaver}" "${commit:--}" "$verdict"
done
[ "$GATE_DRY" = 1 ] && exit 0
printf 'summary: %s host(s), %s OLDER, %s UNKNOWN, %s skipped. The gate passes only with 0 OLDER, 0 UNKNOWN and 0 skipped among hosts that hold a write key.\n' "${#ROWS[@]}" "$older" "$unknown" "$skipped"
[ "$older" = 0 ] && [ "$unknown" = 0 ] && [ "$skipped" = 0 ]
