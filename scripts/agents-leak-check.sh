#!/usr/bin/env bash
# agents-leak-check.sh — confidentiality denylist scan over agents/
#
# WeftOS's agents/ directory is checked into a PUBLIC repo. This script exists because
# some packages under agents/ were ported from a private client engagement repo
# (~/Clients/ctox/sansone) and generalized for reuse — see each package's
# weftos-package.yaml `provenance` block. This scan is the safety net that keeps
# client-identifying content out of agents/ going forward, on every run, not just at
# port time.
#
# Usage: scripts/agents-leak-check.sh
# Exit code: 0 = clean, non-zero = at least one denylisted pattern matched.
#
# NOTE: this script's own source necessarily contains the denylisted terms as literal
# grep patterns — that is how a denylist scanner works, the same way a secret-scanner's
# rule file contains the shape of the secrets it looks for. The terms below are named
# directly in this project's own confidentiality instructions; nothing here is content
# discovered by inspecting client material, and the script's job is exactly to keep those
# terms OUT of the files it scans.

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
  "client/org name (Sansone)|sansone|the engagement client's own name"
  "client/org name (CTOx)|ctox|the engagement methodology vendor's name"
  "client/org name (Fortunian)|fortunian|a client-adjacent org name"
  "client domain|sansonegroup\.com|the client's live domain, found in the source repo"
  "board ticket id|\bSO-[0-9]+\b|a client-board ticket reference (e.g. SO-635)"
  "underwriting vocabulary|underwrit|client business-rule vocabulary (underwriting)"
  "borrower vocabulary|borrower|client business-rule vocabulary (borrower/deal data)"
  "internal ADR series id|ctoxos-[0-9]+|the client engagement's own decision-record series"
  "source system name (Smartsheet)|smartsheet|the client's specific granted third-party system — use the generic external-system-reader template's vocabulary instead"
  "email address|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}|a real email address"
  "absolute home path|/Users/[A-Za-z0-9_.-]+|a local absolute path that can leak a username/machine"
  "client repo path|~/Clients|a reference to the private client-engagement checkout"
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

# Extra terms that must not be named in this public script (one extended regex per line,
# '#' comments allowed) are read from a private file outside the repo:
# $WEFTOS_LEAK_TERMS, default ~/.config/weftos/leak-terms.txt. Missing file: built-in rules only.
LEAK_TERMS_FILE="${WEFTOS_LEAK_TERMS:-$HOME/.config/weftos/leak-terms.txt}"
if [[ -r "$LEAK_TERMS_FILE" ]]; then
  while IFS= read -r term || [[ -n "$term" ]]; do
    [[ -z "${term// }" || "$term" == \#* ]] && continue
    RULES+=("private term|$term|a name from the private leak-terms file")
  done < "$LEAK_TERMS_FILE"
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
