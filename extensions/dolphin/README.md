# Dolphin integration

Two separate pieces:

- **`onedrive.desktop`** — the right-click "OneDrive" submenu (pin / unpin /
  sync now). A plain file; the main `install.sh` writes it directly.
- **`onedrive-overlay.cpp`** — a `KOverlayIconPlugin` that draws sync-state
  emblems on file icons. This is C++ and has to be compiled against the KDE
  Frameworks 6 development headers, so the main installer only builds it when
  asked with `--with-dolphin-overlay`.

## Building the overlay plugin

```bash
curl -fsSL .../install.sh | bash -s -- --with-dolphin-overlay
```

The installer checks for the build dependencies, installs them with your
package manager if it recognises the distribution, then configures and builds
with CMake.

To build it by hand:

```bash
cmake -B build -S . -DCMAKE_BUILD_TYPE=Release
cmake --build build -j"$(nproc)"
sudo cmake --install build
kquitapp6 dolphin   # restart Dolphin to load the plugin
```

### If configure fails on CMAKE_LIBRARY_OUTPUT_DIRECTORY

`kcoreaddons_add_plugin()` refuses to run when `CMAKE_LIBRARY_OUTPUT_DIRECTORY`
is unset, and reports "set it explicitly or include KDECMakeSettings" even
though `KDECMakeSettings` *is* included — ECM 6.28 with CMake 4 does not always
set it. The CMakeLists sets the output directories itself for that reason; if
you see this error, you are building an older copy of the sources.

### Why it installs system-wide

Qt only scans the plugin directories in `QCoreApplication::libraryPaths()` —
by default the system Qt plugin directory, not `~/.local`. A plugin installed
under `$HOME` is therefore never loaded unless `QT_PLUGIN_PATH` is also set,
which is easy to get wrong and hard to debug (the plugin is simply, silently
absent). Installing into the directory reported by `qtpaths6 --plugin-dir` is
the one location Dolphin is guaranteed to look in.

## How the state reaches the plugin

The FUSE filesystem serves a `user.onedrive.syncstate` extended attribute for
every item in the sync directory. The plugin reads it with `getxattr` on a
background thread and caches the result for five seconds — `getOverlays()` is
called from Dolphin's main thread and must never block on a network-backed
filesystem.

The values are produced by `crates/vfs/src/filesystem.rs`:

| Value      | Meaning                                | Emblem                |
|------------|----------------------------------------|-----------------------|
| `synced`   | On disk and matching OneDrive           | `vcs-normal`          |
| `pinned`   | Always kept on device                   | `vcs-normal`          |
| `syncing`  | Transfer in progress                    | `vcs-update-required` |
| `cloud`    | Cloud-only placeholder                  | `onedrive-cloud`      |
| `partial`  | Folder with a mix of both               | `onedrive-partial`    |
| `local`    | Edited here, not yet uploaded           | `onedrive-upload`     |
| `error`    | Sync failed                             | `vcs-conflicting`     |
| `conflict` | Changed in both places                  | `vcs-conflicting`     |

Adding a state on the Rust side means adding it to `overlaysForState()` too —
an unmapped state renders no emblem at all rather than failing visibly.

You can check what the filesystem reports for any file:

```bash
getfattr -n user.onedrive.syncstate ~/OneDrive/some-file
```
