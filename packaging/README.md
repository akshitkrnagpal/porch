# Porch CLI and native menu

The archive contains `porch`, `porch-tray`, `porch-login`, and `install.py`. The
macOS ARM64 archive also includes `Porch.app`. Linux x86_64 uses a GTK 3 /
AppIndicator tray. Windows is deferred. Artifacts are private GitHub Actions
artifacts, not signed/notarized public releases.

For LAN/Tailscale enrollment, security, and the wire protocol, see the
[two-machine guide](docs/discovery.md) included in the archive (or
[repository guide](../docs/discovery.md) in a source checkout).

## Install and launch

Extract the archive, enter its directory, then run:

```sh
python3 install.py
```

This installs binaries in `~/.local/bin`, using a temporary file, fsync, and
rename rather than truncating a running executable. On macOS it also installs
`~/Applications/Porch.app`. Add `~/.local/bin` to PATH if needed. Existing apps,
proxy sessions, discovery sharing, trust stores, hosts files, and login startup
are unchanged. `--cli-only` installs just the headless CLI; `--prefix PATH` and
`--app-dir PATH` select user-controlled destinations.

macOS:

```sh
open ~/Applications/Porch.app
# Or: porch tray
```

Linux with a graphical session and a tray host:

```sh
porch tray
# Or: porch-tray
```

The roof icon opens a native menu: local apps, grouped paired machines, health,
Open in browser, Copy URL, Refresh apps, Reconnect discovery, and proxy start or
stop. Quit Porch menu closes only the menu process. App processes, the proxy,
and an explicitly enabled discovery agent continue. A failed CLI action appears
in the menu; proxy privilege prompts must be completed in a terminal.

The tray polls the CLI in a background Tokio worker; native UI work stays on the
OS main thread. `PORCH_STATE_DIR` or `porch-tray --state-dir DIR` selects an
isolated state directory. The CLI is resolved beside the tray, then at
`~/.local/bin/porch`; `PORCH_BIN` explicitly overrides it.

Downloaded unsigned macOS bundles may require the normal macOS Open / Privacy &
Security approval. Porch does not bypass Gatekeeper. A locally built bundle can
be launched with `open dist/Porch.app`.

## Linux dependencies and desktop limits

Headless `porch` does not link GTK or AppIndicator and works without DISPLAY.
Only `porch-tray` needs these desktop libraries. Prebuilt Linux GNU artifacts
require glibc 2.39 or newer; build from source on older distributions.
Debian/Ubuntu build dependencies:

```sh
sudo apt-get install libgtk-3-dev libayatana-appindicator3-dev libdbus-1-dev
```

Runtime packages on Ubuntu 24.04 are `libgtk-3-0t64` and
`libayatana-appindicator3-1` (older Debian/Ubuntu use `libgtk-3-0`). A session
D-Bus and a StatusNotifier/AppIndicator host are also required. KDE and many
other desktops include one. GNOME usually needs an AppIndicator/KStatusNotifier
extension. The tray exits with a CLI fallback hint when no graphical session or
tray host is present; it never substitutes a full-page browser dashboard.

## Optional login startup

Nothing installs login startup by default. After installation, explicitly run:

```sh
porch-login enable
porch-login status
porch-login disable
```

macOS uses only `~/Library/LaunchAgents/dev.porch.tray.plist` and logs to
`~/Library/Logs/Porch/tray.log`. Enabling starts the menu in the current user GUI
session; disabling unloads that menu. Linux writes only a marked
`$XDG_CONFIG_HOME/autostart/porch-tray.desktop` (default `~/.config/autostart`)
for the next graphical login; disabling does not terminate the current tray.
Neither option enables app sharing or changes the proxy. `--binary ABSOLUTE_PATH`
selects the menu executable. macOS records an explicit `PORCH_STATE_DIR` when
set while enabling. Custom Linux state can be supplied by the desktop session
environment or by launching the tray manually.

## Build and package

```sh
cargo build --release --bin porch
cargo build --release --features tray --bin porch-tray
python3 scripts/package.py --target aarch64-apple-darwin
# On Linux: --target x86_64-unknown-linux-gnu
```

The package script creates `dist/porch-VERSION-TARGET.tar.gz`, and on macOS also
`dist/Porch.app`. Cross-target builds can use `--binaries target/TARGET/release`.
The CI tray job compiles each native platform, checks menu model/actions, and
runs a Linux no-DISPLAY fallback. The release-artifact workflow packages both
binaries without creating a GitHub Release. Physical Linux desktop visibility,
macOS rendered menu interaction, and login startup behavior require native GUI
verification; compilation/model tests alone do not establish those observations.
