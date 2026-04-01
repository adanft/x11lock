#!/bin/bash
#
# x11lock installer
# Usage: curl -sL https://raw.githubusercontent.com/adanft/x11lock/main/install.sh | sudo bash
#

set -euo pipefail

REPO="adanft/x11lock"
BINARY_NAME="x11lock"
INSTALL_PATH="/usr/local/bin/${BINARY_NAME}"
TMP_FILE="$(mktemp)"

cleanup() {
    rm -f "$TMP_FILE"
}

trap cleanup EXIT

echo "==> x11lock installer"

# Check if running as root
if [ "$EUID" -ne 0 ]; then
    echo "Error: This script must be run as root (use sudo)"
    exit 1
fi

# Get latest release version
echo "==> Checking for latest version..."
LATEST_TAG=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)

if [ -z "$LATEST_TAG" ]; then
    echo "Error: Could not fetch latest release"
    exit 1
fi

echo "==> Latest version: ${LATEST_TAG}"

# Get binary download URL
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/${BINARY_NAME}"

# Download to temp
echo "==> Downloading..."
curl -fL --progress-bar -o "$TMP_FILE" "${DOWNLOAD_URL}"

# Make executable
chmod +x "$TMP_FILE"

# Install
echo "==> Installing to ${INSTALL_PATH}..."
mv "$TMP_FILE" "${INSTALL_PATH}"

echo ""
echo "==> Installed successfully!"
echo "==> Run 'x11lock' to lock the screen"
echo ""
