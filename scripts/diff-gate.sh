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
# Rules: unsafe, new-dep, build-script, proc-macro, net, process, ffi, include, ci-workflow,
#        gitattributes-change, toolchain-config, special-file, approvals-change.
#
# The diff is taken with --text --no-textconv --no-renames and pinned prefixes, quoting and
# colour, so .gitattributes and git config cannot hide or misname hunks; any .gitattributes
# change is itself a finding. File paths come from `git diff --raw -z`. A git failure, or a
# diff the parser does not recognise, exits 2.
#
# approvals-change: rows added to config/diff-gate-approvals.toml pass when their
# canonical sha256 is in the owner ledger (config/diff-gate-owner-ledger.toml) AS IT IS
# AT THE TIP OF THE BASE REF, so a revoke there takes effect at once. Removed or edited
# rows, and any change to the ledger, need the owner flag. Approvals are read from the
# head commit (the working tree only with --worktree).
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
export LC_ALL=C   # byte semantics for awk, sort and grep, whatever the caller's locale
unset GIT_DIFF_OPTS GIT_EXTERNAL_DIFF

die() { echo "diff-gate: $*" >&2; exit 2; }

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
        -h|--help) sed -n '2,/^set -uo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "diff-gate: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

TMP="$(mktemp -d)" || die "mktemp failed"
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
BASE_TIP="$(git rev-parse "$BASE_REF^{commit}")"
BASE="$(git merge-base "$BASE_TIP" "$HEAD_REF" 2>/dev/null || echo "$BASE_TIP")"
HEAD_SHA="$(git rev-parse "$HEAD_REF^{commit}")"

# ── Diff ─────────────────────────────────────────────────────────────
# Every git call that produces diff text pins what changes its format: prefixes (diff.noprefix,
# diff.mnemonicPrefix, diff.srcPrefix/dstPrefix), quoting, colour, relative paths, renames,
# context, external and textconv drivers, submodule format and attributes (-diff, binary).
# --no-renames: a renamed or copied file is a deletion plus a new file, so its whole content
# is scanned (a .txt renamed to .rs, or to build.rs, is new code).
GITC=(git -c core.quotePath=false -c color.ui=false -c diff.noprefix=false -c diff.mnemonicPrefix=false)
COMMON=(--no-renames --no-ext-diff --no-textconv --no-relative --ignore-submodules=none)
DIFF_OPTS=("${COMMON[@]}" -U0 --inter-hunk-context=0 --no-color --text --src-prefix=a/ --dst-prefix=b/ --submodule=short)
RAW_OPTS=("${COMMON[@]}" --raw -z)

