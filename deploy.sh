#!/usr/bin/env bash
set -euo pipefail

SERVICE=onedrive-linux
BIN_DIR="$HOME/.local/bin"
UNIT_SRC="$(dirname "$0")/config/systemd/onedrive-linux.service"
UNIT_DEST="$HOME/.config/systemd/user/onedrive-linux.service"

echo "==> Building release..."
cargo build --release

echo "==> Stopping daemon..."
systemctl --user stop "$SERVICE" 2>/dev/null || true

echo "==> Installing binaries to $BIN_DIR/"
mkdir -p "$BIN_DIR"
cp target/release/onedrive-daemon "$BIN_DIR/"
cp target/release/odctl "$BIN_DIR/"
cp target/release/onedrive-flyout "$BIN_DIR/"

if [ ! -f "$UNIT_DEST" ] || ! cmp -s "$UNIT_SRC" "$UNIT_DEST"; then
    echo "==> Installing systemd unit..."
    mkdir -p "$(dirname "$UNIT_DEST")"
    cp "$UNIT_SRC" "$UNIT_DEST"
    systemctl --user daemon-reload
fi

echo "==> Starting daemon..."
systemctl --user start "$SERVICE"

echo "==> Done. Status:"
systemctl --user status "$SERVICE" --no-pager | head -5
