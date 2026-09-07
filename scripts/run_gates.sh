#!/bin/sh
# Canonical gate runner (M2 test architecture, no new framework).
#
# Invokes EXACTLY the docs/architecture-map.md section 7 sequence with the
# native commands below (fail fast). Safe to call from any directory:
# the script resolves the repository root itself; the caller need not cd.
#
# Usage:
#   scripts/run_gates.sh [--unit|--live]
#     (default)  full release-like gate: unit + live
#     --unit     fast gates only (no daemons, no browsers): cargo test +
#                cargo fmt + every tests/test_*.py except the three live
#                suites (auto-discovered, sorted — new unit files need no
#                runner edit) + node --test + javac bridge/checks
#     --live     daemon-backed live suites only (needs cargo build first;
#                the script builds when the binary is missing)
#
# Filters (live suites only, honored via tests/_live_home.py load_tests):
#   TEST_LANG=py|node|java|browser[,..]  run only matching live tests
#                                        (cross-cutting tests always run)
#   SKIP_BROWSER=1                        drop live chrome tests
# JS unit tests always run in full (stubbed transports, no chrome needed).
#
# Summaries print per-section elapsed time; test counts come from each
# harness's own output. No secrets or log dumps on failure: rerun the
# named file directly to inspect.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

mode="full"
if [ "${1:-}" = "--unit" ]; then mode="unit"; fi
if [ "${1:-}" = "--live" ]; then mode="live"; fi
if [ "${1:-}" != "" ] && [ "$mode" = "full" ]; then
  echo "usage: scripts/run_gates.sh [--unit|--live]" >&2
  exit 2
fi

TOTAL_START=$(date +%s)

section() {
  echo "### gate: $1"
  SEC_START=$(date +%s)
}

passed() {
  SEC_END=$(date +%s)
  echo "ok: $1 ($((SEC_END - SEC_START))s)"
}

fail() {
  echo "FAIL: $1 (rerun the native command above to inspect)" >&2
  exit 1
}

run_unit() {
  section "cargo test"
  cargo test || fail "cargo test"
  passed "cargo test"

  section "cargo fmt --check"
  cargo fmt --check || fail "cargo fmt --check"
  passed "cargo fmt --check"

  section "python unit (auto-discovered tests/test_*.py minus live)"
  # Deterministic sorted list: every tests/test_*.py is a unit gate
  # EXCEPT the three daemon-backed live suites (run by run_live) and any
  # helper module. New unit files are picked up with no runner edit.
  for t in $(printf '%s\n' tests/test_*.py | LC_ALL=C sort); do
    case "$t" in
      tests/test_live.py|tests/test_m5_live.py|tests/test_ux_live.py|tests/_live_home.py)
        continue;;
    esac
    python3 "$t" || fail "$t"
  done
  passed "python unit"

  section "node --test tests/*.test.js"
  node --test tests/*.test.js || fail "node --test"
  passed "node --test"

  section "javac bridge + checks"
  JTMP=$(mktemp -d)
  trap 'rm -rf "$JTMP"' EXIT
  javac -d "$JTMP/classes" bridge/java/src/*.java || fail "javac bridge"
  javac -cp "$JTMP/classes" -d "$JTMP/checks" \
    tests/BJavaCheck.java tests/CJavaCheck.java \
    tests/M4JavaCheck.java tests/M5JavaCheck.java \
    tests/M6JavaCheck.java tests/M7JavaCheck.java || fail "javac checks"
  rm -rf "$JTMP"
  trap - EXIT
  passed "javac"
}

run_live() {
  if [ ! -x "target/debug/agent-debugger" ]; then
    section "cargo build"
    cargo build || fail "cargo build"
    passed "cargo build"
  fi
  section "live (TEST_LANG=${TEST_LANG:-all} SKIP_BROWSER=${SKIP_BROWSER:-0})"
  python3 tests/test_live.py || fail "tests/test_live.py"
  passed "tests/test_live.py"
  python3 tests/test_m5_live.py || fail "tests/test_m5_live.py"
  passed "tests/test_m5_live.py"
  python3 tests/test_ux_live.py || fail "tests/test_ux_live.py"
  passed "tests/test_ux_live.py"
}

if [ "$mode" = "unit" ] || [ "$mode" = "full" ]; then run_unit; fi
if [ "$mode" = "live" ] || [ "$mode" = "full" ]; then run_live; fi

TOTAL_END=$(date +%s)
echo "ALL GATES PASSED ($((TOTAL_END - TOTAL_START))s, mode=$mode)"
