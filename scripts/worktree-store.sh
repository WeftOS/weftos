#!/usr/bin/env bash
# ADR-126 S2 hooks: worktree stores around agent-git-flow, plus the desktop union mount.
#
#   worktree-store.sh new <name> [wt.sh args]   wt.sh new, then create the worktree store
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
#   worktree-store.sh mount-config <slug>       print the rclone union remote (worktree over project)
#
# All store operations go through `weaver store worktree <verb>` (S1 owns the `weaver store` surface;
# the verbs are create|publish|promote|fetch|claim|done|list). Git work stays here, not in Rust.
# Store failures never block git flow: they warn and exit 0 unless WTS_STRICT=1.
#
# Env: WTS_WEAVER (default: weaver), WTS_AGF (default: ~/.claude/skills/agent-git-flow/scripts),
#      WTS_DRY_RUN=1 print the weaver calls instead of running them, WTS_SLUG (default: branch
#      with '/' -> '-'), WTS_PROJECT (project ULID; default: from `weaver project id`).
set -euo pipefail

weaver=${WTS_WEAVER:-weaver}
agf=${WTS_AGF:-$HOME/.claude/skills/agent-git-flow/scripts}
strict=${WTS_STRICT:-0}

warn() { printf 'worktree-store: %s\n' "$*" >&2; }
die() { warn "$*"; exit 1; }

slug_of() { printf '%s' "${WTS_SLUG:-$(git symbolic-ref --quiet --short HEAD | tr '/' '-')}"; }

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
    (cd "$root" && slug=$(slug_of) && check_slug "$slug" && store create --project "$(project_id)" --slug "$slug" \
        --branch "$(git symbolic-ref --short HEAD)" --base "$(git rev-parse HEAD)")
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
    store promote --project "$(project_id)" --slug "$slug"
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
mount-config)
    slug=${1:?usage: mount-config <slug>}; check_slug "$slug"
    # Desktop union view (ADR-126 §4): worktree bucket first and writable, project bucket read-only.
    # Bucket names come from `weaver store worktree buckets`, never hand-built.
    if [ "${WTS_DRY_RUN:-0}" = 1 ]; then wb="w-DRY-$slug"; pb="p-DRY"; else
        read -r wb pb < <("$weaver" store worktree buckets --project "$(project_id)" --slug "$slug") \
            || die "cannot resolve bucket names"
    fi
    [[ $wb =~ ^[a-z0-9-]{3,63}$ && $pb =~ ^[a-z0-9-]{3,63}$ ]] || die "unexpected bucket names"
    cat <<CFG
# rclone.conf fragment. Credentials come from the environment (RCLONE_S3_*), never from this file.
[wts-$slug-wt]
type = s3
provider = Other
env_auth = true
endpoint = \${WEFTOS_STORE_ENDPOINT}

[wts-$slug-pr]
type = s3
provider = Other
env_auth = true
endpoint = \${WEFTOS_STORE_ENDPOINT}

[wts-$slug]
type = union
upstreams = wts-$slug-wt:$wb wts-$slug-pr:$pb:ro
action_policy = epall
create_policy = epff
search_policy = ff
# Mount with:  rclone mount wts-$slug: ~/mnt/$slug --exclude '.wt/**'
# Limit: rclone union has no tombstones, so deleting a project-only file through the mount is
# refused (project is :ro). Delete through the weftos-store client to record a tombstone.
CFG
    ;;
-h|--help|"") sed -n '2,25p' "$0" ;;
*) die "unknown command: $cmd" ;;
esac
