#!/bin/sh
# Canonical M5.2 Java owner gate: st-parameterized helpers moved from
# BridgeSession live in exactly one new owner (no dual definitions, no
# compat aliases), moved bodies acquire no lock / spawn no thread /
# perform no socket IO, retained orchestration stays in BridgeSession,
# and the BridgeCli-blocked setup group stays put.
#
# Callgraph evidence (reviewed per method, not re-derived here):
# - Track group (BridgeSnapshot): all six trackChanges production sites
#   run inside the awaitStopInner Phase-B synchronized (st.sessionLock)
#   block; all three changeFieldsJson sites inside dispatchInner
#   synchronized sections (incl. via waitJson); storeTrack/compareTrack/
#   degradeTrack/trackWarn/jsonTotal/jsonStrings production calls never
#   escape the group. M7JavaCheck hammers compareTrack/changeFieldsJson
#   under sessionLock from two threads.
# - Text group (BridgeSnapshot, read-only): timeoutText/withCaptureStage/
#   captureExitContextJson/waitContextJson read st/cfg fields and format
#   text only — same call sites/threads as before, no sync change.
# - Proto group (BridgeProto): pure builders; busyError reads
#   st.outstanding with its sole production caller under sessionLock.
# - Eval pair (BridgeEval): frameIdentity pure, frameIdentityOf JDI-read.
# - REJECTED (kept in BridgeSession, asserted below): the setup-phase
#   text group — setupErrorJson has a production caller outside
#   BridgeSession (BridgeCli.java), so the atomic-group rule keeps
#   setupErrorText/setupPhaseOf/phaseOfError/setupErrorJson together.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SRC="$ROOT/bridge/java/src"
FAIL=0

fail() {
  echo "FAIL: $1" >&2
  FAIL=1
}

# Gated symbol lists (single source for sections 4/5 and the self-test).
RETAINED="buildTargetIdentity buildTargetIdentityInner seedHint serveLoop \
  dispatch dispatchInner awaitStopInner parkedRecheck plantPending \
  armWatch setMethods handleOne closeFromConn cleanup"
SETUP="setupErrorText setupPhaseOf phaseOfError setupErrorJson"

# Comment-stripped, newline-joined code of one file (stdout). Declarations
# stay detectable, comments can never satisfy, and multiline signatures
# still match (no newline survives between `static` and the name).
code_of() {
  sed -e 's#://#\x01#g' "$1" | awk '
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
      sub(/\/\/.*$/, "", line)
      gsub(/\x01/, "://", line)
      print line
    }' | tr '\n' ' '
}

# True iff $1 contains an actual `static ... $2(` declaration. The
# `[^{};]*` gap cannot span statements, so call sites (`return
# dispatchInner(...)`, `cfg.seedHint`) and similarly-named symbols
# (`dispatchInner` vs `dispatch`, `cleanupVm` vs `cleanup`) never match —
# only the declaration itself does.
has_static_decl() {
  printf '%s\n' "$(code_of "$1")" | grep -Eq "static[^{};]*[^A-Za-z0-9_]$2[ ]*\("
}

# Self-test (negative control): `--self-test` builds a comment-only
# fixture naming every gated symbol — including fake `static ... name(`
# lines — and requires the declaration check to REJECT each one, then
# requires the real tree to ACCEPT each one.
if [ "${1:-}" = "--self-test" ]; then
  TSELF=$(mktemp -d)
  trap 'rm -rf "$TSELF"' EXIT
  {
    echo '// comment-only fixture: names every gated symbol, declares nothing'
    # shellcheck disable=SC2086
    for m in $RETAINED $SETUP; do
      echo "// static void $m() { /* fake declaration */ }"
      echo "// see $m for details"
    done
  } > "$TSELF/BridgeSession.java"
  # shellcheck disable=SC2086
  for m in $RETAINED $SETUP; do
    if has_static_decl "$TSELF/BridgeSession.java" "$m"; then
      echo "SELF-TEST FAIL: comment-only fixture satisfied check for $m" >&2
      exit 1
    fi
  done
  # shellcheck disable=SC2086
  for m in $RETAINED $SETUP; do
    if ! has_static_decl "$SRC/BridgeSession.java" "$m"; then
      echo "SELF-TEST FAIL: real tree missing declaration for $m" >&2
      exit 1
    fi
  done
  echo "ok: check_java_owners self-test (comments cannot satisfy, real decls detected)"
  exit 0
