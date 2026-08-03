# OneDrive for Linux

[![CI](https://github.com/franzjeger/OneDriveForLinux/actions/workflows/ci.yml/badge.svg)](https://github.com/franzjeger/OneDriveForLinux/actions/workflows/ci.yml)

A native OneDrive client for Linux featuring:

- **Files On-Demand** via FUSE — files appear in your filesystem at full size but are only downloaded when accessed
- **Background daemon** with systemd user service
- **System tray** integration (KDE/GNOME via StatusNotifier/AppIndicator)
- **OAuth2 device code flow** — no client secret required
- **Full Microsoft Graph API** sync with delta polling
- **Conflict resolution** — local conflicts are renamed with timestamps
- **Pin / unpin** — keep chosen files or folders always on device, or free space back to cloud-only
- **Download integrity** — every download is verified against the server's QuickXorHash
- **Dolphin overlay icons** — sync-state emblems in KDE's file manager (see `extensions/dolphin/`)
- **Status flyout** — left-click the tray icon for a window with live status, storage usage, and recent activity; expired sign-ins are fixed in two clicks with a graphical device-code flow
- **`odctl`** CLI for status, pause/resume, pinning, forced sync, and re-authentication

---

## Quick Install

One command downloads the latest release, verifies its checksum, installs the binaries, systemd service, and Dolphin right-click menu, signs you in, and starts syncing. Pick the line that matches you:

**A — you already have an Azure app registration** (or an existing `config.toml`):

```bash
curl -fsSL https://raw.githubusercontent.com/franzjeger/OneDriveForLinux/main/install.sh | bash
```

It reuses your existing config, or asks for the client ID once. To skip the prompt entirely:

```bash
curl -fsSL https://raw.githubusercontent.com/franzjeger/OneDriveForLinux/main/install.sh | bash -s -- --client-id <YOUR-CLIENT-ID>
```

**B — you don't have an Azure app registration yet:**

```bash
curl -fsSL https://raw.githubusercontent.com/franzjeger/OneDriveForLinux/main/install.sh | bash -s -- --setup-azure
```

This creates the app registration as part of the install — automatically via the `az` CLI if you're logged in as an admin, otherwise by opening the Azure portal and guiding you through it.

Uninstall with `install.sh --uninstall` (add `--purge` to also remove config and sign-in).

The manual steps below do the same thing, for those who prefer it.

---

## Prerequisites

```bash
# Arch / CachyOS
sudo pacman -S fuse3 dbus

# Debian / Ubuntu
sudo apt install fuse3 libfuse3-dev dbus
```

Ensure your user is in the `fuse` group (if required by your distro):

```bash
sudo usermod -aG fuse $USER
```

---

## Step 1 — Register an Azure Application

1. Go to <https://portal.azure.com> and open **Azure Active Directory → App registrations → New registration**.
2. Name it anything (e.g. `OneDriveLinux`).
3. Under **Supported account types**, choose **Accounts in any organizational directory and personal Microsoft accounts**.
4. Leave the redirect URI blank — we use device code flow.
5. Click **Register**.
6. Copy the **Application (client) ID** — you will need it for the config.
7. Go to **API permissions → Add a permission → Microsoft Graph → Delegated permissions**.
8. Add: `Files.ReadWrite.All`, `offline_access`, `User.Read`.
9. Click **Grant admin consent** (or ask your tenant admin to do so).

> **Personal accounts (Outlook.com / Hotmail.com):** No admin consent is needed. Just add the permissions and proceed.

---

## Step 2 — Create the Configuration File

```bash
mkdir -p ~/.config/onedrive-linux
cat > ~/.config/onedrive-linux/config.toml << 'EOF'
# Required: paste your Azure app client ID here
client_id = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

# Optional overrides (these are the defaults):
# tenant_id = "common"          # use "common" for personal/multi-tenant
# sync_dir = "~/OneDrive"       # local sync directory
# on_demand = true              # enable Files On-Demand via FUSE
# delta_poll_interval_secs = 30 # how often to poll for remote changes
# max_upload_threads = 4
# max_download_threads = 4
# excluded_patterns = ["*.tmp", "~$*", ".~lock.*", "desktop.ini", "thumbs.db"]
EOF
```

---

## Step 3 — Build and Install

```bash
cd /path/to/OneDriveForLinux

# One-shot build + install + (re)start of the systemd service:
./deploy.sh

# Or manually:
cargo build --release
mkdir -p ~/.local/bin
cp target/release/onedrive-daemon ~/.local/bin/
cp target/release/odctl ~/.local/bin/
cp target/release/onedrive-flyout ~/.local/bin/
```

> The systemd unit runs the daemon from `~/.local/bin` — make sure it is on your `PATH`.

---

## Step 4 — First Run (Authentication)

Run the daemon once in the foreground to complete authentication:

```bash
RUST_LOG=info onedrive-daemon
```

It will print a URL and a short code. Open the URL in any browser, enter the code, and sign in with your Microsoft account. The daemon will then start syncing automatically.

---

## Step 5 — Enable as a systemd User Service

```bash
mkdir -p ~/.config/systemd/user/
cp config/systemd/onedrive-linux.service ~/.config/systemd/user/

systemctl --user daemon-reload
systemctl --user enable --now onedrive-linux.service

# Check status
systemctl --user status onedrive-linux.service
journalctl --user -u onedrive-linux.service -f
```

---

## Using `odctl`

```bash
# Show sync status of all files
odctl status

# Pause / resume sync
odctl pause
odctl resume

# Force sync a specific file or directory
odctl sync ~/OneDrive/Documents/report.docx
odctl sync   # syncs the entire OneDrive root

# Keep files/folders always on device (downloads now, never evicted)
odctl pin ~/OneDrive/Documents ~/OneDrive/Photos/family.jpg

# Free up space — back to cloud-only placeholders
odctl unpin ~/OneDrive/Photos

# Are my pinned files actually on disk?
odctl pin-status
odctl pin-status ~/OneDrive/Documents

# Show current config
odctl config

# Re-authenticate via the running daemon (prints a device code)
odctl auth

# Sign out (removes saved tokens)
odctl auth --signout
```

---

## Files On-Demand

When `on_demand = true` (the default), the sync directory is mounted as a FUSE filesystem. Files appear with their real sizes but occupy **zero local disk space** until you access them. Opening a file triggers a transparent download — the same behaviour as OneDrive on macOS/Windows.

Use `odctl pin` to keep specific files or folders permanently on device — pinned items are downloaded immediately, survive delta syncs, and are never evicted until you `odctl unpin` them.

To disable Files On-Demand and always download everything:

```toml
on_demand = false
```

---

## Tray Icons

The daemon communicates icon state via the StatusNotifier/AppIndicator D-Bus protocol, using standard freedesktop icon names so any theme works out of the box:

| Icon name | Meaning |
|-----------|---------|
| `folder-cloud` | Up to date |
| `emblem-synchronizing` | Syncing |
| `dialog-error` | Sync error or sign-in required |
| `media-playback-pause` | Sync paused |

---

## Dolphin Overlay Icons (KDE)

`extensions/dolphin/` integrates OneDrive into KDE's file manager:

- **Overlay icons** — a `KOverlayIconPlugin` shows sync-state emblems (synced ✓, syncing, cloud-only, error) by reading the `user.onedrive.syncstate` extended attribute served by the FUSE mount.
- **Right-click menu** — an "OneDrive" submenu on any file or folder with *Always keep on this device*, *Free up space*, and *Sync now* (a KIO service menu calling `odctl`; requires `odctl` on your `PATH`).

```bash
cd extensions/dolphin
./build.sh && ./install.sh   # requires KDE Frameworks 6 dev packages
```

---

## Development

CI enforces formatting, lints, tests, and a dependency security audit on every PR:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Releases are automated: pushing a tag like `v0.1.0` builds the binaries and publishes a GitHub Release with a checksummed tarball.

---

## Architecture

```
crates/
  graph-client/   Microsoft Graph API client + OAuth2 device code flow
  sync-engine/    Delta sync, SQLite state, inotify watcher, conflict resolution
  vfs/            FUSE filesystem (Files On-Demand)
  daemon/         systemd entry point, D-Bus server, signal handling
  tray/           System tray via ksni (StatusNotifierItem)
  cli/            odctl — D-Bus client for controlling the daemon
```

---

## Troubleshooting

**"Config file not found"** — create `~/.config/onedrive-linux/config.toml` as shown above.

**FUSE mount fails** — ensure `fuse3` is installed and your user can access `/dev/fuse` (add to `fuse` group or use `allow_other` in FUSE options).

**"connect to daemon via D-Bus"** — the daemon must be running before using `odctl`. Start it with `systemctl --user start onedrive-linux.service`.

**Rate limiting** — the Graph API enforces per-user rate limits. The client respects `Retry-After` headers automatically.

---

## License

MIT
