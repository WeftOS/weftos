#!/usr/bin/env bash
# diff-gate-tokens-stub.sh: stands in for tools/diff-gate-tokens in the bash fixture suite, so
# the suite needs no cargo build. It finds nothing unless told to:
#   DIFF_GATE_STUB_OUT       printed as the helper's output
#   DIFF_GATE_STUB_RC        exit status for --manifest (default 0)
#   DIFF_GATE_STUB_PROTOCOL  what --protocol prints (default: the real protocol line)
case "${1:-}" in
    --protocol) echo "${DIFF_GATE_STUB_PROTOCOL:-diff-gate-tokens protocol 1}"; exit 0 ;;
    --manifest) [ -r "${2:-}" ] || exit 2 ;;
    *) exit 2 ;;
esac
[ -n "${DIFF_GATE_STUB_OUT:-}" ] && printf '%s\n' "$DIFF_GATE_STUB_OUT"
exit "${DIFF_GATE_STUB_RC:-0}"
