#!/bin/sh
# Canonical M5.1 owner-routing gate: production code in
# bridge/browser/src/browserbridge.js must mutate owner state only through
# owner methods — never a raw `_mutationTail` chain, never
# `server.active`/`server.closing` writes, never a wholesale `this.server`
# or `this._mutationChain` replacement, never direct `_mutationChain`
# tail/depth writes, and never bare `activeConns`/`closing` aliases outside
# the owner classes. Reads (server.active/server.closing counter reads,
# chain entry via _mutationRun) stay allowed.
#
# Robustness: owner-class bodies (SerialChain, ServerState) and all
# comments are removed before matching, so the routing-rule comment
# itself (which names the forbidden spellings) and owner internals
# (this.tail, this.closing = true, ...) can never false-positive.
# `://` (URLs in string literals) is shielded before `//`-comment
# stripping so string tails survive.
#
# `--self-test` (negative + positive control, no production effect):
# a comment-only fixture naming every forbidden spelling (plus a fake
# owner-class body with real violations, proving class stripping) must
# scan clean, while a planted fixture with one real violation per pattern
# must be fully detected; the real tree must scan clean with its owner
# bodies verifiably stripped.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TARGET="$ROOT/bridge/browser/src/browserbridge.js"

# Comment-stripped, owner-class-free code of one file (stdout). Class
# stripping is load-bearing (owner internals name the same spellings);
# the self-test proves it on the real tree below.
clean_of() {
  awk '
    /^class (SerialChain|ServerState)[ ({]/ { in_class = 1; next }
    in_class && /^\}/ { in_class = 0; next }
    in_class { next }
    { print }
  ' "$1" | sed -e 's#://#\x01#g' | awk '
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
    }'
}

# Violation lines (`lineno:content`) of one file (stdout, empty = clean).
scan_file() {
  clean_of "$1" | grep -nE \
    -e '_mutationTail' \
    -e 'activeConns' \
    -e 'this\._mutationChain\.(tail|depth)[[:space:]]*(\+\+|--|[+-]?=([^=]|$))' \
    -e 'server\.active[[:space:]]*(\+\+|--|[+-]?=([^=]|$))' \
    -e '\.closing[[:space:]]*=([^=]|$)' \
    || true
  # Owner replacement: the sole legit spellings are the Session
  # constructions (`this.server = new ServerState()`,
  # `this._mutationChain = new SerialChain()`), filtered here so a
  # wholesale swap anywhere else still trips.
  clean_of "$1" | grep -nE -e 'this\.server[[:space:]]*=[^=]' \
    | grep -v 'new ServerState()' || true
  clean_of "$1" | grep -nE -e 'this\._mutationChain[[:space:]]*=[^=]' \
    | grep -v 'new SerialChain()' || true
}

if [ "${1:-}" = "--self-test" ]; then
  TSELF=$(mktemp -d)
  trap 'rm -rf "$TSELF"' EXIT
  cat > "$TSELF/comment_only.js" <<'EOF'
// Comment-only fixture: names every forbidden spelling, violates nothing.
/* _mutationTail activeConns server.active++ server.closing = true
   this.closing = true this.server = null this._mutationChain = null
   this._mutationChain.tail = null this._mutationChain.depth = 0 */
// Fake owner body: real violations, but inside a stripped class.
class ServerState {
  claimClose() { this.closing = true; return true; }
}
class Session {
  ok() { return this.server.closing; }
}
EOF
  if [ -n "$(scan_file "$TSELF/comment_only.js")" ]; then
    echo "SELF-TEST FAIL: comment-only fixture flagged:" >&2
    scan_file "$TSELF/comment_only.js" >&2
    exit 1
  fi
  cat > "$TSELF/planted.js" <<'EOF'
class EvilSession {
  evil() {
    this._queue._mutationTail = [];
    this.pool.activeConns += 1;
    this._mutationChain.tail = null;
    this._mutationChain.depth = 0;
    this.server.active += 1;
    this.server.closing = true;
    this.closing = true;
    this.server = null;
    this._mutationChain = null;
  }
}
EOF
  # One violation line per planted line (9): fewer means a pattern missed.
  N=$(scan_file "$TSELF/planted.js" | wc -l | tr -d ' ')
  if [ "$N" != "9" ]; then
    echo "SELF-TEST FAIL: planted fixture yielded $N lines (want 9):" >&2
    scan_file "$TSELF/planted.js" >&2
    exit 1
  fi
  # Stripping control on the real tree: owner internals gone, Session
  # construction intact (proves the cleaner is neither blind nor vacuous).
  if [ "$(clean_of "$TARGET" | grep -c 'this\.tail = Promise' || true)" != "0" ]; then
    echo "SELF-TEST FAIL: owner-class stripping lost (this.tail visible)" >&2
    exit 1
  fi
  if [ "$(clean_of "$TARGET" | grep -c 'this\.server = new ServerState()' || true)" != "1" ]; then
    echo "SELF-TEST FAIL: Session construction missing from cleaned tree" >&2
    exit 1
  fi
  if [ -n "$(scan_file "$TARGET")" ]; then
    echo "SELF-TEST FAIL: real tree flagged:" >&2
    scan_file "$TARGET" >&2
    exit 1
  fi
  echo "ok: check_browserbridge_owners self-test (comments pass, planted caught, stripping verified)"
  exit 0
fi

VIOLATIONS=$(scan_file "$TARGET")

if [ -n "$VIOLATIONS" ]; then
  echo "FAIL: direct owner-state writes outside owner classes:" >&2
  echo "$VIOLATIONS" | sed "s#^#bridge/browser/src/browserbridge.js:#" >&2
  exit 1
fi
echo "ok: browserbridge owner routing (no direct writes outside owners)"
