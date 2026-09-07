#!/bin/sh
# Canonical M4 owner-routing gate: production code in
# bridge/node/src/nodebridge.js must mutate owner state only through owner
# methods — never direct writes to workers.table/pending/order/seenIds/
# exited/ignored/droppedExited or server.active/closing outside the owner
# classes. Reads (get/has/live lists, counter reads) stay allowed.
#
# Robustness: owner-class bodies (WorkerRegistry, ServerState, SerialChain)
# and all comments are removed before matching, so the routing-rule comment
# itself (which names the forbidden spellings) and owner internals
# (this.table.set, this.closing = true, ...) can never false-positive.
# `://` (URLs in string literals) is shielded before `//`-comment
# stripping so string tails survive.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TARGET="$ROOT/bridge/node/src/nodebridge.js"

TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT

awk '
  /^class (WorkerRegistry|ServerState|SerialChain)[ ({]/ { in_class = 1; next }
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
  -e 'workers\.table\.(set|delete|clear)[[:space:]]*\(' \
  -e 'workers\.pending\.(set|delete|clear)[[:space:]]*\(' \
  -e 'workers\.order\.(push|splice|pop|shift|unshift)[[:space:]]*\(?' \
  -e 'workers\.seenIds\.(add|delete)[[:space:]]*\(' \
  -e 'workers\.exited\.(push|shift|pop|splice)[[:space:]]*\(?' \
  -e 'workers\.ignored[[:space:]]*(\+|[+-]?=([^=]|$))' \
  -e 'workers\.droppedExited[[:space:]]*(\+|[+-]?=([^=]|$))' \
  -e 'server\.active[[:space:]]*(\+\+|--|[+-]?=([^=]|$))' \
  -e 'server\.closing[[:space:]]*=([^=]|$)' \
  "$TMP.clean" || true)

if [ -n "$VIOLATIONS" ]; then
  echo "FAIL: direct owner-state writes outside owner classes:" >&2
  echo "$VIOLATIONS" | sed "s#^#bridge/node/src/nodebridge.js:#" >&2
  exit 1
fi
echo "ok: nodebridge owner routing (no direct writes outside owners)"
