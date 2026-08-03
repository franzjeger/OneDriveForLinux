#!/usr/bin/env bash
# OneDrive for Linux — one-command installer.
#
# Already have an Azure app registration (or an existing config):
#   curl -fsSL .../install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- --client-id <YOUR-CLIENT-ID>
#
# No Azure app registration yet — set one up as part of the install:
#   curl -fsSL .../install.sh | bash -s -- --setup-azure
#
# Options:
#   --client-id <ID>   Use this Azure app client ID (skips the prompt)
#   --setup-azure      Create the Azure app registration during install
#   --version vX.Y.Z   Install a specific release (default: latest)
#   --local            Install binaries from the current directory instead of downloading
#   --no-service       Install files only; don't enable/start the systemd service
#   --with-dolphin-overlay
#                      Also build and install the Dolphin sync-state overlay
#                      plugin (needs a C++ toolchain and the KDE Frameworks 6
#                      development packages; installs system-wide via sudo)
#   --uninstall        Remove binaries, service, and Dolphin menu (config/tokens kept)
#   --purge            With --uninstall: also remove config, tokens, and local database
set -euo pipefail

REPO="franzjeger/OneDriveForLinux"
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="$HOME/.config/systemd/user"
SERVICE="onedrive-linux.service"
MENU_DIR="$HOME/.local/share/kio/servicemenus"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICON_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
CONFIG_DIR="$HOME/.config/onedrive-linux"

