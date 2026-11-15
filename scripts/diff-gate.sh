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
# Rules: unsafe, new-dep, cargo-unparsed, build-script, proc-macro, net, process, ffi, include,
#        ci-workflow, rust-parse, rust-tokens (the token helper, tools/diff-gate-tokens, built from
#        the base), gitattributes-change, toolchain-config, special-file, sensitive-crate, approvals-change.
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
#        scripts/diff-gate.sh --cargo-entries FILE
#   --json        machine-readable findings on stdout
#   --worktree    compare the working tree (tracked changes + untracked files) with the
#                 base instead of HEAD; this is what `scripts/build.sh gate` uses
#   --base REF    same as DIFF_GATE_BASE
#   --ledger-hash print "sha256 <TAB> rule <TAB> path <TAB> pattern" for each row of FILE
#                 (default: the approvals file), for the owner to build ledger entries
#   --cargo-entries print what the Cargo.toml reader (pass 3) makes of FILE: one line per
#                 dependency entry, target path and unparsed statement (for tests and review)
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
CARGO_ENTRIES=""
WORKTREE=0
BASE_REF="${DIFF_GATE_BASE:-}"
HEAD_REF="${DIFF_GATE_HEAD:-HEAD}"

while [ $# -gt 0 ]; do
    case "$1" in
        --json) JSON=1 ;;
        --worktree) WORKTREE=1 ;;
        --base) BASE_REF="${2:-}"; shift ;;
        --head) HEAD_REF="${2:-}"; shift ;;
        --cargo-entries) CARGO_ENTRIES="${2:-}"; [ -n "$CARGO_ENTRIES" ] || { echo "diff-gate: --cargo-entries needs a file" >&2; exit 2; }; shift ;;
        --ledger-hash) LEDGER_HASH="$APPROVALS"; case "${2:-}" in ""|-*) ;; *) LEDGER_HASH="$2"; shift ;; esac ;;
        -h|--help) sed -n '2,/^set -uo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "diff-gate: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

TMP="$(mktemp -d)" || die "mktemp failed"
trap 'rm -rf "$TMP"' EXIT

# ── Approvals and ledger parsing ─────────────────────────────────────
# parse_approvals: TOML on stdin -> rule <TAB> glob <TAB> pattern <TAB> reason <TAB> approved_by <TAB> line <TAB> blob
# blob (optional) is a git blob id, 40 or 64 hex digits, lower-cased. Any other key is a setup
# error, so a misspelt `blob` can never leave a row unscoped by accident.
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
        } else if (hasb && (b !~ /^[0-9a-f]+$/ || (length(b) != 40 && length(b) != 64))) {
            print "diff-gate: malformed approval entry near line " start " (blob must be a 40 or 64 hex digit git blob id)" > "/dev/stderr"; bad = 1
        } else print r "\t" p "\t" pt "\t" re "\t" ab "\t" start "\t" b
        open = 0
    }
    /^[ \t]*#/ || /^[ \t]*$/ { next }
    /^[ \t]*\[\[approval\]\]/ { flush(); open = 1; start = NR; r = p = pt = re = ab = b = ""; hasb = 0; next }
    /^[ \t]*blob[ \t]*=/ { b = tolower(val($0)); hasb = 1; next }
    /^[ \t]*rule[ \t]*=/ { r = val($0); next }
    /^[ \t]*path[ \t]*=/ { p = val($0); next }
    /^[ \t]*pattern[ \t]*=/ { pt = val($0); next }
    /^[ \t]*reason[ \t]*=/ { re = val($0); next }
    /^[ \t]*approved_by[ \t]*=/ { ab = val($0); next }
    { print "diff-gate: unexpected line " NR " in approvals (keys: rule, path, pattern, blob, reason, approved_by)" > "/dev/stderr"; bad = 1 }
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
# parsed values, plus "blob=B\n" when the row has a blob, so quoting style and key order do not
# change the hash; any value change does. Rows without a blob hash as they did before blobs.
hash_rows() {
    local r g pt re ab ln b h
    while IFS="$(printf '\t')" read -r r g pt re ab ln b; do
        h="$( { printf 'rule=%s\npath=%s\npattern=%s\nreason=%s\napproved_by=%s\n' "$r" "$g" "$pt" "$re" "$ab"
                [ -z "$b" ] || printf 'blob=%s\n' "$b"; } | sha256_stdin)" || return 1
        printf '%s\t%s\t%s\t%s\t%s\n' "$h" "$ln" "$r" "$g" "$pt"
    done
}

# ── Cargo.toml key paths (pass 3) ────────────────────────────────────
# cargo_entries: a Cargo.toml on stdin -> one line per dependency entry, Cargo target path and
# unparsed statement: key <TAB> sources <TAB> workspace=true <TAB> line <TAB> text
#   key  <section>|<name>   a dependency entry; section is dependencies, dev-dependencies,
#                           build-dependencies (or the _ spellings), target.<T>.<one of those>,
#                           workspace.dependencies, patch.<registry> or replace
#        target-path|<v>    a [lib]/[[bin]]/[[example]]/[[test]]/[[bench]] path value
#        unparsed|<ctx>|<text>|<n>  a statement it cannot read, or a key path that names a
#                           dependency table in a form it does not know (fail closed)
#   sources  the entry's git/path/registry/registry-index/package/rev/tag/branch values, sorted
# A small TOML reader, not a regex per line: CRLF and a lone CR end a line; table headers,
# array-of-tables headers, dotted keys, bare, basic ("..." with escapes) and literal ('...')
# key segments, inline tables and arrays (also over several lines), and all four string forms.
# Every key is resolved to its full path (header + dotted key + inline-table keys) before it is
# classified, so `[dependencies] evil = ...`, `dependencies.evil = ...`, `dependencies = { evil
# = ... }` and `[target.'cfg(unix)'] dependencies.evil = ...` are the same entry.
cargo_entries() { # [keys]: also print build / proc-macro / crate-type / links lines (line mapping only)
    awk -v keys="${1:-}" '
    function tstrip(s,   i, n, c, q) { # drop a trailing TOML comment; a # inside a string stays
        if (index(s, "#") == 0) return s
        n = length(s); q = ""
        for (i = 1; i <= n; i++) {
            c = substr(s, i, 1)
            if (q == "\"") { if (c == "\\") i++; else if (c == "\"") q = ""; continue }
            if (q == "\047") { if (c == "\047") q = ""; continue }
            if (c == "\"" || c == "\047") { q = c; continue }
            if (c == "#") { s = substr(s, 1, i - 1); sub(/[ \t]+$/, "", s); return s }
        }
        return s
    }
    # Cursor over logical lines LT[1..NL]: pk(0) is the current character, "\n" at the end of a
    # line, "" at the end of the file. Strings never look past the end of their line.
    function pk(k,   p, L) {
        if (li > NL) return ""
        p = ci + k; L = length(LT[li])
        if (p <= L) return substr(LT[li], p, 1)
        return (p == L + 1) ? "\n" : ""
    }
    function adv() { if (li > NL) return; if (ci > length(LT[li])) { li++; ci = 1 } else ci++ }
    function skipws() { while (pk(0) == " " || pk(0) == "\t") adv() }
    function skipall(   c) { # spaces, newlines and comments
        while (1) {
            c = pk(0)
            if (c == " " || c == "\t" || c == "\n") adv()
            else if (c == "#") ci = length(LT[li]) + 1
            else return
        }
    }
    function hexn(n,   i, c, v, d) {
        v = 0; HX = ""
        for (i = 1; i <= n; i++) {
            c = pk(0); d = index("0123456789abcdef", tolower(c))
            if (c == "" || c == "\n" || d == 0) return -1
            v = v * 16 + d - 1; HX = HX c; adv()
        }
        return v
    }
    function bstr(   s, c, h) { # "..." on one line, escapes decoded; sets SV
        adv(); s = ""
        while (1) {
            c = pk(0)
            if (c == "" || c == "\n") return 0
            adv()
            if (c == "\"") { SV = s; return 1 }
            if (c != "\\") { s = s c; continue }
            c = pk(0); if (c == "" || c == "\n") return 0
            adv()
            if (c == "u" || c == "U" || c == "x") {
                h = hexn(c == "u" ? 4 : (c == "U" ? 8 : 2)); if (h < 0) return 0
                s = s ((h > 0 && h < 128) ? sprintf("%c", h) : "\\u{" tolower(HX) "}"); continue
            }
            if (c == "\"" || c == "\\") s = s c
            else if (c == "n") s = s "\n"; else if (c == "t") s = s "\t"; else if (c == "r") s = s "\r"
            else if (c == "b") s = s "\b"; else if (c == "f") s = s "\f"; else if (c == "e") s = s "\033"
            else return 0
        }
    }
    function lstr(   s, c) { # literal string on one line; sets SV
        adv(); s = ""
        while (1) {
            c = pk(0)
            if (c == "" || c == "\n") return 0
            adv()
            if (c == "\047") { SV = s; return 1 }
            s = s c
        }
    }
    function mlstr(q,   s, c, n) { # """...""" or '\'''\''...; escapes kept as written; sets SV
        adv(); adv(); adv(); s = ""
        if (pk(0) == "\n") adv()   # a newline right after the opening quotes is not content
        while (1) {
            c = pk(0)
            if (c == "") return 0
            if (q == "\"" && c == "\\") { adv(); c = pk(0); if (c == "") return 0; s = s "\\" c; adv(); continue }
            if (c == q && pk(1) == q && pk(2) == q) {
                n = 0; while (pk(0) == q) { n++; adv() }
                if (n > 5) return 0
                while (n-- > 3) s = s q
                SV = s; return 1
            }
            s = s c; adv()
        }
    }
    function keyseg(   c, s) { # one key segment; sets SV
        c = pk(0)
        if (c == "\"") return bstr()
        if (c == "\047") return lstr()
        s = ""
        while ((c = pk(0)) != "" && c ~ /^[A-Za-z0-9_-]$/) { s = s c; adv() }
        if (s == "") return 0
        SV = s; return 1
    }
    function parsekey(P,   q) { # dotted key -> KP, the full path below P (segments joined by SUBSEP)
        q = P
        while (1) {
            skipws(); if (!keyseg()) return 0
            q = (q == "" ? SV : q SUBSEP SV); skipws()
            if (pk(0) != ".") { KP = q; return 1 }
            adv()
        }
    }
    function rec(t, P, v, l) { NRC++; RCT[NRC] = t; RCP[NRC] = P; RCV[NRC] = v; RCL[NRC] = l } # not RT: a gawk built-in
    function value(P, l,   c, s) {
        c = pk(0)
        if (c == "\"" || c == "\047") {
            if (pk(1) == c && pk(2) == c) { if (!mlstr(c)) return 0 }
            else if (c == "\"") { if (!bstr()) return 0 }
            else if (!lstr()) return 0
            rec("V", P, SV, l); return 1
        }
        if (c == "{") return itable(P, l)
        if (c == "[") return array(P, l)
        s = ""
        while ((c = pk(0)) != "" && c ~ /^[A-Za-z0-9_+.:-]$/) { s = s c; adv() }
        if (s == "") return 0
        if (s ~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/ && pk(0) == " " && pk(1) ~ /^[0-9]$/) {
            adv(); s = s " "; while ((c = pk(0)) != "" && c ~ /^[0-9:.Z+-]$/) { s = s c; adv() }
        }
        rec("V", P, s, l); return 1
    }
    function itable(P, l,   c, kl, q) { # { k = v, ... }; newlines and comments allowed (TOML 1.1)
        adv(); rec("N", P, "", l); skipall()
        if (pk(0) == "}") { adv(); return 1 }
        while (1) {
            skipall(); kl = li
            if (!parsekey(P)) return 0
            q = KP; skipws(); if (pk(0) != "=") return 0
            adv(); skipws()
            if (!value(q, kl)) return 0
            skipall(); c = pk(0)
            if (c == "}") { adv(); return 1 }
            if (c != ",") return 0
            adv(); skipall()
            if (pk(0) == "}") { adv(); return 1 }
        }
    }
    function array(P, l,   c) { # [ v, ... ]; every element has the array path
        adv(); rec("N", P, "", l)
        while (1) {
            skipall()
            if (pk(0) == "]") { adv(); return 1 }
            if (!value(P, li)) return 0
            skipall(); c = pk(0)
            if (c == "]") { adv(); return 1 }
            if (c != ",") return 0
            adv()
        }
    }
    function eol() { skipws(); if (pk(0) == "#") ci = length(LT[li]) + 1; return (pk(0) == "\n" || pk(0) == "") }
    function unparsed(l) { # record the statement at line l, then resume on the line after the cursor
        rec("U", H, "", l)
        if (li < l) li = l
        if (li <= NL) { ci = length(LT[li]) + 1; adv() }
    }
    function parse(   c, kl, aot, kp) {
        li = 1; ci = 1; H = ""
        while (1) {
            skipall(); if (li > NL) return
            kl = li
            if (pk(0) == "[") {
                adv(); aot = 0; if (pk(0) == "[") { adv(); aot = 1 }
                skipws()
                if (!parsekey("")) { H = UNK; unparsed(kl); continue }
                skipws(); if (pk(0) != "]") { H = UNK; unparsed(kl); continue }
                adv()
                if (aot && pk(0) != "]") { H = UNK; unparsed(kl); continue }
                if (aot) adv()
                H = KP; rec("N", H, "", kl)
                if (!eol()) unparsed(kl)
                continue
            }
            if (!parsekey(H)) { unparsed(kl); continue }
            kp = KP; skipws()
            if (pk(0) != "=") { unparsed(kl); continue }
            adv(); skipws()
            if (!value(kp, kl) || !eol()) { unparsed(kl); continue }
        }
    }
    function esc(s) { gsub(/%/, "%25", s); gsub(/\./, "%2E", s); gsub(/\|/, "%7C", s); gsub(/\t/, "%09", s); gsub(/\n/, "%0A", s); gsub(/\r/, "%0D", s); return s }
    function escv(s) { gsub(/%/, "%25", s); gsub(/\t/, "%09", s); gsub(/\n/, "%0A", s); gsub(/\r/, "%0D", s); return s }
    function jn(s, a, b,   i, o) { o = ""; for (i = a; i <= b; i++) o = o (i > a ? "." : "") esc(s[i]); return o }
    function isdt(x) { return (x ~ /^(dev[-_]|build[-_])?dependencies$/) }
    function text(l,   t) { t = tstrip(LT[l]); sub(/^[ \t]+/, "", t); sub(/[ \t]+$/, "", t); gsub(/\t/, " ", t); return t }
    function unp(r,   t, k, n, s) {
        n = split(RCP[r], s, SUBSEP); t = text(RCL[r]); k = "unparsed|" jn(s, 1, n) "|" esc(t); occ[k]++
        print k "|" occ[k] "\t\t0\t" LN[RCL[r]] "\t" t
    }
    # One logical line per LF; a CR before it is dropped, a CR anywhere else also ends a line (a
    # lone CR is not a TOML newline, so Cargo rejects such a file; splitting there can only
    # show more keys). LN maps a logical line to the physical line git numbers.
    # A UTF-8 BOM before the first statement is not part of it (Cargo strips it too; R3-A4).
    NR == 1 && substr($0, 1, 3) == "\357\273\277" { $0 = substr($0, 4) }
    { r = $0; sub(/\r$/, "", r); n = split(r, part, "\r"); if (n == 0) { n = 1; part[1] = "" }
      for (i = 1; i <= n; i++) { NL++; LT[NL] = part[i]; LN[NL] = NR } }
    END {
        UNK = "\001unknown-table"
        parse()
        nsrc = split("git path registry registry-index package rev tag branch", sl, " ")
        for (i = 1; i <= nsrc; i++) SRC[sl[i]] = 1
        for (r = 1; r <= NRC; r++) {
            if (RCT[r] == "U" || index(RCP[r], UNK) == 1) { unp(r); continue }
            n = split(RCP[r], s, SUBSEP)
            if (n >= 2 && (s[1] == "package" || s[1] == "workspace") && s[2] == "metadata") continue
            if (s[1] == "features") continue
            pl = 0
            if (isdt(s[1])) pl = 1
            else if (s[1] == "target" && n >= 3 && isdt(s[3])) pl = 3
            else if (s[1] == "workspace" && n >= 2 && s[2] == "dependencies") pl = 2
            else if (s[1] == "patch" && n >= 2) pl = 2
            else if (s[1] == "replace") pl = 1
            if (pl) {
                if (n == pl) { if (RCT[r] != "N") unp(r); continue }
                k = jn(s, 1, pl) "|" esc(s[pl + 1])
                if (!(k in el)) { el[k] = RCL[r]; eo[++ne] = k }
                if (n >= pl + 2 && RCT[r] == "V") {
                    if (s[pl + 2] in SRC) { ns[k]++; es[k, ns[k]] = jn(s, pl + 2, n) "=" escv(RCV[r]) }
                    if (n == pl + 2 && s[pl + 2] == "workspace" && RCV[r] == "true") ew[k] = 1
                }
                continue
            }
            if (keys != "" && RCT[r] == "V") { # for mapping what cargo metadata reports back to a line
                kk = ""
                if (n == 2 && s[1] == "package" && s[2] ~ /^(build|links)$/) kk = "pkg|" s[2]
                else if (n == 2 && s[1] == "lib" && s[2] ~ /^proc[-_]macro$/) kk = "lib|proc-macro"
                else if (n == 2 && s[1] ~ /^(lib|example|bin)$/ && s[2] ~ /^crate[-_]type$/) kk = s[1] "|crate-type"
                if (kk != "" && !(kk in kseen)) { kseen[kk] = 1; print kk "|" escv(RCV[r]) "\t\t0\t" LN[RCL[r]] "\t" text(RCL[r]) }
            }
            if (n == 2 && s[1] ~ /^(lib|bin|example|test|bench)$/ && s[2] == "path" && RCT[r] == "V") {
                print "target-path|" escv(RCV[r]) "\t\t0\t" LN[RCL[r]] "\t" text(RCL[r]); continue
            }
            # tables on the way to a dependency table: [target], [target.<T>], [patch], [workspace]
            if (RCT[r] == "N" && ((n == 1 && s[1] ~ /^(target|patch|workspace)$/) || (n == 2 && s[1] == "target"))) continue
            for (i = 1; i <= n; i++) if (s[i] ~ /dependencies/ || s[i] ~ /^(patch|replace|target)$/) { unp(r); break }
        }
        for (e = 1; e <= ne; e++) {
            k = eo[e]; m = ns[k] + 0
            for (i = 2; i <= m; i++) { v = es[k, i]; for (j = i - 1; j >= 1 && es[k, j] > v; j--) es[k, j + 1] = es[k, j]; es[k, j + 1] = v }
            src = ""; for (i = 1; i <= m; i++) src = src es[k, i] ";"
            print k "\t" src "\t" (ew[k] ? 1 : 0) "\t" LN[el[k]] "\t" text(el[k])
        }
    }
    '
}