# add_entry <status> <oldmode> <newmode> <path>: file list for path rules; the patch must carry
# one "diff --git" header per entry, two for a type change.
add_entry() {
    case "$4" in *[[:cntrl:]]*) die "path with a control character in the diff (refused)";; esac
    printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >>"$TMP/files"
    [ -n "${5:-}" ] && return 0
    case "$1" in D|A|M) echo "$4" >>"$TMP/hdrs" ;; T) printf '%s\n%s\n' "$4" "$4" >>"$TMP/hdrs" ;; *) die "unexpected diff status '$1' for $4" ;; esac
}
read_raw() { # git diff --raw -z output on stdin
    local meta path
    while IFS= read -r -d '' meta; do
        IFS= read -r -d '' path || die "truncated --raw diff"
        set -- $meta
        [ $# -eq 5 ] || die "unexpected --raw line: $meta"
        add_entry "$5" "${1#:}" "$2" "$path"
    done
}
: >"$TMP/files"; : >"$TMP/hdrs"
if [ "$WORKTREE" = 1 ]; then
    "${GITC[@]}" diff "${RAW_OPTS[@]}" "$BASE" -- >"$TMP/raw" || die "git diff --raw failed"
    read_raw <"$TMP/raw"
    "${GITC[@]}" diff "${DIFF_OPTS[@]}" "$BASE" -- >"$TMP/diff" || die "git diff failed"
    git ls-files -z -o --exclude-standard >"$TMP/untracked" || die "git ls-files failed"
    while IFS= read -r -d '' f; do
        if [ -d "$f" ]; then add_entry A 000000 160000 "${f%/}" nohdr; continue; fi
        mode=100644; [ -L "$f" ] && mode=120000
        add_entry A 000000 "$mode" "$f"
        "${GITC[@]}" diff --no-index "${DIFF_OPTS[@]}" -- /dev/null "$f" >>"$TMP/diff"
        [ $? -le 1 ] || die "git diff --no-index failed for $f"
    done <"$TMP/untracked"
else
    "${GITC[@]}" diff "${RAW_OPTS[@]}" "$BASE" "$HEAD_SHA" -- >"$TMP/raw" || die "git diff --raw failed"
    read_raw <"$TMP/raw"
    "${GITC[@]}" diff "${DIFF_OPTS[@]}" "$BASE" "$HEAD_SHA" -- >"$TMP/diff" || die "git diff failed"
fi
# A NUL byte would end an awk record early and desync the lexer; make it a plain control byte.
tr '\000' '\001' <"$TMP/diff" >"$TMP/diff.t" && mv "$TMP/diff.t" "$TMP/diff" || die "tr failed"

# ── Pass 1: path rules, Cargo.toml keys, added .rs lines ─────────────
# Findings: rule <TAB> path <TAB> line <TAB> text. Added lines of .rs files go to rs-added
# (path <TAB> line <TAB> text) for the lexer stage. Header paths are checked, in order,
# against the -z file list; any mismatch is a setup error (exit 2).
awk -v cargo_list="$TMP/cargo-paths" -v rs_out="$TMP/rs-added" -v flist="$TMP/files" -v hlist="$TMP/hdrs" '
function emit(rule, path, ln, text) {
    gsub(/\t/, " ", text); sub(/^[ \t]+/, "", text)
    if (length(text) > 160) text = substr(text, 1, 157) "..."
    print rule "\t" path "\t" ln "\t" text
}
function bad(msg) { print "diff-gate: " msg > "/dev/stderr"; err = 1; exit 3 }
function cunq(q,   out, i, c, n, v, nd) { # C-style unquote of the inside of a git-quoted name
    out = ""; n = length(q)
    for (i = 1; i <= n; i++) {
        c = substr(q, i, 1)
        if (c != "\\") { out = out c; continue }
        c = substr(q, ++i, 1)
        if (c ~ /[0-7]/) { v = c + 0; nd = 1; while (nd < 3 && substr(q, i + 1, 1) ~ /[0-7]/) { i++; nd++; v = v * 8 + substr(q, i, 1) } out = out sprintf("%c", v) }
        else if (c == "t") out = out "\t"; else if (c == "n") out = out "\n"; else out = out c
    }
    return out
}
function hdrpath(rest,   L, h, p1, p2) { # "a/P b/P" or "\"a/Q\" \"b/Q\"" -> P
    L = length(rest)
    if ((L - 1) % 2) bad("cannot parse diff header: " rest)
    h = (L - 1) / 2; p1 = substr(rest, 1, h); p2 = substr(rest, h + 2)
    if (substr(rest, h + 1, 1) != " ") bad("cannot parse diff header: " rest)
    if (p1 ~ /^"a\// && p2 ~ /^"b\// && substr(p1, 4) == substr(p2, 4) && p1 ~ /"$/) return cunq(substr(p1, 4, length(p1) - 4))
    if (p1 ~ /^a\// && p2 ~ /^b\// && substr(p1, 3) == substr(p2, 3)) return substr(p1, 3)
    bad("cannot parse diff header: " rest)
}
FILENAME == hlist { nh++; hp[nh] = $0; next }
FILENAME == flist {
    split($0, f, "\t"); st = f[1]; nm = f[3]; p = f[4]; lp = tolower(p)
    if (p ~ /^\.github\/workflows\// || p == "scripts/diff-gate.sh") emit("ci-workflow", p, 0, st == "D" ? "file deleted or moved away" : "file changed")
    if (lp ~ /(^|\/)\.gitattributes$/) emit("gitattributes-change", p, 0, st == "D" ? "file deleted or moved away" : "file changed")
    if (lp ~ /(^|\/)\.cargo\/config/ || lp ~ /(^|\/)rust-toolchain(\.toml)?$/) emit("toolchain-config", p, 0, st == "D" ? "file deleted" : "file changed")
    if (st != "D" && nm == "120000") emit("special-file", p, 0, "symlink added or changed")
    if (st != "D" && nm == "160000") emit("special-file", p, 0, "submodule or nested repository added or changed")
    if ((st == "A" || st == "T") && lp ~ /(^|\/)build\.rs$/) emit("build-script", p, 0, "new build.rs")
    if (lp ~ /(^|\/)cargo\.toml$/ && !(p in seen)) { seen[p] = 1; print p > cargo_list }
    next
}
/^diff --git / {
    cur = hdrpath(substr($0, 12)); hi++
    if (hi > nh || hp[hi] != cur) bad("diff header " hi " (" cur ") does not match the file list")
    lcur = tolower(cur); iscargo = (lcur ~ /(^|\/)cargo\.toml$/); isrs = (lcur ~ /\.rs$/)
    inhunk = 0; skipc = 0; next
}
!inhunk && /^\+\+\+ \/dev\/null/ { skipc = 1; next }
!inhunk && /^(\+\+\+|---) / { next }
/^@@ / { s = $0; sub(/^@@ -[0-9,]+ \+/, "", s); ln = s + 0; inhunk = 1; next }
inhunk && !skipc && /^\+/ {
    text = substr($0, 2); this = ln; ln++
    if (iscargo) {
        t = text
        # build / proc-macro keys anywhere: bare, quoted, dotted, or inside an inline table
        if (match(t, /(^|[{,. \t])["\047]?build["\047]?[ \t]*=/) && substr(t, RSTART + RLENGTH) !~ /^[ \t]*false/) emit("build-script", cur, this, text)
        if (t ~ /(^|[{,. \t])["\047]?proc[-_]macro["\047]?[ \t]*=[ \t]*true/) emit("proc-macro", cur, this, text)
        if (t ~ /"[^"]*\\[^"]*"[ \t]*[.=]/ || t ~ /^[ \t]*\[.*\\/) emit("build-script", cur, this, "escaped key: " text)
        next
    }
    if (isrs) { gsub(/\t/, " ", text); print cur "\t" this "\t" text > rs_out }
    next
}
inhunk && /^ / { ln++; next }
END { if (!err && hi != nh) { print "diff-gate: diff has " hi " file header(s), file list expects " nh > "/dev/stderr"; exit 3 } }
' "$TMP/hdrs" "$TMP/files" "$TMP/diff" >"$TMP/findings" || die "pass 1 failed (diff could not be parsed)"
: >>"$TMP/rs-added"

show_at() { # <rev|WORKTREE> <path>: absent -> nothing; present but unreadable -> exit 2
    if [ "$1" = WORKTREE ]; then
        [ -e "$2" ] || [ -L "$2" ] || return 0
        cat -- "$2" || die "cannot read $2"
        return 0
    fi
    git rev-parse --verify -q "$1:$2" >/dev/null 2>&1 || return 0
    git cat-file blob "$1:$2" || die "cannot read $1:$2"
}
head_src="$HEAD_SHA"; [ "$WORKTREE" = 1 ] && head_src=WORKTREE

# ── Pass 2: lex added .rs lines ──────────────────────────────────────
# A line-by-line regex cannot know whether a line starts inside a string or a block comment,
# so each .rs file with added lines is lexed in full (nested block comments, strings, raw
# strings, char literals vs lifetimes) and the added line is also lexed on its own. Rules run
# on both results; either one finding a token is a finding (false positives over misses).
# Lexed text keeps every byte position, so findings carry token spans for the approvals stage.
# Emits: rule <TAB> path <TAB> line <TAB> text <TAB> "" <TAB> groups <TAB> trimmed text
: >"$TMP/rs-index"
if [ -s "$TMP/rs-added" ]; then
    mkdir -p "$TMP/blobs" || die "mkdir failed"
    n=0
    cut -f1 "$TMP/rs-added" | awk '!s[$0]++' >"$TMP/rs-paths" || die "cut failed"
    while IFS= read -r p; do
        n=$((n + 1)); show_at "$head_src" "$p" | tr '\000' '\001' >"$TMP/blobs/$n"
        [ "${PIPESTATUS[0]}${PIPESTATUS[1]}" = 00 ] || die "cannot read $p"
        printf '%s\t%s\n' "$TMP/blobs/$n" "$p" >>"$TMP/rs-index"
    done <"$TMP/rs-paths"
    set -- "$TMP"/blobs/*
    awk -v idx="$TMP/rs-index" -v add="$TMP/rs-added" -v addr_types='SocketAddrV4|SocketAddrV6|SocketAddr|IpAddr|Ipv4Addr|Ipv6Addr|AddrParseError' '
    BEGIN { HASHES = "################################################################"; SP = "        " }
    function sp(k) { while (length(SP) < k) SP = SP SP; return substr(SP, 1, k) }
    function lex(s,   out, i, n, c, c2, j, h, k, m, a, b, rest, term) {
        out = ""; i = 1; n = length(s)
        while (i <= n) {
            if (LB > 0) {
                rest = substr(s, i); a = index(rest, "/*"); b = index(rest, "*/")
                if (a == 0 && b == 0) { out = out sp(n - i + 1); break }
                if (a > 0 && (b == 0 || a < b)) { LB++; out = out sp(a + 1); i += a + 1 } else { LB--; out = out sp(b + 1); i += b + 1 }
                continue
            }
            if (LS == 1) {
                for (j = i; j <= n; j++) { c = substr(s, j, 1); if (c == "\\") { j++; continue } if (c == "\"") break }
                if (j > n) { out = out sp(n - i + 1); break }
                out = out sp(j - i) "\""; i = j + 1; LS = 0; continue
            }
            if (LS == 2) {
                term = "\"" substr(HASHES, 1, LH); k = index(substr(s, i), term)
                if (k == 0) { out = out sp(n - i + 1); break }
                out = out sp(k - 1) term; i += k - 1 + length(term); LS = 0; continue
            }
            rest = substr(s, i)
            if (!match(rest, /["\047\/]/)) { out = out rest; break }
            out = out substr(rest, 1, RSTART - 1); i += RSTART - 1; c = substr(s, i, 1); c2 = substr(s, i + 1, 1)
            if (c == "/") {
                if (c2 == "/") { out = out sp(n - i + 1); break }
                if (c2 == "*") { LB = 1; out = out "  "; i += 2; continue }
                out = out c; i++; continue
            }
            if (c == "\"") {
                j = i - 1; h = 0
                while (j >= 1 && substr(s, j, 1) == "#") { h++; j-- }
                LS = 1
                if (j >= 1 && substr(s, j, 1) == "r") {
                    a = (j > 1) ? substr(s, j - 1, 1) : ""; b = (j > 2) ? substr(s, j - 2, 1) : ""
                    if (a !~ /[A-Za-z0-9_]/ || ((a == "b" || a == "c") && b !~ /[A-Za-z0-9_]/)) { LS = 2; LH = h }
                }
                out = out c; i++; continue
            }
            # char literal or lifetime/label
            if (c2 == "\\") {
                k = index(substr(s, i + 3, 12), "\047")
                if (k > 0) { out = out "\047" sp(k + 1) "\047"; i += k + 3 } else { out = out c; i++ }
                continue
            }
            m = 1
            if (c2 >= "\200") while (substr(s, i + 1 + m, 1) >= "\200" && substr(s, i + 1 + m, 1) < "\300") m++
            if (c2 != "" && c2 != "\047" && substr(s, i + 1 + m, 1) == "\047") { out = out "\047" sp(m) "\047"; i += m + 2; continue }
            out = out c; i++
        }
        return out
    }
    function blankre(s, re,   out, r) { # replace every match of re with spaces
        out = ""
        while (match(s, re)) { out = out substr(s, 1, RSTART - 1) sp(RLENGTH); s = substr(s, RSTART + RLENGTH) }
        return out s
    }
    # tok(rule, ns, text): append "rule s e g" records (1-based token span, group span) to TK
    function scan(rule, s, re, grp,   off, a, b, gs, ge, ok) {
        off = 0
        while (match(s, re)) {
            a = off + RSTART; b = a + RLENGTH - 1; off = b; s = substr(s, RSTART + RLENGTH)
            if (rule != "net" && rule != "process") {
                # trim the boundary characters the regex needed
                while (substr(NS, a, 1) !~ /[A-Za-z_#]/ && a < b) a++
                while (substr(NS, b, 1) !~ /[A-Za-z0-9_(]/ && b > a) b--
            }
            if (rule == "ffi" && substr(NS, a, 6) == "extern" && substr(NS, b + 1) ~ /^[ \t]+crate([^A-Za-z0-9_]|$)/) continue
            if (rule == "include") { ok = incl_ok(a, b); if (ok) continue }
            gs = a; ge = b
            if (grp) {
                while (gs > 1 && substr(NS, gs - 1, 1) ~ /[A-Za-z0-9_:]/) gs--
                while (ge < length(NS) && substr(NS, ge + 1, 1) ~ /[A-Za-z0-9_:]/) ge++
            }
            TK[rule] = TK[rule] a " " b " " gs " " ge ";"
        }
    }
    function incl_ok(a, b,   t) { # a literal path ending in .rs is fine
        t = substr(TXT, b + 1)
        if (substr(NS, a, 4) == "path") return (t ~ /^[ \t]*=[ \t]*"[^"\\]*\.[rR][sS]"/)
        return (t ~ /^[ \t]*"[^"\\]*\.[rR][sS]"[ \t]*[)\]}]/)
    }
    function rules(ns,   nt, inner) {
        NS = ns
        scan("unsafe", ns, "(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)", 0)
        nt = ns
        if (match(nt, /std::net::\{[^}]*\}/)) {
            inner = substr(nt, RSTART, RLENGTH); gsub(addr_types, "", inner)
            if (inner ~ /^std::net::\{[ ,]*\}$/) nt = substr(nt, 1, RSTART - 1) sp(RLENGTH) substr(nt, RSTART + RLENGTH)
        }
        nt = blankre(nt, "(std::)?net::(" addr_types ")")
        NS = nt
        scan("net", nt, "std::net|tokio::net|async_std::net|mio::net|TcpStream|TcpListener|TcpSocket|UdpSocket|(reqwest|ureq|hyper|isahc|surf|attohttpc|curl|socket2|tungstenite|quinn)::|(use|crate)[ \t]+(reqwest|ureq|hyper|isahc|surf|attohttpc|curl|socket2|tungstenite|quinn)([^A-Za-z0-9_]|$)|[{,][ \t]*net([ \t]*[,}]|[ \t]+as[ \t]|::\\*|::\\{|$)|libc::(socket|connect|bind|listen|accept)", 1)
        NS = ns
        scan("process", ns, "process::\\{[^}]*Command|process::\\{|process::Command|Command::new|std::process[ \t]*(;|,|}|[ \t]as[ \t]|$)|[{,][ \t]*process[ \t]*(,|}|[ \t]as[ \t]|$)|tokio::process|async_process|CommandExt|libc::(exec[a-z]*|fork|vfork|system|posix_spawn[a-z]*|popen)|nix::unistd::(fork|exec[a-z]*)", 1)
        # #[link] and link_name only act on an extern block, which is itself a finding
        scan("ffi", ns, "(^|[^A-Za-z0-9_])extern([^A-Za-z0-9_]|$)|(^|[^A-Za-z0-9_])(no_mangle|export_name|link_section)([^A-Za-z0-9_]|$)", 0)
        scan("include", ns, "(^|[^A-Za-z0-9_])include([ \t]*![ \t]*[([{]|[ \t]+as([^A-Za-z0-9_]|$))", 0)
        if (ns ~ /#!?\[/) scan("include", ns, "(^|[^A-Za-z0-9_])path[ \t]*=", 0)
    }
    FILENAME == idx { split($0, f, "\t"); pathof[f[1]] = f[2]; next }
    FILENAME == add {
        split($0, f, "\t"); k = f[1] SUBSEP f[2]; want[k] = 1
        na++; ap[na] = f[1]; al[na] = f[2]; at[na] = substr($0, length(f[1]) + length(f[2]) + 3); next
    }
    FNR == 1 { LB = 0; LS = 0; LH = 0; P = pathof[FILENAME] }
    { s = $0; sub(/\r$/, "", s); o = lex(s); if ((P SUBSEP FNR) in want) { full[P SUBSEP FNR] = o; ftxt[P SUBSEP FNR] = s } }
    END {
        for (q = 1; q <= na; q++) {
            k = ap[q] SUBSEP al[q]; TXT = at[q]; sub(/\r$/, "", TXT)
            if (!(k in full)) { print "diff-gate: added line " ap[q] ":" al[q] " not found in the file" > "/dev/stderr"; exit 3 }
            if (ftxt[k] != TXT) { gsub(/\t/, " ", ftxt[k]); if (ftxt[k] != TXT) { print "diff-gate: " ap[q] ":" al[q] " differs from the diff" > "/dev/stderr"; exit 3 } }
            delete TK; LB = 0; LS = 0; LH = 0
            rules(full[k]); rules(lex(TXT))
            lead = TXT; sub(/[^ \t].*$/, "", lead); off = length(lead); T = substr(TXT, off + 1)
            for (r in TK) {
                # dedupe tokens, shift into trimmed-text coordinates, and group: "s-e;s-e|s-e"
                split(TK[r], tl, ";"); delete seen; delete gl; ng = 0; out = ""
                for (i in tl) {
                    if (tl[i] == "") continue
                    split(tl[i], v, " ")
                    tk = (v[1] - off) "-" (v[2] - off); g = (v[3] - off) "-" (v[4] - off)
                    if ((g SUBSEP tk) in seen) continue
                    seen[g SUBSEP tk] = 1
                    if (!(g in gl)) { gord[++ng] = g; gl[g] = tk } else gl[g] = gl[g] ";" tk
                }
                for (i = 1; i <= ng; i++) out = out (i > 1 ? "|" : "") gl[gord[i]]
                d = T; if (length(d) > 160) d = substr(d, 1, 157) "..."
                print r "\t" ap[q] "\t" al[q] "\t" d "\t\t" out "\t" T
            }
        }
    }' "$TMP/rs-index" "$TMP/rs-added" "$@" >>"$TMP/findings" || die "lexer stage failed"
fi

# ── Pass 3: Cargo.toml entries (needs the full file, not hunks) ──────
# Emits per entry: key <TAB> source <TAB> workspace=true <TAB> line <TAB> text
# Keys: <section>|<name> for dependency, [patch.*] and [replace] tables; target-path|<value>
# for a [lib]/[[bin]]/... path that is not a .rs file.
cargo_entries() {
    awk '
    function flush() {
        if (intab) print key "\t" src "\t" ws "\t" tl "\t" tt
        intab = 0
    }
    function sources(t,   s, ks, i, n) {
        s = ""; n = split("git path registry registry-index package rev tag branch", ks, " ")
        for (i = 1; i <= n; i++)
            if (match(t, "(^|[^A-Za-z_-])[\"\047]?" ks[i] "[\"\047]?[ \t]*=[ \t]*(\"[^\"]*\"|\047[^\047]*\047)")) s = s substr(t, RSTART, RLENGTH)
        return s
    }
    function norm(h,   out, i, c, q) { # unquote a dotted key; dots inside quotes become %2E
        out = ""; q = ""
        for (i = 1; i <= length(h); i++) {
            c = substr(h, i, 1)
            if (q != "") { if (c == q) q = ""; else out = out (c == "." ? "%2E" : c); continue }
            if (c == "\"" || c == "\047") { q = c; continue }
            if (c == " " || c == "\t") continue
            out = out c
        }
        return out
    }
    function tpath(t) {
        if (match(t, /(^|[^A-Za-z_-])["\047]?path["\047]?[ \t]*=[ \t]*("[^"]*"|\047[^\047]*\047)/)) {
            v = substr(t, RSTART, RLENGTH); sub(/^[^=]*=[ \t]*/, "", v); v = substr(v, 2, length(v) - 2)
            if (tolower(v) !~ /\.rs$/) print "target-path|" v "\t\t0\t" NR "\t" t
        }
    }
    { t = $0; sub(/^[ \t]+/, "", t); sub(/[ \t]+$/, "", t) }
    t ~ /^\[/ {
        flush(); mode = ""; started = 1
        h = t; sub(/^\[+[ \t]*/, "", h); sub(/[ \t]*\]+[ \t]*(#.*)?$/, "", h); h = norm(h); target = (h ~ /^(lib|bin|example|test|bench)$/)
        if (h ~ /^package\.metadata/) next
        if (h ~ /(^|\.)(dev-|build-)?dependencies$/ || h ~ /^patch\.[^.]+$/ || h == "replace") { mode = "list"; sec = h }
        else if (h ~ /(^|\.)(dev-|build-)?dependencies\.[^.]+$/ || h ~ /^patch\.[^.]+\.[^.]+$/ || h ~ /^replace\.[^.]+$/) {
            mode = "tab"; name = h; sub(/.*\./, "", name)
            sec = substr(h, 1, length(h) - length(name) - 1)
            key = sec "|" name; intab = 1; src = ""; ws = 0; tl = NR; tt = t
        }
        next
    }
    target { tpath(t) }
    !started && t ~ /^["\047]?(lib|bin|example|test|bench)["\047]?[ \t]*[.=]/ { tpath(t) }
    mode == "list" && match(t, /^("[^"]*"|\047[^\047]*\047|[A-Za-z0-9_-]+)([ \t]*\.[ \t]*("[^"]*"|\047[^\047]*\047|[A-Za-z0-9_-]+))*[ \t]*=/) {
        name = t; if (name ~ /^["\047]/) { q = substr(name, 1, 1); name = substr(name, 2); sub(q ".*$", "", name) } else sub(/[ \t.=].*$/, "", name)
        print sec "|" norm("\"" name "\"") "\t" sources(t) "\t" (t ~ /workspace[ \t]*=[ \t]*true/ ? 1 : 0) "\t" NR "\t" t
        next
    }
    mode == "tab" {
        src = src sources(t)
        if (t ~ /^workspace[ \t]*=[ \t]*true/) ws = 1
    }
    END { flush() }
    '
}

if [ -s "$TMP/cargo-paths" ]; then
    show_at "$BASE" Cargo.toml >"$TMP/root.toml"
    cargo_entries <"$TMP/root.toml" >"$TMP/root-entries" || die "cannot parse the base Cargo.toml"
    grep '^workspace\.dependencies|' "$TMP/root-entries" | cut -f1 >"$TMP/ws-root"
    while IFS= read -r p; do
        show_at "$BASE" "$p" >"$TMP/base.toml"; show_at "$head_src" "$p" >"$TMP/head.toml"
        cargo_entries <"$TMP/base.toml" >"$TMP/base-entries" || die "cannot parse $p on the base"
        cargo_entries <"$TMP/head.toml" >"$TMP/head-entries" || die "cannot parse $p"
        awk -F'\t' -v path="$p" '
        FILENAME ~ /ws-root$/ { wsroot[$1] = 1; next }
        FILENAME ~ /base-entries$/ { bsrc[$1] = bsrc[$1] $2; inbase[$1] = 1; next }
        {
            if (!($1 in hsrc)) { order[++n] = $1; hline[$1] = $4; htext[$1] = $5 }
            hsrc[$1] = hsrc[$1] $2; hws[$1] = hws[$1] || $3
        }
        END {
            for (i = 1; i <= n; i++) {
                k = order[i]; sec = substr(k, 1, index(k, "|") - 1)
                if (sec == "target-path") { if (!(k in inbase)) printf "include\t%s\t%s\ttarget path to a non-.rs file: %s\n", path, hline[k], htext[k]; continue }
                if (!(k in inbase)) {
                    if (sec !~ /^(patch|replace)/ && hws[k] && (("workspace.dependencies|" substr(k, index(k, "|") + 1)) in wsroot)) continue
                    printf "new-dep\t%s\t%s\tnew entry in [%s]: %s\n", path, hline[k], sec, htext[k]
                } else if (hsrc[k] != bsrc[k]) {
                    printf "new-dep\t%s\t%s\tsource changed in [%s]: %s\n", path, hline[k], sec, htext[k]
                }
            }
        }' "$TMP/ws-root" "$TMP/base-entries" "$TMP/head-entries" >>"$TMP/findings" || die "cargo pass failed for $p"
    done <"$TMP/cargo-paths"
fi

# ── Approvals ────────────────────────────────────────────────────────
# Read from the head commit (the working tree only in --worktree mode), the same source the
# approvals-change check below compares, so a checkout that differs from --head cannot
# approve anything.
# Emits: rule <TAB> glob <TAB> pattern <TAB> literal(0|1) <TAB> reason <TAB> approved_by
head_src="$HEAD_SHA"; [ "$WORKTREE" = 1 ] && head_src=WORKTREE
show_at "$BASE" "$APPROVALS" >"$TMP/appr.base.toml"
show_at "$head_src" "$APPROVALS" >"$TMP/appr.head.toml"
: >"$TMP/approvals"
if [ -s "$TMP/appr.head.toml" ]; then
    parse_approvals <"$TMP/appr.head.toml" >"$TMP/approvals.raw" || { echo "diff-gate: cannot use $APPROVALS" >&2; exit 2; }
    while IFS="$(printf '\t')" read -r r g pt re ab _; do
        lit=0
        if [ "$pt" != "*" ]; then
            printf '' | grep -Eq -e "$pt" 2>/dev/null; [ $? -eq 2 ] && lit=1
        fi
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$r" "$g" "$pt" "$lit" "$re" "$ab"
    done <"$TMP/approvals.raw" >"$TMP/approvals"
fi

# ── approvals-change: approvals rows and the owner ledger ────────────
# Findings here carry a 5th field: the decision id that clears an added row.
# The approving ledger is the one at the TIP of the base ref, not at the merge-base, so an
# entry the owner revoked on the target branch stops approving rows on branches forked
# before the revoke. Whether the ledger itself changed is still judged on the diff
# (merge-base vs head), like every other finding.
show_at "$BASE_TIP" "$LEDGER" >"$TMP/ledger.tip.toml"
show_at "$BASE" "$LEDGER" >"$TMP/ledger.base.toml"
show_at "$head_src" "$LEDGER" >"$TMP/ledger.head.toml"
parse_ledger <"$TMP/ledger.tip.toml" >"$TMP/ledger" || { echo "diff-gate: cannot use $LEDGER on the base" >&2; exit 2; }
parse_ledger <"$TMP/ledger.head.toml" >/dev/null || { echo "diff-gate: cannot use $LEDGER" >&2; exit 2; }
if ! cmp -s "$TMP/ledger.base.toml" "$TMP/ledger.head.toml"; then
    printf 'approvals-change\t%s\t0\towner ledger changed\n' "$LEDGER" >>"$TMP/findings"
fi
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
# Content findings (unsafe, net, process, ffi, include) carry token spans: each occurrence on
# the line (a keyword, or one path expression such as std::net::TcpListener::bind) is approved
# only by an approval match that overlaps one of its tokens, so a row cannot approve a second
# occurrence elsewhere on the same line. Other findings match the whole reported text.
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
function covers(i, T, s, e,   rest, off, p, a, b) { # does a match of row i overlap T[s..e]?
    if (ap[i] == "*") return 1
    rest = T; off = 0
    while ((p = index(rest, ap[i])) > 0) {
        if (off + p <= e && off + p + length(ap[i]) - 1 >= s) return 1
        off += p; rest = substr(rest, p + 1)
    }
    if (al[i] == 1) return 0
    rest = T; off = 0
    while (match(rest, ap[i])) {
        a = off + RSTART; b = a + RLENGTH - 1
        if (a > e) return 0
        if (b >= s && RLENGTH > 0) return 1
        if (ap[i] ~ /\^/ || a >= length(T)) return 0   # a later start would move "^"; stay strict
        off = a; rest = substr(T, a + 1)
    }
    return 0
}
FILENAME == apf { na++; ar[na] = $1; ag[na] = g2r($2); ap[na] = $3; al[na] = $4; aw[na] = $5; ab[na] = $6; next }
{
    status = "FAIL"; detail = ""
    if ($1 == "approvals-change") {
        if (owner == "1") { status = "approved"; detail = "DIFF_GATE_OWNER_APPROVED=1" }
        else if ($5 != "") { status = "approved"; detail = "owner ledger (base tip): " $5 }
        else detail = "owner only: set DIFF_GATE_OWNER_APPROVED=1 (CI: diff-gate-approved label)"
    } else if ($6 != "") {
        ng = split($6, groups, "|"); open = 0; first = 0; col = 0
        for (gi = 1; gi <= ng; gi++) {
            nt = split(groups[gi], toks, ";"); hit = 0
            for (i = 1; i <= na && !hit; i++) {
                if (ar[i] != $1 || $2 !~ ag[i]) continue
                for (ti = 1; ti <= nt && !hit; ti++) { split(toks[ti], se, "-"); if (covers(i, $7, se[1] + 0, se[2] + 0)) hit = i }
            }
            if (hit) { if (!first) first = hit } else { open++; if (!col) { split(toks[1], se, "-"); col = se[1] } }
        }
        if (!open) { status = "approved"; detail = ab[first] ": " aw[first] }
        else if (first) detail = open " of " ng " occurrences on the line not approved (first at column " col ")"
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
' "$TMP/approvals" "$TMP/findings" >"$TMP/classified.u" || die "classification failed"
sort -t"$(printf '\t')" -k1,1 -k2,2 -k3,3 -k4,4n "$TMP/classified.u" >"$TMP/classified" || die "sort failed"

# ── Report ───────────────────────────────────────────────────────────
nfail="$(grep -c '^FAIL' "$TMP/classified" || true)"
napp="$(grep -c '^approved' "$TMP/classified" || true)"
nfiles="$(awk 'END { print NR }' "$TMP/files")"
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
