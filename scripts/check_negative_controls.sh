#!/bin/sh
# check_negative_controls.sh — prove each key regression test actually
# catches its bug: temporarily revert the fix, require the test to FAIL,
# then restore byte-identically.
#
# A test that passes with the fix reverted is vacuous (it guards
# nothing); this script fails loudly in that case. Manual gate (takes
# ~1-2 min for the watchdog-bound cases) — run before releases, not on
# every push. Every revert asserts its anchor text first, so a drifted
# codebase errors instead of silently testing nothing.
#
# Quoting: revert specs below use \n escapes (backslash-n, decoded by
# python, never literal newlines) so `python3 -c` stays single-line.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

PASS=0
FAIL=0

# expect_fail <label> <file> <anchor> <replacement> <test-cmd...>
expect_fail() {
  label="$1"; file="$2"; anchor="$3"; repl="$4"; shift 4
  bak="$(mktemp)"
  cp "$file" "$bak"
  sum_orig="$(sha256sum "$bak" | cut -d' ' -f1)"
  if ! python3 -c "
import sys
p = sys.argv[1]
s = open(p).read()
a = sys.argv[2].encode().decode('unicode_escape')
r = sys.argv[3].encode().decode('unicode_escape')
assert a in s, 'anchor missing: ' + a[:60]
assert s.count(a) == 1, 'anchor not unique: ' + a[:60]
open(p, 'w').write(s.replace(a, r, 1))
" "$file" "$anchor" "$repl"; then
    echo "FAIL ($label): revert anchor missing or ambiguous (code drifted?)" >&2
    cp "$bak" "$file"; rm -f "$bak"
    FAIL=$((FAIL + 1))
    return
  fi
  if "$@" >/dev/null 2>&1; then
    echo "FAIL ($label): test PASSED with the fix reverted (vacuous?)" >&2
    cp "$bak" "$file"; rm -f "$bak"
    FAIL=$((FAIL + 1))
    return
  fi
  cp "$bak" "$file"; rm -f "$bak"
  sum_restored="$(sha256sum "$file" | cut -d' ' -f1)"
  if [ "$sum_orig" != "$sum_restored" ]; then
    echo "FAIL ($label): restore mismatch" >&2
    FAIL=$((FAIL + 1))
    return
  fi
  echo "ok ($label): reverted code fails, file restored byte-identically"
  PASS=$((PASS + 1))
}

expect_fail "node mono deadline" bridge/node/src/nodebridge.js \
  "function monoNow() {\n  return Number(process.hrtime.bigint() / 1000000n);\n}" \
  "function monoNow() {\n  return Date.now();\n}" \
  node --test --test-name-pattern="pump timeout" tests/mono_clock.test.js

expect_fail "browser mono deadline" bridge/browser/src/browserbridge.js \
  "function monoNow() {\n  return Number(process.hrtime.bigint() / 1000000n);\n}" \
  "function monoNow() {\n  return Date.now();\n}" \
  node --test --test-name-pattern="pump timeout" tests/mono_clock.test.js

expect_fail "node graceful close" bridge/node/src/nodebridge.js \
  "if (!graceful) conn.destroy();" \
  "conn.destroy();" \
  node --test --test-name-pattern="normal close" tests/review2_fixes.test.js

expect_fail "py park-clock commit" bridge/py/src/pybridge.py \
  "        self._commit_park_clock(eff)\n        if not self.frames:" \
  "        # TEMP-REVERT\n        if not self.frames:" \
  python3 tests/test_pump_handoff.py PumpHandoffTests.test_stack_unavailable_still_publishes_park_epoch

expect_fail "py deadline recheck" bridge/py/src/pybridge.py \
  "                r = self._shared_park_hit(start_seq, want)\n                if r:\n                    return r\n                raise StopTimeout(self.timeout_text(timeout))" \
  "                raise StopTimeout(self.timeout_text(timeout))  # TEMP-REVERT" \
  python3 tests/test_pump_handoff.py PumpHandoffTests.test_deadline_rechecks_shared_handoff_before_timeout

expect_fail "run_live heal" tests/run_live.py \
  "    passed = set(retry_result.passed_ids)" \
  "    passed = set(retry_ids) - still_bad - skipped_on_retry  # TEMP-REVERT" \
  python3 tests/test_run_live_retry.py RetryMergeTests.test_class_skip_on_retry_keeps_failure

expect_fail "postinstall fail-closed" scripts/postinstall.js \
  '    throw new Error(`Checksum mismatch for ${path.basename(archivePath)} (published checksum unparseable; refusing unverified archive)`);' \
  '    return; // TEMP-REVERT' \
  node --test --test-name-pattern="unparseable" tests/install_checksum.test.js

