#!/usr/bin/env bash
# diff-gate.sh — pre-build static gate over what a diff ADDS.
#
# Runs before any crate compiles (check 0 of `scripts/build.sh gate`, and the first
# job of the PR gates workflow) so nothing is built on a rejected diff. The idea of
# a static, pre-build gate over the diff comes from ruvnet/rune (MIT); this is an
# independent reimplementation, no Rune code is used. See
# docs/research/rune-evaluation-2026-10-29.md and docs/development/diff-gate.md.
#
# Only added lines and new files are inspected. Existing code never fails the gate.
#
# Rules: unsafe, new-dep, build-script, proc-macro, net, process, ffi, ci-workflow,
#        approvals-change.
#
# Usage: scripts/diff-gate.sh [--json] [--worktree] [--base REF] [--head REF]
#   --json       machine-readable findings on stdout
#   --worktree   compare the working tree (tracked changes + untracked files) with the
#                base instead of HEAD; this is what `scripts/build.sh gate` uses
#   --base REF   same as DIFF_GATE_BASE
# Environment:
#   DIFF_GATE_BASE            base commit-ish; the merge-base with HEAD is used. Default:
#                             newest origin/<AGF_TARGET_PATTERN> (config/agent-git.env),
#                             else origin/master.
#   DIFF_GATE_OWNER_APPROVED  1 lets a change to config/diff-gate-approvals.toml pass.
#                             Owner only. Agents must never set it.
#   GITHUB_STEP_SUMMARY       if set, a findings table is appended to it.
# Exit: 0 clean (approved findings allowed), 1 findings, 2 usage or setup error.

set -uo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "diff-gate: not in a git repo" >&2; exit 2; }
cd "$ROOT" || exit 2

APPROVALS="config/diff-gate-approvals.toml"
JSON=0
WORKTREE=0
BASE_REF="${DIFF_GATE_BASE:-}"
HEAD_REF="${DIFF_GATE_HEAD:-HEAD}"

while [ $# -gt 0 ]; do
    case "$1" in
        --json) JSON=1 ;;
        --worktree) WORKTREE=1 ;;
        --base) BASE_REF="${2:-}"; shift ;;
        --head) HEAD_REF="${2:-}"; shift ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "diff-gate: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

TMP="$(mktemp -d)" || exit 2
trap 'rm -rf "$TMP"' EXIT

# ── Base resolution ──────────────────────────────────────────────────
default_base_ref() {
    local pattern="" f
    for f in .agent-git.env config/agent-git.env; do
        [ -f "$f" ] || continue
        pattern="$(grep -h '^AGF_TARGET_PATTERN=' "$f" | tail -1 | sed -e "s/^AGF_TARGET_PATTERN=//" -e "s/^['\"]//" -e "s/['\"].*$//")"
        [ -n "$pattern" ] && break
    done
    [ -n "$pattern" ] || pattern='target-0.8.*'
    local newest
    newest="$(git for-each-ref --sort=-version:refname --format='%(refname:short)' "refs/remotes/origin/$pattern" | head -1)"
    if [ -n "$newest" ]; then echo "$newest"; else echo "origin/master"; fi
}

if [ -z "$BASE_REF" ] || echo "$BASE_REF" | grep -Eq '^0+$'; then
    BASE_REF="$(default_base_ref)"
fi
if ! git rev-parse --verify -q "$BASE_REF^{commit}" >/dev/null; then
    echo "diff-gate: base '$BASE_REF' not found (fetch it, or set DIFF_GATE_BASE)" >&2
    exit 2
fi
if ! git rev-parse --verify -q "$HEAD_REF^{commit}" >/dev/null; then
    echo "diff-gate: head '$HEAD_REF' not found" >&2
    exit 2
fi
BASE="$(git merge-base "$BASE_REF" "$HEAD_REF" 2>/dev/null || git rev-parse "$BASE_REF^{commit}")"
HEAD_SHA="$(git rev-parse "$HEAD_REF^{commit}")"

