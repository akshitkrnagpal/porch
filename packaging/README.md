# Release archives

Each release archive contains:

- macOS (`aarch64-apple-darwin`, `x86_64-apple-darwin`): `porch`, `porch-tray`, `porch-login`, `install.py` and `Porch.app`.
- Linux (`x86_64-unknown-linux-gnu`): `porch`, `porch-tray` (a GTK 3 / AppIndicator tray), `porch-login` and `install.py`.
- Linux, static (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`): `porch` and `install.py` only. These run on any distribution, including Alpine and ARM64 servers.
- Windows (`x86_64-pc-windows-msvc`, preview): `porch.exe`, `porch-tray.exe` (a notification-area app) and `install.py`. There is no `porch-login` on Windows.

Archives are attached to tagged GitHub Releases along with a `SHA256SUMS` file. They aren't signed or notarized. `homebrew/porch.rb` is the Homebrew formula template that `scripts/render_formula.py` fills in for each release.

**What's in `Porch.app`.** The app runs the same `porch-tray` as the archive root and carries its own `porch` in `Porch.app/Contents/MacOS`, so it keeps working wherever you move it, even without the command-line install. In the download, those two files are tar hard links to the files at the archive root, so each program is downloaded once. The usual extractors (`tar`, Finder's Archive Utility, Homebrew, `install.py`, `porch self-update`) write them back as ordinary, complete files; nothing extracted is a symlink or depends on a relative path. A tool that can't recreate tar hard links would leave the app without its executables, so use `tar -xzf`. Homebrew installs only `porch`.

To pair machines over your LAN or Tailscale, see the [guide to apps across machines](docs/discovery.md) included in the archive ([`../docs/discovery.md`](../docs/discovery.md) in a source checkout).

## Install

Extract the archive, change into its folder, and run:

```sh
python3 install.py
# Or, without extracting first:
python3 install.py --archive porch-VERSION-TARGET.tar.gz
```

On macOS and Linux this installs `porch`, `porch-tray` and `porch-login` in `~/.local/bin`, and on macOS also `~/Applications/Porch.app`. On Windows (`py install.py`) the binaries go to `%LOCALAPPDATA%\Programs\Porch\bin`. If the folder isn't on your PATH, the installer prints the line to add. It writes each binary to a temporary file and renames it into place, so a running copy is never overwritten mid-write.

The installed `Porch.app` is a complete copy with its own `porch` inside: the terminal runs `~/.local/bin/porch` and the menu runs the copy in the app. Both come from the same release. `porch self-update` replaces both, but it finds the app only in `~/Applications` or in the folder named by `PORCH_APP_DIR`. Rerunning a newer `install.py` (or an older one with `--archive`) or `brew upgrade akshitkrnagpal/tap/porch` also upgrades.

Installing doesn't touch running apps, the proxy, sharing, trust stores, the hosts file or login startup. `--cli-only` installs only the CLI, and `--prefix PATH` and `--app-dir PATH` change where files go. To update later, run `porch self-update`.

## Launch the menu

```sh
open ~/Applications/Porch.app    # macOS
porch tray                       # macOS, Linux or Windows; same as running porch-tray directly
```

The roof icon opens a menu that lists each app's URL, this machine's first and then each paired machine's, split by separators. Click a URL to open it; on macOS hold Option to copy it instead (Linux and Windows show **Open in browser** and **Copy URL** in a small submenu). The menu also has **Refresh apps**, **Reconnect discovery** and a switch to start or stop the proxy. **Quit Porch menu** closes only the menu: apps, the proxy and the discovery agent keep running. If a CLI action fails, the error appears in the menu. Actions that need sudo have to be run in a terminal.

The tray runs CLI commands in a background thread and keeps all UI work on the main thread. `PORCH_STATE_DIR` or `porch-tray --state-dir DIR` points it at a different state directory. It looks for the CLI next to itself, then at `~/.local/bin/porch` (`%LOCALAPPDATA%\Programs\Porch\bin\porch.exe` on Windows); set `PORCH_BIN` to override.

macOS may ask you to approve an unsigned app downloaded from the internet in System Settings → Privacy & Security; Porch doesn't bypass Gatekeeper. A bundle you build yourself opens with `open dist/Porch.app`.

## Linux desktop requirements

The `porch` CLI doesn't link GTK, AppIndicator or OpenSSL and works without a display. Only `porch-tray` needs the desktop libraries. The `linux-gnu` binaries need glibc 2.39 or newer; on older distributions use a static `linux-musl` archive. Every archive has a signed build-provenance attestation (`gh attestation verify porch-*.tar.gz --repo akshitkrnagpal/porch`), and `scripts/install.sh` installs the right one without Python.

Build dependencies on Debian and Ubuntu:

```sh
sudo apt-get install libgtk-3-dev libayatana-appindicator3-dev libdbus-1-dev
```

At runtime, Ubuntu 24.04 needs `libgtk-3-0t64` and `libayatana-appindicator3-1` (`libgtk-3-0` on older Debian and Ubuntu), plus a session D-Bus and a StatusNotifier/AppIndicator host. KDE and most other desktops include one; GNOME usually needs the AppIndicator extension. Without a graphical session or tray host, the tray exits and suggests the equivalent CLI commands.

## Start the menu at login

Nothing starts at login unless you ask. On macOS and Linux:

```sh
porch-login enable
porch-login status
porch-login disable
```

On macOS this writes `~/Library/LaunchAgents/dev.porch.tray.plist`, logs to `~/Library/Logs/Porch/tray.log`, and starts the menu right away; `disable` unloads it. On Linux it writes `$XDG_CONFIG_HOME/autostart/porch-tray.desktop` (default `~/.config/autostart`), which takes effect at your next login; `disable` removes it but doesn't quit a running tray. Neither turns on sharing or changes the proxy. `--binary ABSOLUTE_PATH` picks a different tray executable. On macOS, a `PORCH_STATE_DIR` set when you run `enable` is saved in the LaunchAgent. On Linux, set it in your desktop session's environment or start the tray by hand.

This only starts the menu. To start the proxy or the discovery agent at login, use `porch service install` or `porch discovery autostart enable`.

## Build and package

```sh
cargo build --release --bin porch
cargo build --release --features tray --bin porch-tray
python3 scripts/package.py --target aarch64-apple-darwin
# Intel Mac: --target x86_64-apple-darwin
# Linux: --target x86_64-unknown-linux-gnu
# Windows: --target x86_64-pc-windows-msvc
```

The release profile is tuned for size (`opt-level = "s"`, fat LTO, one codegen unit, stripped symbols), so release builds take a few minutes. The script creates `dist/porch-VERSION-TARGET.tar.gz`, and on macOS also `dist/Porch.app`. For a cross-compiled build, pass `--binaries target/TARGET/release`. `python3 scripts/release_size.py BEFORE_DIR AFTER_DIR` compares two sets of archives; `docs/release-size.md` in a source checkout has the current numbers.

CI builds the tray on macOS, Linux and Windows, tests the menu contents and actions, and checks that the Linux tray fails cleanly without a display. The release workflow publishes a GitHub Release only for a pushed `v*` tag that matches the version in `Cargo.toml`; running it by hand only uploads build artifacts. Whether the tray icon actually shows on a Linux desktop, how the macOS menu behaves when clicked, and whether login startup works still have to be checked by hand on a real desktop.