fi

# 1. Moved methods defined in the new owner (exactly once each).
check_defined() {
  file="$1"; shift
  for m in "$@"; do
    n=$(grep -c "static .* $m(" "$SRC/$file" || true)
    if [ "$n" != "1" ]; then
      fail "$file defines $m $n times (want exactly once)"
    fi
  done
}

check_defined BridgeSnapshot.java \
  trackChanges storeTrack compareTrack degradeTrack changeFieldsJson \
  trackWarn jsonTotal jsonStrings \
  timeoutText withCaptureStage captureExitContextJson waitContextJson

check_defined BridgeProto.java \
  truncField jsonLong jsonString jsonStringArray unavailableEntry \
  busyError overloadedJson

check_defined BridgeEval.java frameIdentity frameIdentityOf

# IDENT_FIELD_CAP moved with truncField (IDENT_TOTAL_CAP stays with the
# retained identity builder in BridgeSession).
if [ "$(grep -c "static final int IDENT_FIELD_CAP" "$SRC/BridgeProto.java")" != "1" ]; then
  fail "BridgeProto.java must declare IDENT_FIELD_CAP exactly once"
fi

# 2. No dual definitions / compat aliases left in BridgeSession.
for m in trackChanges storeTrack compareTrack degradeTrack changeFieldsJson \
    trackWarn jsonTotal jsonStrings \
    timeoutText withCaptureStage captureExitContextJson waitContextJson \
    truncField jsonLong jsonString jsonStringArray unavailableEntry \
    busyError overloadedJson frameIdentity frameIdentityOf; do
  if grep -q "static .* $m(" "$SRC/BridgeSession.java"; then
    fail "BridgeSession.java still defines $m (dual definition/alias)"
  fi
done
if grep -q "IDENT_FIELD_CAP" "$SRC/BridgeSession.java"; then
  fail "BridgeSession.java still mentions IDENT_FIELD_CAP"
fi

# 3. Moved M5.2 sections acquire no lock, spawn no thread, touch no socket.
#    (Scoped to the M5.2 marker ranges so pre-existing owners like
#    StreamGobbler / invokeCall workers outside the ranges never match;
#    comments stripped first so the lock-contract docs themselves — which
#    name the forbidden spellings — can never false-positive, same idiom
#    as the node/browser owner gates.)
for f in BridgeSnapshot.java BridgeProto.java BridgeEval.java; do
  seg=$(sed -n '/M5.2/,/end M5.2/p' "$SRC/$f" \
    | sed -e 's#://#\x01#g' \
    | awk '
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
        sub(/\/\/.*$/, "", line)
        gsub(/\x01/, "://", line)
        print line
      }')
  for tok in 'synchronized' 'new Thread' 'Executors' 'ServerSocket' '[^a-zA-Z]Socket' \
      'getOutputStream' 'getInputStream' '\.wait(' '\.notify'; do
    if printf '%s\n' "$seg" | grep -q -- "$tok"; then
      fail "$f M5.2 section contains forbidden token: $tok"
    fi
  done
done

# 4. Retained orchestration stays in BridgeSession as real declarations
#    (M5.2 keeps the single sessionLock owner + sole event consumer +
#    ownership-changing paths). Comment-stripped declaration match: prose
#    or commented-out code mentioning these names cannot satisfy.
# shellcheck disable=SC2086
for m in $RETAINED; do
  if ! has_static_decl "$SRC/BridgeSession.java" "$m"; then
    fail "BridgeSession.java lost retained declaration: $m"
  fi
done
if ! grep -q "sessionLock" "$ROOT/bridge/java/src/BridgeModel.java"; then
  fail "SessionState lost sessionLock"
fi

# 5. Rejected setup group stays together in BridgeSession as real
#    declarations (BridgeCli calls setupErrorJson from outside, blocking
#    the atomic-group move). Same comment-stripped match as section 4.
# shellcheck disable=SC2086
for m in $SETUP; do
  if ! has_static_decl "$SRC/BridgeSession.java" "$m"; then
    fail "BridgeSession.java lost rejected setup-group member: $m"
  fi
done

if [ "$FAIL" != "0" ]; then
  exit 1
fi
echo "ok: java owner routing (M5.2 moves single-homed, sections lock/thread/socket-free)"
