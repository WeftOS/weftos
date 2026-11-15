#!/usr/bin/env bash
# Sourced by wo069-e2e-test.sh (uses its helpers and $WORK $PROJECT $S $WEAVER). Design note Q9: a worktree that
# switches branch after its first publish, under (a) today's slug-at-registration and (b) `register --follow-branch`.
# Both start from a normal published worktree (store created by the new-worktree hook), then `git switch -c`, edit a
# doc, and sweep. The follower's view is printed as Q9 rows so the numbers can go straight into the note.
echo "== Q9: branch change after the first publish"
q9_new() { # <name> <branch> [register flags]
    local d="$WORK/wt-q$1"
    G "$WORK/repo" worktree add -q -b "$2" "$d" master
    mkdir -p "$d/docs"; printf '# Q9 %s before\n' "$1" >"$d/docs/q9.md"
    ( cd "$d" && as writer "$S" store-create && as writer "$S" register ${3:+"$3"} ) >/dev/null 2>&1
}
q9_view() { # <label> <slug>...: what the follower sees (registry list, folder, search)
    local label=$1; shift; local s
    as follower "$S" browse --json >"$WORK/q9-list.json" 2>&1
    echo "Q9	$label	worktrees: $(jq -r '[.worktrees[] | "\(.slug)[branch=\(.branch) docs=\(.doc_count)]"] | join(" ")' "$WORK/q9-list.json" 2>/dev/null)"
    for s in "$@"; do
        echo "Q9	$label	$s docs/: $(as follower "$S" browse "$s" docs/ --json 2>&1 | jq -r '[.entries[]? | "\(.name)=\(.title)"] | join(", ")' 2>/dev/null)"
    done
    echo "Q9	$label	search 'moved': $(as follower "$S" search moved 2>&1 | awk '{printf "%s%s:%s", sep, $1, $2; sep=", "}')"
}
q9_new a q9/a; q9_new b q9/b --follow-branch
( cd "$WORK/wt-qa" && as writer "$S" sweep --only q9--a ) >/dev/null 2>&1; ( cd "$WORK/wt-qb" && as writer "$S" sweep --only q9--b ) >/dev/null 2>&1
check "Q9 setup: both worktrees published under their branch slugs" "as follower '$S' browse --json | jq -e '[.worktrees[].slug] | (index(\"q9--a\") != null and index(\"q9--b\") != null)'"
q9_view before q9--a q9--b | tee -a "$WORK/steps.tsv"

for w in a b; do
    G "$WORK/wt-q$w" switch -q -c "q9/$w-moved"; printf '# Q9 %s moved\n' "$w" >"$WORK/wt-q$w/docs/q9-moved.md"
done
: >"$WORK/calls-writer.log"
timed "Q9 (a) sweep, slug fixed at registration" "as writer '$S' sweep --only q9--a"; cp "$WORK/last.out" "$WORK/q9a.out"; cp "$WORK/last.err" "$WORK/q9a.err"
cp "$WORK/calls-writer.log" "$WORK/q9a.calls"; : >"$WORK/calls-writer.log"
timed "Q9 (b) sweep, slug follows the branch" "as writer '$S' sweep --only q9--b"; cp "$WORK/last.out" "$WORK/q9b.out"; cp "$WORK/last.err" "$WORK/q9b.err"
cp "$WORK/calls-writer.log" "$WORK/q9b.calls"
{ echo "Q9	(a) sweep line: $(tr '\t' ' ' <"$WORK/q9a.out")  store write: $(grep -E '"(uploaded|unchanged)"' "$WORK/q9a.err" | tr -d ' \n')"; echo "Q9	(a) weaver verbs: $(awk '{print $3}' "$WORK/q9a.calls" | tr '\n' ' ')  slug/branch: $(grep -oE -- '--(slug|branch) [^ ]+' "$WORK/q9a.calls" | tr '\n' ';')"
  echo "Q9	(b) sweep line: $(tr '\t' ' ' <"$WORK/q9b.out")  store write: $(grep -E '"(uploaded|unchanged)"' "$WORK/q9b.err" | tr -d ' \n')"; echo "Q9	(b) weaver verbs: $(awk '{print $3}' "$WORK/q9b.calls" | tr '\n' ' ')  slug/branch: $(grep -oE -- '--(slug|branch) [^ ]+' "$WORK/q9b.calls" | tr '\n' ';')"
} | tee -a "$WORK/steps.tsv"
q9_view after q9--a q9--a-moved q9--b q9--b-moved 2>&1 | tee -a "$WORK/steps.tsv"
timed "follower follow after both switches" "as follower '$S' follow"; sed 's/^/Q9	follow	/' "$WORK/last.out" | tee -a "$WORK/steps.tsv"
check "Q9 (a): the sweep line flags the branch drift" "grep -q 'branch-drift=q9/a->q9/a-moved' '$WORK/q9a.out'"
check "Q9 (b): the sweep line reports the slug move, no drift" "grep -q 'slug-moved=q9--b->q9--b-moved' '$WORK/q9b.out' && ! grep -q branch-drift '$WORK/q9b.out'"
check "Q9 (a): the old slug now serves the NEW branch's tree (doc q9-moved.md under q9--a)" "as follower '$S' browse q9--a docs/ --json | jq -e '[.entries[].name] | index(\"q9-moved.md\") != null'"
check "Q9 (b): the new slug exists and holds the new doc" "as follower '$S' browse q9--b-moved docs/ --json | jq -e '[.entries[].name] | index(\"q9-moved.md\") != null'"
check "Q9 (b): the old slug's store is left as it was (no moved doc)" "as follower '$S' browse q9--b docs/ --json | jq -e '[.entries[].name] | index(\"q9-moved.md\") == null'"
check "Q9 (b): a second sweep is idle" "as writer '$S' sweep --only q9--b-moved | grep -q 'unchanged'"