# ── Diff ─────────────────────────────────────────────────────────────
DIFF_OPTS=(-U0 --no-color --no-ext-diff -M)
if [ "$WORKTREE" = 1 ]; then
    git diff "${DIFF_OPTS[@]}" "$BASE" -- >"$TMP/diff" 2>/dev/null
    git ls-files -o --exclude-standard | while IFS= read -r f; do
        git diff --no-index "${DIFF_OPTS[@]}" -- /dev/null "$f" 2>/dev/null
    done >>"$TMP/diff"
else
    git diff "${DIFF_OPTS[@]}" "$BASE" "$HEAD_SHA" -- >"$TMP/diff" 2>/dev/null
fi

# ── Pass 1: added lines and new files ────────────────────────────────
# Emits: rule <TAB> path <TAB> line <TAB> text
awk -v cargo_list="$TMP/cargo-paths" -v sq="'" -v addr_types='SocketAddrV4|SocketAddrV6|SocketAddr|IpAddr|Ipv4Addr|Ipv6Addr|AddrParseError' '
function emit(rule, path, ln, text) {
    gsub(/\t/, " ", text); sub(/^[ \t]+/, "", text)
    if (length(text) > 160) text = substr(text, 1, 157) "..."
    print rule "\t" path "\t" ln "\t" text
}
/^diff --git / {
    n = split($0, parts, " b/"); cur = parts[n]
    inhunk = 0; skipc = 0
    if (cur ~ /^\.github\/workflows\// || cur == "scripts/diff-gate.sh") emit("ci-workflow", cur, 0, "file changed")
    if (cur == "config/diff-gate-approvals.toml") emit("approvals-change", cur, 0, "file changed")
    if (cur ~ /(^|\/)Cargo\.toml$/ && !(cur in seen)) { seen[cur] = 1; print cur > cargo_list }
    next
}
!inhunk && /^new file mode/ { if (cur ~ /(^|\/)build\.rs$/) emit("build-script", cur, 0, "new build.rs"); next }
!inhunk && /^\+\+\+ \/dev\/null/ { skipc = 1; next }
!inhunk && /^(\+\+\+|---) / { next }
/^@@ / { s = $0; sub(/^@@ -[0-9,]+ \+/, "", s); ln = s + 0; inhunk = 1; next }
inhunk && !skipc && /^\+/ {
    text = substr($0, 2); this = ln; ln++
    if (cur ~ /(^|\/)Cargo\.toml$/) {
        t = text; sub(/^[ \t]+/, "", t)
        if (t ~ /^build[ \t]*=/ && t !~ /^build[ \t]*=[ \t]*false/) emit("build-script", cur, this, text)
        if (t ~ /^proc[-_]macro[ \t]*=[ \t]*true/) emit("proc-macro", cur, this, text)
        next
    }
    if (cur !~ /\.rs$/) next
    if (text ~ /^[ \t]*(\/\/|\/\*|\*)/) next
    ns = text
    gsub(sq "\"" sq, "q", ns)
    gsub(/"([^"\\]|\\.)*"/, "\"\"", ns)
    gsub(/\/\*[^*]*\*\//, "", ns)
    sub(/\/\/.*$/, "", ns)
    # Address types do no I/O; only sockets and clients count as network use.
    nt = ns
    if (match(nt, /std::net::\{[^}]*\}/)) {
        inner = substr(nt, RSTART, RLENGTH)
        gsub(addr_types, "", inner)
        if (inner ~ /^std::net::\{[ ,]*\}$/) inner = ""
        nt = substr(nt, 1, RSTART - 1) inner substr(nt, RSTART + RLENGTH)
    }
    gsub("std::net::(" addr_types ")", "", nt)
    if (ns ~ /(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)/) emit("unsafe", cur, this, text)
    if (nt ~ /std::net|TcpStream|TcpListener|UdpSocket|reqwest::|ureq::|hyper::Client/) emit("net", cur, this, text)
    if (ns ~ /std::process::Command|Command::new|tokio::process|libc::(exec|fork)|std::process::\{[^}]*Command/) emit("process", cur, this, text)
    if ((ns ~ /(^|[^A-Za-z0-9_])extern[ \t]+""/ && text ~ /extern[ \t]+"C/) || ns ~ /no_mangle/) emit("ffi", cur, this, text)
    next
}
inhunk && /^[ -]/ { next }
' "$TMP/diff" >"$TMP/findings"

# ── Pass 2: Cargo.toml dependency entries (needs the full file, not hunks) ──
# Emits per entry: key <TAB> source <TAB> workspace=true <TAB> line <TAB> text
cargo_entries() {
    awk '
    function flush() {
        if (intab) print key "\t" src "\t" ws "\t" tl "\t" tt
        intab = 0
    }
    function sources(t,   m, s) {
        s = ""
        if (match(t, /(^|[^A-Za-z_-])git[ \t]*=[ \t]*"[^"]*"/)) s = s substr(t, RSTART, RLENGTH)
        if (match(t, /(^|[^A-Za-z_-])path[ \t]*=[ \t]*"[^"]*"/)) s = s substr(t, RSTART, RLENGTH)
        return s
    }
    { t = $0; sub(/^[ \t]+/, "", t); sub(/[ \t]+$/, "", t) }
    t ~ /^\[/ {
        flush(); mode = ""
        h = t; sub(/^\[+[ \t]*/, "", h); sub(/[ \t]*\]+.*$/, "", h)
        if (h ~ /^package\.metadata/) next
        if (h ~ /(^|\.)(dev-|build-)?dependencies$/) { mode = "list"; sec = h }
        else if (h ~ /(^|\.)(dev-|build-)?dependencies\.[^.]+$/) {
            mode = "tab"; name = h; sub(/.*dependencies\./, "", name)
            sec = substr(h, 1, length(h) - length(name) - 1)
            key = sec "|" name; intab = 1; src = ""; ws = 0; tl = NR; tt = t
        }
        next
    }
    mode == "list" && t ~ /^[A-Za-z0-9_-]+([.][A-Za-z_-]+)?[ \t]*=/ {
        name = t; sub(/[ \t.=].*$/, "", name)
        print sec "|" name "\t" sources(t) "\t" (t ~ /workspace[ \t]*=[ \t]*true/ ? 1 : 0) "\t" NR "\t" t
        next
    }
    mode == "tab" {
        src = src sources(t)
        if (t ~ /^workspace[ \t]*=[ \t]*true/) ws = 1
    }
    END { flush() }
    '
}

show_at() { # <rev|WORKTREE> <path>
    if [ "$1" = WORKTREE ]; then cat "$2" 2>/dev/null; else git show "$1:$2" 2>/dev/null; fi
}

if [ -s "$TMP/cargo-paths" ]; then
    head_src="$HEAD_SHA"; [ "$WORKTREE" = 1 ] && head_src=WORKTREE
    git show "$BASE:Cargo.toml" 2>/dev/null | cargo_entries | grep '^workspace\.dependencies|' | cut -f1 >"$TMP/ws-root" || true
    while IFS= read -r p; do
        show_at "$BASE" "$p" | cargo_entries >"$TMP/base-entries"
        show_at "$head_src" "$p" | cargo_entries >"$TMP/head-entries"
        awk -F'\t' -v path="$p" '
        FILENAME ~ /ws-root$/ { wsroot[$1] = 1; next }
        FILENAME ~ /base-entries$/ { bsrc[$1] = bsrc[$1] $2; inbase[$1] = 1; next }
        {
            if (!($1 in hsrc)) { order[++n] = $1; hline[$1] = $4; htext[$1] = $5 }
            hsrc[$1] = hsrc[$1] $2; hws[$1] = hws[$1] || $3
        }
        END {
            for (i = 1; i <= n; i++) {
                k = order[i]
                if (!(k in inbase)) {
                    if (hws[k] && (("workspace.dependencies|" substr(k, index(k, "|") + 1)) in wsroot)) continue
                    printf "new-dep\t%s\t%s\tnew entry in [%s]: %s\n", path, hline[k], substr(k, 1, index(k, "|") - 1), htext[k]
                } else if (hsrc[k] != bsrc[k]) {
                    printf "new-dep\t%s\t%s\tsource changed in [%s]: %s\n", path, hline[k], substr(k, 1, index(k, "|") - 1), htext[k]
                }
            }
        }' "$TMP/ws-root" "$TMP/base-entries" "$TMP/head-entries" >>"$TMP/findings"
    done <"$TMP/cargo-paths"
fi

# ── Approvals ────────────────────────────────────────────────────────
# Emits: rule <TAB> glob <TAB> pattern <TAB> literal(0|1) <TAB> reason <TAB> approved_by
: >"$TMP/approvals"
if [ -f "$APPROVALS" ]; then
    awk -v sq="'" '
    function val(s,   v) {
        v = s; sub(/^[^=]*=[ \t]*/, "", v)
        if (v ~ /^"/) { sub(/^"/, "", v); sub(/"[ \t]*(#.*)?$/, "", v); gsub(/\\"/, "\"", v); gsub(/\\\\/, "\\", v) }
        else if (substr(v, 1, 1) == sq) { v = substr(v, 2); sub(sq "[ \t]*(#.*)?$", "", v) }
        return v
    }
    function flush() {
        if (!open) return
        if (r == "" || p == "" || pt == "" || re == "" || ab == "") {
            print "diff-gate: malformed approval entry near line " start " (needs rule, path, pattern, reason, approved_by)" > "/dev/stderr"; bad = 1
        } else print r "\t" p "\t" pt "\t" re "\t" ab
        open = 0
    }
    /^[ \t]*#/ || /^[ \t]*$/ { next }
    /^[ \t]*\[\[approval\]\]/ { flush(); open = 1; start = NR; r = p = pt = re = ab = ""; next }
    /^[ \t]*rule[ \t]*=/ { r = val($0); next }
    /^[ \t]*path[ \t]*=/ { p = val($0); next }
    /^[ \t]*pattern[ \t]*=/ { pt = val($0); next }
    /^[ \t]*reason[ \t]*=/ { re = val($0); next }
    /^[ \t]*approved_by[ \t]*=/ { ab = val($0); next }
    END { flush(); exit bad }
    ' "$APPROVALS" >"$TMP/approvals.raw" || { echo "diff-gate: cannot use $APPROVALS" >&2; exit 2; }
    while IFS="$(printf '\t')" read -r r g pt re ab; do
        lit=0
        if [ "$pt" != "*" ]; then
            printf '' | grep -Eq -e "$pt" 2>/dev/null; [ $? -eq 2 ] && lit=1
        fi
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$r" "$g" "$pt" "$lit" "$re" "$ab"
    done <"$TMP/approvals.raw" >"$TMP/approvals"
fi

# ── Classify: approved vs failed ─────────────────────────────────────
# Emits: status <TAB> rule <TAB> path <TAB> line <TAB> text <TAB> detail
awk -F'\t' -v owner="${DIFF_GATE_OWNER_APPROVED:-0}" -v apf="$TMP/approvals" '
function g2r(g,   r, i, c, n) {
    r = "^"; n = length(g)
    for (i = 1; i <= n; i++) {
        c = substr(g, i, 1)
        if (c == "*") {
            if (substr(g, i + 1, 1) == "*") {
                i++
                if (substr(g, i + 1, 1) == "/") { i++; r = r "(.*/)?" } else r = r ".*"
            } else r = r "[^/]*"
        } else if (c == "?") r = r "[^/]"
        else if (index("\\.+(){}|^$[]", c) > 0) r = r "\\" c
        else r = r c
    }
    return r "$"
}
FILENAME == apf { na++; ar[na] = $1; ag[na] = g2r($2); ap[na] = $3; al[na] = $4; aw[na] = $5; ab[na] = $6; next }
{
    status = "FAIL"; detail = ""
    if ($1 == "approvals-change") {
        if (owner == "1") { status = "approved"; detail = "DIFF_GATE_OWNER_APPROVED=1" }
        else detail = "owner only: set DIFF_GATE_OWNER_APPROVED=1 (CI: diff-gate-approved label)"
    } else {
        for (i = 1; i <= na; i++) {
            if (ar[i] != $1 || $2 !~ ag[i]) continue
            if (ap[i] == "*" || index($4, ap[i]) > 0 || (al[i] == 0 && $4 ~ ap[i])) {
                status = "approved"; detail = ab[i] ": " aw[i]; break
            }
        }
    }
    print status "\t" $1 "\t" $2 "\t" $3 "\t" $4 "\t" detail
}
' "$TMP/approvals" "$TMP/findings" | sort -t"$(printf '\t')" -k1,1 -k2,2 -k3,3 -k4,4n >"$TMP/classified"

# ── Report ───────────────────────────────────────────────────────────
nfail="$(grep -c '^FAIL' "$TMP/classified" || true)"
napp="$(grep -c '^approved' "$TMP/classified" || true)"
nfiles="$(grep -c '^diff --git ' "$TMP/diff" || true)"
base_label="$(git rev-parse --short "$BASE")"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        echo "### diff-gate"
        echo "Base \`$base_label\`, head \`$(git rev-parse --short "$HEAD_SHA")\`, $nfiles file(s) changed: **$nfail failed**, $napp approved."
        if [ -s "$TMP/classified" ]; then
            echo; echo "| Status | Rule | Location | Added line | Note |"; echo "|---|---|---|---|---|"
            awk -F'\t' '{ t = $5; gsub(/\|/, "\\|", t); d = $6; gsub(/\|/, "\\|", d)
                print "| " $1 " | `" $2 "` | `" $3 ":" $4 "` | `" t "` | " d " |" }' "$TMP/classified"
        fi
    } >>"$GITHUB_STEP_SUMMARY"
fi

if [ "$JSON" = 1 ]; then
    awk -F'\t' -v base="$(git rev-parse "$BASE")" -v head="$HEAD_SHA" -v nfail="$nfail" -v napp="$napp" '
    function j(s) { gsub(/\\/, "\\\\", s); gsub(/"/, "\\\"", s); return "\"" s "\"" }
    BEGIN { printf "{\"base\":%s,\"head\":%s,\"failed\":%d,\"approved\":%d,\"findings\":[", j(base), j(head), nfail, napp }
    { printf "%s{\"status\":%s,\"rule\":%s,\"path\":%s,\"line\":%d,\"text\":%s,\"detail\":%s}", (NR > 1 ? "," : ""), j($1), j($2), j($3), $4, j($5), j($6) }
    END { print "]}" }' "$TMP/classified"
else
    echo "diff-gate: base $base_label, $nfiles file(s) changed, $nfail failed, $napp approved"
    awk -F'\t' '{ s = ($1 == "FAIL" ? "FAIL    " : "approved"); printf "  %s %-16s %s:%s\n            %s\n", s, $2, $3, $4, $5; if ($6 != "") printf "            -> %s\n", $6 }' "$TMP/classified"
    if [ "$nfail" -gt 0 ]; then
        echo "diff-gate: REJECTED. Fix the diff, or ask the owner to approve in $APPROVALS (docs/development/diff-gate.md)."
    else
        echo "diff-gate: OK"
    fi
fi

[ "$nfail" -eq 0 ]
