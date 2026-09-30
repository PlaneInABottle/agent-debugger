// Unit-test guard: trap process.exit inside test files.
//
// A bridge calling process.exit() (e.g. the abandonment guard when
// owner.json is missing) kills the whole test runner process with code 0:
// the file then "passes" vacuously with every subtest result lost. This
// guard turns that silent green into a loud failure naming the trap.
//
// Loaded via `node --test --import ./tests/guard-exit.js`. Unconditional
// is safe: the runner itself shuts down via exitCode, never process.exit
// (verified: a trivial suite exits 0 with the guard active, while an
// explicit process.exit(0) in a test body fails loudly).
process.exit = function trappedExit(code) {
  throw new Error(
    `process.exit(${code}) trapped in unit test: a bridge exited the runner ` +
      `process instead of throwing (test helper missing owner.json?)`
  );
};
