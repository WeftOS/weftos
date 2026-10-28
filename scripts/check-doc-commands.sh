#!/usr/bin/env bash
# Doc-drift check: every `weaver ...` / `weft ...` command in the verified
# deployment docs must name subcommands and long flags that exist in the built
# binary (`--help`), and every key in a `[kernel.mesh]` TOML block must be a
# field of MeshConfig. Fails (exit 1) on any drift.
#
# Usage:
#   scripts/check-doc-commands.sh [--weaver PATH] [--weft PATH] [--list] [--self-test]
#   scripts/build.sh check-doc-commands
#
# Defaults: target/debug/weaver and target/debug/weft (scripts/build.sh native-debug).
# --list prints the extracted commands without checking them.
# --self-test runs the checker over fixtures: every drifted form below must be
# reported and every valid form must pass.
#
# Scope (override with DOC_SPECS="file:all file:sop ..."):
#   docs/guides/*.md, docs/reference/*.md, docs/cogs/*.md   whole files
#   docs/src/content/docs/clawft/*.mdx                      whole files
#   docs/src/content/docs/weftos/guides/*.mdx               whole files
#   docs/guides/weftos-deployment-sops.md                   SOP 1 and SOP 3
# Extracted: `weaver`/`weft` commands in fenced lines and inline code spans, at
# the start of a line or after `&&`, `;`, `|`, `$(`, leading `VAR=val` words and
# `sudo`; and `ExecStart=.../weaver ...` lines. A command is cut at ` > `,
# ` >> ` and ` # `.
# Flags are checked against the Options section of the final command's own
# `--help` (clap lists propagated global flags there too); a flag declared only
# on a parent command is reported.
# Not checked: positional values, short flags, flag values, config keys other
# than [kernel.mesh].
# Keep prose that names a NON-existent command out of backticks that start
# with `weaver ` or `weft `, or it will be reported as drift.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

WEAVER="target/debug/weaver"
WEFT="target/debug/weft"
LIST_ONLY=false
SELF_TEST=false
while [ $# -gt 0 ]; do
    case "$1" in
        --weaver) WEAVER="${2:?--weaver needs a path}"; shift 2 ;;
        --weft) WEFT="${2:?--weft needs a path}"; shift 2 ;;
        --list) LIST_ONLY=true; shift ;;
        --self-test) SELF_TEST=true; shift ;;
        -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# Default scope: the verified deployment docs plus every guide, reference and
