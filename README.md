<p align="center">
  <img src="assets/icon/app-tile-256.png" alt="Porch icon" width="128" height="128">
</p>

<h1 align="center">Porch</h1>

<p align="center">
  Stable, named URLs for your dev servers, on this machine and every machine you pair with.
</p>

<p align="center">
  <a href="https://github.com/akshitkrnagpal/porch/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/akshitkrnagpal/porch?label=release"></a>
  <a href="https://github.com/akshitkrnagpal/porch/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/akshitkrnagpal/porch/ci.yml?branch=main&label=CI"></a>
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/github/license/akshitkrnagpal/porch"></a>
  <img alt="macOS and Linux" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-555">
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#open-apps-on-your-other-machines">Other machines</a> ·
  <a href="docs/reference.md">Reference</a>
</p>

---

Your web app is on 3000. Or was it 5173? The API took 8787 today because something else had 8080. Porch ends the guessing. Run your dev command through it and the app gets a name you can remember:

```console
$ porch run pnpm dev
http://web.localhost:7462 -> 4480
```

Porch picked a free port, handed it to your app as `PORT`, started a small local proxy, and routed `web.localhost` to it. Restart tomorrow and the URL is the same.

Then pair your laptop with your desktop, and the desktop's apps open by name too: `http://web.mac-mini.localhost:7462`.

## Why Porch

- **Names, not port numbers.** `web.localhost`, `api.localhost`, `docs.localhost`. Bookmarks keep working and OAuth callbacks stop breaking.
- **No sudo, no `/etc/hosts`.** Browsers resolve `*.localhost` on their own, and the proxy runs on port 7462 as your user. HTTPS is one opt-in flag away.
- **Knows your framework.** Vite, Astro, Angular, React Router, Rsbuild, Expo, React Native and Wrangler get the right port and host flags without any config.
- **Monorepos in one command.** Run `porch` at a pnpm, npm, Yarn or Bun workspace root and every package comes up with its own URL. Turborepo keeps its task graph and TUI.
- **Your other machines, one click away.** Pair two machines with a 6-digit code. Over your LAN or Tailscale, the connection is encrypted and pinned to that machine's certificate.
- **A menu bar app.** See every app on every paired machine, open it, copy its URL, or start and stop the proxy.

## Install

**Homebrew** (macOS and Linux, CLI only):

```sh
brew install akshitkrnagpal/tap/porch
```

