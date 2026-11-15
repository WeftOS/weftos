#!/usr/bin/env bash
# WO-069 trigger (prototype, branch w2/wo069-a-trigger): register / sweep / install-hook.
# SOURCED by worktree-store.sh (not run directly): it uses that script's warn, die, slug_of,
# check_slug, project_id and $WTS_SELF. Publishing goes through `worktree-store.sh share`
# run inside the registered worktree, so there is one publish code path.
#
# State (local, per machine), consistent with the follower state next to it:
#   ${WTS_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/weftos/worktree-store}/reg/<project>/<slug>
# one key=value file per registered worktree: path, project, slug, registered_at, digest, head,
# published_at, missing_since. Lock dirs live in <state>/lock/. Values never contain newlines.
#
# Tree digest (what "unchanged" means): SHA-256 over `head <HEAD sha>` plus, for each name in the
# sorted, de-duplicated `git ls-files -co --exclude-standard -z` list: a regular file under 1 MiB
# contributes its name, size and content SHA-256; a larger file contributes name, size and mtime
# (a content hash of a big file would make an idle sweep read it all); a listed-but-missing file
# (deleted, not yet staged) contributes a "deleted" record; a symlink its name and target.
# Not in the digest: the merge base (a target-branch move alone does not republish) and whether a
# file is on the secret denylist (editing a denylisted file republishes identical content: harmless).
# Needs perl with Digest::SHA (core); present on macOS and Linux.

WTS_HOOK_MARK='# wts-managed post-commit hook (worktree-store.sh install-hook)'