# docs-site page. The deployment SOPs file is checked for SOP 1 and SOP 3 only.
#
# KNOWN_DRIFT: pages that still name commands the CLI does not have (reported
# 2026-10-27, owned by the docs lanes that rewrite them). Delete a line as its
# page is fixed; do not add new pages here.
KNOWN_DRIFT="
docs/guides/kernel.md
docs/guides/mcp-integration.md
docs/guides/mcp.md
docs/guides/plugins.md
docs/guides/skill-signing.md
docs/guides/tool-calls.md
docs/guides/voice.md
docs/guides/weftos-deferred-requirements.md
docs/guides/workspaces.md
docs/src/content/docs/clawft/architecture.mdx
docs/src/content/docs/clawft/security.mdx
docs/src/content/docs/weftos/guides/assessment.mdx
docs/src/content/docs/weftos/guides/deployment-sops.mdx
"
default_docs() {
    local f
    echo "docs/guides/weftos-deployment-sops.md:sop"
    for f in docs/guides/*.md docs/reference/*.md docs/cogs/*.md \
             docs/src/content/docs/clawft/*.mdx docs/src/content/docs/weftos/guides/*.mdx; do
        [ -f "$f" ] || continue
        [ "$f" = "docs/guides/weftos-deployment-sops.md" ] && continue
        case "$KNOWN_DRIFT" in *"
$f
"*) continue ;; esac
        echo "$f:all"
    done
}
DOCS="${DOC_SPECS:-$(default_docs | tr '\n' ' ')}"
MESH_SRC="crates/clawft-types/src/config/kernel.rs"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/doc-commands.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# ── extraction ───────────────────────────────────────────────────────
# Prints `file:line:command` for each command, `file:line:@key` for each
# [kernel.mesh] TOML key.
extract() { # file mode
    awk -v file="$1" -v mode="$2" '
    function emit(c,   n, i, segs, t) {
        sub(/ # .*/, "", c)
        gsub(/ && | ; |;[ \t]+| \| |\$\(|\(/, "\001", c)
        n = split(c, segs, "\001")
        for (i = 1; i <= n; i++) {
            t = segs[i]
            sub(/ >>? .*/, "", t)
            sub(/^[ \t]+/, "", t)
            sub(/^\$ +/, "", t)
            while (t ~ /^[A-Za-z_][A-Za-z0-9_]*=[^ ]* /) sub(/^[A-Za-z_][A-Za-z0-9_]*=[^ ]* +/, "", t)
            sub(/^sudo +/, "", t)
            sub(/[ \t\\]+$/, "", t)
            if (t ~ /^(weaver|weft) / ) print file ":" start ":" t
        }
    }
    BEGIN { on = (mode == "all") }
    mode == "sop" && /^## SOP 1/ { on = 1 }
    mode == "sop" && /^## SOP 2/ { on = 0 }
    mode == "sop" && /^## SOP 3/ { on = 1 }
    mode == "sop" && /^## SOP 4/ { on = 0 }
    !on { next }
    /^[ \t]*```/ {
        infence = !infence
        intoml = 0
        if (infence && $0 ~ /```toml/) intoml = 1
        mesh = 0
        next
    }
    infence {
        if (intoml) {
            if ($0 ~ /^\[/) mesh = ($0 ~ /^\[kernel\.mesh\]/)
            else if (mesh && $0 ~ /^[a-z_]+ *=/) {
                k = $0; sub(/ *=.*/, "", k); print file ":" NR ":@" k
            }
            next
        }
        line = $0; start = NR
        while (line ~ /\\[ \t]*$/ && (getline nxt) > 0) { sub(/\\[ \t]*$/, "", line); line = line " " nxt }
        if (line ~ /^[ \t]*ExecStart=/) {
            sub(/^[ \t]*ExecStart=/, "", line)
            sub(/^[^ ]*\//, "", line)
        }
        emit(line)
        next
    }
    {
        rest = $0; start = NR
        while (match(rest, /`[^`]+`/)) {
            span = substr(rest, RSTART + 1, RLENGTH - 2)
            rest = substr(rest, RSTART + RLENGTH)
            emit(span)
        }
    }' "$1"
}

if $SELF_TEST; then
    for b in "$WEAVER" "$WEFT"; do [ -x "$b" ] || { echo "error: $b not found" >&2; exit 2; }; done
    printf '%s\n' '```bash' \
        'cd /x && weaver bogus' \
        'FOO=1 weaver kernel nonsense' \
        'sudo weaver kernel start --nonflag' \
        'echo hi | weaver mesh bogusverb' \
        'x; weaver cluster zzz' \
        'echo $(weaver kernel stat)' \
        'weaver kernel status --not-a-flag' \
        '```' 'Inline: `weaver kernel start --nonflag2` and `cd /y && weft nonexist`.' > "$WORK/bad.md"
    printf '%s\n' '```bash' \
        'cd /x && weaver kernel status' \
        'FOO=1 weaver kernel start --foreground' \
        'sudo weaver mesh status --json' \
        'echo hi | weaver cluster nodes' \
        '```' 'Inline: `weaver kernel stop --force`.' > "$WORK/good.md"
    st_fail=0
    out="$(DOC_SPECS="$WORK/bad.md:all" "$0" --weaver "$WEAVER" --weft "$WEFT" 2>&1)"; rc=$?
    n=$(printf '%s\n' "$out" | grep -c '^DRIFT')
    if [ "$rc" -ne 1 ] || [ "$n" -lt 9 ]; then echo "self-test FAILED: bad fixture gave exit $rc and $n DRIFT lines (want 1 and 9)"; printf '%s\n' "$out"; st_fail=1; fi
    out="$(DOC_SPECS="$WORK/good.md:all" "$0" --weaver "$WEAVER" --weft "$WEFT" 2>&1)"; rc=$?
    if [ "$rc" -ne 0 ]; then echo "self-test FAILED: good fixture exit $rc"; printf '%s\n' "$out"; st_fail=1; fi
    [ "$st_fail" -eq 0 ] && echo "self-test passed (9 drifted forms reported, 5 valid forms accepted)"
    exit "$st_fail"
fi

: > "$WORK/all.txt"
for spec in $DOCS; do
    extract "${spec%%:*}" "${spec##*:}" >> "$WORK/all.txt"
done

if $LIST_ONLY; then
    sort -t: -k1,1 -k2,2n -k3 -u "$WORK/all.txt"
    exit 0
fi

# ── help parsing ─────────────────────────────────────────────────────
# help_of BIN sub... -> help text (cached)
help_of() {
    local bin="$1"; shift
    local key
    key="$WORK/help-$(basename "$bin")-$(printf '%s_' "$@" | tr -c 'A-Za-z0-9_\n' '_')"
    if [ ! -f "$key" ]; then
        "$bin" "$@" --help > "$key" 2>&1 || true
    fi
    cat "$key"
}
subcommands_of() { # bin sub... -> subcommand names
    help_of "$@" | awk '/^Commands:/ {on=1; next} on && /^$/ {exit} on && NF {print $1}'
}
flags_of() { # bin sub... -> long flags from the Options section only
    help_of "$@" | awk '/^Options:/ {on=1; next} on && /^[A-Za-z]/ {on=0}
        on && /^(  -[a-zA-Z], |      )--[a-z0-9]/ { s=$0; sub(/^(  -[a-zA-Z], |      )/, "", s); sub(/[^a-z0-9-].*/, "", s); print s }' | sort -u
}

fail=0
checked=0
report() { echo "DRIFT $1: $2"; fail=$((fail + 1)); }

check_command() { # loc command
    local loc="$1" cmd="$2" bin tok first
    local -a toks path flags
    read -r -a toks <<< "$cmd"
    first="${toks[0]}"
    case "$first" in
        weaver) bin="$WEAVER" ;;
        weft) bin="$WEFT" ;;
    esac
    path=(); flags=()
    local stopped=false t
    for t in "${toks[@]:1}"; do
        case "$t" in
            --*) flags+=("${t%%=*}") ;;
            -*) ;;
            *)
                $stopped && continue
                if subcommands_of "$bin" ${path[@]+"${path[@]}"} | grep -qx -- "$t"; then
                    path+=("$t")
                else
                    stopped=true
                    # a non-subcommand word right after a command that has
                    # subcommands is an unknown subcommand, not a positional
                    if [ -n "$(subcommands_of "$bin" ${path[@]+"${path[@]}"})" ] && ! [[ "$t" == \<* || "$t" == \"* || "$t" == \$* ]]; then
                        report "$loc" "\`$first ${path[*]+${path[*]}}\` has no subcommand \`$t\` (in: $cmd)"
                    fi
                fi
                ;;
        esac
    done
    local f
    for f in ${flags[@]+"${flags[@]}"}; do
        [ "$f" = "--help" ] && continue
        flags_of "$bin" ${path[@]+"${path[@]}"} | grep -qx -- "$f" \
            || report "$loc" "\`$first ${path[*]+${path[*]}}\` has no flag \`$f\` in its own --help Options (in: $cmd)"
    done
    checked=$((checked + 1))
}

for b in "$WEAVER" "$WEFT"; do
    if [ ! -x "$b" ]; then
        echo "error: $b not found; build it with scripts/build.sh native-debug or pass --weaver/--weft" >&2
        exit 2
    fi
done

# MeshConfig field names and aliases
awk '/^pub struct MeshConfig/ {on=1; next} on && /^}/ {exit}
     on && /pub [a-z_]+:/ { s=$0; sub(/.*pub /, "", s); sub(/:.*/, "", s); print s }
     on && /alias = "/ { s=$0; sub(/.*alias = "/, "", s); sub(/".*/, "", s); print s }' "$MESH_SRC" | sort -u > "$WORK/mesh-keys.txt"
if [ ! -s "$WORK/mesh-keys.txt" ]; then
    echo "error: could not read MeshConfig fields from $MESH_SRC" >&2
    exit 2
fi

keys=0
while IFS= read -r row; do
    loc="${row%%:*}"; rest="${row#*:}"; line="${rest%%:*}"; cmd="${rest#*:}"
    if [ "${cmd:0:1}" = "@" ]; then
        key="${cmd:1}"
        keys=$((keys + 1))
        grep -qx -- "$key" "$WORK/mesh-keys.txt" || report "$loc:$line" "[kernel.mesh] key \`$key\` is not a MeshConfig field ($MESH_SRC)"
    else
        check_command "$loc:$line" "$cmd"
    fi
done < <(sort -t: -k1,1 -k2,2n -k3 -u "$WORK/all.txt")

echo "checked $checked commands and $keys [kernel.mesh] keys against $WEAVER and $WEFT: $fail drift finding(s)"
[ "$fail" -eq 0 ]
