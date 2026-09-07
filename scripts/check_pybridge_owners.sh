#!/bin/sh
# Canonical M3 owner-routing gate: production code in
# bridge/py/src/pybridge.py must mutate owner state only through owner
# methods — never direct writes through the Session same-object read-view
# aliases (`self.targets[...] =` / dict mutation, `target_order`
# append/remove/clear, `_seen_ids` / `exited_targets` / `_attach_pending`
# container mutation, wholesale alias reassignment), never direct
# `targets_reg.serving =` (use set_serving/reset_serving), never direct
# `server_state.active/closing =` (use try_admit/release/claim_close/
# mark_closing), and never a wholesale `targets_reg`/`server_state`
# replacement. Reads (targets.get/[...]/`in`, order/history iteration,
# serving/counter comparisons) stay allowed.
#
# Robustness: TargetRegistry/ServerState class bodies (indent-aware strip:
# a class runs until the next non-indented line, so owner internals like
# `self.table.pop` / `self.serving =` can never false-positive), plus all
# `#` comments and `"""`/`'''` docstrings, are removed before matching —
# so the routing-rule prose itself (which names the forbidden spellings)
# can never false-positive either. `=` matches use the `[^=!<>=]*[+-]?=`
# idiom so `==`/`!=` comparisons never trip.
#
# `--self-test` (negative + positive control, no production effect):
# a comment/docstring-only fixture naming every forbidden spelling (plus
# a fake owner-class body with real violations, proving class stripping)
# must scan clean, while a planted fixture with one real violation per
# line must be fully detected; the real tree must scan clean with its
# owner bodies verifiably stripped.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TARGET="$ROOT/bridge/py/src/pybridge.py"

