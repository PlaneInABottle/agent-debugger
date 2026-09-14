#!/bin/sh
# bench_debug_loop.sh — reproducible debugger-loop cost measurement.
#
# Seeds a one-line bug into temp copies of the bundled fixtures, then times
# the scripted diagnosis flow (start at the return line, read the snapshot,
# eval the suspect, continue, close). Prints wall time, CLI calls, target
# launches, and source edits (always zero here).
#
# The print-debugging half of the comparison lives in docs/benchmark.md as
# a documented ESTIMATE (an expert's minimal edit+run path for the same
# seeded bug) — this script only produces the MEASURED debugger half, so
# no number here is a guess.
#
# Usage: scripts/bench_debug_loop.sh
# Requires: cargo, python3, node (same as the unit gate). No network after
# first provisioning. Never touches the real ~/.agent-debugger sessions:
# scratch sessions are closed at the end of each run.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

if [ ! -x "target/debug/agent-debugger" ]; then
  echo "### building debug binary"
  cargo build
fi
BIN="target/debug/agent-debugger"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Seeded bug (identical shape both runtimes): discount/subtrahend applied
# twice. Symptom: printed total is too low. Diagnosis target: name the
# wrong variable and the line that corrupts it.
sed 's/total = subtotal - discount/total = subtotal - discount - discount/' \
  examples/py-demo/app.py > "$WORK/buggy_app.py"
sed 's/const sum = a + b;/const sum = a + b - b;/' \
  examples/node-demo/calc.js > "$WORK/buggy_calc.js"

calls=0
run() {
  "$BIN" "$@"
}

# Idempotent scratch sessions: an aborted earlier run leaks a parked
# session (close it first or start fails with "already exists").
run --session bench-py close > /dev/null 2>&1 || true
run --session bench-node close > /dev/null 2>&1 || true
calls=0

bench_py() {
  echo "### python: seeded double-discount bug"
  start_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
  out=$(run --session bench-py py start "$WORK/buggy_app.py" \
    --break "$WORK/buggy_app.py:26" --timeout 30)
  calls=$((calls + 1))
  printf "%s" "$out" | python3 -c "
import json,sys
d = json.load(sys.stdin)
assert d.get('ok'), 'start failed: ' + json.dumps(d)[:500]
d = d['data']
locs = {v['name']: v['value'] for v in d['frames'][0]['locals']}
print('snapshot locals:', locs)
assert 'discount' in locs and 'total' in locs, 'snapshot must show suspects'
sub = float(locs['subtotal']); disc = float(locs['discount']); tot = float(locs['total'])
assert abs(tot - (sub - 2*disc)) < 1e-9, 'double-discount visible in one snapshot'
print('diagnosis: total == subtotal - 2*discount -> line 26 applies discount twice')
"
  run --session bench-py eval "total" > /dev/null
  calls=$((calls + 1))
  run --session bench-py continue --timeout 10 > /dev/null 2>&1 || true
  calls=$((calls + 1))
  run --session bench-py close > /dev/null
  calls=$((calls + 1))
  end_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
  echo "py wall_ms=$((end_ms - start_ms))"
}

bench_node() {
  echo "### node: seeded double-subtract bug"
  start_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
  out=$(run --session bench-node node start "$WORK/buggy_calc.js" \
    --break "$WORK/buggy_calc.js:3" --timeout 30)
  calls=$((calls + 1))
  printf "%s" "$out" | python3 -c "
import json,sys
d = json.load(sys.stdin)
assert d.get('ok'), 'start failed: ' + json.dumps(d)[:500]
d = d['data']
locs = {v['name']: v['value'] for v in d['frames'][0]['locals']}
print('snapshot locals:', locs)
assert locs.get('a') == '40' and locs.get('b') == '2', 'snapshot must show inputs'
assert locs.get('sum') == '40', 'seeded bug visible: 40 + 2 - 2'
print('diagnosis: sum == a + b - b -> line 2 subtracts b twice')
"
  run --session bench-node eval "sum" > /dev/null
  calls=$((calls + 1))
  run --session bench-node continue --timeout 10 > /dev/null 2>&1 || true
  calls=$((calls + 1))
  run --session bench-node close > /dev/null
  calls=$((calls + 1))
  end_ms=$(python3 -c 'import time; print(int(time.time()*1000))')
  echo "node wall_ms=$((end_ms - start_ms))"
}

bench_py
bench_node
echo "cli_calls=$calls target_launches=2 source_edits=0"
