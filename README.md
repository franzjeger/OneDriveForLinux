# OneDrive for Linux

A native OneDrive client for Linux featuring:

- **Files On-Demand** via FUSE — files appear in your filesystem at full size but are only downloaded when accessed
- **Background daemon** with systemd user service
- **System tray** integration (KDE/GNOME via StatusNotifier/AppIndicator)
- **OAuth2 device code flow** — no client secret required
- **Full Microsoft Graph API** sync with delta polling
- **Conflict resolution** — local conflicts are renamed with timestamps
- **`odctl`** CLI for status, pause/resume, and forced sync

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
cargo build --release

# Install binaries
cp target/release/onedrive-daemon ~/.cargo/bin/
cp target/release/odctl ~/.cargo/bin/
```

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

# Show current config
odctl config

# Sign out (removes saved tokens)
odctl auth --signout
```

---

## Files On-Demand

When `on_demand = true` (the default), the sync directory is mounted as a FUSE filesystem. Files appear with their real sizes but occupy **zero local disk space** until you access them. Opening a file triggers a transparent download — the same behaviour as OneDrive on macOS/Windows.

To disable Files On-Demand and always download everything:

```toml
on_demand = false
```

---

## Tray Icons

The daemon communicates icon state via the StatusNotifier/AppIndicator D-Bus protocol. Name your icons in your theme's `icons/` directory:

| Name | Meaning |
|------|---------|
| `onedrive-idle` | Up to date (cloud with checkmark) |
| `onedrive-sync-0` … `onedrive-sync-3` | Syncing animation frames |
| `onedrive-error` | Sync error (red X) |
| `onedrive-paused` | Sync paused |

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
