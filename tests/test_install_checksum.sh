#!/bin/sh
# install.sh verify_checksum regression: match passes, tamper fails,
# missing sidecar warns-but-continues.
set -eu

# install.sh supports Darwin/Linux only (it exits 1 elsewhere), and the
# file:// + curl/sha256sum interop under Git Bash cannot exercise the
# real path (a failed sidecar download reads as "missing", so tamper
# can never fail). Skip on Windows with reason; ubuntu/macos keep
# covering the helper.
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    echo "skip: installer checksum needs a unix curl/sha256sum (install.sh is Darwin/Linux-only)"
    exit 0
    ;;
esac

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

# Extract only the verify_checksum function (avoid running the installer).
TMP_FN="$(mktemp)"
trap 'rm -f "$TMP_FN"' EXIT
awk '/^verify_checksum\(\)/,/^}/' "$ROOT/install.sh" > "$TMP_FN"
log_info() { printf "%s\n" "$1"; }
log_error() { printf "%s\n" "$1" >&2; }
# shellcheck disable=SC1090
. "$TMP_FN"

pass=0
fail=0
ok() { pass=$((pass + 1)); echo "ok: $1"; }
no() { fail=$((fail + 1)); echo "FAIL: $1" >&2; }

D="$(mktemp -d)"
P="$(mktemp -d)"
trap 'rm -rf "$D" "$P" "$TMP_FN"' EXIT
echo "hello-installer" > "$D/pkg.tar.gz"
(cd "$D" && (sha256sum pkg.tar.gz 2>/dev/null || shasum -a 256 pkg.tar.gz) > "$P/pkg.tar.gz.sha256")

# 1. Matching checksum passes (file:// URL works with curl).
if verify_checksum "$D/pkg.tar.gz" "file://$P/pkg.tar.gz.sha256" >/dev/null 2>&1; then
  ok "matching checksum passes"
else
  no "matching checksum must pass"
fi

# 2. Tampered archive fails.
echo "tampered" >> "$D/pkg.tar.gz"
if verify_checksum "$D/pkg.tar.gz" "file://$P/pkg.tar.gz.sha256" >/dev/null 2>&1; then
  no "tampered archive must fail verification"
else
  ok "tampered archive fails"
fi

# 3. Missing sidecar (old release): warn, exit 0.
echo "fresh" > "$D/other.tar.gz"
if verify_checksum "$D/other.tar.gz" "file://$D/does-not-exist.sha256" >/dev/null 2>&1; then
  ok "missing sidecar continues with warning"
else
  no "missing sidecar must not fail install"
fi

echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