if [ -n "$CARGO_ENTRIES" ]; then
    [ -f "$CARGO_ENTRIES" ] || { echo "diff-gate: $CARGO_ENTRIES not found" >&2; exit 2; }
    cargo_entries <"$CARGO_ENTRIES"
    exit $?
fi

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
RAW_OPTS=("${COMMON[@]}" --raw -z --full-index --no-abbrev)

# add_entry <status> <oldmode> <newmode> <path> <blob> [nohdr]: file list for path rules; the
# patch must carry one "diff --git" header per entry, two for a type change. blob is the
# post-change git blob id (all zeros for a deletion); approval rows with `blob` compare it.
add_entry() {
    case "$4" in *[[:cntrl:]]*) die "path with a control character in the diff (refused)";; esac
    printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >>"$TMP/files"
    [ -n "${6:-}" ] && return 0
    case "$1" in D|A|M) echo "$4" >>"$TMP/hdrs" ;; T) printf '%s\n%s\n' "$4" "$4" >>"$TMP/hdrs" ;; *) die "unexpected diff status '$1' for $4" ;; esac
}
# wt_blob <path>: blob id of the working-tree bytes, the content a local build reads: no clean
# filter or eol conversion, and a symlink's target string, not the file it points to.
wt_blob() {
    if [ -L "$1" ]; then printf '%s' "$(readlink "$1")" | git hash-object --stdin
    elif [ -f "$1" ]; then git hash-object --no-filters -- "$1"
    fi
}
read_raw() { # git diff --raw -z output on stdin
    local meta path blob
    while IFS= read -r -d '' meta; do
        IFS= read -r -d '' path || die "truncated --raw diff"
        set -- $meta
        [ $# -eq 5 ] || die "unexpected --raw line: $meta"
        blob="$4"
        # against the working tree git prints zeros for a modified file: hash what is on disk
        if [ "$WORKTREE" = 1 ] && [ "$5" != D ] && [ "$2" != 160000 ]; then blob="$(wt_blob "$path")"; fi
        add_entry "$5" "${1#:}" "$2" "$path" "$blob"
    done
}
: >"$TMP/files"; : >"$TMP/hdrs"
if [ "$WORKTREE" = 1 ]; then
    "${GITC[@]}" diff "${RAW_OPTS[@]}" "$BASE" -- >"$TMP/raw" || die "git diff --raw failed"
    read_raw <"$TMP/raw"
    "${GITC[@]}" diff "${DIFF_OPTS[@]}" "$BASE" -- >"$TMP/diff" || die "git diff failed"
    git ls-files -z -o --exclude-standard >"$TMP/untracked" || die "git ls-files failed"
    add_untracked() {
        local mode=100644; [ -L "$1" ] && mode=120000
        add_entry A 000000 "$mode" "$1" "$(wt_blob "$1")"
        "${GITC[@]}" diff --no-index "${DIFF_OPTS[@]}" -- /dev/null "$1" >>"$TMP/diff"
        [ $? -le 1 ] || die "git diff --no-index failed for $1"
    }
    while IFS= read -r -d '' f; do
        if [ -d "$f" ]; then add_entry A 000000 160000 "${f%/}" "" nohdr; continue; fi
        add_untracked "$f"
    done <"$TMP/untracked"
    # RV-E1: a git-ignored untracked file is still read by a local build (a gitignored
    # src/gen.rs behind `mod gen;`). Ignored files of the kinds cargo reads (.rs, Cargo.toml,
    # .cargo/config*, rust-toolchain*) are scanned like untracked ones, each with a warning.
    # Inside an ignored directory they are looked for too, except under build output (target/,
    # node_modules/, .embuild/) and in nested repositories. Ignored .gitattributes and workflow
    # files are not: git and CI never read them.
    in_scope() {
        case "$1" in
            *.[rR][sS]|[cC][aA][rR][gG][oO].[tT][oO][mM][lL]|*/[cC][aA][rR][gG][oO].[tT][oO][mM][lL]) return 0 ;;
            .cargo/config*|*/.cargo/config*|rust-toolchain|rust-toolchain.toml|*/rust-toolchain|*/rust-toolchain.toml) return 0 ;;
        esac
        return 1
    }
    build_output() { case "/$1/" in */target/*|*/node_modules/*|*/.embuild/*|*/.git/*) return 0 ;; esac; return 1; }
    git ls-files -z -o -i --exclude-standard --directory >"$TMP/ignored" || die "git ls-files failed"
    : >"$TMP/ignored-scan"
    while IFS= read -r -d '' f; do
        case "$f" in
            */) d="${f%/}"; build_output "$d" && continue; [ -e "$d/.git" ] && continue
                find "$d" \( -name target -o -name node_modules -o -name .embuild -o -name .git \) -prune -o \( -type f -o -type l \) \
                    \( -iname '*.rs' -o -iname cargo.toml -o -name 'rust-toolchain*' -o -path '*.cargo/config*' \) -print0 >>"$TMP/ignored-scan" \
                    || die "find failed in $d" ;;
            *) printf '%s\0' "$f" >>"$TMP/ignored-scan" ;;
        esac
    done <"$TMP/ignored"
    while IFS= read -r -d '' f; do
        f="${f#./}"; in_scope "$f" || continue; build_output "$f" && continue
        echo "diff-gate: WARNING: git-ignored file scanned (--worktree; a local build can read it): $f" >&2
        add_untracked "$f"
    done <"$TMP/ignored-scan"
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
awk -v cargo_list="$TMP/cargo-paths" -v rs_out="$TMP/rs-added" -v rs_rm="$TMP/rs-removed" -v flist="$TMP/files" -v hlist="$TMP/hdrs" '
function emit(rule, path, ln, text) {
    gsub(/\t/, " ", text); sub(/^[ \t]+/, "", text)
    if (length(text) > 160) text = substr(text, 1, 157) "..."
    print rule "\t" path "\t" ln "\t" text
}
function bad(msg) { print "diff-gate: " msg > "/dev/stderr"; err = 1; exit 3 }
function emitc(rule, path, ln, text,   m) { # Cargo.toml line finding; field 7 is what rows match
    gsub(/\t/, " ", text); sub(/^[ \t]+/, "", text); m = text
    if (length(text) > 160) text = substr(text, 1, 157) "..."
    print rule "\t" path "\t" ln "\t" text "\t\t\t" m
}
function tstrip(s,   i, n, c, q) { # drop a trailing TOML comment; a # inside a string stays
    if (index(s, "#") == 0) return s
    n = length(s); q = ""
    for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (q == "\"") { if (c == "\\") i++; else if (c == "\"") q = ""; continue }
        if (q == "\047") { if (c == "\047") q = ""; continue }
        if (c == "\"" || c == "\047") { q = c; continue }
        if (c == "#") { s = substr(s, 1, i - 1); sub(/[ \t]+$/, "", s); return s }
    }
    return s
}
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
    if (p ~ /^\.github\/workflows\// || p == "scripts/diff-gate.sh" || p ~ /^tools\/diff-gate-tokens\//) emit("ci-workflow", p, 0, st == "D" ? "file deleted or moved away" : "file changed")
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
/^@@ / { s = $0; sub(/^@@ -[0-9,]+ \+/, "", s); ln = s + 0; o = $0; sub(/^@@ -/, "", o); oln = o + 0; inhunk = 1; next }
# removed .rs lines (path <TAB> old line <TAB> new-side line <TAB> text), for the sensitive-crate
# check that a test cfg was not taken off a module declaration that stays (RV-F1)
inhunk && !skipc && /^-/ { if (isrs) { text = substr($0, 2); gsub(/\t/, " ", text); print cur "\t" oln "\t" ln "\t" text > rs_rm } oln++; next }
inhunk && !skipc && /^\+/ {
    text = substr($0, 2); this = ln; ln++
    if (iscargo) {
        # CRLF: the CR is not part of the line. A lone CR is read as a line break, as pass 3
        # does, so a key after it is seen (RV-B1).
        sub(/\r$/, "", text); ncr = split(text, crp, "\r"); if (ncr == 0) next
        for (ci = 1; ci <= ncr; ci++) {
            # A trailing comment is not part of the key or value, so it can neither trigger a
            # rule nor be what an approval pattern matches (G2).
            t = tstrip(crp[ci])
            # build / proc-macro keys anywhere: bare, quoted, dotted, or inside an inline table
            if (match(t, /(^|[{,. \t])["\047]?build["\047]?[ \t]*=/) && substr(t, RSTART + RLENGTH) !~ /^[ \t]*false/) emitc("build-script", cur, this, t)
            if (t ~ /(^|[{,. \t])["\047]?proc[-_]macro["\047]?[ \t]*=[ \t]*true/) emitc("proc-macro", cur, this, t)
            if (t ~ /"[^"]*\\[^"]*"[ \t]*[.=]/ || t ~ /^[ \t]*\[.*\\/) emitc("build-script", cur, this, "escaped key: " t)
        }
        next
    }
    if (isrs) { gsub(/\t/, " ", text); print cur "\t" this "\t" text > rs_out }
    next
}
inhunk && /^ / { ln++; next }
END { if (!err && hi != nh) { print "diff-gate: diff has " hi " file header(s), file list expects " nh > "/dev/stderr"; exit 3 } }
' "$TMP/hdrs" "$TMP/files" "$TMP/diff" >"$TMP/findings" || die "pass 1 failed (diff could not be parsed)"
: >>"$TMP/rs-added"; : >>"$TMP/rs-removed"

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

# g2r: approvals glob -> anchored ERE (* within a directory, ** across). Shared by the awk stages.
G2R_AWK='
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
}'

# ── Sensitive crates (G1 interim, owner option c) ────────────────────
# config/diff-gate-sensitive.toml AT THE TIP OF THE BASE REF lists [[sensitive]] and [[exempt]]
# path globs; the copy in the diff under review is never used (a change to it is an
# approvals-change finding below). parse_sensitive: TOML on stdin -> S|E <TAB> glob.
SENSITIVE="config/diff-gate-sensitive.toml"
parse_sensitive() {
    awk '
    function val(s,   v) { v = s; sub(/^[^=]*=[ \t]*/, "", v); sub(/^["\047]/, "", v); sub(/["\047][ \t]*(#.*)?$/, "", v); gsub(/\t/, " ", v); return v }
    function flush() {
        if (!open) return
        if (p == "" || re == "") { print "diff-gate: malformed sensitive-crate entry near line " start " (needs path, reason)" > "/dev/stderr"; bad = 1 }
        else print kind "\t" p
        open = 0
    }
    /^[ \t]*#/ || /^[ \t]*$/ { next }
    /^[ \t]*\[\[sensitive\]\][ \t]*(#.*)?$/ { flush(); open = 1; kind = "S"; start = NR; p = re = ""; next }
    /^[ \t]*\[\[exempt\]\][ \t]*(#.*)?$/ { flush(); open = 1; kind = "E"; start = NR; p = re = ""; next }
    open && /^[ \t]*path[ \t]*=/ { p = val($0); next }
    open && /^[ \t]*reason[ \t]*=/ { re = val($0); next }
    { print "diff-gate: unexpected line " NR " in the sensitive-crate list" > "/dev/stderr"; bad = 1 }
    END { flush(); exit bad }
    '
}
show_at "$BASE_TIP" "$SENSITIVE" >"$TMP/sens.tip.toml"
show_at "$BASE" "$SENSITIVE" >"$TMP/sens.base.toml"
show_at "$head_src" "$SENSITIVE" >"$TMP/sens.head.toml"
parse_sensitive <"$TMP/sens.tip.toml" >"$TMP/sens" || { echo "diff-gate: cannot use $SENSITIVE on the base" >&2; exit 2; }
parse_sensitive <"$TMP/sens.head.toml" >/dev/null || { echo "diff-gate: cannot use $SENSITIVE" >&2; exit 2; }
# is_sensitive <path>: 1 if path matches a [[sensitive]] glob and no [[exempt]] glob, else 0
is_sensitive() {
    awk -F'\t' -v p="$1" "$G2R_AWK"'
    $1 == "S" && p ~ g2r($2) { s = 1 }
    $1 == "E" && p ~ g2r($2) { e = 1 }
    END { print (s && !e) ? 1 : 0 }' "$TMP/sens"
}

# ── Sensitive crates: a test cfg taken off a module declaration (RV-F1) ──
# Deleting the `#[cfg(test)]` line above a `mod` declaration adds no line, yet it moves the
# module's existing code (an exempt *_tests.rs file, or an inline test module) into the normal
# build. In a sensitive file, a removed line holding a cfg/cfg_attr attribute that names `test`
# is a sensitive-crate finding when, on the base, the item it was attached to (after any other
# attributes, comments and blank lines) is a `mod` declaration and that declaration line stays.
# Changing the attribute (to cfg(any(test, ...)), cfg(not(test)), ...) removes the old line too.
if [ -s "$TMP/rs-removed" ]; then
    awk -F'\t' -v sensf="$TMP/sens" "$G2R_AWK"'
    FILENAME == sensf { if ($1 == "S") sg[++ns] = g2r($2); next }
    { x = $4; gsub(/[ \t]/, "", x); if (x !~ /#!?\[cfg(_attr)?\(/ || x !~ /(^|[^A-Za-z0-9_])test([^A-Za-z0-9_]|$)/) next
      for (i = 1; i <= ns; i++) if ($1 ~ sg[i]) { if (!seen[$1]++) print $1; next } }
    ' "$TMP/sens" "$TMP/rs-removed" >"$TMP/rm-paths" || die "removed-line scan failed"
    while IFS= read -r p; do
        show_at "$BASE" "$p" >"$TMP/rm-base.rs"
        awk -F'\t' -v path="$p" -v rmf="$TMP/rs-removed" '
        function after_attrs(t,   i, n, c, k) { # skip leading attributes and comments; AD/BC carry over lines
            n = length(t); i = 1
            while (1) {
                if (BC) { k = index(substr(t, i), "*/"); if (!k) return ""; i += k + 1; BC = 0 }
                if (AD > 0) {
                    for (; i <= n; i++) { c = substr(t, i, 1); if (c == "[") AD++; else if (c == "]" && --AD == 0) { i++; break } }
                    if (AD > 0) return ""
                }
                while (i <= n && substr(t, i, 1) ~ /[ \t]/) i++
                if (i > n || substr(t, i, 2) == "//") return ""
                if (substr(t, i, 2) == "/*") { BC = 1; i += 2; continue }
                if (match(substr(t, i), /^#[ \t]*!?[ \t]*\[/)) { i += RLENGTH; AD = 1; continue }
                return substr(t, i)
            }
        }
        FILENAME == rmf {
            if ($1 != path) next
            R[$2] = 1; x = $4; gsub(/[ \t]/, "", x)
            if (x ~ /#!?\[cfg(_attr)?\(/ && x ~ /(^|[^A-Za-z0-9_])test([^A-Za-z0-9_]|$)/) { C[++nc] = $2; CN[$2] = $3; CT[$2] = $4 }
            next
        }
        { b = $0; sub(/\r$/, "", b); B[FNR] = b; nb = FNR }
        END {
            for (q = 1; q <= nc; q++) {
                L = C[q]; AD = 0; BC = 0; j = L; rem = after_attrs(B[L])
                while (rem == "" && j < nb) { j++; rem = after_attrs(B[j]) }
                if (rem == "" || j == L || (j in R)) continue
                if (rem !~ /^(pub[ \t]*(\([^)]*\))?[ \t]+)?(unsafe[ \t]+)?mod[ \t]+(r#)?[A-Za-z_][A-Za-z0-9_]*[ \t]*[;{]/) continue
                t = CT[L]; sub(/^[ \t]+/, "", t); d = t; if (length(d) > 120) d = substr(d, 1, 117) "..."
                printf "sensitive-crate\t%s\t%s\ttest cfg removed from a module declaration that stays (line %s on the base): %s\t\t\t%s\n", path, CN[L], L, d, t
            }
        }' "$TMP/rs-removed" "$TMP/rm-base.rs" >>"$TMP/findings" || die "removed-cfg check failed for $p"
    done <"$TMP/rm-paths"
fi

# ── The token helper (tools/diff-gate-tokens, built from the trusted base) ──
# Located here because the reachability step (before pass 2) uses it as well as pass 2b.
TOKENS_PROTOCOL="diff-gate-tokens protocol 2"
TOK_WHY=""; TOK_BIN=""; TOK_TRIED=0
# real_path <path>: the path with every symlink in its existing part resolved (cd -P); the part
# that does not exist yet is kept as written, and a ".." in it fails (it could climb anywhere).
real_path() {
    local p="$1" rest="" d
    while [ ! -d "$p" ]; do
        case "${p##*/}" in ..|.) return 1 ;; esac
        rest="/${p##*/}$rest"; p="${p%/*}"; [ -n "$p" ] || p=/
    done
    d="$(cd -P -- "$p" 2>/dev/null && pwd -P)" || return 1
    printf '%s%s\n' "${d%/}" "$rest"
}
REALROOT="$(cd -P -- "$ROOT" && pwd -P)" || die "cannot resolve the checkout path"
tokens_bin() { # sets TOK_BIN, or TOK_WHY and fails; runs in this shell (no subshell)
    local ref tree cache bin stage realcache realbin
    if [ -n "${DIFF_GATE_TOKENS_BIN:-}" ]; then
        echo "diff-gate: token helper from DIFF_GATE_TOKENS_BIN=$DIFF_GATE_TOKENS_BIN (not built from the base)" >&2
        bin="$DIFF_GATE_TOKENS_BIN"
    else
        ref="${DIFF_GATE_TOKENS_REF:-$BASE_TIP}"
        tree="$(git rev-parse -q --verify "$ref:tools/diff-gate-tokens" 2>/dev/null)" || { TOK_WHY="no tools/diff-gate-tokens at the trusted base $(git rev-parse --short "$ref" 2>/dev/null)"; return 1; }
        cache="${DIFF_GATE_CACHE:-${RUNNER_TEMP:-${XDG_CACHE_HOME:-$HOME/.cache}}/weftos-diff-gate}"
        case "$cache" in /*) ;; *) cache="$ROOT/$cache" ;; esac
        case "$cache/" in "$ROOT"/*) TOK_WHY="cache $cache is inside the checkout (refused)"; return 1 ;; esac
        # RV-C1: compare resolved paths, so a symlink (anywhere in the cache path) into the
        # checkout is refused too. A change could commit a binary at the predictable path.
        realcache="$(real_path "$cache")" || { TOK_WHY="cache $cache cannot be resolved (refused)"; return 1; }
        case "$realcache/" in "$REALROOT"/*) TOK_WHY="cache $cache is inside the checkout (refused; it resolves to $realcache)"; return 1 ;; esac
        bin="$cache/tokens-$tree/diff-gate-tokens"
        if [ ! -x "$bin" ]; then
            command -v cargo >/dev/null 2>&1 || { TOK_WHY="cargo not found to build tools/diff-gate-tokens"; return 1; }
            mkdir -p "$cache" && stage="$(mktemp -d "$cache/build.XXXXXX")" || { TOK_WHY="cannot create $cache"; return 1; }
            mkdir -p "$stage/src" && git archive "$tree" | tar -x -C "$stage/src" || { TOK_WHY="cannot extract the helper source"; rm -rf "$stage"; return 1; }
            echo "diff-gate: building the token helper from the base (tree $tree)" >&2
            if ! (cd "$stage/src" && CARGO_TARGET_DIR="$stage/target" cargo build --release --locked --quiet) >"$stage/build.log" 2>&1; then
                tail -5 "$stage/build.log" >&2; TOK_WHY="cargo build of tools/diff-gate-tokens failed"; rm -rf "$stage"; return 1
            fi
            # Publish atomically: a concurrent run either sees no binary or the whole one.
            mkdir -p "$stage/out" && mv "$stage/target/release/diff-gate-tokens" "$stage/out/" \
                && { mv "$stage/out" "$cache/tokens-$tree" 2>/dev/null || true; }
            rm -rf "$stage"
        fi
    fi
    if [ -z "${DIFF_GATE_TOKENS_BIN:-}" ]; then # the cached binary: a plain file whose directory is outside the checkout
        [ -L "$bin" ] && { TOK_WHY="helper $bin is a symlink (refused)"; return 1; }
        realbin="$(real_path "${bin%/*}")" || { TOK_WHY="cannot resolve ${bin%/*} (refused)"; return 1; }
        case "$realbin/" in "$REALROOT"/*) TOK_WHY="helper directory ${bin%/*} resolves into the checkout (refused)"; return 1 ;; esac
    fi
    [ -x "$bin" ] || { TOK_WHY="helper $bin is not executable"; return 1; }
    [ "$("$bin" --protocol 2>/dev/null)" = "$TOKENS_PROTOCOL" ] || { TOK_WHY="helper $bin does not speak '$TOKENS_PROTOCOL'"; return 1; }
    TOK_BIN="$bin"
}
# tokens_ready: tokens_bin once per run (the reachability step and pass 2b both use it)
tokens_ready() {
    if [ "$TOK_TRIED" = 0 ]; then TOK_TRIED=1; tokens_bin || true; fi
    [ -n "$TOK_BIN" ]
}

# ── Cargo's own reading of the manifests (R3-A1..A4, R3-B1) ──────────
# The awk reader above is a second opinion. What Cargo builds is judged by Cargo: for every
# package a change can affect, `cargo metadata --no-deps --offline --frozen` runs on the base
# and on the head, and the gate compares what it reports (build script, proc-macro, crate
# types, links, targets and their paths, dependencies with kind, target, rename and source).
# Safety: the tree cargo reads is a copy in $TMP holding only the Cargo.toml files (their
# bytes; a symlinked one gets its target's bytes when that target is a file in the same tree)
# and empty placeholders for every .rs path, so target auto-discovery sees what it would see in
# the checkout. No .cargo/config and no rust-toolchain file is copied: the path rules flag
# those. cargo is the toolchain binary itself (`rustup which cargo`, resolved outside the
# checkout), not the rustup proxy, so no toolchain file can pick it, and it runs under env -i
# with a throwaway HOME and CARGO_HOME, PATH=/usr/bin:/bin, CARGO_NET_OFFLINE and
# RUSTC=/nonexistent. `metadata --no-deps` runs no build script and no proc-macro and needs no
# rustc; --offline --frozen forbid the network. A head that cargo cannot read is a
# cargo-unparsed finding, never a pass.
# Affected packages: every changed Cargo.toml; the package around any .rs file added, deleted
# or retyped (target discovery), around a changed .cargo/config* or rust-toolchain* (forced),
# and around any changed .rs file of a sensitive crate (reachability below); and a symlinked
# Cargo.toml whose target file changed.
META="$TMP/meta"; mkdir -p "$META/home" "$META/cargo-home" "$META/base" "$META/head" || die "mkdir failed"
META="$(cd -P "$META" && pwd -P)" || die "cannot resolve $TMP"
: >"$TMP/meta-findings"; : >"$TMP/meta-raw"; : >"$TMP/meta-head.rec"; : >"$TMP/meta-base.rec"
META_STATE="ok"; META_WHY=""
# meta_list <rev|WORKTREE>: "mode <TAB> path" for every file of that side (no nested repos)
meta_list() {
    if [ "$1" = WORKTREE ]; then
        { git ls-files -z -c -o --exclude-standard; cat "$TMP/ignored-scan" 2>/dev/null; } | tr '\000' '\n' | awk '!s[$0]++' |
            while IFS= read -r f; do f="${f#./}"; if [ -L "$f" ]; then printf '120000\t%s\n' "$f"; elif [ -f "$f" ]; then printf '100644\t%s\n' "$f"; fi; done
    else
        git ls-tree -r -z --full-tree "$1" | tr '\000' '\n' | awk -F'\t' '{ split($1, m, " "); if (m[2] == "blob") print m[1] "\t" $2 }'
    fi
}
# meta_index <rev|WORKTREE> <dir>: list that side (dir.lst, dir.rs, dir.man) and resolve its
# symlinked manifests (dir.links: manifest <TAB> target, "!" for one that leaves the tree)
meta_index() {
    local side="$1" dir="$2" mode p t
    if [ "$side" = WORKTREE ]; then git ls-files -z -c -o --exclude-standard >"$TMP/nl.chk"; else git ls-tree -r -z --name-only --full-tree "$side" >"$TMP/nl.chk"; fi
    [ "$(tr -cd '\n' <"$TMP/nl.chk" | wc -c)" -eq 0 ] || die "a path in the tree holds a newline (refused)"
    meta_list "$side" >"$dir.lst" || die "cannot list $side"
    awk -F'\t' 'tolower($2) ~ /\.rs$/ { print $2 }' "$dir.lst" >"$dir.rs"
    awk -F'\t' '{ n = split(tolower($2), a, "/"); if (a[n] == "cargo.toml") print }' "$dir.lst" >"$dir.man"
    : >"$dir.links"
    while IFS="$(printf '\t')" read -r mode p; do
        [ "$mode" = 120000 ] || continue
        if [ "$side" = WORKTREE ]; then t="$(readlink "$p")"; else t="$(git cat-file blob "$side:$p")"; fi
        case "$t" in /*) t="/!" ;; esac
        case "$p" in */*) t="${p%/*}/$t" ;; esac
        t="$(printf '%s' "$t" | awk -F/ '{ k = 0; for (i = 1; i <= NF; i++) { if ($i == "" || $i == ".") continue; if ($i == ".." || $i == "!") { if (k == 0 || $i == "!") { print "!"; exit } k--; continue } s[++k] = $i } o = ""; for (i = 1; i <= k; i++) o = o (i > 1 ? "/" : "") s[i]; print o }')"
        printf '%s\t%s\n' "$p" "$t" >>"$dir.links"
    done <"$dir.man"
}
# meta_build <rev|WORKTREE> <dir>: cargo's view of that side in dir: every Cargo.toml's bytes
# (a symlinked one: its target's, when that is a file of this tree) and an empty file per .rs
meta_build() {
    local side="$1" dir="$2" mode p t
    { awk '{ d = $0; if (sub(/\/[^\/]*$/, "", d)) print d }' "$dir.rs"; awk -F'\t' '{ d = $2; if (sub(/\/[^\/]*$/, "", d)) print d }' "$dir.man"; } |
        sort -u | (cd "$dir" && tr '\n' '\000' | xargs -0 mkdir -p) || die "cannot build the cargo tree"
    (cd "$dir" && tr '\n' '\000' | xargs -0 touch) <"$dir.rs" || die "cannot build the cargo tree"
    while IFS="$(printf '\t')" read -r mode p; do
        rm -f "$dir/$p"
        if [ "$mode" = 120000 ]; then
            t="$(awk -F'\t' -v p="$p" '$1 == p { print $2; exit }' "$dir.links")"
            if [ "$t" != "!" ] && awk -F'\t' -v t="$t" '$2 == t && $1 != "120000" { f = 1 } END { exit !f }' "$dir.lst"; then
                show_at "$side" "$t" >"$dir/$p"
            else
                printf 'symlinked manifest that leaves the tree or names no file\n' >"$dir/$p"
            fi
        else
            show_at "$side" "$p" >"$dir/$p"
        fi
    done <"$dir.man"
}
# meta_affected: manifests (head or base paths) whose package a change can affect
meta_affected() {
    awk -F'\t' -v sensf="$TMP/sens" -v hm="$META/head.man" -v bm="$META/base.man" -v hl="$META/head.links" -v bl="$META/base.links" "$G2R_AWK"'
    function near(p,   d) { d = p; while (1) { if (!sub(/\/[^\/]*$/, "", d)) d = ""; if ((d == "" ? "Cargo.toml" : d "/Cargo.toml") in M) { print (d == "" ? "Cargo.toml" : d "/Cargo.toml"); return } if (d == "") return } }
    FILENAME == sensf { if ($1 == "S") sg[++ns] = g2r($2); next }
    FILENAME == hm || FILENAME == bm { M[$2] = 1; next }
    FILENAME == hl || FILENAME == bl { L[$2] = L[$2] SUBSEP $1; next }
    {
        st = $1; p = $4; lp = tolower(p); n = split(lp, a, "/")
        if (a[n] == "cargo.toml") { print p; next }
        if ((st == "A" || st == "D" || st == "T") && lp ~ /\.rs$/) near(p)
        if (lp ~ /(^|\/)\.cargo\/config/ || lp ~ /(^|\/)rust-toolchain(\.toml)?$/) { q = p; sub(/(^|\/)\.[cC][aA][rR][gG][oO]\/[^\/]*$/, "", q); near(q "/x") }
        if (lp ~ /\.rs$/) for (i = 1; i <= ns; i++) if (p ~ sg[i]) { near(p); break }
        if (p in L) { k = split(substr(L[p], 2), b, SUBSEP); for (i = 1; i <= k; i++) print b[i] }
    }' "$TMP/sens" "$META/head.man" "$META/base.man" "$META/head.links" "$META/base.links" "$TMP/files" | awk '!s[$0]++'
}
# meta_cargo: the cargo binary of the toolchain, resolved outside the checkout
meta_cargo() {
    local c=""
    if [ -n "${DIFF_GATE_CARGO:-}" ]; then c="$DIFF_GATE_CARGO"
    else
        c="$(cd "$META" && rustup which cargo 2>/dev/null)" || c=""
        [ -n "$c" ] || c="$(cd "$META" && command -v cargo 2>/dev/null)" || c=""
    fi
    [ -n "$c" ] && [ -x "$c" ] || { META_WHY="cargo not found"; return 1; }
    case "$(real_path "${c%/*}" 2>/dev/null)/" in "$REALROOT"/*) META_WHY="cargo $c resolves into the checkout (refused)"; return 1 ;; esac
    CARGO_BIN="$c"
}
# The JSON cargo prints, flattened by a small reader (cargo writes it, so it is well formed;
# anything else fails closed), into records: P mp links / T mp kinds crate_types src name /
# D mp kind target key package source path registry. Paths are relative to the side's tree.
META_JSON_AWK='
function hexv(h,   i, v) { v = 0; h = tolower(h); for (i = 1; i <= length(h); i++) v = v * 16 + index("0123456789abcdef", substr(h, i, 1)) - 1; return v }
function escv(s) { gsub(/%/, "%25", s); gsub(/\t/, "%09", s); gsub(/\n/, "%0A", s); gsub(/\r/, "%0D", s); return s }
function relp(p) { if (p == root) return "."; if (index(p, root "/") == 1) return substr(p, length(root) + 2); if (index(p, par "/") == 1) return "../" substr(p, length(par) + 2); return p }
function push(t) { d++; T[d] = t; K[d] = (t == "a") ? 0 : ""; km = (t == "o") }
function emit(ty, v,   n, m, f) {
    if (d == 1 && K[1] == "workspace_root") return
    if (K[1] != "packages" || d < 3) return
    n = K[2] + 0; if (n > np) np = n
    if (d == 3 && (K[3] == "manifest_path" || K[3] == "links")) { PK[n, K[3]] = (ty == "s" ? v : ""); return }
    if (K[3] == "targets") {
        m = K[4] + 0; if (m + 1 > nt[n]) nt[n] = m + 1; f = K[5]
        if (d == 5 && (f == "src_path" || f == "name")) TG[n, m, f] = v
        else if (d == 6 && (f == "kind" || f == "crate_types")) TG[n, m, f] = TG[n, m, f] (TG[n, m, f] == "" ? "" : ",") v
        return
    }
    if (K[3] == "dependencies" && d == 5) { m = K[4] + 0; if (m + 1 > nd[n]) nd[n] = m + 1; DP[n, m, K[5]] = (ty == "s" ? v : "") }
}
function str(s) { if (T[d] == "o" && km) { K[d] = s; km = 0 } else emit("s", s) }
function feed(c,   v) {
    if (ins) {
        if (uh > 0) { ux = ux c; if (--uh == 0) { v = hexv(ux); sb = sb ((v > 0 && v < 128) ? sprintf("%c", v) : "\\u{" tolower(ux) "}") } return }
        if (es) { es = 0
            if (c == "u") { uh = 4; ux = ""; return }
            if (c == "n") c = "\n"; else if (c == "t") c = "\t"; else if (c == "r") c = "\r"; else if (c == "b") c = "\b"; else if (c == "f") c = "\f"
            else if (c != "\"" && c != "\\" && c != "/") bad = 1
            sb = sb c; return }
        if (c == "\\") { es = 1; return }
        if (c == "\"") { ins = 0; str(sb); return }
        sb = sb c; return
    }
    if (c == "\"") { if (lt != "") bad = 1; ins = 1; sb = ""; return }
    if (c ~ /[-0-9a-zA-Z.+]/) { lt = lt c; return }
    if (lt != "") { emit("l", lt); lt = "" }
    if (c == "{") { push("o"); return }
    if (c == "[") { push("a"); return }
    if (c == "}" || c == "]") { if (d < 1 || T[d] != (c == "}" ? "o" : "a")) bad = 1; d--; if (d < 0) d = 0; return }
    if (c == ",") { if (T[d] == "a") K[d]++; else km = 1; return }
    if (c == ":") { km = 0; return }
    if (c != " " && c != "\t" && c != "\n" && c != "\r") bad = 1
}
BEGIN { RS = ","; par = root; sub(/\/[^\/]*$/, "", par); np = -1 }
{ if (NR > 1) feed(","); L = length($0); for (i = 1; i <= L; i++) feed(substr($0, i, 1)) }
END {
    if (lt != "") emit("l", lt)
    if (bad || ins || d != 0 || NR == 0) exit 3
    for (n = 0; n <= np; n++) {
        mp = relp(PK[n, "manifest_path"]); if (mp == "") exit 3
        printf "P\t%s\t%s\n", escv(mp), escv(PK[n, "links"])
        for (m = 0; m < nt[n]; m++) printf "T\t%s\t%s\t%s\t%s\t%s\n", escv(mp), escv(TG[n, m, "kind"]), escv(TG[n, m, "crate_types"]), escv(relp(TG[n, m, "src_path"])), escv(TG[n, m, "name"])
        for (m = 0; m < nd[n]; m++) {
            k = DP[n, m, "rename"]; if (k == "") k = DP[n, m, "name"]
            pth = DP[n, m, "path"]; if (pth != "") pth = relp(pth)
            printf "D\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", escv(mp), escv(DP[n, m, "kind"]), escv(DP[n, m, "target"]), escv(k), escv(DP[n, m, "name"]), escv(DP[n, m, "source"]), escv(pth), escv(DP[n, m, "registry"])
        }
    }
}'
# meta_run <base|head> <manifest>: cargo metadata for the workspace holding manifest (once per
# workspace). Records go to meta-<side>.rec; a failure is noted in meta-<side>.fail.
meta_run() {
    local side="$1" mp="$2" dir="$META/$1" out rc err
    grep -qxF -- "$mp" "$META/$side.covered" 2>/dev/null && return 0
    [ -f "$dir/$mp" ] || return 0
    out="$META/run.json"; err="$META/run.err"
    (cd "$dir" && env -i PATH=/usr/bin:/bin HOME="$META/home" CARGO_HOME="$META/cargo-home" CARGO_NET_OFFLINE=true \
        RUSTC=/nonexistent/rustc CARGO_TERM_COLOR=never "$CARGO_BIN" metadata --no-deps --offline --frozen --format-version 1 \
        --manifest-path "$dir/$mp") >"$out" 2>"$err"; rc=$?
    if [ "$rc" -ne 0 ] || ! awk -v root="$dir" "$META_JSON_AWK" <"$out" >"$META/run.rec"; then
        [ "$rc" -ne 0 ] || printf 'cargo metadata output could not be read\n' >>"$err"
        printf '%s\t%s\n' "$mp" "$(grep -v '^[[:space:]]*$' "$err" | sed "s|$dir/||g" | tr '\t\n' '  ' | cut -c1-300)" >>"$META/$side.fail"
        echo "$mp" >>"$META/$side.covered"
        return 0
    fi
    cat "$META/run.rec" >>"$TMP/meta-$side.rec"
    { echo "$mp"; awk -F'\t' '$1 == "P" { print $2 }' "$META/run.rec"; } >>"$META/$side.covered"
}
: >"$META/base.fail"; : >"$META/head.fail"; : >"$META/base.covered"; : >"$META/head.covered"
meta_index "$head_src" "$META/head"
meta_index "$BASE" "$META/base"
meta_affected >"$TMP/meta-affected" || die "cannot list the packages a change affects"
if [ -s "$TMP/meta-affected" ]; then
    meta_build "$head_src" "$META/head"
    meta_build "$BASE" "$META/base"
    if meta_cargo; then
        while IFS= read -r mp; do meta_run head "$mp"; meta_run base "$mp"; done <"$TMP/meta-affected"
    else
        META_STATE="unavailable"
        echo "diff-gate: cargo metadata unavailable: $META_WHY" >&2
        while IFS= read -r mp; do
            [ -f "$META/head/$mp" ] && printf 'cargo-unparsed\t%s\t0\tcargo metadata unavailable, cannot judge what Cargo builds (%s)\t\t\t%s\n' "$mp" "$META_WHY" "$META_WHY" >>"$TMP/meta-findings"
        done <"$TMP/meta-affected"
    fi
fi
# a head cargo cannot read is a finding; the package's records are then missing on that side
while IFS="$(printf '\t')" read -r mp why; do
    printf 'cargo-unparsed\t%s\t0\tcargo cannot read this package: %s\t\t\t%s\n' "$mp" "$why" "$why" >>"$TMP/meta-findings"
done <"$META/head.fail"

# ── Name-exempt files: built by a non-test root? (R3-B1..B4) ─────────
# A file under a [[sensitive]] glob that is exempt by its name (tests/, *_tests.rs, ...) is
# test-only only if no non-test Cargo target of its package builds it. The roots are what
# cargo metadata (above) reports for that package on the head: lib, bin, proc-macro and build
# script targets (test, bench and example targets are test roots). The token helper walks the
# modules from them on rustc's tokens (`mod`, #[path], cfg_attr path, include!, at any layout,
# with escapes decoded, case-insensitively, following symlinks inside the extracted crate).
# Without the helper, without cargo's answer for the package, or when the walk meets something
# it cannot follow (a computed include!), nothing in that package is exempt.
: >"$TMP/ex-ok"; : >"$TMP/ex-cands"
cut -f1 "$TMP/rs-added" | awk '!s[$0]++' | awk -F'\t' -v sensf="$TMP/sens" "$G2R_AWK"'
    FILENAME == sensf { if ($1 == "S") sg[++ns] = g2r($2); else if ($1 == "E") eg[++ne] = g2r($2); next }
    { s = 0; e = 0; for (i = 1; i <= ns; i++) if ($0 ~ sg[i]) s = 1; for (i = 1; i <= ne; i++) if ($0 ~ eg[i]) e = 1; if (s && e) print }
    ' "$TMP/sens" - >"$TMP/ex-cands" || die "exempt scan failed"
if [ -s "$TMP/ex-cands" ]; then
    # each candidate's package: the nearest head package (from cargo metadata) above it
    awk -F'\t' 'FILENAME == ARGV[1] { if ($1 == "P") { d = $2; if (!sub(/\/[^\/]*$/, "", d)) d = ""; PD[d] = $2 } next }
        { d = $0; while (1) { if (!sub(/\/[^\/]*$/, "", d)) d = ""; if (d in PD) { print $0 "\t" PD[d] "\t" d; next } if (d == "") break } print $0 "\t\t" }' \
        "$TMP/meta-head.rec" "$TMP/ex-cands" >"$TMP/ex-pkg" || die "exempt scan failed"
    REACH_WHY=""
    if [ "$META_STATE" != ok ]; then REACH_WHY="cargo metadata unavailable"
    elif ! tokens_ready; then REACH_WHY="token helper unavailable: $TOK_WHY"
    else
        R="$TMP/reach"; mkdir -p "$R/tree" || die "mkdir failed"; : >"$R/roots"
        awk -F'\t' '$2 != "" && !s[$2]++ { print $2 "\t" $3 }' "$TMP/ex-pkg" >"$R/pkgs"
        while IFS="$(printf '\t')" read -r mp d; do
            [ -n "$d" ] || d=.
            if [ "$WORKTREE" = 1 ]; then
                (find "$d" \( -name target -o -name .git \) -prune -o \( -type f -o -type l \) -print0 | tar --null -T - -cf - | tar -x -C "$R/tree") 2>/dev/null \
                    || { REACH_WHY="cannot copy $d"; break; }
            else
                git archive --format=tar "$HEAD_SHA" -- "$d" | tar -x -C "$R/tree" || { REACH_WHY="cannot extract $d"; break; }
            fi
            awk -F'\t' -v mp="$mp" '$1 == "T" && $2 == mp && $3 !~ /(^|,)(test|bench|example)(,|$)/ { print $5 }' "$TMP/meta-head.rec" >>"$R/roots"
        done <"$R/pkgs"
        if [ -z "$REACH_WHY" ] && ! "$TOK_BIN" --reach "$R/tree" "$R/roots" >"$R/out" 2>"$R/err"; then
            REACH_WHY="helper --reach failed: $(head -c 200 "$R/err" | tr '\t\n' '  ')"
        fi
    fi
    # Emits the test-only ones to ex-ok; says why for the others.
    awk -F'\t' -v why="$REACH_WHY" -v ro="$TMP/reach/out" '
        FILENAME == ro { if ($1 == "N") N[$2] = 1; else if ($1 == "U") U[++nu] = $2 SUBSEP $3; next }
        {
            f = $1; mp = $2; d = $3; w = why
            if (w == "" && mp == "") w = "cargo metadata names no package around it"
            if (w == "" && (f in N)) w = "a non-test target of " mp " builds it"
            for (i = 1; i <= nu && w == ""; i++) { split(U[i], u, SUBSEP); if (d == "" || index(u[1], d "/") == 1) w = "the module walk cannot follow " u[1] " (" u[2] ")" }
            if (w == "") print f > "/dev/stdout"
            else print "diff-gate: " f " is not treated as test-only: " w > "/dev/stderr"
        }' "$(test -f "$TMP/reach/out" && echo "$TMP/reach/out" || echo /dev/null)" "$TMP/ex-pkg" >"$TMP/ex-ok" || die "exempt check failed"
fi

# ── Pass 2: lex added .rs lines ──────────────────────────────────────
# A line-by-line regex cannot know whether a line starts inside a string or a block comment,
# so each .rs file with added lines is lexed in full (nested block comments, strings, raw
# strings, char literals vs lifetimes) and the added line is also lexed on its own. Rules run
# on both results; either one finding a token is a finding (false positives over misses).
# Lexed text keeps every byte position, so findings carry token spans for the approvals stage.
# In a sensitive file every added non-blank line that is not test-only is also a sensitive-crate
# finding. Test-only lines are found on the full-file lex: a #[cfg(test)] or
# #[cfg(all(test, ...))] attribute, then only attributes, comments or blank lines, then
# `mod name {`; the region ends where its brace closes. A line with code after the closing
# brace is not test-only. A region counts only if it closes and the file's braces balance at
# the end (and never go negative), so a lexer confusion cannot exempt code.
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
    awk -v sensf="$TMP/sens" -v idx="$TMP/rs-index" -v add="$TMP/rs-added" -v exf="$TMP/ex-ok" -v addr_types='SocketAddrV4|SocketAddrV6|SocketAddr|IpAddr|Ipv4Addr|Ipv6Addr|AddrParseError' "$G2R_AWK"'
    BEGIN { HASHES = "################################################################"; SP = "        " }
    function insg(p,   i) { for (i = 1; i <= nsg; i++) if (p ~ sg[i]) return 1; return 0 }
    function insens(p,   i) { # sensitive and not test-only (a name-exempt file only when ex-ok says so)
        if (!insg(p)) return 0
        for (i = 1; i <= neg; i++) if (p ~ eg[i]) return !(p in EXOK)
        return 1
    }
    # Test-region tracking over the lexed full file (strings, chars and comments are blank).
    function attr_end(s, i,   d, n, c) { # s[i] is "[": index of its matching "]", 0 if none
        d = 0; n = length(s)
        for (; i <= n; i++) { c = substr(s, i, 1); if (c == "[") d++; else if (c == "]" && --d == 0) return i }
        return 0
    }
    function testattr(a) { gsub(/[ \t]/, "", a); return (a == "#[cfg(test)]" || substr(a, 1, 15) == "#[cfg(all(test,") }
    function walk(o, from, bpos,   i, n, c) { # brace depth over o[from..]; opens the region at bpos; returns where it closes
        n = length(o)
        for (i = from; i <= n; i++) {
            c = substr(o, i, 1)
            if (c == "{") { if (i == bpos) { TR = 1; TRD = DEP } DEP++ }
            else if (c == "}") { DEP--; if (DEP < 0) BROKEN = 1; if (TR && DEP == TRD) { TR = 0; return i } }
        }
        return 0
    }
    function closed(o, endp, ln) { # region closed at o[endp]: keep ln unless code follows
        if (substr(o, endp + 1) ~ /^[ \t]*$/) RL = RL " " ln
        FT = FT RL; RL = ""
        walk(o, endp + 1, 0)
    }
    function track(o, ln,   pos, rest, e, endp) {
        if (BROKEN) return
        if (TR) {
            endp = walk(o, 1, 0)
            if (endp) closed(o, endp, ln); else RL = RL " " ln
            return
        }
        pos = 1
        while (1) {
            rest = substr(o, pos)
            if (rest ~ /^[ \t]*$/) break
            if (match(rest, /^[ \t]*#[ \t]*\[/)) {
                e = attr_end(o, pos + RLENGTH - 1)
                if (!e) { PEND = 0; break }
                if (testattr(substr(o, pos, e - pos + 1))) PEND = 1
                pos = e + 1; continue
            }
            if (PEND && match(rest, /^[ \t]*(pub[ \t]*(\([^)]*\))?[ \t]+)?mod[ \t]+(r#)?[A-Za-z_][A-Za-z0-9_]*[ \t]*\{/)) {
                RL = PL; PL = ""; PEND = 0
                endp = walk(o, 1, pos + RLENGTH - 1)
                if (endp) closed(o, endp, ln); else RL = RL " " ln
                return
            }
            PEND = 0; break
        }
        walk(o, 1, 0)
        if (PEND) PL = PL " " ln; else PL = ""
    }
    function finish(   n, i, t) { # end of a file: keep its test lines only if its braces balanced
        if (!BROKEN && !TR && DEP == 0) { n = split(FT, t, " "); for (i = 1; i <= n; i++) TST[PP SUBSEP t[i]] = 1 }
        DEP = 0; TR = 0; TRD = 0; PEND = 0; BROKEN = 0; RL = ""; PL = ""; FT = ""
    }
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
        scan("process", ns, "process::\\{[^}]*Command|process::\\{|process::Command|Command::new|std::process[ \t]*(;|,|}|[ \t]as[ \t]|$)|[{,][ \t]*process[ \t]*(,|}|[ \t]as[ \t]|$)|tokio::process|async_process|CommandExt|libc::(exec[a-z]*|fork|vfork|system|posix_spawn[a-z]*|popen)|nix::unistd::(fork|exec[a-z]*)|nix::spawn::posix_spawn[a-z]*", 1)
        # #[link] and link_name only act on an extern block, which is itself a finding
        scan("ffi", ns, "(^|[^A-Za-z0-9_])extern([^A-Za-z0-9_]|$)|(^|[^A-Za-z0-9_])(no_mangle|export_name|link_section)([^A-Za-z0-9_]|$)", 0)
        scan("include", ns, "(^|[^A-Za-z0-9_])include([ \t]*![ \t]*[([{]|[ \t]+as([^A-Za-z0-9_]|$))", 0)
        if (ns ~ /#!?\[/) scan("include", ns, "(^|[^A-Za-z0-9_])path[ \t]*=", 0)
    }
    FILENAME == sensf { if ($0 ~ /^S\t/) sg[++nsg] = g2r(substr($0, 3)); else if ($0 ~ /^E\t/) eg[++neg] = g2r(substr($0, 3)); next }
    FILENAME == idx { split($0, f, "\t"); pathof[f[1]] = f[2]; next }
    FILENAME == add {
        split($0, f, "\t"); k = f[1] SUBSEP f[2]; want[k] = 1
        na++; ap[na] = f[1]; al[na] = f[2]; at[na] = substr($0, length(f[1]) + length(f[2]) + 3); next
    }
    FILENAME == exf { EXOK[$0] = 1; next }
    FNR == 1 { if (PP != "") finish(); LB = 0; LS = 0; LH = 0; P = pathof[FILENAME]; PP = P; SENSG = insg(P) }
    { s = $0; sub(/\r$/, "", s); o = lex(s); if (SENSG) track(o, FNR); if ((P SUBSEP FNR) in want) { full[P SUBSEP FNR] = o; ftxt[P SUBSEP FNR] = s } }
    END {
        if (PP != "") finish()
        for (q = 1; q <= na; q++) {
            k = ap[q] SUBSEP al[q]; TXT = at[q]; sub(/\r$/, "", TXT)
            if (!(k in full)) { print "diff-gate: added line " ap[q] ":" al[q] " not found in the file" > "/dev/stderr"; exit 3 }
            if (ftxt[k] != TXT) { gsub(/\t/, " ", ftxt[k]); if (ftxt[k] != TXT) { print "diff-gate: " ap[q] ":" al[q] " differs from the diff" > "/dev/stderr"; exit 3 } }
            delete TK; LB = 0; LS = 0; LH = 0
            rules(full[k]); rules(lex(TXT))
            lead = TXT; sub(/[^ \t].*$/, "", lead); off = length(lead); T = substr(TXT, off + 1)
            if (T != "" && !(k in TST) && insens(ap[q])) {
                d = T; if (length(d) > 160) d = substr(d, 1, 157) "..."
                print "sensitive-crate\t" ap[q] "\t" al[q] "\t" d "\t\t\t" T
            }
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
    }' "$TMP/sens" "$TMP/rs-index" "$TMP/rs-added" "$TMP/ex-ok" "$@" >>"$TMP/findings" || die "lexer stage failed"
fi

# ── Pass 2b: token scan (tools/diff-gate-tokens, built from the trusted base) ──
# The line lexer above is a heuristic; this pass lexes every changed .rs file the way rustc
# does (proc-macro2 tokens, syn for `use` trees), resolves aliases and globs within the file,
# scans doctests, and also reports unchanged lines whose meaning an edit changed. Its
# findings are merged with the lexer's. The helper is built from the base tip's
# tools/diff-gate-tokens (never from the change under review) into a cache keyed by that
# tree's hash, outside the checkout. If it cannot be built or run, every changed .rs file
# gets an owner-only "rust-tokens" finding: never a silent pass.
# DIFF_GATE_TOKENS_BIN (a prebuilt helper) is for local testing only; CI never sets it.
: >"$TMP/tok-manifest"; : >"$TMP/tok-out"; mkdir -p "$TMP/tok" || die "mkdir failed"
awk -F'\t' '$1 != "D" && tolower($4) ~ /\.rs$/ { print $4 }' "$TMP/files" | awk '!s[$0]++' >"$TMP/tok-paths"
# Split the added .rs lines by file in one pass (rs-added-by/<n> for the n-th path), so the
# manifest loop below does not rescan every added line once per file.
mkdir -p "$TMP/rs-added-by" || die "mkdir failed"
awk -F'\t' -v d="$TMP/rs-added-by" 'FILENAME == ARGV[1] { ix[$0] = FNR; next }
    ($1 in ix) { f = d "/" ix[$1]; print >>f; close(f) }' "$TMP/tok-paths" "$TMP/rs-added" || die "splitting added lines failed"
n=0
while IFS= read -r p; do
    n=$((n + 1))
    show_at "$head_src" "$p" | tr '\000' '\001' >"$TMP/tok/h$n"
    [ "${PIPESTATUS[0]}${PIPESTATUS[1]}" = 00 ] || die "cannot read $p"
    bfile=""
    if git rev-parse --verify -q "$BASE:$p" >/dev/null 2>&1; then
        show_at "$BASE" "$p" | tr '\000' '\001' >"$TMP/tok/b$n" || die "cannot read $p on the base"
        bfile="$TMP/tok/b$n"
    fi
    added="$( [ -f "$TMP/rs-added-by/$n" ] && cut -f2 "$TMP/rs-added-by/$n" | paste -sd, - )"
    printf '%s\t%s\t%s\t%s\n' "$p" "$TMP/tok/h$n" "$bfile" "$added" >>"$TMP/tok-manifest"
done <"$TMP/tok-paths"

if [ -s "$TMP/tok-manifest" ]; then
    if tokens_ready; then
        if ! "$TOK_BIN" --manifest "$TMP/tok-manifest" >"$TMP/tok-out" 2>"$TMP/tok-err"; then
            TOK_WHY="helper failed: $(head -c 200 "$TMP/tok-err" | tr '\t\n' '  ')"
        elif ! awk -F'\t' 'FILENAME == ARGV[1] { ok[$1] = 1; next }
                 !(($2 in ok) && $3 ~ /^[0-9]+$/ && $1 ~ /^[a-z-]+$/ && (NF == 4 || (NF == 7 && $5 == "" && $6 ~ /^[0-9]+-[0-9]+(\|[0-9]+-[0-9]+)*$/))) { exit 1 }' \
                 "$TMP/tok-paths" "$TMP/tok-out"; then
            TOK_WHY="helper output is malformed"
        fi
    fi
    if [ -n "$TOK_WHY" ]; then
        echo "diff-gate: token helper unavailable: $TOK_WHY" >&2
        awk -v why="$TOK_WHY" '{ gsub(/\t/, " ", why); print "rust-tokens\t" $0 "\t0\thelper unavailable: review-required (" why ")" }' "$TMP/tok-paths" >>"$TMP/findings"
    else
        cat "$TMP/tok-out" >>"$TMP/findings"
    fi
fi
# One finding per rule and line: merge the lexer's and the helper's occurrence spans.
awk -F'\t' -v OFS='\t' '
NF == 7 {
    k = $1 SUBSEP $2 SUBSEP $3 SUBSEP $7
    if (k in g) { n = split($6, a, "|"); for (i = 1; i <= n; i++) if (index("|" g[k] "|", "|" a[i] "|") == 0) g[k] = g[k] "|" a[i]; next }
    g[k] = $6; ord[++no] = k; rec[k] = $0; next
}
{ print }
END { for (i = 1; i <= no; i++) { k = ord[i]; split(rec[k], f, "\t"); print f[1], f[2], f[3], f[4], f[5], g[k], f[7] } }
' "$TMP/findings" >"$TMP/findings.m" && mv "$TMP/findings.m" "$TMP/findings" || die "merging findings failed"

# ── Pass 3: Cargo.toml entries (needs the full file, not hunks) ──────
# cargo_entries (defined above) reads each manifest in full on the base and the head.
if [ -s "$TMP/cargo-paths" ]; then
    show_at "$BASE" Cargo.toml >"$TMP/root.toml"
    cargo_entries <"$TMP/root.toml" >"$TMP/root-entries" || die "cannot parse the base Cargo.toml"
    grep '^workspace\.dependencies|' "$TMP/root-entries" | cut -f1 >"$TMP/ws-root"
    while IFS= read -r p; do
        show_at "$BASE" "$p" >"$TMP/base.toml"; show_at "$head_src" "$p" >"$TMP/head.toml"
        cargo_entries <"$TMP/base.toml" >"$TMP/base-entries" || die "cannot parse $p on the base"
        cargo_entries <"$TMP/head.toml" >"$TMP/head-entries" || die "cannot parse $p"
        awk -F'\t' -v path="$p" -v sens="$(is_sensitive "$p")" '
        FILENAME ~ /ws-root$/ { wsroot[$1] = 1; next }
        FILENAME ~ /base-entries$/ { bsrc[$1] = bsrc[$1] $2; inbase[$1] = 1; next }
        {
            if (!($1 in hsrc)) { order[++n] = $1; hline[$1] = $4; htext[$1] = $5 }
            hsrc[$1] = hsrc[$1] $2; hws[$1] = hws[$1] || $3
        }
        END {
            for (i = 1; i <= n; i++) {
                k = order[i]; sec = substr(k, 1, index(k, "|") - 1)
                if (sec == "unparsed") {
                    # fail closed: a statement the reader cannot place, or a key path naming a
                    # dependency table in a form it does not know, that the base did not have
                    if (!(k in inbase)) printf "cargo-unparsed\t%s\t%s\tcannot classify this Cargo.toml statement: %s\t\t\t%s\n", path, hline[k], htext[k], htext[k]
                    continue
                }
                if (sec == "target-path") {
                    if (k in inbase) continue
                    if (tolower(substr(k, 13)) !~ /\.rs$/) printf "include\t%s\t%s\ttarget path to a non-.rs file: %s\t\t\t%s\n", path, hline[k], htext[k], htext[k]
                    # in a sensitive crate any new target path counts: it could point at an exempt file
                    if (sens == 1) printf "sensitive-crate\t%s\t%s\tnew or changed target path: %s\t\t\t%s\n", path, hline[k], htext[k], htext[k]
                    continue
                }
                if (!(k in inbase)) {
                    if (sec !~ /^(patch|replace)/ && hws[k] && (("workspace.dependencies|" substr(k, index(k, "|") + 1)) in wsroot)) continue
                    printf "new-dep\t%s\t%s\tnew entry in [%s]: %s\t\t\t%s\n", path, hline[k], sec, htext[k], htext[k]
                } else if (hsrc[k] != bsrc[k]) {
                    printf "new-dep\t%s\t%s\tsource changed in [%s]: %s\t\t\t%s\n", path, hline[k], sec, htext[k], htext[k]
                }
            }
        }' "$TMP/ws-root" "$TMP/base-entries" "$TMP/head-entries" >>"$TMP/findings" || die "cargo pass failed for $p"
    done <"$TMP/cargo-paths"
fi

# ── Cargo's view, base against head (see "Cargo's own reading" above) ──
# Findings: a build script that now runs (new, moved, or turned on: a build key out of
# [package.metadata], `build = false` deleted, a manifest next to an existing build.rs); a
# proc-macro library (proc-macro = true or crate-type = ["proc-macro"]); a new cdylib, staticlib
# or dylib crate type (ffi); a links value (build-script); a target path that is new (include
# when it is not .rs; sensitive-crate in a sensitive crate); a dependency that is new or whose
# package, source, path or registry changed (new-dep). Each is reported on the manifest line
# the awk reader finds for it, with the same text pass 3 uses, so existing rows keep matching
# and a finding both see is reported once; when no line is found, on line 0 with a text that
# names what cargo reports. A package cargo could not read on the base counts as new.
if [ -s "$TMP/meta-head.rec" ]; then
    [ -s "$TMP/ws-root" ] || { cargo_entries <"$META/base/Cargo.toml" 2>/dev/null | grep '^workspace\.dependencies|' | cut -f1 >"$TMP/ws-root"; } 2>/dev/null
    : >>"$TMP/ws-root"
    awk -F'\t' -v OFS='\t' -v bf="$META/base.fail" -v brec="$TMP/meta-base.rec" -v bx="$META/base.rs" -v hx="$META/head.rs" -v flist="$TMP/files" '
    function has(list, x) { return index("," list ",", "," x ",") > 0 }
    function bn(p,   a, n) { n = split(tolower(p), a, "/"); return a[n] }
    FILENAME == bf { BF[$1] = 1; next }
    FILENAME == bx { BX[$0] = 1; next }
    FILENAME == hx { HX[$0] = 1; next }
    FILENAME == flist { ST[$4] = $1; next }
    FILENAME == brec {
        if ($1 == "P") { BP[$2] = 1; BL[$2] = $3; d = $2; if (!sub(/\/[^\/]*$/, "", d)) d = "."; BDIR[d] = 1 }
        else if ($1 == "T") {
            BT[$2, $5] = 1
            if (has($3, "custom-build")) BB[$2, $5] = 1
            if (has($3, "proc-macro") || has($4, "proc-macro")) BPM[$2] = 1
            n = split($4, c, ","); for (i = 1; i <= n; i++) BCT[$2, c[i]] = 1
        } else if ($1 == "D") { BD[$2, $3, $4, $5] = $6 SUBSEP $7 SUBSEP $8 SUBSEP $9; BE[$6 SUBSEP $7 SUBSEP $8 SUBSEP $9] = 1 }
        next
    }
    $1 == "P" { HPO[++nh] = $2; HL[$2] = $3; next }
    $1 == "T" { HT[++nt] = $0; next }
    $1 == "D" { HD[++nd] = $0; next }
    END {
        for (i = 1; i <= nh; i++) { mp = HPO[i]; known[mp] = ((mp in BP) && !(mp in BF)) }
        for (i = 1; i <= nt; i++) {
            split(HT[i], t, "\t"); mp = t[2]; k = known[mp]
            # a build script whose file is missing builds nothing; a non-.rs one is assumed present
            hx = (t[5] in HX) || tolower(t[5]) !~ /\.rs$/; bx = (t[5] in BX) || tolower(t[5]) !~ /\.rs$/
            if (has(t[3], "custom-build") && hx && !(k && ((mp, t[5]) in BB) && bx)) {
                if (!((ST[t[5]] == "A" || ST[t[5]] == "T") && bn(t[5]) == "build.rs")) print "BUILD", mp, t[5]
            }
            if ((has(t[3], "proc-macro") || has(t[4], "proc-macro")) && !(k && (mp in BPM)) && !pm[mp]++) print "PROCM", mp, t[5]
            n = split(t[4], c, ","); for (j = 1; j <= n; j++) if (c[j] ~ /^(cdylib|staticlib|dylib)$/ && !(k && ((mp, c[j]) in BCT)) && !ct[mp, c[j]]++) print "CTYPE", mp, c[j], t[6]
            if (!(k && ((mp, t[5]) in BT))) print "TPATH", mp, t[5], t[3], t[6]
        }
        for (i = 1; i <= nh; i++) { mp = HPO[i]; if (HL[mp] != "" && !(known[mp] && HL[mp] == BL[mp])) print "LINKS", mp, HL[mp] }
        for (i = 1; i <= nd; i++) {
            split(HD[i], x, "\t"); mp = x[2]; a = x[6] SUBSEP x[7] SUBSEP x[8] SUBSEP x[9]
            if (!known[mp] || !((mp, x[3], x[4], x[5]) in BD)) {
                b = (x[8] != "" && x[8] !~ /^(\.\.\/|\/)/ && (x[8] in BDIR)) || (a in BE) || (x[7] == "registry+https://github.com/rust-lang/crates.io-index" && x[8] == "" && x[9] == "")
                print "DEPNEW", mp, x[3], x[4], x[5], x[6], x[7], x[8], x[9], b
            } else if (BD[mp, x[3], x[4], x[5]] != a) print "DEPCHG", mp, x[3], x[4], x[5], x[6], x[7], x[8], x[9], 0
        }
    }' "$META/base.fail" "$META/base.rs" "$META/head.rs" "$TMP/files" "$TMP/meta-base.rec" "$TMP/meta-head.rec" >"$TMP/meta-raw" || die "cargo metadata comparison failed"
    # the awk reader's lines for each manifest with a raw finding, for the line mapping
    : >"$TMP/meta-ent"
    cut -f2 "$TMP/meta-raw" | awk '!s[$0]++' | while IFS= read -r mp; do
        cargo_entries keys <"$META/head/$mp" | awk -v mp="$mp" '{ print mp "\t" $0 }' >>"$TMP/meta-ent"
    done
    awk -F'\t' -v OFS='\t' -v ent="$TMP/meta-ent" -v wsf="$TMP/ws-root" -v sensf="$TMP/sens" -v flist="$TMP/files" "$G2R_AWK"'
    function dec(s) { gsub(/%2E/, ".", s); gsub(/%7C/, "|", s); gsub(/%09/, "\t", s); gsub(/%0A/, "\n", s); gsub(/%0D/, "\r", s); gsub(/%25/, "%", s); return s }
    function dirn(p) { if (!sub(/\/[^\/]*$/, "", p)) p = ""; return p }
    function normp(p,   n, a, i, o, k, st) { n = split(p, a, "/"); k = 0
        for (i = 1; i <= n; i++) { if (a[i] == "" || a[i] == ".") continue; if (a[i] == "..") { if (k > 0) k--; else st[++k] = ".."; continue } st[++k] = a[i] }
        o = ""; for (i = 1; i <= k; i++) o = o (i > 1 ? "/" : "") st[i]; return o }
    function disp(t) { gsub(/\t/, " ", t); return (length(t) > 160) ? substr(t, 1, 157) "..." : t }
    function out(rule, mp, ln, msg, m) { print rule, mp, ln, disp(msg), "", "", m }
    function sens(p,   i, s, e) { for (i = 1; i <= ns; i++) if (p ~ sg[i]) s = 1; for (i = 1; i <= ne; i++) if (p ~ eg[i]) e = 1; return s && !e }
    function msec(kind, tgt) { return (tgt != "" ? "target." tgt "." : "") (kind == "dev" ? "dev-dependencies" : kind == "build" ? "build-dependencies" : "dependencies") }
    FILENAME == sensf { if ($1 == "S") sg[++ns] = g2r($2); else if ($1 == "E") eg[++ne] = g2r($2); next }
    FILENAME == wsf { WS[$1] = 1; next }
    FILENAME == flist { ST[$4] = $1; next }
    FILENAME == ent {
        mp = $1; k = $2; ln = $5; tx = $6
        if (k ~ /^(pkg|lib|bin|example)\|/) { split(k, q, "|"); kk = q[1] "|" q[2]; if (!((mp, kk) in KL)) { KL[mp, kk] = ln; KT[mp, kk] = tx } next }
        if (index(k, "target-path|") == 1) { v = normp((dirn(mp) == "" ? "" : dirn(mp) "/") dec(substr(k, 13))); if (!((mp, v) in PL)) { PL[mp, v] = ln; PT[mp, v] = tx } next }
        p = index(k, "|"); if (!p) next
        sec = substr(k, 1, p - 1); nm = dec(substr(k, p + 1))
        n = split(sec, sg2, "."); last = dec(sg2[n]); gsub(/_/, "-", last)
        if (n == 1 && last ~ /^(dev-|build-)?dependencies$/) ns2 = last
        else if (n == 3 && sg2[1] == "target" && last ~ /^(dev-|build-)?dependencies$/) ns2 = "target." dec(sg2[2]) "." last
        else next
        if (!((mp, ns2, nm) in DL)) { DL[mp, ns2, nm] = ln; DT[mp, ns2, nm] = tx; DS[mp, ns2, nm] = sec; DW[mp, ns2, nm] = $4 }
        next
    }
    $1 == "BUILD" {
        if (($2, "pkg|build") in KL) out("build-script", $2, KL[$2, "pkg|build"], KT[$2, "pkg|build"], KT[$2, "pkg|build"])
        else out("build-script", $2, 0, "build script " $3 " (Cargo runs it; no build key names it)", "build script " $3)
        next
    }
    $1 == "PROCM" {
        if (($2, "lib|proc-macro") in KL) out("proc-macro", $2, KL[$2, "lib|proc-macro"], KT[$2, "lib|proc-macro"], KT[$2, "lib|proc-macro"])
        else if (($2, "lib|crate-type") in KL) out("proc-macro", $2, KL[$2, "lib|crate-type"], KT[$2, "lib|crate-type"], KT[$2, "lib|crate-type"])
        else out("proc-macro", $2, 0, "the library is a proc-macro crate (" $3 ")", "proc-macro " $3)
        next
    }
    $1 == "CTYPE" {
        for (kk in KL) { split(kk, q, SUBSEP); if (q[1] == $2 && q[2] ~ /crate-type$/) { out("ffi", $2, KL[kk], "new crate type " $3 ": " KT[kk], KT[kk]); next } }
        out("ffi", $2, 0, "new crate type " $3 " on target " $4, "crate-type " $3); next
    }
    $1 == "LINKS" {
        if (($2, "pkg|links") in KL) out("build-script", $2, KL[$2, "pkg|links"], "links changed: " KT[$2, "pkg|links"], KT[$2, "pkg|links"])
        else out("build-script", $2, 0, "links = " $3, "links = " $3)
        next
    }
    $1 == "TPATH" {
        mp = $2; src = $3; mapped = ((mp, src) in PL)
        if (tolower(src) !~ /\.rs$/) { if (mapped) out("include", mp, PL[mp, src], "target path to a non-.rs file: " PT[mp, src], PT[mp, src]); else out("include", mp, 0, "target path to a non-.rs file: " src, src) }
        if (sens(mp)) {
            if (mapped) out("sensitive-crate", mp, PL[mp, src], "new or changed target path: " PT[mp, src], PT[mp, src])
            else if ($4 !~ /(^|,)(test|bench|example)(,|$)/ && ST[src] != "A") out("sensitive-crate", mp, 0, "new " $4 " target " $5 ": " src, src)
        }
        next
    }
    $1 == "DEPNEW" || $1 == "DEPCHG" {
        mp = $2; s2 = msec($3, $4); key = $5; mapped = ((mp, s2, key) in DL)
        if ($1 == "DEPNEW" && mapped && DW[mp, s2, key] == 1 && $10 == 1 && (("workspace.dependencies|" key) in WS)) next
        what = ($1 == "DEPNEW" ? "new entry in [" : "source changed in [")
        src = ($7 != "" ? $7 : ($8 != "" ? "path " $8 : "?")); if ($9 != "") src = src " registry " $9; if ($6 != key) src = src " package " $6
        if (mapped) out("new-dep", mp, DL[mp, s2, key], what DS[mp, s2, key] "]: " DT[mp, s2, key], DT[mp, s2, key])
        else out("new-dep", mp, 0, what s2 "]: " key " (as Cargo reads it: " src ")", key " = " src)
        next
    }' "$TMP/sens" "$TMP/ws-root" "$TMP/files" "$TMP/meta-ent" "$TMP/meta-raw" >>"$TMP/meta-findings" || die "cargo metadata mapping failed"
fi
cat "$TMP/meta-findings" >>"$TMP/findings"
# one finding per rule, path, line and matched text: pass 1, pass 3 and cargo often see the same one
awk -F'\t' '{ k = $1 FS $2 FS $3 FS (NF >= 7 ? $7 : $4) } !s[k]++' "$TMP/findings" >"$TMP/findings.d" && mv "$TMP/findings.d" "$TMP/findings" || die "dedupe failed"

# matchall(re, rule): does the regex approve every finding of that rule, the way "*" does?
# (RV-D1: ".*", "^.*$", ".+", "(.*)", "." ...). It is asked of a fixed, varied set of single
# characters; a pattern that matches every one of them matches any non-empty text, and no
# finding text is empty. unsafe/net/process/ffi findings are approved per token, by a match of
# at least one character, so for those an empty match ("^", "x*") does not count; other rules
# (include too: a Cargo target path finding is matched as text) take any match. awk regexes, as
# the classification below uses.
MATCHALL_AWK='
function matchall(re, rule,   n, i, pr, need) {
    need = (rule ~ /^(unsafe|net|process|ffi)$/)
    n = split("a x q Z 0 _ - # { } ; = ( ) [ ] : / . ~ <", pr, " ")
    pr[++n] = " "; pr[++n] = "\t"; pr[++n] = "\""; pr[++n] = "\\"
    for (i = 1; i <= n; i++) if (!match(pr[i], re) || (need && RLENGTH < 1)) return 0
    return 1
}
# broad(pat, lit, rule): R3-D1. For a rule whose findings rows match as text (every rule but
# the token-matched unsafe, net, process and ffi), a pattern shorter than 4 characters, or one
# that matches 8 or more of these 16 sample finding texts, approves nearly every finding of the
# rule in its path, so it is named unscoped like "*". A row written for one entry (`evil = `,
# `^serde = "1"`, `signal_pgid`) matches at most one or two of them.
function broad(pat, lit, rule,   n, i, c, sm) {
    if (rule ~ /^(unsafe|net|process|ffi)$/) return 0
    if (length(pat) < 4) return 1
    n = split("evil = { git = \"https://example.com/x\" }|serde = \"1.0\"|tokio = { version = \"1\", features = [\"full\"] }|build = \"build.rs\"|proc-macro = true|crate-type = [\"cdylib\"]|path = \"src/bin/tool.rs\"|cannot classify this Cargo.toml statement: [dependencies]|pub fn stop_all() { super::signal_pgid(-1, 9); }|let x = compute(42);|impl Drop for Guard { fn drop(&mut self) {} }|mod helpers;|#[path = \"x.rs\"]|links = \"z\"|version.workspace = true|}", sm, "|")
    c = 0
    for (i = 1; i <= n; i++) if (lit ? index(sm[i], pat) > 0 : match(sm[i], pat)) c++
    return c >= 8
}'

# ── Approvals ────────────────────────────────────────────────────────
# Read from the head commit (the working tree only in --worktree mode), the same source the
# approvals-change check below compares, so a checkout that differs from --head cannot
# approve anything.
# Emits: rule <TAB> glob <TAB> pattern <TAB> literal(0|1) <TAB> reason <TAB> approved_by <TAB> blob <TAB> line
#        <TAB> matches-everything(0|1): "*", or a regex matchall() says approves every finding (RV-D1)
head_src="$HEAD_SHA"; [ "$WORKTREE" = 1 ] && head_src=WORKTREE
show_at "$BASE" "$APPROVALS" >"$TMP/appr.base.toml"
show_at "$head_src" "$APPROVALS" >"$TMP/appr.head.toml"
: >"$TMP/approvals"; : >"$TMP/appr.trusted"
if [ -s "$TMP/appr.head.toml" ]; then
    parse_approvals <"$TMP/appr.head.toml" >"$TMP/approvals.raw" || { echo "diff-gate: cannot use $APPROVALS" >&2; exit 2; }
    # head rows that are trusted: their hash is a row of the base tip's approvals or in the tip ledger
    show_at "$BASE_TIP" "$APPROVALS" >"$TMP/appr.tip.toml"
    { parse_approvals <"$TMP/appr.tip.toml" 2>/dev/null || true; } | hash_rows | cut -f1 >"$TMP/appr.tip.h" || exit 2
    show_at "$BASE_TIP" "$LEDGER" | parse_ledger 2>/dev/null | cut -f1 >>"$TMP/appr.tip.h"
    hash_rows <"$TMP/approvals.raw" | awk -F'\t' 'FILENAME == ARGV[1] { t[$1] = 1; next } ($1 in t) { print $2 }' "$TMP/appr.tip.h" - >"$TMP/appr.trusted" || exit 2
    while IFS="$(printf '\t')" read -r r g pt re ab ln b; do
        lit=0; all=0
        if [ "$pt" = "*" ]; then all=1
        else
            printf '' | grep -Eq -e "$pt" 2>/dev/null; [ $? -eq 2 ] && lit=1
            # in its own awk: a pattern grep accepts may not compile in awk, which is fatal there
            [ "$lit" = 0 ] && DG_RE="$pt" DG_RULE="$r" awk "$MATCHALL_AWK"' BEGIN { exit !matchall(ENVIRON["DG_RE"], ENVIRON["DG_RULE"]) }' 2>/dev/null && all=1
            [ "$all" = 0 ] && DG_RE="$pt" DG_RULE="$r" DG_LIT="$lit" awk "$MATCHALL_AWK"' BEGIN { exit !broad(ENVIRON["DG_RE"], ENVIRON["DG_LIT"] + 0, ENVIRON["DG_RULE"]) }' 2>/dev/null && all=2
        fi
        # Rows read from the head approve only when the same row (same canonical hash) is on the
        # trusted base tip, or the owner ledger at the tip lists it, or the owner flag is set: a
        # row the owner removed on the base tip stops approving older branches at once, as a
        # revoked ledger entry does (round 3 limit).
        if [ "${DIFF_GATE_OWNER_APPROVED:-0}" != 1 ] && ! grep -qxF -- "$ln" "$TMP/appr.trusted"; then
            echo "diff-gate: approval row at $APPROVALS line $ln is not on the trusted base tip and has no owner ledger entry: it approves nothing" >&2
            continue
        fi
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$r" "$g" "$pt" "$lit" "$re" "$ab" "$b" "$ln" "$all"
    done <"$TMP/approvals.raw" >"$TMP/approvals"
fi

# Unscoped rows. A path-rule finding (ci-workflow, build-script for a new build.rs,
# toolchain-config, special-file, gitattributes-change) carries fixed text, so a row for it
# without `blob` approves every future change to the matching files, whatever its pattern. A
# "*" row without `blob` for any other rule approves every finding of that rule in the path.
# Both still work; each is named on every run so it is not forgotten.
awk -F'\t' '
BEGIN { n = split("file changed|file deleted or moved away|file deleted|symlink added or changed|submodule or nested repository added or changed|new build.rs", fx, "|") }
$7 != "" { next }
{
    pathrule = ($1 ~ /^(ci-workflow|build-script|toolchain-config|special-file|gitattributes-change)$/)
    hit = ($9 == 1 || $9 == 2)
    if (pathrule) for (i = 1; i <= n && !hit; i++) hit = (index(fx[i], $3) > 0 || ($4 == 0 && fx[i] ~ $3))
    if (!hit) next
    if (pathrule) printf "diff-gate: WARNING: unscoped approval row (approvals line %s): rule=%s path=%s has no blob, so it approves EVERY future change to that path\n", $8, $1, $2
    else if ($3 == "*") printf "diff-gate: WARNING: unscoped approval row (approvals line %s): rule=%s path=%s pattern \"*\" has no blob, so it approves every %s finding there\n", $8, $1, $2, $1
    else if ($9 == 1) printf "diff-gate: WARNING: unscoped approval row (approvals line %s): rule=%s path=%s pattern \"%s\" matches any text and has no blob, so it approves every %s finding there, like \"*\"\n", $8, $1, $2, $3, $1
    else printf "diff-gate: WARNING: unscoped approval row (approvals line %s): rule=%s path=%s pattern \"%s\" is broad (shorter than 4 characters, or it matches most sample finding texts) and has no blob, so it approves nearly every %s finding there\n", $8, $1, $2, $3, $1
}' "$TMP/approvals" >"$TMP/unscoped" || die "approvals scan failed"
[ -s "$TMP/unscoped" ] && cat "$TMP/unscoped" >&2

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
if ! cmp -s "$TMP/sens.base.toml" "$TMP/sens.head.toml"; then
    printf 'approvals-change\t%s\t0\tsensitive-crate list changed\n' "$SENSITIVE" >>"$TMP/findings"
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
awk -F'\t' -v owner="${DIFF_GATE_OWNER_APPROVED:-0}" -v apf="$TMP/approvals" -v flist="$TMP/files" "$G2R_AWK"'
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
function bok(i, path) { return (abl[i] == "" || ((path in fb) && fb[path] == abl[i])) } # blob scope
function why(i) { return ab[i] ": " aw[i] (abl[i] != "" ? " (blob " substr(abl[i], 1, 12) ")" : (us[i] ? " [UNSCOPED row: no blob]" : "")) }
FILENAME == apf {
    na++; ar[na] = $1; ag[na] = g2r($2); ap[na] = $3; al[na] = $4; aw[na] = $5; ab[na] = $6; abl[na] = $7
    us[na] = ($7 == "" && ($9 == 1 || $9 == 2 || $1 ~ /^(ci-workflow|build-script|toolchain-config|special-file|gitattributes-change)$/)); next
}
FILENAME == flist { split($0, f, "\t"); fb[f[4]] = tolower(f[5]); next }
{
    status = "FAIL"; detail = ""
    if ($1 == "approvals-change" || $1 == "rust-tokens") {
        if (owner == "1") { status = "approved"; detail = "DIFF_GATE_OWNER_APPROVED=1" }
        else if ($5 != "") { status = "approved"; detail = "owner ledger (base tip): " $5 }
        else detail = "owner only: set DIFF_GATE_OWNER_APPROVED=1 (CI: diff-gate-approved label)"
    } else if ($6 != "") {
        ng = split($6, groups, "|"); open = 0; first = 0; col = 0
        for (gi = 1; gi <= ng; gi++) {
            nt = split(groups[gi], toks, ";"); hit = 0
            for (i = 1; i <= na && !hit; i++) {
                if (ar[i] != $1 || $2 !~ ag[i] || !bok(i, $2)) continue
                for (ti = 1; ti <= nt && !hit; ti++) { split(toks[ti], se, "-"); if (covers(i, $7, se[1] + 0, se[2] + 0)) hit = i }
            }
            if (hit) { if (!first) first = hit } else { open++; if (!col) { split(toks[1], se, "-"); col = se[1] } }
        }
        if (!open) { status = "approved"; detail = why(first) }
        else if (first) detail = open " of " ng " occurrences on the line not approved (first at column " col ")"
    } else {
        # Field 7, when present, is the text rows match: for Cargo.toml findings the entry line
        # itself (comment stripped), never the "new entry in [...]:" wording the gate adds.
        mt = ($7 != "" ? $7 : $4)
        for (i = 1; i <= na; i++) {
            if (ar[i] != $1 || $2 !~ ag[i] || !bok(i, $2)) continue
            if (ap[i] == "*" || index(mt, ap[i]) > 0 || (al[i] == 0 && mt ~ ap[i])) {
                status = "approved"; detail = why(i); break
            }
        }
    }
    if (status == "FAIL" && $1 == "sensitive-crate") detail = "sensitive crate: needs owner review; approve the reviewed file version with a blob row"
    print status "\t" $1 "\t" $2 "\t" $3 "\t" $4 "\t" detail
}
' "$TMP/approvals" "$TMP/files" "$TMP/findings" >"$TMP/classified.u" || die "classification failed"
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
        if [ -s "$TMP/unscoped" ]; then echo; sed 's/^diff-gate: /- /' "$TMP/unscoped"; fi
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
