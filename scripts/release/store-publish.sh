#!/usr/bin/env bash
# store-publish.sh <artifacts-dir> <tag> [--execute]
#
# ADR-126 S4 wiring: publish a release's artifacts into the channel's release store with
# `weaver store release publish` (crates/clawft-weave/src/commands/release_cmd.rs), once per build id.
#
# SAFE BY DEFAULT. Nothing is uploaded unless all three hold:
#   1. WEFTOS_RELEASE_STORE_PUBLISH=1 (the feature is off otherwise: the script prints "disabled" and exits 0,
#      so a workflow step that calls it is a no-op until the owner turns it on),
#   2. --execute is given,
#   3. every input below validates.
# Without --execute (and with the switch on) it is a dry run: it validates the inputs locally and prints the
# exact weaver command. A dry run never starts weaver, never reads the signing seeds, never touches the network.
#
# It runs AFTER sign-release.sh in the host job (the signed weftos-release.json is one of the files published)
# and it does not replace the GitHub Release upload.
#
# Inputs (environment):
#   WEFTOS_RELEASE_STORE        weftos://<mesh>/projects/<ULID>/releases/<channel>/store   (required)
#   WEFTOS_RELEASE_STORE_KEY    path to the signing-key JSON, mode 0600                     (required)
#   WEFTOS_RELEASE_STORE_S3ENV  path to the env file with MESH_S3_ENDPOINT / _ACCESS_KEY / _SECRET_KEY, mode 0600 (required)
#   WEFTOS_RELEASE_STORE_LOCK   optional "governance:<days>" or "compliance:<days>" (compliance cannot be undone by anyone)
#   WEFTOS_RELEASE_BUILDER      optional builder name (default ci)
#   WEAVER                      optional path to weaver (default: weaver on PATH; only used with --execute)
#   GITHUB_SHA                  source revision (default: git rev-parse HEAD)
# Secrets are passed by file path only; this script never echoes their contents.
set -euo pipefail

die() { echo "store-publish: $*" >&2; exit 1; }

execute=false
args=()
for a in "$@"; do
    case "$a" in
        --execute) execute=true ;;
        -h|--help) sed -n '2,29p' "$0"; exit 0 ;;
        -*) die "unknown option $a" ;;
        *) args+=("$a") ;;
    esac
done
[[ ${#args[@]} -eq 2 ]] || die "usage: store-publish.sh <artifacts-dir> <tag> [--execute]"
dir=${args[0]}
tag=${args[1]}

if [[ ${WEFTOS_RELEASE_STORE_PUBLISH:-0} != 1 ]]; then
    echo "store-publish: disabled (set WEFTOS_RELEASE_STORE_PUBLISH=1 to enable); nothing done"
    exit 0
fi

[[ -d $dir ]] || die "$dir is not a directory"
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+[0-9A-Za-z.+_-]*$ ]] || die "tag $tag is not a vX.Y.Z release tag"
store=${WEFTOS_RELEASE_STORE:-}
[[ $store =~ ^weftos://[A-Za-z0-9._-]+/projects/[0-9A-HJKMNP-TV-Z]{26}/releases/[A-Za-z0-9._-]+/store$ ]] \
    || die "WEFTOS_RELEASE_STORE must be weftos://<mesh>/projects/<ULID>/releases/<channel>/store"
key=${WEFTOS_RELEASE_STORE_KEY:-}
s3env=${WEFTOS_RELEASE_STORE_S3ENV:-}
[[ -n $key && -n $s3env ]] || die "WEFTOS_RELEASE_STORE_KEY and WEFTOS_RELEASE_STORE_S3ENV are required"

mode_of() { stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1"; }
for f in "$key" "$s3env"; do
    [[ -f $f ]] || die "$f is not a file"
    m=$(mode_of "$f")
    [[ $m =~ ^[0-7]?[0-7]00$ ]] || die "$f has mode $m; chmod 600"
done
for v in MESH_S3_ENDPOINT MESH_S3_ACCESS_KEY MESH_S3_SECRET_KEY; do
    grep -q "^${v}=." "$s3env" || die "$s3env has no $v"
done

# The directory must hold regular files only: weaver refuses symlinks and special files.
[[ -n $(find "$dir" -type f -print -quit) ]] || die "$dir has no files to publish"
[[ -z $(find "$dir" ! -type f ! -type d -print -quit) ]] || die "$dir holds symlinks or special files; weaver refuses them"

rev=${GITHUB_SHA:-$(git rev-parse HEAD 2>/dev/null || true)}
[[ $rev =~ ^[0-9a-f]{7,64}$ ]] || die "no source revision (set GITHUB_SHA)"
build_id=$tag
cmd=("${WEAVER:-weaver}" store release publish "$store"
     --build-id "$build_id" --source-rev "$rev" --dir "$dir"
     --builder "${WEFTOS_RELEASE_BUILDER:-ci}"
     --signing-key "$key" --s3-env "$s3env" --ci-route direct)
if [[ -n ${WEFTOS_RELEASE_STORE_LOCK:-} ]]; then
    [[ $WEFTOS_RELEASE_STORE_LOCK =~ ^(governance|compliance):([0-9]{1,4})$ ]] \
        || die "WEFTOS_RELEASE_STORE_LOCK must be governance:<days> or compliance:<days>"
    cmd+=(--lock "${BASH_REMATCH[1]}" --lock-days "${BASH_REMATCH[2]}")
fi

if [[ $execute != true ]]; then
    echo "store-publish: DRY RUN (add --execute to publish); inputs valid; would run:"
    printf '  '; printf '%q ' "${cmd[@]}"; echo
    exit 0
fi
command -v "${cmd[0]}" >/dev/null || die "${cmd[0]} not found"
echo "store-publish: publishing $build_id to $store"
exec "${cmd[@]}"
