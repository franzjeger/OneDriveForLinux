#!/bin/bash
mkdir -p build && cd build
cmake .. -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=$HOME/.local
make -j$(nproc)
make install

# Install service menu
SERVICEMENU_DIR="$HOME/.local/share/kio/servicemenus"
mkdir -p "$SERVICEMENU_DIR"
install -m 755 ../onedrive.desktop "$SERVICEMENU_DIR/onedrive.desktop"