**Release download** (CLI plus the menu bar app). Grab `porch-<version>-<target>.tar.gz` from the [latest release](https://github.com/akshitkrnagpal/porch/releases/latest), then:

```sh
tar -xzf porch-*-aarch64-apple-darwin.tar.gz
python3 porch-*-aarch64-apple-darwin/install.py
```

The installer puts `porch` in `~/.local/bin` and, on macOS, `Porch.app` in `~/Applications`. Every release ships a `SHA256SUMS` file to check the download against.

**From source:**

```sh
cargo install --git https://github.com/akshitkrnagpal/porch porch
```

Keep it current with `porch self-update`. For Homebrew installs it runs `brew upgrade` for you.

## Quick start

```sh
cd my-app
porch                      # runs the package's "dev" script at http://my-app.localhost:7462
porch run pnpm dev         # any command; the name comes from package.json or the folder
porch api node server.js   # pick the name yourself: http://api.localhost:7462
porch alias db 5432        # name something Porch didn't start
porch list                 # every route and whether it's answering
```

```console
$ porch list
HEALTH         URL -> TARGET
ok             http://web.localhost:7462  -> http://127.0.0.1:4480 (pid 28686)
not listening  http://db.localhost:7462  -> http://127.0.0.1:5432
```

Apps receive `PORT`, `HOST` and `PORCH_URL`. If an app ignores `PORT` and listens somewhere else, Porch says which port it found and how to point the route at it. When something looks off, `porch doctor --fix` finds and repairs the usual problems: a busy port, a dead route, a proxy left over from an older version.

Want a guided first run? `porch setup` asks four questions (port, HTTPS, start at login, sharing) and is safe to run again.

## Open apps on your other machines

On the machine with the apps:

```console
$ porch pair
Pairing code: 482913
```

On the machine you're sitting at:

```console
$ porch pair 482913
Paired with Akshits-Mac-mini.local
```

Now both machines see each other's apps:

```sh
porch list --all                  # local apps and every paired machine's apps
porch open web --peer akshits-mac-mini   # http://web.akshits-mac-mini.localhost:7462
```

Remote apps get stable, readable URLs served through your own local proxy, so WebSockets, streaming and hot reload work as they do locally. Porch finds your machines over the LAN and over Tailscale. Nothing is shared until you pair, and `porch unpair` revokes access on both sides.

To keep a machine shared across reboots, run `porch discovery autostart enable` on it. See [Apps across machines](docs/discovery.md) for the security model and the protocol.

## Menu bar app

<p align="center">
  <img src="assets/screenshots/menu-bar.png" alt="The Porch menu bar app listing this machine's running apps (docs, api, web) and a paired Mac mini" width="430">
</p>

On macOS, open `Porch.app`. On Linux, run `porch tray` (GTK 3 with an AppIndicator host).

The menu lists local apps and paired machines with their status. Click an app to open it, copy its URL, refresh, pair a new device, or start and stop the proxy. The icon shows solid dots while the proxy runs, dims them when it stops, and adds a badge when a paired machine is reachable. To launch it at login, run `porch-login enable`.

## More

- **Share outside your network:** add `--tailscale`, `--funnel`, `--ngrok` or `--cloudflare` to a launch for a public or tailnet URL. Porch cleans it up when the app exits.
- **Docker Compose:** `porch compose up` gives each HTTP service a name and a stable port.
- **Git worktrees:** a linked worktree on branch `fix/login` serves at `login.web.localhost`, so two checkouts of one app never collide.
- **HTTPS:** `porch proxy start --https` serves `https://web.localhost:7462` with HTTP/2 and a local certificate authority trusted for your user, with no sudo (macOS asks for your password or Touch ID once). Add `--port 443` to drop the port number; binding 443 asks for sudo. `porch trust --system` trusts the CA machine-wide for tools that ignore per-user stores.
- **LAN devices:** `--lan` publishes `web.local` over mDNS for phones and tablets on the same network.
- **Coding agents:** `porch mcp` is a stdio MCP server, so your agent can list routes and look up URLs.

## Coming from Portless

Porch is a standalone Rust rewrite of [Portless](https://github.com/vercel-labs/portless), with pairing across machines and a native menu bar app added. It reads `portless.json`, the `portless` key in `package.json` and `PORTLESS_*` variables. Porch's own `porch.json`, `porch` key and `PORCH_*` variables take precedence when both exist. The [feature comparison](docs/feature-comparison.md) lists every capability and how it is tested.

## Platform support

| Platform | Status |
| --- | --- |
| macOS, Apple Silicon | Supported, tested in CI |
| macOS, Intel | Release builds, not tested in CI |
| Linux x86_64 (glibc 2.39+) | Supported, tested in CI |
| Windows | Not yet |

macOS builds are not notarized. On older Linux distributions, build from source.

## Documentation

- [Command and configuration reference](docs/reference.md)
- [Apps across machines](docs/discovery.md)
- [Feature comparison with Portless](docs/feature-comparison.md)
- [Release archive contents](packaging/README.md)

## Contributing

Issues and pull requests are welcome. To build and test:

```sh
cargo build --release
cargo test --all-targets
```

The tests start real processes, so they need Node.js with npm, Python 3 and Git. They use temporary state and never touch your hosts file, trust store or startup services. The menu bar app builds with `cargo build --release --features tray --bins`.

To cut a release, bump `version` in `Cargo.toml`, merge, then push a matching tag such as `v0.3.2`. The [release workflow](.github/workflows/release.yml) builds the archives and publishes the GitHub Release.

## License

[MIT](LICENSE)
