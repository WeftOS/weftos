#!/usr/bin/env bash
# agents-leak-check.sh — confidentiality denylist scan over agents/
#
# WeftOS's agents/ directory is checked into a PUBLIC repo. This script exists because
# some packages under agents/ were ported from a private client engagement repo
# and generalized for reuse — see each package's
# weftos-package.yaml `provenance` block. This scan is the safety net that keeps
# client-identifying content out of agents/ going forward, on every run, not just at
# port time.
#
# Usage: scripts/agents-leak-check.sh
# Exit code: 0 = clean, non-zero = at least one denylisted pattern matched.
#
# NOTE: client-identifying patterns are NOT kept in this public script. They are read
# from a private file outside the repo (see WEFTOS_LEAK_PATTERNS below). Only generic
# checks (emails, home paths, credential shapes, roster keys) live here.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${1:-$REPO_ROOT/agents}"

if [[ ! -d "$TARGET_DIR" ]]; then
  echo "agents-leak-check: target directory not found: $TARGET_DIR" >&2
  exit 2
fi

fail=0
hits=0

# name | pattern (extended regex, case-insensitive) | note
declare -a RULES=(
  "email address|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}|a real email address"
  "absolute home path|/Users/[A-Za-z0-9_.-]+|a local absolute path that can leak a username/machine"
  "roster entry (roster:)|^[[:space:]]*roster[[:space:]]*:[[:space:]]*\$|a literal roster key — verify no real personnel list follows (project rosters belong in project context, referenced by key name only, never embedded here)"
  "roster entry (people:)|^[[:space:]]*people[[:space:]]*:[[:space:]]*\$|a literal people key — verify no real personnel list follows"
  "roster entry (team_roster:)|^[[:space:]]*team_roster[[:space:]]*:[[:space:]]*\$|a literal team_roster key — verify no real personnel list follows"
  "credential shape (board token)|\bwfb_[a-f0-9]{64}\b|a live WeftOS dashboard harness credential"
  "credential shape (Anthropic key)|\bsk-ant-[A-Za-z0-9_-]{20,}\b|a live Anthropic API key"
  "credential shape (generic sk- key)|\bsk-[A-Za-z0-9_-]{20,}\b|a generic secret-key-shaped token"
  "credential shape (GitHub token)|\bgh[pousr]_[A-Za-z0-9]{20,}\b|a live GitHub token"
  "credential shape (AWS access key)|\bAKIA[0-9A-Z]{16}\b|a live AWS access key id"
  "credential shape (private key block)|-----BEGIN[ A-Z]*PRIVATE KEY-----|an embedded private key"
)

# Client-identifying patterns (one extended regex per line, '#' comments allowed) are read
# from a private file outside the repo: $WEFTOS_LEAK_PATTERNS, default
# ~/.config/weftos/leak-patterns.txt. If the file is missing the generic checks still run,
# a notice is printed, and the script does not fail.
LEAK_PATTERNS_FILE="${WEFTOS_LEAK_PATTERNS:-$HOME/.config/weftos/leak-patterns.txt}"
if [[ -r "$LEAK_PATTERNS_FILE" ]]; then
  while IFS= read -r term || [[ -n "$term" ]]; do
    [[ -z "${term// }" || "$term" == \#* ]] && continue
    RULES+=("private pattern|$term|a pattern from the private leak-patterns file")
  done < "$LEAK_PATTERNS_FILE"
else
  echo "agents-leak-check: notice: no private patterns file at $LEAK_PATTERNS_FILE; running generic checks only" >&2
fi

echo "agents-leak-check: scanning $TARGET_DIR"
echo

for rule in "${RULES[@]}"; do
  IFS='|' read -r name pattern note <<<"$rule"
  matches=$(grep -RniE --exclude-dir=.git -- "$pattern" "$TARGET_DIR" 2>/dev/null || true)
  if [[ -n "$matches" ]]; then
    fail=1
    count=$(printf '%s\n' "$matches" | grep -c .)
    hits=$((hits + count))
    echo "FAIL  [$name] $count hit(s) — $note"
    printf '%s\n' "$matches" | sed 's/^/      /'
    echo
  fi
done

if [[ "$fail" -eq 0 ]]; then
  echo "agents-leak-check: clean — no denylisted terms found under $TARGET_DIR"
  exit 0
else
  echo "agents-leak-check: FAILED — $hits total hit(s) across the denylist above"
  echo "Remove or generalize the flagged content, then re-run this script."
  exit 1
fi
