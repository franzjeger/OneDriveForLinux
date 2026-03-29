#!/usr/bin/env bash
set -euo pipefail

echo "==> Building release..."
cargo build --release

echo "==> Stopping daemon..."
systemctl --user stop onedrive-daemon 2>/dev/null || true

echo "==> Installing binaries to ~/.local/bin/"
cp target/release/onedrive-daemon ~/.local/bin/
cp target/release/odctl ~/.local/bin/

echo "==> Starting daemon..."
systemctl --user start onedrive-daemon

echo "==> Done. Status:"
systemctl --user status onedrive-daemon --no-pager | head -5
