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
# approvals-change: rows added to config/diff-gate-approvals.toml pass when their
# canonical sha256 is in the owner ledger (config/diff-gate-owner-ledger.toml) AS IT IS
# ON THE BASE. Removed or edited rows, and any change to the ledger, need the owner flag.
#
# Usage: scripts/diff-gate.sh [--json] [--worktree] [--base REF] [--head REF]
#        scripts/diff-gate.sh --ledger-hash [FILE]
#   --json        machine-readable findings on stdout
#   --worktree    compare the working tree (tracked changes + untracked files) with the
#                 base instead of HEAD; this is what `scripts/build.sh gate` uses
#   --base REF    same as DIFF_GATE_BASE
#   --ledger-hash print "sha256 <TAB> rule <TAB> path <TAB> pattern" for each row of FILE
#                 (default: the approvals file), for the owner to build ledger entries
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
LEDGER="config/diff-gate-owner-ledger.toml"
JSON=0
LEDGER_HASH=""
WORKTREE=0
BASE_REF="${DIFF_GATE_BASE:-}"
HEAD_REF="${DIFF_GATE_HEAD:-HEAD}"

while [ $# -gt 0 ]; do
    case "$1" in
        --json) JSON=1 ;;
        --worktree) WORKTREE=1 ;;
        --base) BASE_REF="${2:-}"; shift ;;
        --head) HEAD_REF="${2:-}"; shift ;;
        --ledger-hash) LEDGER_HASH="$APPROVALS"; case "${2:-}" in ""|-*) ;; *) LEDGER_HASH="$2"; shift ;; esac ;;
        -h|--help) sed -n '2,37p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "diff-gate: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

TMP="$(mktemp -d)" || exit 2
trap 'rm -rf "$TMP"' EXIT

# ── Approvals and ledger parsing ─────────────────────────────────────
# parse_approvals: TOML on stdin -> rule <TAB> glob <TAB> pattern <TAB> reason <TAB> approved_by <TAB> line
parse_approvals() {
    awk -v sq="'" '
    function val(s,   v) {
        v = s; sub(/^[^=]*=[ \t]*/, "", v)
        if (v ~ /^"/) { sub(/^"/, "", v); sub(/"[ \t]*(#.*)?$/, "", v); gsub(/\\"/, "\"", v); gsub(/\\\\/, "\\", v) }
        else if (substr(v, 1, 1) == sq) { v = substr(v, 2); sub(sq "[ \t]*(#.*)?$", "", v) }
        gsub(/\t/, " ", v)
        return v
    }
    function flush() {
        if (!open) return
        if (r == "" || p == "" || pt == "" || re == "" || ab == "") {
            print "diff-gate: malformed approval entry near line " start " (needs rule, path, pattern, reason, approved_by)" > "/dev/stderr"; bad = 1
        } else print r "\t" p "\t" pt "\t" re "\t" ab "\t" start
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
    '
}

