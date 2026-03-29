#!/bin/bash
set -e

SRCDIR="$(cd "$(dirname "$0")" && pwd)"
BUILDDIR="$SRCDIR/build-manual"
INSTALLDIR="$HOME/.local"

mkdir -p "$BUILDDIR"
cd "$BUILDDIR"

QT_CFLAGS=$(pkg-config --cflags Qt6Core Qt6Gui)
KIO_INC="-I/usr/include/KF6/KIOCore -I/usr/include/KF6/KIOWidgets -I/usr/include/KF6 -I/usr/include/KF6/KCoreAddons -I/usr/include/KF6/KIO"

# MOC pass
/usr/lib/qt6/moc \
    $QT_CFLAGS $KIO_INC \
    "$SRCDIR/onedrive-overlay.cpp" \
    -o onedrive-overlay.moc

# Compile
g++ -std=c++17 -fPIC -O2 \
    $QT_CFLAGS $KIO_INC \
    -I"$BUILDDIR" \
    -c "$SRCDIR/onedrive-overlay.cpp" -o onedrive-overlay.o

# Link shared library
g++ -shared -fPIC \
    onedrive-overlay.o \
    $(pkg-config --libs Qt6Core) \
    -L/usr/lib -lKF6KIOCore \
    -o onedrive_overlay.so

# Install
PLUGIN_DIR="$INSTALLDIR/lib/qt6/plugins/kf6/overlayicon"
mkdir -p "$PLUGIN_DIR"
cp onedrive_overlay.so "$PLUGIN_DIR/"
cp "$SRCDIR/onedrive-overlay.json" "$PLUGIN_DIR/"

echo "Installed to $PLUGIN_DIR/onedrive_overlay.so"
echo "Restart Dolphin to activate: kquitapp6 dolphin; dolphin &"
