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
#   --uninstall        Remove binaries, service, and Dolphin menu (config/tokens kept)
#   --purge            With --uninstall: also remove config, tokens, and local database
set -euo pipefail

REPO="franzjeger/OneDriveForLinux"
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="$HOME/.config/systemd/user"
SERVICE="onedrive-linux.service"
MENU_DIR="$HOME/.local/share/kio/servicemenus"
CONFIG_DIR="$HOME/.config/onedrive-linux"

VERSION=""
CLIENT_ID=""
SETUP_AZURE=0
LOCAL=0
NO_SERVICE=0
UNINSTALL=0
PURGE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --client-id) CLIENT_ID="$2"; shift 2 ;;
        --setup-azure) SETUP_AZURE=1; shift ;;
        --local) LOCAL=1; shift ;;
        --no-service) NO_SERVICE=1; shift ;;
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
        VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
            | grep -m1 '"tag_name"' | cut -d'"' -f4)
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
cat > "$MENU_DIR/onedrive.desktop" <<'EOF'
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
Exec=odctl pin %F

[Desktop Action OneDriveUnpin]
Name=Free up space
Name[nb]=Frigjør plass
Icon=folder-cloud
Exec=odctl unpin %F

[Desktop Action OneDriveSync]
Name=Sync now
Name[nb]=Synkroniser nå
Icon=view-refresh
Exec=odctl sync %f
EOF
ok "Dolphin right-click menu installed"

# ── Config ───────────────────────────────────────────────────────────────────
write_config() {
    mkdir -p "$CONFIG_DIR"
    cat > "$CONFIG_DIR/config.toml" <<EOF
client_id = "$1"
# tenant_id = "common"
# sync_dir = "~/OneDrive"
# on_demand = true
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
    [ -f "$CONFIG_DIR/tokens.json" ] || die "Sign-in did not complete — re-run the installer to try again."
    ok "Signed in"
fi

# ── Start ────────────────────────────────────────────────────────────────────
if [ "$NO_SERVICE" = 0 ]; then
    say "Starting OneDrive…"
    systemctl --user enable --now "$SERVICE"
    ok "OneDrive is running — look for the cloud icon in your system tray."
else
    warn "Service not started (missing config or --no-service). Start later with:"
    echo "     systemctl --user enable --now $SERVICE"
fi

echo
ok "Done! Useful commands: odctl status · odctl pin <path> · left-click the tray icon"