# parse_ledger: TOML on stdin -> sha256 <TAB> decision <TAB> date. Each [[entry]] needs
# decision, date (YYYY-MM-DD) and sha256 (64 lowercase hex); anything else is a setup error.
parse_ledger() {
    awk '
    function val(s,   v) { v = s; sub(/^[^=]*=[ \t]*/, "", v); sub(/^["\047]/, "", v); sub(/["\047][ \t]*(#.*)?$/, "", v); gsub(/\t/, " ", v); return v }
    function flush() {
        if (!open) return
        if (d == "" || dt !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/ || h !~ /^[0-9a-f]+$/ || length(h) != 64) {
            print "diff-gate: malformed owner ledger entry near line " start " (needs decision, date YYYY-MM-DD, sha256)" > "/dev/stderr"; bad = 1
        } else print h "\t" d "\t" dt
        open = 0
    }
    /^[ \t]*#/ || /^[ \t]*$/ { next }
    /^[ \t]*\[\[entry\]\]/ { flush(); open = 1; start = NR; d = dt = h = ""; next }
    /^[ \t]*decision[ \t]*=/ { d = val($0); next }
    /^[ \t]*date[ \t]*=/ { dt = val($0); next }
    /^[ \t]*sha256[ \t]*=/ { h = val($0); next }
    /^[ \t]*[A-Za-z_]+[ \t]*=/ { next }
    { print "diff-gate: unexpected line " NR " in owner ledger" > "/dev/stderr"; bad = 1 }
    END { flush(); exit bad }
    '
}

sha256_stdin() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then shasum -a 256 | cut -d' ' -f1
    elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 -r | cut -d' ' -f1
    else echo "diff-gate: no sha256 tool (sha256sum, shasum or openssl)" >&2; return 1; fi
}

# hash_rows: parse_approvals output on stdin -> sha256 <TAB> line <TAB> rule <TAB> glob <TAB> pattern
# The canonical row text is "rule=R\npath=P\npattern=X\nreason=Y\napproved_by=Z\n" over the
# parsed values, so quoting style and key order do not change the hash; any value change does.
hash_rows() {
    local r g pt re ab ln h
    while IFS="$(printf '\t')" read -r r g pt re ab ln; do
        h="$(printf 'rule=%s\npath=%s\npattern=%s\nreason=%s\napproved_by=%s\n' "$r" "$g" "$pt" "$re" "$ab" | sha256_stdin)" || return 1
        printf '%s\t%s\t%s\t%s\t%s\n' "$h" "$ln" "$r" "$g" "$pt"
    done
}

if [ -n "$LEDGER_HASH" ]; then
    [ -f "$LEDGER_HASH" ] || { echo "diff-gate: $LEDGER_HASH not found" >&2; exit 2; }
    parse_approvals <"$LEDGER_HASH" >"$TMP/lh" || exit 2
    hash_rows <"$TMP/lh" | cut -f1,3- || exit 2
    exit 0
fi

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
    old = parts[1]; sub(/^diff --git a\//, "", old)
    inhunk = 0; skipc = 0
    if (cur ~ /^\.github\/workflows\// || cur == "scripts/diff-gate.sh") emit("ci-workflow", cur, 0, "file changed")
    else if (old ~ /^\.github\/workflows\// || old == "scripts/diff-gate.sh") emit("ci-workflow", old, 0, "file moved away")
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
    parse_approvals <"$APPROVALS" >"$TMP/approvals.raw" || { echo "diff-gate: cannot use $APPROVALS" >&2; exit 2; }
    while IFS="$(printf '\t')" read -r r g pt re ab _; do
        lit=0
        if [ "$pt" != "*" ]; then
            printf '' | grep -Eq -e "$pt" 2>/dev/null; [ $? -eq 2 ] && lit=1
        fi
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$r" "$g" "$pt" "$lit" "$re" "$ab"
    done <"$TMP/approvals.raw" >"$TMP/approvals"
fi

# ── approvals-change: approvals rows and the owner ledger ────────────
# Findings here carry a 5th field: the base-ledger decision id that clears an added row.
head_src="$HEAD_SHA"; [ "$WORKTREE" = 1 ] && head_src=WORKTREE
show_at "$BASE" "$LEDGER" >"$TMP/ledger.base.toml"
show_at "$head_src" "$LEDGER" >"$TMP/ledger.head.toml"
parse_ledger <"$TMP/ledger.base.toml" >"$TMP/ledger" || { echo "diff-gate: cannot use $LEDGER on the base" >&2; exit 2; }
parse_ledger <"$TMP/ledger.head.toml" >/dev/null || { echo "diff-gate: cannot use $LEDGER" >&2; exit 2; }
if ! cmp -s "$TMP/ledger.base.toml" "$TMP/ledger.head.toml"; then
    printf 'approvals-change\t%s\t0\towner ledger changed\n' "$LEDGER" >>"$TMP/findings"
fi
show_at "$BASE" "$APPROVALS" >"$TMP/appr.base.toml"
show_at "$head_src" "$APPROVALS" >"$TMP/appr.head.toml"
if ! cmp -s "$TMP/appr.base.toml" "$TMP/appr.head.toml"; then
    { parse_approvals <"$TMP/appr.base.toml" || echo "diff-gate: base $APPROVALS is malformed; its malformed entries are ignored" >&2; } >"$TMP/appr.base.raw"
    parse_approvals <"$TMP/appr.head.toml" >"$TMP/appr.head.raw" || { echo "diff-gate: cannot use $APPROVALS" >&2; exit 2; }
    hash_rows <"$TMP/appr.base.raw" >"$TMP/appr.base.h" || exit 2
    hash_rows <"$TMP/appr.head.raw" >"$TMP/appr.head.h" || exit 2
    awk -F'\t' -v path="$APPROVALS" '
    function desc(r, g, p,   t) { t = "rule=" r " path=" g " pattern=" p; if (length(t) > 140) t = substr(t, 1, 137) "..."; return t }
    FILENAME ~ /ledger$/ { led[$1] = $2; next }
    FILENAME ~ /base\.h$/ { bc[$1]++; bd[$1] = desc($3, $4, $5); next }
    { hc[$1]++; if (hc[$1] > bc[$1]) printf "approvals-change\t%s\t%s\trow added: %s\t%s\n", path, $2, desc($3, $4, $5), (($1 in led) ? led[$1] : "") }
    END { for (h in bc) for (i = hc[h] + 0; i < bc[h]; i++) printf "approvals-change\t%s\t0\trow removed or edited: %s\t\n", path, bd[h] }
    ' "$TMP/ledger" "$TMP/appr.base.h" "$TMP/appr.head.h" >>"$TMP/findings"
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
        else if ($5 != "") { status = "approved"; detail = "owner ledger (base): " $5 }
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
