#!/usr/bin/env sh
# Install claude-resume: download the release binary for this machine and configure Claude Code.
#
#   curl -fsSL https://raw.githubusercontent.com/mucahitkantepe/claude-resume/master/install.sh | sh
#
# Environment:
#   CLAUDE_RESUME_VERSION      release tag to install, e.g. v0.5.0 (default: the latest release)
#   CLAUDE_RESUME_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   CLAUDE_RESUME_NO_INIT=1    only install the binary; leave Claude Code's settings alone
set -eu

REPO="mucahitkantepe/claude-resume"
BIN_NAME="claude-resume"
INSTALL_DIR="${CLAUDE_RESUME_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${CLAUDE_RESUME_VERSION:-latest}"

case "$(uname -s)" in
  Darwin) OS="apple-darwin" ;;
  Linux) OS="unknown-linux-gnu" ;;
  *) echo "Unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  arm64 | aarch64) ARCH="aarch64" ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
# Must match the asset names produced by .github/workflows/release.yml.
ASSET="${BIN_NAME}-${ARCH}-${OS}.tar.gz"

if [ "$VERSION" = "latest" ]; then
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
fi
# Tests point this at a local directory.
BASE_URL="${CLAUDE_RESUME_DOWNLOAD_URL:-$BASE_URL}"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fetch() {
  curl --proto '=https,file' --tlsv1.2 -fsSL "$1" -o "$2"
}

echo "Downloading ${ASSET} (${VERSION})..."
fetch "${BASE_URL}/${ASSET}" "${TMP}/${ASSET}"

# Every release publishes a checksum next to each archive; without one, nothing is installed.
if ! fetch "${BASE_URL}/${ASSET}.sha256" "${TMP}/${ASSET}.sha256" 2>/dev/null; then
  echo "Could not download ${ASSET}.sha256 to verify the download; nothing was installed." >&2
  exit 1
fi
expected=$(cut -d ' ' -f 1 <"${TMP}/${ASSET}.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "${TMP}/${ASSET}" | cut -d ' ' -f 1)
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "${TMP}/${ASSET}" | cut -d ' ' -f 1)
else
  echo "Neither sha256sum nor shasum is available to verify the download." >&2
  exit 1
fi
if [ "$expected" != "$actual" ]; then
  echo "Checksum mismatch for ${ASSET}: expected ${expected}, got ${actual}" >&2
  exit 1
fi
echo "Checksum verified."

tar -xzf "${TMP}/${ASSET}" -C "$TMP"
mkdir -p "$INSTALL_DIR"
cp "${TMP}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}.tmp"
chmod 755 "${INSTALL_DIR}/${BIN_NAME}.tmp"
mv -f "${INSTALL_DIR}/${BIN_NAME}.tmp" "${INSTALL_DIR}/${BIN_NAME}"
echo "Installed $("${INSTALL_DIR}/${BIN_NAME}" --version) to ${INSTALL_DIR}/${BIN_NAME}"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "${INSTALL_DIR} is not on your PATH. Add it, for example:"
    echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.zshrc"
    echo ""
    ;;
esac

if [ "${CLAUDE_RESUME_NO_INIT:-}" != "1" ]; then
  "${INSTALL_DIR}/${BIN_NAME}" init
fi
echo "Done! Run 'claude-resume' to browse your sessions."
