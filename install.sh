#!/bin/sh
# agent-debugger installer
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/PlaneInABottle/agent-debugger/master/install.sh | sh
#
# Environment variables:
#   VERSION      - Specific version tag to install (e.g. v0.2.0, default: latest)
#   INSTALL_DIR  - Directory to place binary (default: ~/.local/bin or /usr/local/bin)

set -eu

REPO="PlaneInABottle/agent-debugger"
DEFAULT_VERSION="v0.2.0"

log_info() {
  printf "\033[34m[agent-debugger]\033[0m %s\n" "$1"
}

log_error() {
  printf "\033[31m[agent-debugger error]\033[0m %s\n" "$1" >&2
}

# Verify a downloaded archive against its published .sha256 sidecar.
# Args: <archive_path> <checksum_url>
# - Checksum file missing/unfetchable (old release): warn, continue.
# - Checksum present but mismatch: error, return nonzero (caller aborts).
# - Uses sha256sum when available, else shasum -a 256 (macOS).
verify_checksum() {
  ARCHIVE_PATH="$1"
  CHECKSUM_URL="$2"
  CHECKSUM_FILE="${ARCHIVE_PATH}.sha256"
  if ! curl -fsSL "$CHECKSUM_URL" -o "$CHECKSUM_FILE" 2>/dev/null; then
    log_info "No published checksum found; skipping verification."
    return 0
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$(dirname "$ARCHIVE_PATH")" && sha256sum -c "$(basename "$CHECKSUM_FILE")") || {
      log_error "Checksum mismatch for $(basename "$ARCHIVE_PATH"); download may be corrupt or tampered."
      return 1
    }
  elif command -v shasum >/dev/null 2>&1; then
    (cd "$(dirname "$ARCHIVE_PATH")" && shasum -a 256 -c "$(basename "$CHECKSUM_FILE")") || {
      log_error "Checksum mismatch for $(basename "$ARCHIVE_PATH"); download may be corrupt or tampered."
      return 1
    }
  else
    log_info "No sha256 tool found; skipping verification."
    return 0
  fi
  log_info "Checksum verified."
}

# 1. Detect OS
OS="$(uname -s)"
case "$OS" in
  Darwin)
    TARGET_OS="apple-darwin"
    ;;
  Linux)
    TARGET_OS="unknown-linux-gnu"
    ;;
  *)
    log_error "Unsupported operating system: $OS"
    exit 1
    ;;
esac

# 2. Detect Architecture
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64)
    TARGET_ARCH="x86_64"
    ;;
  arm64|aarch64)
    TARGET_ARCH="aarch64"
    ;;
  *)
    log_error "Unsupported architecture: $ARCH"
    exit 1
    ;;
esac

TARGET="${TARGET_ARCH}-${TARGET_OS}"

# 3. Determine Version
VERSION="${VERSION:-}"
if [ -z "$VERSION" ]; then
  log_info "Fetching latest release version from GitHub..."
  LATEST_TAG=$(curl -sSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null | grep '"tag_name":' | head -n 1 | cut -d '"' -f 4 || true)
  if [ -n "$LATEST_TAG" ]; then
    VERSION="$LATEST_TAG"
  else
    VERSION="$DEFAULT_VERSION"
  fi
fi

# Ensure leading 'v'
case "$VERSION" in
  v*) ;;
  *) VERSION="v$VERSION" ;;
esac

log_info "Installing agent-debugger ${VERSION} for ${TARGET}..."

# 4. Determine Install Directory
if [ -n "${INSTALL_DIR:-}" ]; then
  DEST_DIR="$INSTALL_DIR"
elif [ -d "$HOME/.cargo/bin" ] && echo "$PATH" | tr ':' '\n' | grep -F -qx "$HOME/.cargo/bin"; then
  DEST_DIR="$HOME/.cargo/bin"
elif [ -d "$HOME/.local/bin" ] && echo "$PATH" | tr ':' '\n' | grep -F -qx "$HOME/.local/bin"; then
  DEST_DIR="$HOME/.local/bin"
elif [ -w "/usr/local/bin" ]; then
  DEST_DIR="/usr/local/bin"
else
  DEST_DIR="$HOME/.local/bin"
fi

mkdir -p "$DEST_DIR"

# 5. Download and Extract
ASSET_NAME="agent-debugger-${TARGET}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET_NAME}"
CHECKSUM_URL="${DOWNLOAD_URL}.sha256"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

log_info "Downloading ${DOWNLOAD_URL}..."
if ! curl -fSL "$DOWNLOAD_URL" -o "${TMP_DIR}/${ASSET_NAME}" 2>/dev/null; then
  log_error "Failed to download pre-built binary from ${DOWNLOAD_URL}"
  log_error "Please check that release ${VERSION} exists, or install from source:"
  log_error "  cargo install --path ."
  exit 1
fi

log_info "Verifying checksum..."
verify_checksum "${TMP_DIR}/${ASSET_NAME}" "$CHECKSUM_URL" || exit 1

log_info "Extracting binary..."
tar -xzf "${TMP_DIR}/${ASSET_NAME}" -C "$TMP_DIR"

if [ ! -f "${TMP_DIR}/agent-debugger" ]; then
  log_error "Archive did not contain the agent-debugger binary."
  exit 1
fi

mv "${TMP_DIR}/agent-debugger" "${DEST_DIR}/agent-debugger"
chmod +x "${DEST_DIR}/agent-debugger"

log_info "agent-debugger was installed successfully to ${DEST_DIR}/agent-debugger"

# 6. Verify PATH and Installation
if ! echo "$PATH" | tr ':' '\n' | grep -F -qx "$DEST_DIR"; then
  printf "\n\033[33m[Warning]\033[0m %s is not in your PATH.\n" "$DEST_DIR"
  printf "Add it to your shell configuration (e.g. ~/.zshrc or ~/.bashrc):\n"
  printf "  export PATH=\"%s:\$PATH\"\n\n" "$DEST_DIR"
fi

if ! "${DEST_DIR}/agent-debugger" --version; then
  log_error "Installed binary failed to run; check ${DEST_DIR}/agent-debugger"
  exit 1
fi
printf "\nRun '\033[1magent-debugger doctor\033[0m' to check your debugging environment.\n"
