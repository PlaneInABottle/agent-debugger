#!/bin/sh
# Canonical M4 owner-routing gate: production code in
# bridge/node/src/nodebridge.js must mutate owner state only through owner
# methods — never direct writes to workers.table/pending/order/seenIds/
# exited/ignored/droppedExited (method calls, wholesale replacement,
# clears, or length resets) or server.active/closing, and never a wholesale
# `this.workers` registry replacement outside the owner classes. Reads
# (get/has/live lists, counter reads) stay allowed.
#
# Robustness: owner-class bodies (WorkerRegistry, ServerState, SerialChain)
# and all comments are removed before matching, so the routing-rule comment
# itself (which names the forbidden spellings) and owner internals
# (this.table.set, this.closing = true, ...) can never false-positive.
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
TARGET="$ROOT/bridge/node/src/nodebridge.js"

# Comment-stripped, owner-class-free code of one file (stdout). Class
# stripping is load-bearing (owner internals name the same spellings);
# the self-test proves it on the real tree below.
clean_of() {
  awk '
    /^class (WorkerRegistry|ServerState|SerialChain)[ ({]/ { in_class = 1; next }
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
    -e 'workers\.table\.(set|delete|clear)[[:space:]]*\(' \
    -e 'workers\.pending\.(set|delete|clear)[[:space:]]*\(' \
    -e 'workers\.order\.(push|splice|pop|shift|unshift)[[:space:]]*\(?' \
    -e 'workers\.seenIds\.(add|delete|clear)[[:space:]]*\(' \
    -e 'workers\.exited\.(push|shift|pop|splice)[[:space:]]*\(?' \
    -e 'workers\.(table|pending|order|seenIds|exited)[[:space:]]*=[^=]' \
    -e 'workers\.(order|exited)\.length[[:space:]]*=[^=]' \
    -e 'workers\.ignored[[:space:]]*(\+|[+-]?=([^=]|$))' \
    -e 'workers\.droppedExited[[:space:]]*(\+|[+-]?=([^=]|$))' \
    -e 'server\.active[[:space:]]*(\+\+|--|[+-]?=([^=]|$))' \
    -e 'server\.closing[[:space:]]*=([^=]|$)' \
    || true
  # Registry replacement: the sole legit spelling is the Session
  # construction (`this.workers = new WorkerRegistry()`), filtered here
  # so a wholesale swap anywhere else still trips.
  clean_of "$1" | grep -nE -e 'this\.workers[[:space:]]*=[^=]' \
    | grep -v 'new WorkerRegistry()' || true
}

if [ "${1:-}" = "--self-test" ]; then
  TSELF=$(mktemp -d)
  trap 'rm -rf "$TSELF"' EXIT
  cat > "$TSELF/comment_only.js" <<'EOF'
// Comment-only fixture: names every forbidden spelling, violates nothing.
/* workers.table.set(x) workers.table.delete(x) workers.table.clear()
   workers.pending.set(x) workers.pending.delete(x) workers.pending.clear()
   workers.order.push(x) workers.seenIds.add(x) workers.seenIds.delete(x)
   workers.seenIds.clear() workers.exited.push(x)
   workers.table = {} workers.pending = {} workers.order = []
   workers.seenIds = {} workers.exited = []
   workers.order.length = 0 workers.exited.length = 0
   workers.ignored = 1 workers.droppedExited = 1
   server.active++ server.closing = true this.workers = null */
// Fake owner body: real violations, but inside a stripped class.
class WorkerRegistry {
  track(w) { this.table.set(w.id, w); workers.table.set(w.id, w); }
}
class Session {
  ok() { return this.workers.get('a'); }
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
    this.workers.table.set('a', {});
    this.workers.table.delete('a');
    this.workers.table.clear();
    this.workers.pending.set('k', {});
    this.workers.pending.delete('k');
    this.workers.pending.clear();
    this.workers.order.push('a');
    this.workers.seenIds.add('a');
    this.workers.seenIds.delete('a');
    this.workers.seenIds.clear();
    this.workers.exited.push({});
    this.workers.table = new Map();
    this.workers.pending = new Map();
    this.workers.order = [];
    this.workers.seenIds = new Set();
    this.workers.exited = [];
    this.workers.order.length = 0;
    this.workers.exited.length = 0;
    this.workers.ignored += 1;
    this.workers.droppedExited = 0;
    this.server.active += 1;
    this.server.closing = true;
    this.workers = null;
  }
}
EOF
  # One violation line per planted line (23): fewer means a pattern missed.
  N=$(scan_file "$TSELF/planted.js" | wc -l | tr -d ' ')
  if [ "$N" != "23" ]; then
    echo "SELF-TEST FAIL: planted fixture yielded $N lines (want 23):" >&2
    scan_file "$TSELF/planted.js" >&2
    exit 1
  fi
  # Stripping control on the real tree: owner internals gone, Session
  # construction intact (proves the cleaner is neither blind nor vacuous).
  if [ "$(clean_of "$TARGET" | grep -c 'this\.table\.set(' || true)" != "0" ]; then
    echo "SELF-TEST FAIL: owner-class stripping lost (this.table.set visible)" >&2
    exit 1
  fi
  if [ "$(clean_of "$TARGET" | grep -c 'this\.workers = new WorkerRegistry()' || true)" != "1" ]; then
    echo "SELF-TEST FAIL: Session construction missing from cleaned tree" >&2
    exit 1
  fi
  if [ -n "$(scan_file "$TARGET")" ]; then
    echo "SELF-TEST FAIL: real tree flagged:" >&2
    scan_file "$TARGET" >&2
    exit 1
  fi
  echo "ok: check_nodebridge_owners self-test (comments pass, planted caught, stripping verified)"
  exit 0
fi

VIOLATIONS=$(scan_file "$TARGET")

if [ -n "$VIOLATIONS" ]; then
  echo "FAIL: direct owner-state writes outside owner classes:" >&2
  echo "$VIOLATIONS" | sed "s#^#bridge/node/src/nodebridge.js:#" >&2
  exit 1
fi
echo "ok: nodebridge owner routing (no direct writes outside owners)"