# Comment/docstring-stripped, owner-class-free code of one file (stdout).
# Class stripping is load-bearing (owner internals name the same
# spellings); the self-test proves it on the real tree below.
clean_of() {
  awk '
    function strip_quoted(line, q,   i, rest, j) {
      while ((i = index(line, q)) > 0) {
        rest = substr(line, i + 3)
        if ((j = index(rest, q)) > 0) {
          line = substr(line, 1, i - 1) substr(rest, j + 3)
        } else {
          return substr(line, 1, i - 1) "\001OPEN"
        }
      }
      return line
    }
    /^class (TargetRegistry|ServerState)([^A-Za-z0-9_]|$)/ { in_class = 1; next }
    in_class && /^[^[:space:]]/ { in_class = 0 }
    in_class { next }
    in_doc != "" && (i = index($0, in_doc)) > 0 {
      $0 = substr($0, i + 3)
      in_doc = ""
    }
    in_doc != "" { next }
    {
      line = strip_quoted($0, "\"\"\"")
      if (line ~ /\001OPEN$/) { sub(/\001OPEN$/, "", line); in_doc = "\"\"\"" }
      else {
        line = strip_quoted(line, "\x27\x27\x27")
        if (line ~ /\001OPEN$/) { sub(/\001OPEN$/, "", line); in_doc = "\x27\x27\x27" }
      }
      sub(/#.*$/, "", line)
      print line
    }' "$1"
}

# Violation lines (`lineno:content`) of one file (stdout, empty = clean).
scan_file() {
  clean_of "$1" | grep -nE \
    -e 'self\.targets\[[^]]*\][^=!<>=]*[+-]?=[^=]' \
    -e 'self\.targets\.(pop|clear|update|setdefault)[[:space:]]*\(' \
    -e 'del self\.targets(\[|$)' \
    -e 'self\.target_order\.(append|remove|clear|pop|extend|insert|sort|reverse)[[:space:]]*\(' \
    -e 'self\.target_order\[[^]]*\][^=!<>=]*[+-]?=[^=]' \
    -e 'del self\.target_order(\[|$)' \
    -e 'self\._seen_ids\.(add|discard|remove|clear|pop|update)[[:space:]]*\(' \
    -e 'self\.exited_targets\.(append|clear|pop|extend|insert|remove)[[:space:]]*\(' \
    -e 'self\.exited_targets\[[^]]*\][^=!<>=]*=[^=]' \
    -e 'del self\.exited_targets(\[|$)' \
    -e 'self\._attach_pending\.(append|pop|clear|extend|insert|remove)[[:space:]]*\(' \
    -e 'del self\._attach_pending(\[|$)' \
    -e 'self\.targets_reg\.serving[^=!<>=]*[+-]?=[^=]' \
    -e 'self\.server_state\.(active|closing)[^=!<>=]*[+-]?=[^=]' \
    || true
  # Wholesale replacement: the sole legit spellings are the Session
  # constructions (`self.targets = self.targets_reg.table` and friends,
  # `= TargetRegistry()` / `= ServerState()`), filtered here so a
  # wholesale swap anywhere else still trips. Name-boundary `[[:space:]]*`
  # (not `.*`): `targets_reg.serving =` must not read as a `targets`
  # wholesale, nor `server_state.active =` as a `server_state` one.
  clean_of "$1" | grep -nE \
    -e 'self\.(targets|target_order|_seen_ids|exited_targets|_attach_pending)[[:space:]]*[+-]?=[^=]' \
    -e 'self\.(targets_reg|server_state)[[:space:]]*[+-]?=[^=]' \
    | grep -v -e '= self\.targets_reg\.' -e '= TargetRegistry()' -e '= ServerState()' || true
}

if [ "${1:-}" = "--self-test" ]; then
  TSELF=$(mktemp -d)
  trap 'rm -rf "$TSELF"' EXIT
  cat > "$TSELF/comment_only.py" <<'EOF'
# Comment-only fixture: names every forbidden spelling, violates nothing.
# self.targets["a"] = 1  self.targets.pop("a")  self.targets.clear()
# self.targets.update({})  del self.targets["a"]
# self.target_order.append("a")  self.target_order.remove("a")
# self.target_order.clear()  self.target_order[0] = "a"
# del self.target_order[0]
# self._seen_ids.add("a")  self._seen_ids.clear()
# self.exited_targets.append({})  self.exited_targets[0] = {}
# del self.exited_targets[0]
# self._attach_pending.append(())  self._attach_pending.pop(0)
# del self._attach_pending[0]
# self.targets = {}  self.target_order = []
# self.targets_reg.serving = "x"  self.server_state.active = 1
# self.server_state.closing = True
# self.targets_reg = None  self.server_state = None
"""Docstring naming violations:
self.targets["a"] = 1
self.target_order.append("a")
self.targets_reg.serving = "x"
"""
class TargetRegistry:
    """Fake owner body with real violations (must be stripped)."""
    def evil(self):
        self.targets["a"] = 1
        self.targets_reg.serving = "x"
        self.server_state.active = 1
class Session:
    """Real code: reads only."""
    def get(self, tid):
        return self.targets.get(tid)
EOF
  if [ -n "$(scan_file "$TSELF/comment_only.py")" ]; then
    echo "SELF-TEST FAIL: comment-only fixture flagged:" >&2
    scan_file "$TSELF/comment_only.py" >&2
    exit 1
  fi
  cat > "$TSELF/planted.py" <<'EOF'
class Evil:
    def evil(self):
        self.targets["a"] = 1
        self.targets.pop("a")
        self.targets.clear()
        self.targets.update({})
        del self.targets["a"]
        self.target_order.append("a")
        self.target_order.remove("a")
        self.target_order.clear()
        self.target_order[0] = "a"
        del self.target_order[0]
        self._seen_ids.add("a")
        self._seen_ids.discard("a")
        self._seen_ids.clear()
        self.exited_targets.append({})
        self.exited_targets[0] = {}
        del self.exited_targets[0]
        self._attach_pending.append(())
        self._attach_pending.pop(0)
        del self._attach_pending[0]
        self.targets = {}
        self.target_order = []
        self._seen_ids = set()
        self.exited_targets = []
        self._attach_pending = []
        self.targets_reg.serving = "x"
        self.server_state.active = 1
        self.server_state.closing = True
        self.targets_reg = None
        self.server_state = None
EOF
  # One violation line per planted line (29): fewer means a pattern missed.
  N=$(scan_file "$TSELF/planted.py" | wc -l | tr -d ' ')
  if [ "$N" != "29" ]; then
    echo "SELF-TEST FAIL: planted fixture yielded $N lines (want 29):" >&2
    scan_file "$TSELF/planted.py" >&2
    exit 1
  fi
  # Stripping control on the real tree: owner internals gone, Session
  # construction intact (proves the cleaner is neither blind nor vacuous).
  if [ "$(clean_of "$TARGET" | grep -c 'self\.table\.pop(' || true)" != "0" ]; then
    echo "SELF-TEST FAIL: owner-class stripping lost (self.table.pop visible)" >&2
    exit 1
  fi
  if [ "$(clean_of "$TARGET" | grep -c 'self\.targets_reg = TargetRegistry()' || true)" != "1" ]; then
    echo "SELF-TEST FAIL: Session construction missing from cleaned tree" >&2
    exit 1
  fi
  if [ -n "$(scan_file "$TARGET")" ]; then
    echo "SELF-TEST FAIL: real tree flagged:" >&2
    scan_file "$TARGET" >&2
    exit 1
  fi
  echo "ok: check_pybridge_owners self-test (comments pass, planted caught, stripping verified)"
  exit 0
fi

VIOLATIONS=$(scan_file "$TARGET")

if [ -n "$VIOLATIONS" ]; then
  echo "FAIL: direct owner-state writes outside owner classes:" >&2
  echo "$VIOLATIONS" | sed "s#^#bridge/py/src/pybridge.py:#" >&2
  exit 1
fi
echo "ok: pybridge owner routing (no direct writes outside owners)"
