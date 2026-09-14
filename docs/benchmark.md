# Debug-loop benchmark: debugger vs print debugging

Question: how much does the snapshot-first loop actually save over
edit → run → read-prints? This doc records a reproducible measurement
for the debugger half and a documented estimate for the print half.
Labels matter: **MEASURED** ran here, **ESTIMATE** is reasoned and marked.

## Method

- Fixture: `examples/py-demo/app.py` + `examples/node-demo/calc.js`
  with the same seeded one-line bug (a subtrahend applied twice).
  Symptom: printed total too low. Task: name the wrong variable + line.
- Debugger path (`scripts/bench_debug_loop.sh`, MEASURED): launch with
  one breakpoint at the return line, read the first snapshot, one `eval`
  to confirm, `continue`, `close`. The script asserts the diagnosis
  programmatically (wrong locals = red, not a silent pass).
- Print path (ESTIMATE): an expert's minimal loop for the same bug —
  add one print of the intermediates, run, read, remove the print.
  One edit + one run + one revert per cycle; one cycle suffices here
  because the intermediates directly name the culprit.

## Results (MacBook, 2026-09-15, `scripts/bench_debug_loop.sh`)

| | debugger (MEASURED) | print loop (ESTIMATE) |
|---|---|---|
| Python wall time | ~1.4 s | ~5 s (edit + 2 interpreter runs) |
| Node wall time | ~1.2 s | ~4 s (edit + 2 node runs) |
| CLI/tool calls | 8 (4 per language) | 0 (editor + shell instead) |
| Target launches | 2 (one per language) | 4 (two per language: diagnose + verify) |
| Source edits | 0 | 4 (add + remove print, per language) |

Rerun it: `scripts/bench_debug_loop.sh` (needs cargo/python3/node;
no network after first provisioning; scratch sessions self-close).

## Why the gap widens on real apps

The fixture processes start instantly, so this is the *floor* of the
advantage. Per SKILL.md speed rules, `start` costs 10–60 s on Spring
Boot: print debugging pays a full restart per cycle (edit + rebuild +
rerun), the debugger pays one launch and then inspects freely (~10 ms
per `eval`/`vars`/`step`). The scaling variable is restart cost, and
the debugger's restart count is structurally 1 per session.

## Limits of this number

- One seeded bug, instant-startup fixtures: a floor, not a headline.
- No LLM in the loop: token savings (the real agent cost) are argued,
  not measured — each avoided rebuild cycle is one avoided
  read-the-logs round-trip, but we have not instrumented an agent run.
- A two-cycle print debug (wrong first guess) doubles the print column;
  the debugger column is unaffected (the snapshot shows everything).
