#!/usr/bin/env bash
# ADR-126 S2 hooks: worktree stores around agent-git-flow, plus the desktop union mount.
#
#   worktree-store.sh new <name> [wt.sh args]   wt.sh new, then create the worktree store
#   worktree-store.sh store-create [--branch B] [--base C]
#                                               store half of `new`, for the agent-git-flow post-new hook
#   worktree-store.sh store-promote             store half of `land`, for the agent-git-flow post-land hook
#   worktree-store.sh share [--handoff NOTE]    publish this worktree's tree + unmerged commits
#   worktree-store.sh land [land.sh args]       land.sh, then promote the store (only if it landed)
#   worktree-store.sh fetch <slug> <dest> [--bundle-ref REF]
#                                               pull a shared tree (and its commits) from another node
#   worktree-store.sh claim <slug>              take the merge of a handed-off worktree
#   worktree-store.sh follow [--state FILE]     what changed among unmerged worktrees since last look
#   worktree-store.sh search <query>            docs/planning across all unmerged worktrees
#   worktree-store.sh leftovers [--json]        forgotten / unclaimed / abandoned worktrees
#   worktree-store.sh sync-docs <slug> <cache>  read-only local mirror of a worktree's docs
#   worktree-store.sh overlaps <slugA> <slugB>  files both changed differently (merge-conflict hint)
#   worktree-store.sh mount-config <slug> [--local ROOT]   print the rclone union remote (worktree over project)
#   worktree-store.sh mount <slug> <dir> [--local ROOT]    mount it (rclone, FUSE); umount <dir> unmounts
#
# All store operations go through `weaver store worktree <verb>` (S1 owns the `weaver store` surface;
# the verbs are create|publish|promote|fetch|claim|done|list). Git work stays here, not in Rust.
# Store failures never block git flow: they warn and exit 0 unless WTS_STRICT=1.
# Hook use (.agent-git.env): store-create / store-promote read AGF_HOOK_SLUG, AGF_HOOK_BRANCH and
# AGF_HOOK_BASE (AGF_HOOK_COMMIT is accepted but `promote` takes no commit), so wt.sh and land.sh
# are not wrapped:
#   AGF_HOOK_POST_NEW='scripts/worktree-store.sh store-create'
#   AGF_HOOK_POST_LAND='scripts/worktree-store.sh store-promote'
#
# Env: WTS_WEAVER (default: weaver), WTS_AGF (default: ~/.claude/skills/agent-git-flow/scripts),
#      WTS_DRY_RUN=1 print the weaver calls instead of running them, WTS_SLUG (default: branch
#      with '/' -> '--', colliding branches refused; AGF_HOOK_SLUG only if no branch),
#      WTS_PROJECT (project ULID; default: from `weaver project id`).
set -euo pipefail

weaver=${WTS_WEAVER:-weaver}
agf=${WTS_AGF:-$HOME/.claude/skills/agent-git-flow/scripts}
strict=${WTS_STRICT:-0}

warn() { printf 'worktree-store: %s\n' "$*" >&2; }
die() { warn "$*"; exit 1; }

# Slug = branch with '/' encoded as '--'. Injective: a branch containing '--', '-/' or '/-' is refused
# (feat/x -> feat--x, so feat--x or a-/b vs a/-b would collide). WTS_SLUG overrides; AGF_HOOK_SLUG is
# only trusted when no branch is known (the hook's own slug is the lossy '/' -> '-').
slug_of() {
    local b
    if [ -n "${WTS_SLUG:-}" ]; then printf '%s' "$WTS_SLUG"; return; fi
    b=${AGF_HOOK_BRANCH:-}
    if [ -z "$b" ] && [ -n "${AGF_HOOK_SLUG:-}" ]; then printf '%s' "$AGF_HOOK_SLUG"; return; fi
    [ -n "$b" ] || b=$(git symbolic-ref --quiet --short HEAD)
    case "$b" in
        *--*|*-/*|*/-*) die "branch '$b' has '--', '-/' or '/-', which would collide with another branch's store slug; set WTS_SLUG to choose the slug" ;;
    esac
    printf '%s' "${b//\//--}"
}

