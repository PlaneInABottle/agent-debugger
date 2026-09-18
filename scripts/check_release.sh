#!/bin/sh
# check_release.sh — verify a GitHub release ships every installer asset.
#
# Usage: scripts/check_release.sh <tag> [api-base-override]
#   <tag>                 release tag, e.g. v0.2.1
#   [api-base-override]   test seam: base URL for the releases API
#                         (default: https://api.github.com/repos/<repo>/releases)
#                         Accepts file:// for fixture-driven tests.
#
# Expectations derive from .github/workflows/release.yml (the `target:`
# matrix = single source of truth, same as tests/test_release_assets.py):
# each target needs its archive + per-archive .sha256, plus SHA256SUMS.txt.
# Missing release, missing asset, or missing sidecar = clear FAIL.
# Maintainer tool (run at release time, not in gates): needs curl + python3.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

TAG="${1:-}"
API_BASE="${2:-https://api.github.com/repos/PlaneInABottle/agent-debugger/releases}"
if [ -z "$TAG" ]; then
  echo "usage: scripts/check_release.sh <tag> [api-base-override]" >&2
  exit 2
fi

# Expected targets straight from the release matrix (no second list).
TARGETS=$(grep -o 'target: [a-z0-9_.\-]*' .github/workflows/release.yml \
  | awk '{print $2}' | sort -u)
if [ -z "$TARGETS" ]; then
  echo "FAIL: no targets parsed from .github/workflows/release.yml" >&2
  exit 1
fi

API_JSON="$(mktemp)"
trap 'rm -f "$API_JSON"' EXIT
if ! curl -fsSL --max-time 30 "$API_BASE/tags/$TAG" -o "$API_JSON" 2>/dev/null; then
  echo "FAIL: no release '$TAG' at $API_BASE (check the tag / wait for publish)" >&2
  exit 1
fi

fail=0
for t in $TARGETS; do
  case "$t" in
    *windows*) ext="zip" ;;
    *) ext="tar.gz" ;;
  esac
  for asset in "agent-debugger-${t}.${ext}" "agent-debugger-${t}.${ext}.sha256"; do
    if python3 -c "
import json,sys
names = [a['name'] for a in json.load(open('$API_JSON')).get('assets', [])]
sys.exit(0 if '$asset' in names else 1)
"; then
      echo "ok: $asset"
    else
      echo "FAIL: missing $asset in $TAG" >&2
      fail=1
    fi
  done
done
if python3 -c "
import json,sys
names = [a['name'] for a in json.load(open('$API_JSON')).get('assets', [])]
sys.exit(0 if 'SHA256SUMS.txt' in names else 1)
"; then
  echo "ok: SHA256SUMS.txt"
else
  echo "FAIL: missing SHA256SUMS.txt in $TAG" >&2
  fail=1
fi

[ "$fail" -eq 0 ] && echo "ALL RELEASE ASSETS PRESENT ($TAG)"
exit "$fail"