# Same as expect_fail, but the reverted test is expected to HANG (e.g. an
# unbounded accumulation): the command is killed after <secs>s, and the
# kill (nonzero) counts as the expected failure.
expect_fail_timeout() {
  secs="$1"; label="$2"; file="$3"; anchor="$4"; repl="$5"; shift 5
  bak="$(mktemp)"
  cp "$file" "$bak"
  sum_orig="$(sha256sum "$bak" | cut -d' ' -f1)"
  if ! python3 -c "
import sys
p = sys.argv[1]
s = open(p).read()
a = sys.argv[2].encode().decode('unicode_escape')
r = sys.argv[3].encode().decode('unicode_escape')
assert a in s, 'anchor missing: ' + a[:60]
assert s.count(a) == 1, 'anchor not unique: ' + a[:60]
open(p, 'w').write(s.replace(a, r, 1))
" "$file" "$anchor" "$repl"; then
    echo "FAIL ($label): revert anchor missing or ambiguous (code drifted?)" >&2
    cp "$bak" "$file"; rm -f "$bak"
    FAIL=$((FAIL + 1))
    return
  fi
  if python3 -c "
import subprocess, sys
p = subprocess.run(sys.argv[1:], capture_output=True, timeout=$secs)
sys.exit(p.returncode)
" "$@" >/dev/null 2>&1; then
    echo "FAIL ($label): test PASSED with the fix reverted (vacuous?)" >&2
    cp "$bak" "$file"; rm -f "$bak"
    FAIL=$((FAIL + 1))
    return
  fi
  cp "$bak" "$file"; rm -f "$bak"
  sum_restored="$(sha256sum "$file" | cut -d' ' -f1)"
  if [ "$sum_orig" != "$sum_restored" ]; then
    echo "FAIL ($label): restore mismatch" >&2
    FAIL=$((FAIL + 1))
    return
  fi
  echo "ok ($label): reverted code hangs/fails, file restored byte-identically"
  PASS=$((PASS + 1))
}

expect_fail_timeout 25 "node target-list cap" bridge/node/src/nodebridge.js \
  "            reject(new Error('target list too large'));" \
  "            resolve('[');" \
  node --test --test-name-pattern="discoverAttach rejects" tests/node_target_list.test.js

expect_fail_timeout 25 "node fetch-list cap" bridge/node/src/nodebridge.js \
  "          if (raw.length + d.length > 1024 * 1024) {\n            tooBig = true;\n            req.destroy();\n            resolve(null);" \
  "          if (false) { // TEMP-REVERT\n            tooBig = true;\n            req.destroy();\n            resolve(null);" \
  node --test --test-name-pattern="fetchTargetList degrades" tests/node_target_list.test.js

expect_fail_timeout 25 "browser target-list cap" bridge/browser/src/browserbridge.js \
  "          reject(new Error('target list too large'));" \
  "          resolve('[');" \
  node --test --test-name-pattern="unbounded body" tests/browser_target_list.test.js

expect_fail_timeout 25 "postinstall archive cap" scripts/postinstall.js \
  "      res.on('data', (chunk) => {\n        if (tooBig) return;\n        written += chunk.length;\n        if (written > maxBytes) {" \
  "      res.on('data', (chunk) => {\n        if (tooBig) return;\n        written += chunk.length;\n        if (false) { // TEMP-REVERT" \
  node --test --test-name-pattern="unbounded body" tests/postinstall_bounds.test.js

expect_fail_timeout 25 "postinstall sidecar cap" scripts/postinstall.js \
  "      res.on('data', (chunk) => {\n        if (tooBig) return;\n        if (body.length + chunk.length > maxChars) {" \
  "      res.on('data', (chunk) => {\n        if (tooBig) return;\n        if (false) { // TEMP-REVERT" \
  node --test --test-name-pattern="unbounded sidecar" tests/postinstall_bounds.test.js

expect_fail "postinstall tar-slip guard" scripts/postinstall.js \
  "execSync(\`tar -xzf \"\${archivePath}\" -C \"\${destDir}\" agent-debugger\`" \
  "execSync(\`tar -xzf \"\${archivePath}\" -C \"\${destDir}\"\`" \
  node --test tests/postinstall_extract.test.js

expect_fail "node park-diag mono" bridge/node/src/nodebridge.js \
  "prev.atMono != null" \
  "false" \
  node --test --test-name-pattern="notePark elapsedMs" tests/pause_mono.test.js

expect_fail "node capture-pause mono" bridge/node/src/nodebridge.js \
  "const pauseMs = Math.max(0, Math.round(monoNow() - parkMono));" \
  "const pauseMs = Date.now() - parkMono; // TEMP-REVERT" \
  node --test --test-name-pattern="capture-exit pauseDurationMs" tests/pause_mono.test.js

expect_fail "browser park-diag mono" bridge/browser/src/browserbridge.js \
  "prev.atMono != null" \
  "false" \
  node --test --test-name-pattern="notePark elapsedMs" tests/pause_mono.test.js

expect_fail "browser capture-pause mono" bridge/browser/src/browserbridge.js \
  "const pauseMs = Math.max(0, Math.round(monoNow() - parkMono));" \
  "const pauseMs = Date.now() - parkMono; // TEMP-REVERT" \
  node --test --test-name-pattern="capture-exit pauseDurationMs" tests/pause_mono.test.js

expect_fail "py park-diag mono" bridge/py/src/pybridge.py \
  "elapsed = max(0, int((now_mono - prev[\"atMono\"]) * 1000))" \
  "elapsed = now_ms - prev[\"atMs\"] # TEMP-REVERT" \
  python3 tests/test_pause_mono.py PauseMonoTests.test_park_elapsed_monotonic

expect_fail "py capture-pause mono" bridge/py/src/pybridge.py \
  "pause_ms = max(0, int((mono() - park_mono) * 1000))" \
  "pause_ms = int(time.time() * 1000) - int(park_mono * 1000) # TEMP-REVERT" \
  python3 tests/test_pause_mono.py PauseMonoTests.test_capture_pause_monotonic

echo "negative controls: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
