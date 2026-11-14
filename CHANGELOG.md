# Changelog

All notable changes to Porch. Versions follow [semantic versioning](https://semver.org/).

## Unreleased

### Security
- `--private` now works wherever you put it. After the subcommand or app name (`porch run --private …`, `porch web --private …`, `porch --name web --private …`) and from the MCP `run` tool, it used to be ignored, so the app was offered to paired machines.
- An app launched with `--lan` no longer leaves the proxy in LAN mode for good. LAN mode from an app lasts until the proxy next restarts; `porch proxy start --lan` (or a startup service with `--lan`) still saves it. `porch proxy start --no-lan` turns it off, and `porch list` and `porch doctor` warn while the proxy accepts connections from other devices, including LAN mode saved by an earlier version.
- `porch clean` now unpairs from every paired machine (telling each one that answers), stops the discovery agent and its relays, removes discovery's start-at-login entry, and deletes the pairing keys and credentials, `--detach` logs, `trust.json`, Compose port records and leftover temporary files. Files in the state directory that Porch didn't create are kept and listed.
- Apps get `node_modules/.bin` directories on their PATH only from inside the project (up to the workspace or Git root, or your home directory), no longer from every directory up to `/` or `C:\`, where another user could plant a `node`. On macOS and Linux, directories owned by another user are skipped.
- With `HOME` unset, Porch finds your home directory in the user database instead of using `.porch` in the current directory, and stops with an error if it can't.
- `porch workspace` and the "no dev script" error escape control characters in script names and commands from `package.json` and `porch.json`, so a repository can't send terminal escape sequences.

### Fixed
- Launch options before `--name` (`-d`, `--detach`, `--private`, `--prefix=…`, `--app-port=…`, `--framework`, `--state-dir`) no longer make Porch treat the command as the app's name, and `porch --private -- --filter=web` passes `--filter=web` to the dev script. `porch web --name api …` is refused instead of quietly ignoring `--name`.
- On/off environment variables (`PORCH_PRIVATE`, `PORCH_HTTPS`, `PORCH_LAN`, `PORCH_CLEAN_URLS` and the rest) all accept `1`, `true`, `yes`, `on` and `0`, `false`, `no`, `off` in any case. Anything else is an error; `PORCH_PRIVATE=yes` used to mean off.
- Fields that a newer Porch adds to `state.json` and `proxy.json` are kept when this version rewrites them. Versions up to 0.5.2 drop them, which turned private apps shared and `--upstream https` aliases back into plain HTTP when an older `porch` was still running.

## 0.5.2 (2026-11-14)

### Fixed
- Apps on a paired machine load much faster through their `APP.MACHINE` URLs, especially pages with many requests such as Vite dev servers. The relay sends requests over one HTTP/2 connection to the other machine instead of opening a TLS connection for each, so over HTTPS (`porch setup`) a page's modules are fetched in parallel; they no longer fail with "Remote Porch app is unavailable" once more than 64 are in flight. The app catalog is refreshed in the background rather than while a request waits, and relay sockets disable Nagle's algorithm.
- Opening an app from the menu bar, the dashboard or `porch open --peer` no longer starts by connecting to the other machine: listing its apps goes through the proxy, which keeps that connection and catalog ready for the browser.
- `porch open --peer` and `porch copy --peer` suggest `porch setup --https` once when the URL is plain HTTP, where the browser can only fetch six files at a time.

## 0.5.1 (2026-11-13)

### Fixed
- On macOS, the menu bar icon's "Porch — your dev apps" tooltip could stay on screen after the menu closed. The icon no longer has a tooltip on macOS.
- `porch tray` on Windows couldn't find `porch-tray.exe` beside it.

## 0.5.0 (2026-11-08)

### Security
- Requests from other devices (LAN mode) can no longer set `X-Forwarded-*` headers, so they can't change the scheme or host your app sees.
- Paired machines can only open apps that run on this machine. An alias to another host is never relayed to a peer.
- The 404 page shows other devices app names only, not the ports and URLs behind them.

### New
- `porch run --detach` starts an app in the background and returns once it answers; `porch logs NAME` and `porch stop NAME` manage it. MCP gains matching `run`, `stop` and `logs` tools (the `apps` profile), so agents can start dev servers themselves.
- **URLs without a port, without sudo.** New installs serve `http://web.localhost` when the OS lets this user bind port 80 (macOS, Windows, Linux with a lowered `ip_unprivileged_port_start`), and fall back to 7462 otherwise. `porch setup` asks; `PORCH_CLEAN_URLS=0` opts out.
- Port and host flags for Django, Flask, FastAPI, uvicorn, gunicorn, Streamlit, MkDocs, Jupyter, Hugo, Jekyll and `python -m http.server`, including through `uv run`, `poetry run` and `bundle exec`.
- `--private` keeps an app or alias off paired machines.
- `http://localhost` is a dashboard of every running app.
- Remote aliases (`porch alias staging https://staging.example.com`) send the target's own `Host`, verify its certificate and rewrite its redirects to the Porch URL.
- Install with one command and no Python: `scripts/install.sh` (macOS, Linux) and `scripts/install.ps1` (Windows) verify the checksum and build attestation.
- Static, CLI-only Linux builds for x86_64 and ARM64 that run on any distribution.
- Release archives carry signed build-provenance attestations, and `porch self-update` verifies them when the GitHub CLI is installed (`PORCH_UPDATE_VERIFY=require` makes it mandatory).
- Agent skills (`skills/porch`, `skills/oauth`), docs for coding agents, troubleshooting and recipes, examples, and a JSON Schema for `porch.json`.
- `porch.name`, `porch.ports` and `porch.enable` Compose labels.
- `porch alias NAME PORT --upstream https|https://HOST|http://HOST` talks TLS or a custom Host to a loopback service, verifying its certificate (`--insecure` to opt out per alias).
- The proxy caps open connections and times out clients that don't send a request head.

### Fixed
- An app listening only on IPv6 loopback showed as "not listening" even though the proxy reached it.
- `porch --name NAME --state-dir DIR ...` failed with "unexpected argument".
- Alias targets with a query string silently lost it; they're now rejected with an explanation.
- `porch doctor` could print listener failures and then "No problems found". It now counts every failure and exits 1 while a problem remains.
- The MCP `doctor` tool, marked read-only, accepted `--fix --yes`. Read-only tools now accept only read-only arguments, and repairs moved to a separate `doctor_fix` tool.
- `porch workspace` printed raw package names; it now shows each package's real URL, folder and command.

### Improved
- The proxy keeps routes in memory between requests and re-reads them only when they change. With many routes it handles noticeably more requests per second.
- Every command and flag has help text, and `porch --help` includes examples. MCP tools have real descriptions.
- `porch list` and `porch doctor` use color and check marks (respecting `NO_COLOR`), and URLs are clickable in terminals that support it.
- A redesigned 404 page in light and dark mode lists running apps, with a filter when there are more than five. When an app isn't listening yet, its tab shows a "waiting for the app" page and reloads by itself as soon as the app answers.
- `porch alias` prints the URL it registered, `porch proxy stop` confirms what it did, and `porch workspace` prints a readable table.
- An out-of-range alias port (`porch alias db 99999`) explains the problem instead of failing with a URL parse error.

## 0.4.2 (2026-11-07)
- The tray menu groups apps under a header for each machine.
- Clicking an app in the menu opens it right away, at the URL its row shows.
- App URLs show at the top level of the tray menu.

## 0.4.1 (2026-11-07)
- The test suite is deterministic. Fixed the product races behind its flakes.
- Release archives are 64% smaller: Porch.app executables are deduplicated, the build profile favors size, and native-tls is gone.

## 0.4.0 (2026-11-07)
- Windows support for the CLI, the tray and releases.
- Fixed CA cleanup and Windows trust.
- Code pairing authenticates with SPAKE2 before any credentials are exchanged.
- Every loopback spelling is blocked in proxied redirects.
- `**` works as a recursive glob in workspace patterns.
- HTTPS without sudo: the proxy keeps its high port and trusts its CA for your user only.
- The proxy toggle in the menu header is a switch.

## 0.3.2 (2026-11-07)
- Launch preparation: README, MIT license and security policy.
- The menu bar app and peer catalogs show each app's real health.

## 0.3.1 (2026-11-07)
- Discovery can start at login, and each paired machine is listed once.

## 0.3.0 (2026-11-06)
- New tray and app icon.
- One-command code pairing, with readable URLs for apps on other machines.
- The first run needs no sudo and uses port 7462. Adds a listen check, stale-route cleanup, `porch setup` and `porch doctor --fix`.
- Tagged releases, a Homebrew formula and `porch self-update`.
