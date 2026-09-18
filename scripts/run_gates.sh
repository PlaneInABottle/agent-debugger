#!/bin/sh
# Canonical gate runner (M2 test architecture, no new framework).
#
# Invokes the docs/architecture-map.md section 7 sequence plus the
# installer checksum gate, with the native commands below (fail fast).
# Safe to call from any directory:
# the script resolves the repository root itself; the caller need not cd.
#
# Usage:
#   scripts/run_gates.sh [--unit|--live]
#     (default)  full release-like gate: unit + live
#     --unit     fast gates only (no daemons, no browsers): cargo test +
#                cargo fmt + every tests/test_*.py except the three live
#                suites (auto-discovered, sorted — new unit files need no
#                runner edit) + node --test + javac bridge/checks + executed
#                java checks (B/C/M4-M7, fail fast with their own outputs)
#     --live     daemon-backed live suites only (needs cargo build first;
#                the script builds when the binary is missing)
#
# Filters (live suites only, honored via tests/_live_home.py load_tests):
#   TEST_LANG=py|node|java|browser[,..]  run only matching live tests
#                                        (cross-cutting tests always run)
#   SKIP_BROWSER=1                        drop live chrome tests
# JS unit tests always run in full (stubbed transports, no chrome needed).
#
# Live nonzero policy (tests/_live_home.py check_live_nonzero, enforced by
# tests/run_live.py): every required language (default full = py+node+java
# given the doctor prerequisites, plus browser unless SKIP_BROWSER=1; or
# every explicit TEST_LANG entry) must execute at least one test, and the
# scope must execute at least one overall. An all-skipped scope (e.g.
# missing adapter dependencies) fails instead of passing silently.
# Isolated single-test skips (e.g. one browser test without chrome) still
# pass while another test of that language executes.
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
if [ "$#" -gt 1 ]; then
  echo "usage: scripts/run_gates.sh [--unit|--live]" >&2
  exit 2
fi
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
  # Direct glob (never $(...) word-splitting: whitespace in a future
  # filename must not split, and a no-match glob must not run literally).
  for t in tests/test_*.py; do
    [ -e "$t" ] || continue
    case "$t" in
      tests/test_live.py|tests/test_m5_live.py|tests/test_ux_live.py)
        continue;;
    esac
    python3 "$t" || fail "$t"
  done
  passed "python unit"

  section "node --test tests/*.test.js"
  node --test tests/*.test.js || fail "node --test"
  passed "node --test"

  section "installer checksum (sh)"
  sh tests/test_install_checksum.sh || fail "test_install_checksum.sh"
  passed "installer checksum"

  section "nodebridge owner routing"
  scripts/check_nodebridge_owners.sh || fail "check_nodebridge_owners"
  scripts/check_nodebridge_owners.sh --self-test || fail "check_nodebridge_owners --self-test"
  passed "nodebridge owner routing"

  section "browserbridge owner routing"
  scripts/check_browserbridge_owners.sh || fail "check_browserbridge_owners"
  scripts/check_browserbridge_owners.sh --self-test || fail "check_browserbridge_owners --self-test"
  passed "browserbridge owner routing"

  section "pybridge owner routing"
  scripts/check_pybridge_owners.sh || fail "check_pybridge_owners"
  scripts/check_pybridge_owners.sh --self-test || fail "check_pybridge_owners --self-test"
  passed "pybridge owner routing"

  section "java owner routing (M5.2)"
  scripts/check_java_owners.sh || fail "check_java_owners"
  scripts/check_java_owners.sh --self-test || fail "check_java_owners --self-test"
  passed "java owner routing"

  section "javac bridge + checks"
  JTMP=$(mktemp -d)
  trap 'rm -rf "$JTMP"' EXIT
  # Windows java.exe takes `;` separators (a `:` list is one bad path).
  # Native java/javac also need real Windows paths: Git Bash arg munging
  # converts `;`-lists inconsistently (javac wrote where java never reads
  # -> ClassNotFoundException), so convert once with cygpath (forward
  # slashes are valid for Java) and switch munging off for these
  # invocations only (per-command env: never leaks into later gates).
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
      CP_SEP=";"
      if command -v cygpath >/dev/null 2>&1; then
        JTMPW="$(cygpath -w "$JTMP" | tr '\\' '/')"
        NO_CONV="MSYS2_ARG_CONV_EXCL=*"
      else
        JTMPW="$JTMP"
        NO_CONV=""
      fi
      ;;
    *)
      CP_SEP=":"
      JTMPW="$JTMP"
      NO_CONV=""
      ;;
  esac
  if [ -n "$NO_CONV" ]; then
    JPFX="env $NO_CONV"
  else
    JPFX=""
  fi
  # shellcheck disable=SC2086 # intentional splitting: optional env prefix or nothing
  $JPFX javac -d "$JTMPW/classes" bridge/java/src/*.java || fail "javac bridge"
  # shellcheck disable=SC2086 # intentional splitting: optional env prefix or nothing
  $JPFX javac -cp "$JTMPW/classes" -d "$JTMPW/checks" \
    tests/BJavaCheck.java tests/CJavaCheck.java \
    tests/M4JavaCheck.java tests/M5JavaCheck.java \
    tests/M6JavaCheck.java tests/M7JavaCheck.java \
    tests/SaturationJavaCheck.java tests/FramingJavaCheck.java \
    tests/StrictJavaCheck.java || fail "javac checks"
  passed "javac"

  section "java checks (execute B/C/M4-M7 + saturation + framing + strict)"
  # Compiled checks are fail-fast by construction (each prints its own
  # ok-lines and System.exit(1) on any failure), so the gate just runs
  # every check class against the freshly compiled bridge above.
  for c in BJavaCheck CJavaCheck M4JavaCheck M5JavaCheck M6JavaCheck M7JavaCheck SaturationJavaCheck FramingJavaCheck StrictJavaCheck; do
    # shellcheck disable=SC2086 # intentional splitting: optional env prefix or nothing
    $JPFX java -cp "$JTMPW/classes${CP_SEP}$JTMPW/checks" "$c" || fail "java $c"
  done
  rm -rf "$JTMP"
  trap - EXIT
  passed "java checks"
}

run_live() {
  if [ ! -x "target/debug/agent-debugger" ]; then
    section "cargo build"
    cargo build || fail "cargo build"
    passed "cargo build"
  fi
  section "live (TEST_LANG=${TEST_LANG:-all} SKIP_BROWSER=${SKIP_BROWSER:-0})"
  python3 tests/run_live.py || fail "tests/run_live.py (test_live + test_m5_live + test_ux_live)"
  passed "live"
}

if [ "$mode" = "unit" ] || [ "$mode" = "full" ]; then run_unit; fi
if [ "$mode" = "live" ] || [ "$mode" = "full" ]; then run_live; fi

TOTAL_END=$(date +%s)
echo "ALL GATES PASSED ($((TOTAL_END - TOTAL_START))s, mode=$mode)"
