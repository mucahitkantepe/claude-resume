#!/usr/bin/env sh
# install.sh — zero-dependency installer for recall
# Usage: curl -fsSL https://raw.githubusercontent.com/mucahitkantepe/claude-recall/main/install.sh | sh
set -e

REPO="mucahitkantepe/claude-resume"
BIN_NAME="claude-resume"
INSTALL_DIR="${HOME}/.local/bin"

# ── Detect platform ──────────────────────────────────────────────────────────
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  darwin) OS="apple-darwin" ;;
  linux)  OS="unknown-linux-gnu" ;;
  *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  arm64|aarch64) ARCH="aarch64" ;;
  *)             echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

TARGET="${ARCH}-${OS}"

# ── Download binary ──────────────────────────────────────────────────────────
echo "Installing recall for ${TARGET}..."

LATEST=$(curl --proto '=https' --tlsv1.2 -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | head -1 | sed 's/.*"v\(.*\)".*/\1/' 2>/dev/null || echo "")
if [ -z "$LATEST" ]; then
  echo "Could not determine latest version. Check https://github.com/${REPO}/releases"
  exit 1
fi

URL="https://github.com/${REPO}/releases/download/v${LATEST}/recall-${TARGET}.tar.gz"
echo "Downloading v${LATEST} from ${URL}..."

mkdir -p "$INSTALL_DIR"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

curl --proto '=https' --tlsv1.2 -fsSL "$URL" -o "${TMP}/recall.tar.gz"
tar -xzf "${TMP}/recall.tar.gz" -C "$TMP"
chmod +x "${TMP}/${BIN_NAME}"
mv "${TMP}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"

echo "Installed to ${INSTALL_DIR}/${BIN_NAME}"

# ── Check PATH ───────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "Add ${INSTALL_DIR} to your PATH:"
    echo "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc"
    echo ""
    ;;
esac

# ── Configure Claude Code ────────────────────────────────────────────────────
echo "Configuring Claude Code..."
"${INSTALL_DIR}/${BIN_NAME}" init

echo ""
echo "Done! Run 'claude-resume' to browse sessions."