VERSION=""
CLIENT_ID=""
SETUP_AZURE=0
LOCAL=0
NO_SERVICE=0
OVERLAY=0
UNINSTALL=0
PURGE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --client-id) CLIENT_ID="$2"; shift 2 ;;
        --setup-azure) SETUP_AZURE=1; shift ;;
        --local) LOCAL=1; shift ;;
        --no-service) NO_SERVICE=1; shift ;;
        --with-dolphin-overlay) OVERLAY=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge) PURGE=1; shift ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m ✓ \033[0m%s\n' "$*"; }
warn() { printf '\033[1;33m ! \033[0m%s\n' "$*"; }
die()  { printf '\033[1;31m ✗ %s\033[0m\n' "$*" >&2; exit 1; }

# ── Uninstall ────────────────────────────────────────────────────────────────
if [ "$UNINSTALL" = 1 ]; then
    say "Uninstalling OneDrive for Linux…"
    systemctl --user disable --now "$SERVICE" 2>/dev/null || true
    rm -f "$BIN_DIR/onedrive-daemon" "$BIN_DIR/odctl" "$BIN_DIR/onedrive-flyout"
    rm -f "$UNIT_DIR/$SERVICE" "$MENU_DIR/onedrive.desktop"
    rm -f "$APP_DIR/onedrive-linux.desktop" "$ICON_DIR/onedrive-linux.svg"
    rm -f "$ICON_DIR/onedrive-cloud.svg" "$ICON_DIR/onedrive-partial.svg" \
          "$ICON_DIR/onedrive-upload.svg"
    if [ -n "${QT_PLUGIN_DIR:-}" ] && [ -f "$QT_PLUGIN_DIR/kf6/overlayicon/onedrive-overlay.so" ]; then
        sudo rm -f "$QT_PLUGIN_DIR/kf6/overlayicon/onedrive-overlay.so" || true
    fi
    update-desktop-database "$APP_DIR" 2>/dev/null || true
    systemctl --user daemon-reload 2>/dev/null || true
    if [ "$PURGE" = 1 ]; then
        rm -rf "$CONFIG_DIR" \
               "${XDG_DATA_HOME:-$HOME/.local/share}/onedrive-linux" \
               "${XDG_CACHE_HOME:-$HOME/.cache}/onedrive-linux"
        ok "Removed binaries, service, config, tokens, and local database."
    else
        ok "Removed binaries and service. Config and sign-in kept (use --purge to remove)."
    fi
    exit 0
fi

# ── Dependencies ─────────────────────────────────────────────────────────────
say "Checking dependencies…"
if ! command -v fusermount3 >/dev/null 2>&1; then
    warn "fuse3 is not installed — attempting to install it."
    if command -v pacman >/dev/null 2>&1; then sudo pacman -S --noconfirm fuse3
    elif command -v apt-get >/dev/null 2>&1; then sudo apt-get install -y fuse3
    elif command -v dnf >/dev/null 2>&1; then sudo dnf install -y fuse3
    elif command -v zypper >/dev/null 2>&1; then sudo zypper install -y fuse3
    else die "Please install fuse3 with your package manager, then re-run."
    fi
fi
ok "fuse3 present"

# ── Get the binaries ─────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR" "$UNIT_DIR" "$MENU_DIR"

if [ "$LOCAL" = 1 ]; then
    say "Installing binaries from current directory…"
    for bin in onedrive-daemon odctl onedrive-flyout; do
        [ -f "$bin" ] || die "$bin not found here — run inside an unpacked release."
        install -m 755 "$bin" "$BIN_DIR/"
    done
else
    command -v curl >/dev/null 2>&1 || die "curl is required."
    if [ -z "$VERSION" ]; then
        say "Finding latest release…"
        # Capture first, then parse: piping into an early-exiting reader
        # (grep -m1 / head) SIGPIPEs curl, and with `set -e -o pipefail` that
        # aborts the whole script mid-download.
        release_json=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest") \
            || die "Could not reach GitHub to look up the latest release."
        VERSION=$(printf '%s' "$release_json" \
            | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
            | sed -n '1p')
        [ -n "$VERSION" ] || die "Could not determine the latest release."
    fi
    say "Downloading OneDrive for Linux $VERSION…"
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    TARBALL="onedrive-linux-$VERSION-x86_64.tar.gz"
    BASE="https://github.com/$REPO/releases/download/$VERSION"
    curl -fSL --progress-bar -o "$TMP/$TARBALL" "$BASE/$TARBALL"
    curl -fsSL -o "$TMP/$TARBALL.sha256" "$BASE/$TARBALL.sha256"
    say "Verifying checksum…"
    (cd "$TMP" && sha256sum -c "$TARBALL.sha256" >/dev/null) || die "Checksum mismatch — aborting."
    ok "Checksum OK"
    tar xzf "$TMP/$TARBALL" -C "$TMP"
    SRC="$TMP/onedrive-linux-$VERSION-x86_64"
    for bin in onedrive-daemon odctl onedrive-flyout; do
        install -m 755 "$SRC/$bin" "$BIN_DIR/"
    done
fi
ok "Binaries installed to $BIN_DIR"

# ── PATH ─────────────────────────────────────────────────────────────────────
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        if ! grep -qs '\.local/bin' "$HOME/.profile" 2>/dev/null; then
            printf '\nexport PATH="$HOME/.local/bin:$PATH"\n' >> "$HOME/.profile"
        fi
        warn "$BIN_DIR added to PATH via ~/.profile — takes effect at next login."
        ;;
esac

# ── systemd unit ─────────────────────────────────────────────────────────────
say "Installing systemd user service…"
cat > "$UNIT_DIR/$SERVICE" <<EOF
[Unit]
Description=OneDrive for Linux
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=%h/.local/bin/onedrive-daemon
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
ok "Service installed"

# ── Dolphin right-click menu (plain file, no compilation) ────────────────────
cat > "$MENU_DIR/onedrive.desktop" <<EOF
[Desktop Entry]
Type=Service
X-KDE-ServiceTypes=KonqPopupMenu/Plugin
MimeType=inode/directory;all/allfiles;
Actions=OneDrivePin;OneDriveUnpin;OneDriveSync;
X-KDE-Priority=TopLevel
X-KDE-Submenu=OneDrive
X-KDE-Submenu[nb]=OneDrive
Icon=folder-cloud

[Desktop Action OneDrivePin]
Name=Always keep on this device
Name[nb]=Behold alltid på enheten
Icon=folder-download
Exec=$BIN_DIR/odctl pin %F

[Desktop Action OneDriveUnpin]
Name=Free up space
Name[nb]=Frigjør plass
Icon=folder-cloud
Exec=$BIN_DIR/odctl unpin %F

[Desktop Action OneDriveSync]
Name=Sync now
Name[nb]=Synkroniser nå
Icon=view-refresh
Exec=$BIN_DIR/odctl sync %f
EOF
# Plasma 5.85+ refuses to run a service menu whose .desktop file is not
# executable, reporting "You are not authorized to execute this file" when the
# menu entry is clicked. `cat` creates it mode 644, so set the bit explicitly.
chmod 755 "$MENU_DIR/onedrive.desktop"
ok "Dolphin right-click menu installed"

# ── Application launcher entry ───────────────────────────────────────────────
# This is what makes OneDrive a real app: an icon in the application menu and
# in KRunner, so it never has to be started from a terminal.
say "Installing application launcher…"
mkdir -p "$APP_DIR" "$ICON_DIR"

# Desktop entries have no field code for the home directory, so the folder
# action needs a literal path. Honour a configured sync_dir when there is one.
SYNC_DIR="$HOME/OneDrive"
if [ -f "$CONFIG_DIR/config.toml" ]; then
    CONFIGURED=$(sed -n 's/^[[:space:]]*sync_dir[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' \
                 "$CONFIG_DIR/config.toml" | sed -n '1p')
    [ -n "$CONFIGURED" ] && SYNC_DIR="${CONFIGURED/#\~/$HOME}"
fi

fetch_asset() {
    # $1 = path within the repo, $2 = destination
    if [ "$LOCAL" = 1 ] && [ -f "$1" ]; then
        cp "$1" "$2"
    else
        curl -fsSL "https://raw.githubusercontent.com/$REPO/main/$1" -o "$2" \
            || warn "Could not fetch $1."
    fi
}

fetch_asset assets/onedrive-linux.svg "$ICON_DIR/onedrive-linux.svg"
# Overlay emblems for the Dolphin plugin. Installed unconditionally: they are
# tiny, and a plugin that loads while its icons are missing draws nothing at
# all, which is indistinguishable from the plugin not working.
for emblem in onedrive-cloud onedrive-partial onedrive-upload; do
    fetch_asset "assets/icons/$emblem.svg" "$ICON_DIR/$emblem.svg"
done

cat > "$APP_DIR/onedrive-linux.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=OneDrive
Name[nb]=OneDrive
GenericName=Cloud file sync
GenericName[nb]=Skysynkronisering
Comment=Sync and browse your OneDrive files
Comment[nb]=Synkroniser og bla i OneDrive-filene dine
Exec=$BIN_DIR/onedrive-flyout
Icon=onedrive-linux
Terminal=false
Categories=Network;FileTransfer;Utility;
Keywords=onedrive;cloud;sync;microsoft;
StartupNotify=true
StartupWMClass=onedrive-linux
SingleMainWindow=true
Actions=OpenFolder;Settings;Signin;

[Desktop Action OpenFolder]
Name=Open OneDrive folder
Name[nb]=Åpne OneDrive-mappen
Icon=folder-cloud
Exec=xdg-open $SYNC_DIR

[Desktop Action Settings]
Name=Settings
Name[nb]=Innstillinger
Icon=preferences-system
Exec=$BIN_DIR/onedrive-flyout --settings

[Desktop Action Signin]
Name=Sign in again
Name[nb]=Logg inn på nytt
Icon=dialog-password
Exec=$BIN_DIR/onedrive-flyout --signin
EOF

update-desktop-database "$APP_DIR" 2>/dev/null || true
gtk-update-icon-cache -qtf "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" 2>/dev/null || true
ok "Application launcher installed — search for \"OneDrive\" in your app menu"

# ── Dolphin sync-state overlay plugin (opt-in) ───────────────────────────────
# C++ against KDE Frameworks 6, so it is not built unless asked for. Every
# failure here is non-fatal: the overlay is a nicety, and aborting the install
# over it would leave the user with no working sync client at all.
if [ "$OVERLAY" = 1 ]; then
    say "Building the Dolphin overlay plugin…"

    overlay_give_up() {
        warn "Skipping the Dolphin overlay plugin: $1"
        echo "     Everything else is installed and working."
        echo "     See extensions/dolphin/README.md to build it by hand later."
        OVERLAY=0
    }

    # Qt only searches the plugin directories in its own library paths — a
    # plugin under $HOME is silently never loaded. Install where Dolphin looks.
    QT_PLUGIN_DIR=""
    for qtpaths in qtpaths6 qtpaths; do
        if command -v "$qtpaths" >/dev/null 2>&1; then
            QT_PLUGIN_DIR=$("$qtpaths" --plugin-dir 2>/dev/null) && break
        fi
    done

    if [ -z "$QT_PLUGIN_DIR" ]; then
        overlay_give_up "could not determine the Qt plugin directory (is qtpaths6 installed?)"
    fi
fi

if [ "$OVERLAY" = 1 ]; then
    if ! command -v cmake >/dev/null 2>&1 || ! command -v g++ >/dev/null 2>&1; then
        say "Installing build dependencies…"
        if command -v pacman >/dev/null 2>&1; then
            sudo pacman -S --needed --noconfirm base-devel cmake extra-cmake-modules kio qt6-base \
                || warn "Dependency install failed — continuing anyway."
        elif command -v apt-get >/dev/null 2>&1; then
            sudo apt-get install -y build-essential cmake extra-cmake-modules \
                libkf6kio-dev qt6-base-dev || warn "Dependency install failed — continuing anyway."
        elif command -v dnf >/dev/null 2>&1; then
            sudo dnf install -y gcc-c++ cmake extra-cmake-modules kf6-kio-devel qt6-qtbase-devel \
                || warn "Dependency install failed — continuing anyway."
        else
            overlay_give_up "unknown distribution; install cmake, extra-cmake-modules and the KF6 KIO development package yourself"
        fi
    fi
fi

if [ "$OVERLAY" = 1 ]; then
    # Sources: the working tree with --local, otherwise the release tarball,
    # which ships extensions/ for exactly this.
    if [ "$LOCAL" = 1 ]; then
        OVERLAY_SRC="$PWD/extensions/dolphin"
    else
        OVERLAY_SRC="$SRC/extensions/dolphin"
    fi

    if [ ! -f "$OVERLAY_SRC/CMakeLists.txt" ]; then
        overlay_give_up "plugin sources not found at $OVERLAY_SRC"
    fi
fi

if [ "$OVERLAY" = 1 ]; then
    OVERLAY_BUILD=$(mktemp -d)
    if cmake -S "$OVERLAY_SRC" -B "$OVERLAY_BUILD" \
             -DCMAKE_BUILD_TYPE=Release \
             -DKDE_INSTALL_PLUGINDIR="$QT_PLUGIN_DIR" >/dev/null 2>&1 \
       && cmake --build "$OVERLAY_BUILD" -j"$(nproc)" >/dev/null 2>&1; then
        if sudo cmake --install "$OVERLAY_BUILD" >/dev/null 2>&1; then
            ok "Dolphin overlay plugin installed to $QT_PLUGIN_DIR/kf6/overlayicon"
            echo "     Restart Dolphin to see sync-state emblems: kquitapp6 dolphin"
        else
            overlay_give_up "could not install to $QT_PLUGIN_DIR (needs sudo)"
        fi
    else
        warn "The overlay plugin failed to build. Re-run these to see why:"
        echo "     cmake -S $OVERLAY_SRC -B /tmp/od-overlay -DKDE_INSTALL_PLUGINDIR=$QT_PLUGIN_DIR"
        echo "     cmake --build /tmp/od-overlay"
        echo "     Everything else is installed and working."
    fi
    rm -rf "$OVERLAY_BUILD"
fi

# ── Config ───────────────────────────────────────────────────────────────────
write_config() {
    mkdir -p "$CONFIG_DIR"
    cat > "$CONFIG_DIR/config.toml" <<EOF
client_id = "$1"
# tenant_id = "common"
# sync_dir = "~/OneDrive"
# on_demand = true
# How to sign in: "auto" | "browser" | "device_code".
# Use "browser" if Conditional Access blocks device code (AADSTS53003);
# the app registration then needs http://localhost as a redirect URI.
# auth_method = "auto"
EOF
    ok "Config written to $CONFIG_DIR/config.toml"
}

if [ -f "$CONFIG_DIR/config.toml" ]; then
    ok "Using existing config at $CONFIG_DIR/config.toml"
elif [ -n "$CLIENT_ID" ]; then
    write_config "$CLIENT_ID"
elif [ "$SETUP_AZURE" = 1 ]; then
    say "The daemon will guide you through creating the Azure app registration."
elif [ -r /dev/tty ]; then
    say "No configuration found — an Azure app registration is required."
    echo "   1) I already have a client ID — paste it now"
    echo "   2) Set one up for me (opens Azure; uses the az CLI when available)"
    read -r -p "   Choice [1/2]: " CHOICE < /dev/tty
    if [ "$CHOICE" = "2" ]; then
        SETUP_AZURE=1
    else
        read -r -p "   Client ID: " CLIENT_ID < /dev/tty
        [ -n "$CLIENT_ID" ] || die "A client ID is required (or re-run with --setup-azure)."
        write_config "$CLIENT_ID"
    fi
else
    warn "No config and no terminal — re-run with --client-id <ID> or --setup-azure."
    NO_SERVICE=1
fi

# ── Sign in (and Azure setup, when requested) ────────────────────────────────
# The daemon prints the device code — and, with --setup-azure, walks through
# creating the app registration first. It is stopped again once tokens land.
if [ ! -f "$CONFIG_DIR/tokens.json" ] && [ "$NO_SERVICE" = 0 ] && [ -r /dev/tty ]; then
    # Only one daemon may run at a time, so stop the service while we sign in.
    if systemctl --user is-active --quiet "$SERVICE" 2>/dev/null; then
        say "Stopping the running service so we can sign in…"
        systemctl --user stop "$SERVICE" || true
        sleep 1
    fi
    if [ "$SETUP_AZURE" = 1 ]; then
        say "Setting up Azure and signing in — follow the prompts below."
        WAIT_SECS=900
    else
        say "Signing in to Microsoft — a code will appear below."
        WAIT_SECS=300
    fi
    # stdin from the terminal so interactive prompts work; job control is off
    # in this non-interactive shell, so the child shares our process group and
    # may read the tty.
    "$BIN_DIR/onedrive-daemon" < /dev/tty & DAEMON_PID=$!
    for _ in $(seq 1 "$WAIT_SECS"); do
        [ -f "$CONFIG_DIR/tokens.json" ] && break
        kill -0 "$DAEMON_PID" 2>/dev/null || break
        sleep 1
    done
    sleep 2   # let the first delta sync start before we stop it
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    if [ ! -f "$CONFIG_DIR/tokens.json" ]; then
        warn "Sign-in did not complete."
        echo "     If Microsoft reported AADSTS53003, your tenant's Conditional Access"
        echo "     blocks the device code flow. Add 'auth_method = \"browser\"' to"
        echo "     $CONFIG_DIR/config.toml and register http://localhost as a redirect"
        echo "     URI on the app, then re-run this installer."
        die "Aborting — nothing was started."
    fi
    ok "Signed in"
fi

# ── Start ────────────────────────────────────────────────────────────────────
if [ "$NO_SERVICE" = 0 ]; then
    say "Starting OneDrive…"
    # `enable --now` does nothing to an already-active unit, so re-running the
    # installer to upgrade would leave the previous binary running. Enable for
    # autostart, then restart unconditionally to pick up the new binaries.
    systemctl --user enable "$SERVICE"
    systemctl --user restart "$SERVICE"
    ok "OneDrive is running — look for the cloud icon in your system tray."
else
    warn "Service not started (missing config or --no-service). Start later with:"
    echo "     systemctl --user enable --now $SERVICE"
fi

echo
ok "Done! OneDrive starts automatically at login."
echo "     Open it from your application menu (search for \"OneDrive\"), or"
echo "     left-click the cloud icon in the system tray."
echo "     Command line, if you want it: odctl status · odctl pin <path>"