wts_root() { printf '%s' "${WTS_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/weftos/worktree-store}"; }
wts_now() { printf '%s' "${WTS_NOW:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"; }
jesc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }
shq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
rget() { [ -f "$1" ] || return 0; sed -n "s/^$2=//p" "$1" | head -n1; }
rset() { # file key value (atomic rewrite)
    local f=$1 k=$2 v=$3 tmp; tmp=$(mktemp "$f.XXXXXX")
    { grep -v "^$k=" "$f" 2>/dev/null || true; printf '%s=%s\n' "$k" "$v"; } >"$tmp" && mv "$tmp" "$f"
}
reg_files() { local f; for f in "$(wts_root)"/reg/*/*; do [ -f "$f" ] && printf '%s\n' "$f"; done; return 0; }

tree_digest() { # <worktree dir>
    local top=$1 head
    head=$(git -C "$top" rev-parse --verify -q HEAD || echo none)
    git -C "$top" ls-files -co --exclude-standard -z | (cd "$top" && perl -e '
        use strict; use Digest::SHA;
        my $head = shift; local $/ = "\0"; my @n = <STDIN>; chomp @n;
        my %s; @n = sort grep { !$s{$_}++ } @n;
        my $d = Digest::SHA->new(256); $d->add("head $head\n");
        for my $f (@n) {
            my @st = lstat($f);
            if (!@st) { $d->add("D\0$f\n"); next }
            if (-l _) { $d->add("L\0$f\0" . (readlink($f) // "") . "\n"); next }
            next unless -f _;
            my $sz = $st[7];
            if ($sz < 1048576) {
                open(my $h, "<:raw", $f) or do { $d->add("U\0$f\n"); next };
                my $c = Digest::SHA->new(256); $c->addfile($h); close $h;
                $d->add("F\0$f\0$sz\0" . $c->hexdigest . "\n");
            } else { $d->add("B\0$f\0$sz\0$st[9]\n"); }
        }
        print $d->hexdigest, "\n";' "$head")
}

wts_register() {
    local path="" slug="" top proj f oldpath follow=${WTS_FOLLOW_BRANCH:-0}
    while [ $# -gt 0 ]; do
        case "$1" in
            --path) path=${2:?--path needs a directory}; shift 2 ;;
            --slug) slug=${2:?--slug needs a value}; shift 2 ;;
            --follow-branch) follow=1; shift ;;
            *) die "register: unknown argument $1" ;;
        esac
    done
    [ -n "$path" ] || path=$PWD
    [[ $path != *$'\n'* ]] || die "register: path has a newline"
    top=$(git -C "$path" rev-parse --show-toplevel 2>/dev/null) || die "register: '$path' is not inside a git worktree"
    top=$(cd "$top" && pwd -P)
    [ -n "$slug" ] || slug=$(cd "$top" && slug_of)
    check_slug "$slug"
    proj=$(project_id); [[ $proj =~ ^[A-Za-z0-9]{1,40}$ ]] || die "unexpected project id '$proj'"
    f=$(wts_root)/reg/$proj/$slug
    mkdir -p "$(dirname "$f")"
    oldpath=$(rget "$f" path)
    if [ -f "$f" ] && [ "$oldpath" = "$top" ]; then
        [ "$follow" != 1 ] || [ "$(rget "$f" follow_branch)" = 1 ] || rset "$f" follow_branch 1
        warn "already registered: $slug"; return 0
    fi
    # A new path under an old slug starts over: the old digest describes another tree.
    : >"$f"
    rset "$f" slug "$slug"; rset "$f" project "$proj"; rset "$f" path "$top"
    rset "$f" registered_at "$(wts_now)"
    rset "$f" digest ""; rset "$f" head ""; rset "$f" published_at ""; rset "$f" missing_since ""
    [ "$follow" != 1 ] || rset "$f" follow_branch 1
    rset "$f" branch "$(git -C "$top" symbolic-ref --quiet --short HEAD || true)"   # for the drift note in sweep
    printf 'registered %s %s\n' "$slug" "$top"
}

wts_unregister() {
    local slug=${1:?usage: unregister <slug>} f n=0
    check_slug "$slug"
    while IFS= read -r f; do
        [ "$(rget "$f" slug)" = "$slug" ] || continue
        [ -z "${WTS_PROJECT:-}" ] || [ "$(rget "$f" project)" = "$WTS_PROJECT" ] || continue
        rm -f "$f"; n=$((n+1))
    done < <(reg_files)
    [ "$n" -gt 0 ] || { warn "unregister: '$slug' is not registered"; return 0; }
    printf 'unregistered %s\n' "$slug"
}

wts_registered() {
    local json=0 f first=1
    case "${1:-}" in "") ;; --json) json=1 ;; *) die "usage: registered [--json]" ;; esac
    [ "$json" = 1 ] && printf '['
    while IFS= read -r f; do
        if [ "$json" = 1 ]; then
            [ $first = 1 ] || printf ','; first=0
            printf '{"slug":"%s","project":"%s","path":"%s","registered_at":"%s","head":"%s","published_at":"%s","missing_since":"%s"}' \
                "$(jesc "$(rget "$f" slug)")" "$(jesc "$(rget "$f" project)")" "$(jesc "$(rget "$f" path)")" \
                "$(rget "$f" registered_at)" "$(rget "$f" head)" "$(rget "$f" published_at)" "$(rget "$f" missing_since)"
        else
            printf '%s\t%s\t%s\n' "$(rget "$f" slug)" "$(rget "$f" project)" "$(rget "$f" path)"
        fi
    done < <(reg_files)
    [ "$json" = 1 ] && printf ']\n'
    return 0
}

# slug-for-path <dir>: the registered slug whose path is <dir> (used by the shared hook), or nothing.
wts_slug_for_path() {
    local want=${1:?usage: slug-for-path <dir>} f
    want=$(cd "$want" 2>/dev/null && pwd -P) || return 0
    while IFS= read -r f; do
        [ "$(rget "$f" path)" = "$want" ] && { rget "$f" slug; return 0; }
    done < <(reg_files)
    return 0
}

# Branch-change option (design note Q9). Default (today): the slug is fixed at registration, so a worktree that
# later checks out another branch keeps publishing, under the OLD slug, the NEW branch's tree. With
# `register --follow-branch` (or WTS_FOLLOW_BRANCH=1 at register time) the registration is identified by its
# path: each sweep re-derives the slug from the checked-out branch, creates that store when the slug moved
# (create is idempotent), publishes there and records prev_slug. The old slug's store is left as it was.
follow_slug() { # <reg file> <path>: print the slug to publish under; return 1 with a reason on stdout
    local f=$1 path=$2 want g
    want=$(cd "$path" && unset WTS_SLUG AGF_HOOK_BRANCH AGF_HOOK_SLUG && slug_of 2>/dev/null) || want=""
    if [ -z "$want" ] || ! (check_slug "$want") 2>/dev/null; then printf 'cannot derive a store slug from the checked-out branch (detached HEAD or unusable name)'; return 1; fi
    while IFS= read -r g; do
        [ "$g" != "$f" ] && [ "$(rget "$g" project)" = "$(rget "$f" project)" ] && [ "$(rget "$g" slug)" = "$want" ] \
            && { printf 'slug %s is already registered for %s' "$want" "$(rget "$g" path)"; return 1; }
    done < <(reg_files)
    printf '%s' "$want"
}

# One registered worktree: sets $status and $detail. No weaver call unless the digest moved.
sweep_one() {
    local f=$1 slug proj path digest head lock="" rc want moved="" tref base drift="" cur regb
    slug=$(rget "$f" slug); proj=$(rget "$f" project); path=$(rget "$f" path)
    dry=${WTS_DRY_RUN:-0}; detail=""
    if [ ! -d "$path" ] || [ ! -e "$path/.git" ]; then
        # Orphan: mark only (WO-069 §7). Archiving or deleting the store is a gc policy, not this lane's.
        if [ -z "$(rget "$f" missing_since)" ] && [ "$dry" != 1 ]; then rset "$f" missing_since "$(wts_now)"; fi
        status=orphan; detail="last_head=$(rget "$f" head) published_at=$(rget "$f" published_at) missing_since=$(rget "$f" missing_since)"
        return 0
    fi
    [ -z "$(rget "$f" missing_since)" ] || { [ "$dry" = 1 ] || rset "$f" missing_since ""; }
    lock=$(wts_root)/lock/$proj-$(basename "$f")   # keyed by the registration, which is stable when the slug follows the branch
    mkdir -p "$(dirname "$lock")"
    if ! mkdir "$lock" 2>/dev/null; then
        if kill -0 "$(cat "$lock/pid" 2>/dev/null)" 2>/dev/null; then status=busy; return 0; fi
        rm -rf "$lock"; mkdir "$lock" 2>/dev/null || { status=busy; return 0; }
    fi
    echo $$ >"$lock/pid"
    if [ "$(rget "$f" follow_branch)" = 1 ]; then
        if ! want=$(follow_slug "$f" "$path"); then status=failed; detail=$want; rm -rf "$lock"; return 0; fi
        [ "$want" = "$slug" ] || moved=$slug
        slug=$want
    fi
    cur=$(git -C "$path" symbolic-ref --quiet --short HEAD 2>/dev/null || true); regb=$(rget "$f" branch)
    # Default mode: the slug stays what it was at registration. Say so when the branch has moved on (the sweep then
    # publishes the new branch's tree into the old slug's store); `register --follow-branch` is the alternative.
    if [ "$(rget "$f" follow_branch)" != 1 ] && [ -n "$regb" ] && [ "$cur" != "$regb" ]; then
        drift="branch-drift=$regb->${cur:-detached} (still publishing under slug $slug; see register --follow-branch) "
        warn "$slug: worktree is now on '${cur:-a detached HEAD}', registered on '$regb'; publishing under the old slug"
    fi
    digest=$(tree_digest "$path") || digest=""
    head=$(git -C "$path" rev-parse --verify -q HEAD || echo none)
    if [ -z "$digest" ]; then status=failed; detail="cannot compute the tree digest (perl Digest::SHA?)"
    elif [ -z "$moved" ] && [ "$digest" = "$(rget "$f" digest)" ]; then status=unchanged; detail="head=$head"
    else
        rc=0
        if [ -n "$moved" ] && [ "$dry" != 1 ]; then
            tref=$(cd "$path" && target_ref) && base=$(git -C "$path" merge-base "$tref" HEAD) || base=$head
            store_must create --project "$proj" --slug "$slug" --branch "$(git -C "$path" symbolic-ref --short HEAD)" --base "$base" >&2 || rc=$?
        fi
        [ $rc != 0 ] || (cd "$path" && WTS_SLUG=$slug WTS_PROJECT=$proj "$WTS_SELF" share) >&2 || rc=$?
        if [ $rc != 0 ]; then status=failed; detail="share exited $rc; will retry next sweep"
        elif [ "$dry" = 1 ]; then status=would-publish; detail="head=$head"
        else
            # The digest taken BEFORE publishing: an edit made during the publish shows on the next sweep.
            rset "$f" digest "$digest"; rset "$f" head "$head"; rset "$f" published_at "$(wts_now)"
            [ -z "$moved" ] || { rset "$f" slug "$slug"; rset "$f" prev_slug "$moved"; rset "$f" branch "$cur"; detail="slug-moved=$moved->$slug "; }
            status=published; detail="${detail}head=$head"
        fi
    fi
    detail="$drift$detail"
    rm -rf "$lock"
}

wts_sweep() {
    local only="" json=0 f slug nfail=0 first=1 status detail dry
    while [ $# -gt 0 ]; do
        case "$1" in
            --only) only=${2:?--only needs a slug}; shift 2 ;;
            --json) json=1; shift ;;
            *) die "sweep: unknown argument $1" ;;
        esac
    done
    [ "$json" = 1 ] && printf '['
    while IFS= read -r f; do
        slug=$(rget "$f" slug)
        [ -z "$only" ] || [ "$slug" = "$only" ] || continue
        status=""; detail=""
        sweep_one "$f" || { status=failed; detail="sweep error"; }
        slug=$(rget "$f" slug)   # a follow-branch registration may have moved to a new slug
        [ "$status" = failed ] && nfail=$((nfail+1))
        if [ "$json" = 1 ]; then
            [ $first = 1 ] || printf ','; first=0
            printf '{"slug":"%s","project":"%s","path":"%s","status":"%s","head":"%s","published_at":"%s","missing_since":"%s"}' \
                "$(jesc "$slug")" "$(jesc "$(rget "$f" project)")" "$(jesc "$(rget "$f" path)")" "$status" \
                "$(rget "$f" head)" "$(rget "$f" published_at)" "$(rget "$f" missing_since)"
        else
            printf '%s\t%s\t%s\n' "$slug" "$status" "$detail"
        fi
    done < <(reg_files)
    [ "$json" = 1 ] && printf ']\n'
    # Same contract as the rest of the script: failures warn and exit 0 unless WTS_STRICT=1.
    if [ "$nfail" -gt 0 ]; then warn "$nfail worktree(s) failed to publish"; [ "$strict" = 1 ] && return 1; fi
    return 0
}

# ---- hook accelerator -------------------------------------------------------------------------
# Worktrees share the main repo's hooks dir. With extensions.worktreeConfig a per-worktree
# core.hooksPath is possible: it points at <gitdir>/wts-hooks, which symlinks every other hook of
# the dir it replaces and holds our post-commit. Without it, ONE shared post-commit is installed
# that looks the current worktree up in the registration state and does nothing for unregistered
# worktrees (so installing it from any one worktree enables it for every registered one).
# An existing foreign post-commit is never overwritten: shared mode refuses unless --chain (the
# foreign hook moves to post-commit.wts-orig, runs first, and is restored on uninstall).

write_hook() { # <file> <orig-or-empty>
    {
        printf '#!/bin/sh\n%s\n' "$WTS_HOOK_MARK"
        printf 'export WTS_STATE_DIR=%s\n' "$(shq "$(wts_root)")"
        [ -z "${WTS_WEAVER:-}" ] || printf 'export WTS_WEAVER=%s\n' "$(shq "$WTS_WEAVER")"
        printf 'orig=%s\nscript=%s\nrc=0\n' "$(shq "$2")" "$(shq "$WTS_SELF")"
        cat <<'HOOK'
if [ -n "$orig" ] && [ -x "$orig" ]; then "$orig" "$@" || rc=$?; fi
top=$(git rev-parse --show-toplevel 2>/dev/null) || exit $rc
[ -x "$script" ] || exit $rc
slug=$("$script" slug-for-path "$top" 2>/dev/null)
[ -n "$slug" ] || exit $rc
if [ "${WTS_HOOK_SYNC:-0}" = 1 ]; then "$script" sweep --only "$slug" >/dev/null 2>&1
else ( nohup "$script" sweep --only "$slug" >/dev/null 2>&1 & ); fi
exit $rc
HOOK
    } >"$1"
    chmod +x "$1"
}
is_ours() { [ -f "$1" ] && grep -qF "$WTS_HOOK_MARK" "$1"; }

wts_install_hook() {
    local slug="" chain=0 top gitdir hdir base cfg cur orig h n
    while [ $# -gt 0 ]; do
        case "$1" in
            --slug) slug=${2:?--slug needs a value}; shift 2 ;;
            --chain) chain=1; shift ;;
            *) die "install-hook: unknown argument $1" ;;
        esac
    done
    top=$(git rev-parse --show-toplevel) || die "install-hook: not in a git worktree"
    [ -n "$slug" ] || slug=$(wts_slug_for_path "$top")
    [ -n "$slug" ] && [ -n "$(ls "$(wts_root)"/reg/*/"$slug" 2>/dev/null)" ] || die "install-hook: this worktree is not registered; run 'register' first"
    cfg=$(git config --get extensions.worktreeConfig || true)
    base=$(cd "$top" && cd "$(git rev-parse --git-path hooks)" 2>/dev/null && pwd -P) || base=""
    if [ "$cfg" = true ]; then
        gitdir=$(git rev-parse --absolute-git-dir); hdir=$gitdir/wts-hooks
        cur=$(git config --worktree --get core.hooksPath || true)
        if [ -n "$cur" ] && [ "$cur" != "$hdir" ]; then die "install-hook: this worktree already sets core.hooksPath=$cur; not touching it"; fi
        mkdir -p "$hdir"
        if [ -n "$base" ] && [ "$base" != "$hdir" ]; then
            for h in "$base"/*; do
                [ -e "$h" ] || continue; n=$(basename "$h")
                case "$n" in *.sample|post-commit) continue ;; esac
                ln -sf "$h" "$hdir/$n"
            done
        fi
        orig=""; [ -n "$base" ] && [ -x "$base/post-commit" ] && ! is_ours "$base/post-commit" && orig="$base/post-commit"
        write_hook "$hdir/post-commit" "$orig"
        git config --worktree core.hooksPath "$hdir"
        printf 'installed per-worktree hook: core.hooksPath=%s (slug %s)%s\n' "$hdir" "$slug" "${orig:+; chains $orig}"
    else
        [ -n "$base" ] || die "install-hook: cannot resolve the hooks directory"
        if [ -e "$base/post-commit" ] && ! is_ours "$base/post-commit"; then
            [ "$chain" = 1 ] || die "install-hook: $base/post-commit exists and is not ours; not overwriting it. Re-run with --chain to keep it (it moves to post-commit.wts-orig and still runs)."
            [ ! -e "$base/post-commit.wts-orig" ] || die "install-hook: $base/post-commit.wts-orig already exists"
            mv "$base/post-commit" "$base/post-commit.wts-orig"
        fi
        orig=""; [ -x "$base/post-commit.wts-orig" ] && orig="$base/post-commit.wts-orig"
        write_hook "$base/post-commit" "$orig"
        printf 'installed SHARED hook %s: every registered worktree of this repo now sweeps after commit; unregistered ones are ignored%s\n' "$base/post-commit" "${orig:+; chains $orig}"
    fi
}

wts_uninstall_hook() {
    local gitdir hdir base cur
    gitdir=$(git rev-parse --absolute-git-dir) || die "uninstall-hook: not in a git worktree"
    hdir=$gitdir/wts-hooks
    if [ -d "$hdir" ]; then
        cur=$(git config --worktree --get core.hooksPath 2>/dev/null || true)
        [ "$cur" != "$hdir" ] || git config --worktree --unset core.hooksPath
        rm -rf "$hdir"; printf 'removed per-worktree hook %s\n' "$hdir"; return 0
    fi
    base=$(cd "$(git rev-parse --git-path hooks)" 2>/dev/null && pwd -P) || die "uninstall-hook: cannot resolve the hooks directory"
    if is_ours "$base/post-commit"; then
        rm -f "$base/post-commit"
        [ ! -e "$base/post-commit.wts-orig" ] || mv "$base/post-commit.wts-orig" "$base/post-commit"
        printf 'removed SHARED hook %s (it covered every registered worktree of this repo)\n' "$base/post-commit"
    else
        warn "uninstall-hook: no hook of ours in $base"
    fi
}
