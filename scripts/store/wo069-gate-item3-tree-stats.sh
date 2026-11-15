#!/usr/bin/env bash
# WO-069 lane F2, gate item 3: the SHAPE of a tree, as numbers only. For the owner (or the session that owns the client project's
# tree) to run locally. It prints counts and sizes and NOTHING that identifies a file: no path, no name, no extension, no content.
# It uploads nothing, writes no file, and makes no network call. Pure find / stat / awk; macOS and Linux.
set -euo pipefail
export GATE_NAME=wo069-gate-item3
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-item3-tree-stats.sh [--all-files] [--dry-run] <dir>
       wo069-gate-item3-tree-stats.sh --help

Prints, for the files a worktree publish would share: file count, total bytes, largest file (bytes), a size histogram, directory
count, deepest and mean directory depth, a depth histogram, files over 16 MiB (the per-file cap), symlinks, and whether the tree fits
the 100 000 files / 4 GiB ceiling, with the first-publish time that would mean at the measured 30 to 68 PUT/s. No path or file name.

  <dir>        a git worktree or a plain directory
  --all-files  count every regular file below <dir> except under .git, instead of what git would share (tracked plus untracked and
               not ignored). For a git directory the default matches the publish.
  --dry-run    print what would be run; read nothing

The output is safe to paste into a chat or a ticket: it holds only integers and fixed labels.
EOF
}
ALL=0; DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        --all-files) ALL=1; shift ;;
        -*) gate_die "unknown option $1 (see --help)" ;;
        *) [ -z "$DIR" ] || gate_die "one directory only"; DIR=$1; shift ;;
    esac
done
[ -n "$DIR" ] || { usage >&2; exit 2; }

if stat --version >/dev/null 2>&1; then STATF=(stat -c '%F|%s'); else STATF=(stat -f '%HT|%z'); fi

list_paths() { # NUL-separated relative paths on stdout; run inside $DIR
    if [ "$ALL" = 0 ] && [ -e .git ]; then git ls-files -co --exclude-standard -z 2>/dev/null
    else find . -path ./.git -prune -o \( -type f -o -type l \) -print0; fi
}

if [ "$GATE_DRY" = 1 ]; then
    gate_note "cd $DIR; list paths ($([ "$ALL" = 0 ] && echo 'git ls-files -co --exclude-standard -z, else ')find . -type f -o -type l, excluding .git)"
    gate_note "xargs -0 ${STATF[*]}  (type and size only; names are never printed)"
    gate_note "awk over sizes and over path depth; print integers only"
    exit 0
fi
[ -d "$DIR" ] || gate_die "$DIR is not a directory"
cd "$DIR"

# two passes over the same path list; both print integers and fixed labels to stdout only
{
  { list_paths | xargs -0 "${STATF[@]}" 2>/dev/null || true; } | awk -F'|' '
    { t = tolower($1); n = $2 + 0
      if (t ~ /symbolic/) { sym++; next }
      if (t !~ /regular/) next
      files++; total += n; if (n > max) max = n
      if (n < 1024) b[0]++; else if (n < 16384) b[1]++; else if (n < 262144) b[2]++
      else if (n < 1048576) b[3]++; else if (n < 16777216) b[4]++; else { b[5]++; over++ } }
    END { printf "files %d\ntotal_bytes %d\nlargest_bytes %d\nsymlinks %d\nfiles_over_16MiB %d\n", files, total, max, sym, over
          split("lt_1KiB 1KiB_to_16KiB 16KiB_to_256KiB 256KiB_to_1MiB 1MiB_to_16MiB ge_16MiB", nm, " ")
          for (i = 0; i < 6; i++) printf "size_hist %s %d\n", nm[i + 1], b[i] }'
  { list_paths | tr '\0' '\n' || true; } | awk -F/ '
    $0 != "" { sub(/^\.\//, ""); d = gsub(/\//, "/"); n++; sum += d; if (d > dmax) dmax = d; h[d > 12 ? 12 : d]++
      dir = $0; sub(/[^\/]*$/, "", dir); dirs[dir] = 1 }
    END { for (k in dirs) nd++
          printf "directories %d\nmax_depth %d\nmean_depth_x100 %d\n", nd, dmax, n ? int(sum * 100 / n) : 0
          for (i = 0; i <= 12; i++) printf "depth_hist %s%d %d\n", (i == 12 ? "ge_" : ""), i, h[i] }'
} | awk '
    { print; if ($1 == "files") f = $2; if ($1 == "total_bytes") t = $2; if ($1 == "files_over_16MiB") o = $2 }
    END { printf "fits_100000_files %s\nfits_4GiB %s\nfits_16MiB_per_file %s\n", (f <= 100000) ? "yes" : "NO", (t <= 4294967296) ? "yes" : "NO", (o == 0) ? "yes" : "NO"
          printf "first_publish_minutes_at_30_PUT_s %d\nfirst_publish_minutes_at_68_PUT_s %d\n", f / 30 / 60, f / 68 / 60 }'
