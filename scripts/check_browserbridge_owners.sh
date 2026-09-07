#!/bin/sh
# Canonical M5.1 owner-routing gate: production code in
# bridge/browser/src/browserbridge.js must mutate owner state only through
# owner methods — never a raw `_mutationTail` chain, never
# `server.active`/`server.closing` writes and never bare
# `activeConns`/`closing` aliases outside the owner classes. Reads
# (server.active/server.closing counter reads, chain entry via
# _mutationRun) stay allowed.
#
# Robustness: owner-class bodies (SerialChain, ServerState) and all
# comments are removed before matching, so the routing-rule comment
# itself (which names the forbidden spellings) and owner internals
# (this.tail, this.closing = true, ...) can never false-positive.
# `://` (URLs in string literals) is shielded before `//`-comment
# stripping so string tails survive.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TARGET="$ROOT/bridge/browser/src/browserbridge.js"

TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT

awk '
  /^class (SerialChain|ServerState)[ ({]/ { in_class = 1; next }
  in_class && /^\}/ { in_class = 0; next }
  in_class { next }
  { print }
' "$TARGET" > "$TMP.noclass"

# Strip block comments (possibly multi-line), shielding :// first.
sed -e 's#://#\x01#g' "$TMP.noclass" |
awk '
  in_block && /\*\// { sub(/^.*\*\//, ""); in_block = 0 }
  in_block { next }
  {
    line = $0
    while (match(line, /\/\*/) != 0) {
      before = substr(line, 1, RSTART - 1)
      rest = substr(line, RSTART + 2)
      if (match(rest, /\*\//) != 0) {
        line = before substr(rest, RSTART + 2)
      } else {
        line = before
        in_block = 1
        break
      }
    }
    # Line comment (URLs already shielded).
    sub(/\/\/.*$/, "", line)
    gsub(/\x01/, "://", line)
    print line
  }
' > "$TMP.clean"

VIOLATIONS=$(grep -nE \
  -e '_mutationTail' \
  -e 'activeConns' \
  -e 'server\.active[[:space:]]*(\+\+|--|[+-]?=([^=]|$))' \
  -e '\.closing[[:space:]]*=([^=]|$)' \
  "$TMP.clean" || true)

if [ -n "$VIOLATIONS" ]; then
  echo "FAIL: direct owner-state writes outside owner classes:" >&2
  echo "$VIOLATIONS" | sed "s#^#bridge/browser/src/browserbridge.js:#" >&2
  exit 1
fi
echo "ok: browserbridge owner routing (no direct writes outside owners)"