# Store halves of `new` and `land`: one code path for the verbs and for the agent-git-flow hooks.
# store_create [--branch B] [--base C]: run inside the lane (cwd).
store_create() {
    local branch=${AGF_HOOK_BRANCH:-} base=${AGF_HOOK_BASE:-} slug
    while [ $# -gt 0 ]; do
        case "$1" in
            --branch) branch=${2:?--branch needs a value}; shift 2 ;;
            --base) base=${2:?--base needs a value}; shift 2 ;;
            *) die "store-create: unknown argument $1" ;;
        esac
    done
    [ -n "$branch" ] || branch=$(git symbolic-ref --short HEAD)
    [ -n "$base" ] || base=$(git rev-parse HEAD)
    slug=$(slug_of); check_slug "$slug"
    store create --project "$(project_id)" --slug "$slug" --branch "$branch" --base "$base"
}
store_promote() {
    local slug; slug=$(slug_of); check_slug "$slug"
    store promote --project "$(project_id)" --slug "$slug"
}

# Slugs become part of bucket names and tags: lowercase [a-z0-9-], 1..48 chars.
check_slug() { [[ $1 =~ ^[a-z0-9][a-z0-9-]{0,47}$ ]] || die "bad slug '$1' (want [a-z0-9-], max 48)"; }

# store <verb> ...: best effort (new, land: git flow must not be blocked by the store).
# store_must <verb> ...: the caller depends on the result (share, fetch, claim): any failure,
# including a lost claim or a missing weaver, is a non-zero exit.
store_run() {
    local must=$1; shift
    if [ "${WTS_DRY_RUN:-0}" = 1 ]; then printf 'DRY: %s store worktree %s\n' "$weaver" "$*"; return 0; fi
    if ! command -v "$weaver" >/dev/null 2>&1; then
        warn "'$weaver' not found; store step '$1' not done"; [ "$must" = 1 ] || [ "$strict" = 1 ] && return 1; return 0
    fi
    if ! "$weaver" store worktree "$@"; then
        warn "store step failed: $1"; [ "$must" = 1 ] || [ "$strict" = 1 ] && return 1; return 0
    fi
}
store() { store_run 0 "$@"; }
store_must() { store_run 1 "$@"; }

project_id() {
    if [ -n "${WTS_PROJECT:-}" ]; then printf '%s' "$WTS_PROJECT"; return; fi
    [ "${WTS_DRY_RUN:-0}" = 1 ] && { printf 'DRYPROJECT'; return; }
    "$weaver" project id 2>/dev/null || die "cannot resolve project id; set WTS_PROJECT"
}

target_ref() { git rev-parse --verify --quiet "${AGF_TARGET_REF:-origin/HEAD}" 2>/dev/null || git rev-parse --verify --quiet master; }

cmd=${1:-}; shift || true
case "$cmd" in
new)
    name=${1:?usage: new <name>}; shift
    check_slug "$name"
    "$agf/wt.sh" new "$name" "$@"
    root=""
    while IFS= read -r line; do
        path=${line%%[[:space:]]*}
        [ "${path##*/}" = "$name" ] && { root=$path; break; }
    done < <("$agf/wt.sh" list 2>/dev/null || true)
    [ -n "$root" ] || { warn "cannot locate the new lane '$name'; store not created"; exit 0; }
    # Slug comes from the branch (same rule share/land use), not from the lane name.
    (cd "$root" && store_create)
    ;;
store-create)
    store_create "$@"
    ;;
store-promote)
    store_promote
    ;;
share)
    note=""
    [ "${1:-}" = "--handoff" ] && { note=${2:?--handoff needs a note}; }
    slug=$(slug_of); check_slug "$slug"
    top=$(git rev-parse --show-toplevel)
    tref=$(target_ref) || die "no target ref to compute the base"
    base=$(git merge-base "$tref" HEAD)
    head=$(git rev-parse HEAD)
    tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
    # NUL-separated: tracked + untracked-not-ignored. This is what makes planning docs visible.
    git -C "$top" ls-files -co --exclude-standard -z >"$tmp/paths.z"
    # Paths whose working-tree content differs from HEAD (the bundle will not carry them).
    { git -C "$top" diff --name-only -z HEAD; git -C "$top" ls-files -o --exclude-standard -z; } >"$tmp/uncommitted.z"
    # Changes vs the base incl. deletions; --no-renames so a rename stays delete+add (overlap check).
    { git -C "$top" diff --name-status -z --no-renames "$base"
      git -C "$top" ls-files -o --exclude-standard -z | while IFS= read -r -d '' f; do printf 'A\0%s\0' "$f"; done
    } >"$tmp/changes.z"
    bundle_args=()
    if [ "$base" != "$head" ]; then
        git -C "$top" bundle create "$tmp/branch.bundle" "refs/heads/$(git symbolic-ref --short HEAD)" "^$base" >/dev/null 2>&1 \
            && bundle_args=(--bundle "$tmp/branch.bundle") || warn "bundle failed; sharing files without commits"
    fi
    handoff_args=(); [ -n "$note" ] && handoff_args=(--handoff "$note")
    store_must publish --project "$(project_id)" --slug "$slug" --root "$top" \
        --branch "$(git symbolic-ref --short HEAD)" --base "$base" --head "$head" \
        --paths-z "$tmp/paths.z" --uncommitted-z "$tmp/uncommitted.z" --changes-z "$tmp/changes.z" \
        ${bundle_args[@]+"${bundle_args[@]}"} ${handoff_args[@]+"${handoff_args[@]}"}
    ;;
