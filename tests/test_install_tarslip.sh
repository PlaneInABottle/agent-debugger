#!/bin/sh
# install.sh extraction-guard regression: a symlink member named
# agent-debugger is refused (tar-slip), a regular member passes.
# Runs the REAL extraction block sourced from install.sh (awk range, anchor
# asserted), so deleting the -L branch fails this test by design.
set -eu

# install.sh supports Darwin/Linux only, and symlink-member extraction
# needs unix tar. Skip on Windows with reason; ubuntu/macos cover it.
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    echo "skip: installer tar-slip guard needs unix tar/symlinks (install.sh is Darwin/Linux-only)"
    exit 0
    ;;
esac

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

# Extract only the download-verify-then-extract block's tail (avoid running
# the installer): from the member-only tar through the regular-file gate.
TMP_SNIP="$(mktemp)"
trap 'rm -f "$TMP_SNIP"' EXIT
# Extract only the member-only extraction plus both guard blocks (avoid
# running the installer): from the tar line through the closing fi of the
# regular-file gate.
awk '/^tar -xzf/{f=1} f{print} /did not contain the agent-debugger binary/{t=1} t && /^fi$/{exit}' \
  "$ROOT/install.sh" > "$TMP_SNIP"
grep -q "Refusing symlink member" "$TMP_SNIP" || {
  echo "FAIL: extraction anchor drifted (install.sh block moved?)" >&2
  exit 1
}

pass=0
fail=0
ok() { pass=$((pass + 1)); echo "ok: $1"; }
no() { fail=$((fail + 1)); echo "FAIL: $1" >&2; }

# Run the real installer lines in a subshell (their `exit 1` is the
# assertion, not the runner's death). Prefix assignments before the `.`
# special builtin stay contained in the subshell; log_error is inherited.
log_error() { printf "%s\n" "$1" >&2; }
run_snippet() {
  (TMP_DIR="$1" ASSET_NAME="pkg.tar.gz" DEST_DIR="$1/dest" . "$TMP_SNIP")
}

D="$(mktemp -d)"
trap 'rm -rf "$D" "$TMP_SNIP"' EXIT

# 1. Symlink member named agent-debugger is refused and unlinked.
mkdir -p "$D/evil/outside" "$D/evil/src" "$D/evil/dest"
echo "EVIL" > "$D/evil/outside/payload.txt"
ln -s "$D/evil/outside/payload.txt" "$D/evil/src/agent-debugger"
tar -czf "$D/evil/pkg.tar.gz" -C "$D/evil/src" agent-debugger
if run_snippet "$D/evil" 2>"$D/evil.err"; then
  no "symlink member must be refused"
elif grep -q "Refusing symlink member" "$D/evil.err"; then
  ok "symlink member refused at the symlink gate"
else
  no "refusal must come from the symlink gate (not a later gate)"
fi
if [ -e "$D/evil/agent-debugger" ]; then
  no "planted link must be removed"
else
  ok "planted link removed"
fi
if [ "$(cat "$D/evil/outside/payload.txt")" = "EVIL" ]; then
  ok "outside payload untouched"
else
  no "outside payload must be untouched"
fi

# 2. Regular member passes the guard.
mkdir -p "$D/good/src" "$D/good/dest"
printf "BINARY" > "$D/good/src/agent-debugger"
tar -czf "$D/good/pkg.tar.gz" -C "$D/good/src" agent-debugger
if run_snippet "$D/good" >/dev/null 2>&1; then
  ok "regular member passes"
else
  no "regular member must pass"
fi
if [ -f "$D/good/agent-debugger" ]; then
  ok "regular member extracted"
else
  no "regular member must be extracted"
fi

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
