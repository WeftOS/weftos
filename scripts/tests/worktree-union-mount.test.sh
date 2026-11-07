#!/usr/bin/env bash
# ADR-126 S2 desktop union mount, checked with a real rclone and a real FUSE mount over the daemon's
# local-directory store layout (<root>/buckets/<bucket>/<key>). Skips (exit 0) without rclone/FUSE.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
command -v rclone >/dev/null && [ -e /dev/fuse ] && command -v fusermount3 >/dev/null || { echo "skip: rclone/FUSE not available"; exit 0; }
t=$(mktemp -d); trap 'fusermount3 -u "$t/mnt" 2>/dev/null || true; rm -rf "$t"' EXIT
pass=0; fail=0
ok() { if eval "$2"; then pass=$((pass+1)); else fail=$((fail+1)); echo "FAIL: $1"; fi; }

mkdir -p "$t/bin" "$t/mnt" "$t/store/buckets/p-01abc/assets/models" "$t/store/buckets/w-01abc-aaaaaaaaaaaa/.wt"
cat >"$t/bin/weaver" <<'W'
#!/usr/bin/env bash
[ "$1 $2 $3" = "store worktree buckets" ] && echo "w-01abc-aaaaaaaaaaaa p-01abc"
[ "$1 $2" = "project id" ] && echo 01ABC
exit 0
W
chmod +x "$t/bin/weaver"
export WTS_WEAVER="$t/bin/weaver" WTS_PROJECT=01ABC
P="$t/store/buckets/p-01abc"; W="$t/store/buckets/w-01abc-aaaaaaaaaaaa"
echo HERO >"$P/assets/models/hero.bin"; echo ro >"$P/shared.txt"; echo '{}' >"$W/.wt/meta.json"

"$here/worktree-store.sh" mount s2 "$t/mnt" --local "$t/store" 2>/dev/null
ok "project asset readable with no copy" "[ \"\$(cat '$t/mnt/assets/models/hero.bin')\" = HERO ]"
ok "nothing copied into the worktree bucket" "[ ! -e '$W/assets' ]"
ok ".wt state hidden from the mount" "[ ! -e '$t/mnt/.wt' ]"
echo mine >"$t/mnt/assets/models/mine.bin"
echo edited >"$t/mnt/shared.txt"
rm "$t/mnt/assets/models/hero.bin" 2>/dev/null && rc=0 || rc=$?
ok "deleting a project-only file is refused" "[ $rc != 0 ]"
"$here/worktree-store.sh" umount "$t/mnt"
ok "new file landed in the worktree bucket" "[ \"\$(cat '$W/assets/models/mine.bin')\" = mine ]"
ok "overwrite of a project file made a worktree copy" "[ \"\$(cat '$W/shared.txt')\" = edited ]"
ok "project bucket untouched" "[ \"\$(cat '$P/shared.txt')\" = ro ] && [ ! -e '$P/assets/models/mine.bin' ] && [ -e '$P/assets/models/hero.bin' ]"
ok "mount-config prints no secrets and no unresolved variables" "! \"$here/worktree-store.sh\" mount-config s2 | grep -i 'secret\|\\\${'"
echo "passed=$pass failed=$fail"; [ "$fail" = 0 ]