land)
    slug=$(slug_of); check_slug "$slug"
    "$agf/land.sh" "$@"   # exits non-zero (set -e) if nothing landed: no promotion then
    store_promote
    ;;
fetch)
    slug=${1:?usage: fetch <slug> <dest>}; dest=${2:?usage: fetch <slug> <dest>}; shift 2
    check_slug "$slug"
    ref=refs/shared/$slug
    if [ "${1:-}" = "--bundle-ref" ]; then
        ref=${2:?--bundle-ref needs a ref}
        [[ $ref =~ ^refs/shared/[a-z0-9][a-z0-9/-]*$ && $ref != *//* && $ref != */ ]] || die "bundle ref must be under refs/shared/"
    fi
    mkdir -p "$dest"
    rm -f "$dest/.wt-branch.bundle"   # never reuse a bundle from an earlier fetch
    store_must fetch --project "$(project_id)" --slug "$slug" --dest "$dest" --bundle-out "$dest/.wt-branch.bundle"
    if [ -f "$dest/.wt-branch.bundle" ] && git -C "$dest" rev-parse --git-dir >/dev/null 2>&1; then
        git -C "$dest" fetch --quiet "$dest/.wt-branch.bundle" "+refs/heads/*:$ref/*" || die "bundle fetch failed"
        [ -n "$(git -C "$dest" for-each-ref "$ref")" ] || die "bundle carried no branch; no commits fetched"
        warn "commits fetched under $ref/"
    fi
    ;;
claim)
    slug=${1:?usage: claim <slug>}; check_slug "$slug"
    store_must claim --project "$(project_id)" --slug "$slug"
    ;;
follow)
    pid=$(project_id); [[ $pid =~ ^[A-Za-z0-9]{1,40}$ ]] || die "unexpected project id '$pid'"
    state=${XDG_STATE_HOME:-$HOME/.local/state}/weftos/follow-$pid.json
    [ "${1:-}" = "--state" ] && { state=${2:?--state needs a file}; }
    mkdir -p "$(dirname "$state")"
    store_must follow --project "$(project_id)" --state "$state"
    ;;
search)
    q=${1:?usage: search <query>}
    store_must search --project "$(project_id)" --query "$q"
    ;;
leftovers)
    case "${1:-}" in ""|--json) ;; *) die "usage: leftovers [--json]" ;; esac
    store_must leftovers --project "$(project_id)" ${1:+"$1"}
    ;;
sync-docs)
    slug=${1:?usage: sync-docs <slug> <cache>}; cache=${2:?usage: sync-docs <slug> <cache>}
    check_slug "$slug"; mkdir -p "$cache"
    store_must sync-docs --project "$(project_id)" --slug "$slug" --cache "$cache"
    ;;
overlaps)
    a=${1:?usage: overlaps <slugA> <slugB>}; c=${2:?usage: overlaps <slugA> <slugB>}
    check_slug "$a"; check_slug "$c"
    store_must overlaps --project "$(project_id)" --a "$a" --b "$c"
    ;;
mount-config|mount)
    sub=$cmd
    slug=${1:?usage: $sub <slug> $([ "$sub" = mount ] && echo '<dir> ')[--local STORE_ROOT]}; check_slug "$slug"; shift
    dir=""; [ "$sub" = mount ] && { dir=${1:?usage: mount <slug> <dir> [--local STORE_ROOT]}; shift; }
    local_root=""
    if [ "${1:-}" = "--local" ]; then local_root=${2:?--local needs the store root}; shift 2; fi
    # Desktop union view (ADR-126 §4): worktree bucket first and writable, project bucket read-only.
    # Bucket names come from `weaver store worktree buckets`, never hand-built.
    if [ "${WTS_DRY_RUN:-0}" = 1 ]; then wb="w-DRY-$slug"; pb="p-DRY"; else
        read -r wb pb < <("$weaver" store worktree buckets --project "$(project_id)" --slug "$slug") \
            || die "cannot resolve bucket names"
    fi
    [[ $wb =~ ^[a-z0-9-]{3,63}$ && $pb =~ ^[a-z0-9-]{3,63}$ ]] || die "unexpected bucket names"
    if [ -n "$local_root" ]; then
        # The daemon's local-directory backend keeps each bucket as <root>/buckets/<bucket>/.
        [[ $local_root = /* && $local_root != *[[:space:]]* ]] || die "--local wants an absolute path without spaces"
        remote_type="local"; wt_up="$local_root/buckets/$wb"; pr_up="$local_root/buckets/$pb"
        wt_remote="wts-$slug-wt"; pr_remote="wts-$slug-pr"
        upstreams="$wt_remote:$wt_up $pr_remote:$pr_up:ro"
        remotes="[$wt_remote]
type = local

[$pr_remote]
type = local
"
    else
        endpoint=${WEFTOS_STORE_ENDPOINT:-}
        if [ "$sub" = mount ]; then [ -n "$endpoint" ] || die "set WEFTOS_STORE_ENDPOINT (and RCLONE_S3_ACCESS_KEY_ID / RCLONE_S3_SECRET_ACCESS_KEY), or use --local"; fi
        [ -n "$endpoint" ] || endpoint="<set WEFTOS_STORE_ENDPOINT>"
        [[ $endpoint =~ ^https?://[A-Za-z0-9._:/-]+$ || $endpoint = "<set WEFTOS_STORE_ENDPOINT>" ]] || die "unexpected endpoint '$endpoint'"
        upstreams="wts-$slug-wt:$wb wts-$slug-pr:$pb:ro"
        remotes="# Credentials come from the environment (RCLONE_S3_*), never from this file.
[wts-$slug-wt]
type = s3
provider = Other
env_auth = true
endpoint = $endpoint

[wts-$slug-pr]
type = s3
provider = Other
env_auth = true
endpoint = $endpoint
"
    fi
    cfg=$(cat <<CFG
$remotes
[wts-$slug]
type = union
upstreams = $upstreams
action_policy = epall
create_policy = epff
search_policy = ff
CFG
)
    if [ "$sub" = mount-config ]; then
        printf '%s\n' "$cfg"
        cat <<NOTE
# Mount with:  $0 mount $slug <dir>${local_root:+ --local $local_root}
#   (rclone mount wts-$slug: <dir> --exclude '.wt/**' --vfs-cache-mode writes --vfs-write-back 1s)
# Why those flags: .wt/ holds overlay and sharing state, never project files; the write cache is
# required for normal editors; writes upload after the write-back delay, so unmount waits for it.
# Limits (checked against rclone 1.75.2): the union has no tombstones, so deleting a project-only
# file through the mount is refused (the project upstream is :ro); delete through the store client
# to record a tombstone. Overwriting a project file creates a worktree copy; the project is untouched.
NOTE
        exit 0
    fi
    command -v rclone >/dev/null 2>&1 || die "rclone not found"
    [ -d "$dir" ] || die "mount dir '$dir' must exist"
    conf=$(mktemp "${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/wts-rclone.XXXXXX"); chmod 600 "$conf"
    printf '%s\n' "$cfg" >"$conf"
    # Not exec'd or traced: the config may be sensitive on S3 setups. Removed after the daemon starts.
    RCLONE_CONFIG="$conf" rclone mount "wts-$slug:" "$dir" --exclude '.wt/**' \
        --vfs-cache-mode writes --vfs-write-back 1s --daemon || { rm -f "$conf"; die "rclone mount failed"; }
    rm -f "$conf"   # rclone read it at start
    warn "mounted $dir; unmount with: $0 umount $dir"
    ;;
umount)
    dir=${1:?usage: umount <dir>}
    sleep 3   # let the write-back cache upload (--vfs-write-back 1s); rclone does not flush on unmount
    fusermount3 -u "$dir" 2>/dev/null || fusermount -u "$dir" || die "unmount failed"
    ;;
-h|--help|"") sed -n '2,25p' "$0" ;;
*) die "unknown command: $cmd" ;;
esac
