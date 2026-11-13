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
- The startup service (`porch service install`, or "Start at login" in `porch setup`) runs the proxy as you, not as root: a LaunchAgent on macOS, a systemd user unit on Linux, a logon task on Windows. It needs no sudo. Before, it ran as root a `porch` binary your account could overwrite, so anything running as you could gain root at the next boot. On Linux, a port below 1024 that needs privileges uses a system unit that still runs as you, with only the right to bind that port, from a root-owned copy of porch. `porch service status` and `porch doctor` point out a service from an older version, and `porch service install` or `porch setup` replaces it.
- New local CAs can sign TLS server certificates only, and `porch trust` trusts the CA for TLS only on macOS. Before, someone who stole the CA key could also sign email or code-signing certificates your account trusted. Your existing CA is kept so its trust stays in place; `porch doctor` explains how to replace it, and on macOS running `porch trust` again limits its trust to TLS.
- Porch no longer keeps the private key of a CA it has replaced, and deletes ones older versions kept.
- A web page that requests made-up `*.localhost` HTTPS names can no longer use up the certificate budget your apps need: names that serve something (routes, the dashboard, apps on paired machines) have their own budget and stay cached.
- The Tailscale sharing lock no longer sits under a guessable name in a shared `/tmp`, where another user on a Linux machine could create it first and make `--tailscale` fail.

### Fixed
- Launch options before `--name` (`-d`, `--detach`, `--private`, `--prefix=…`, `--app-port=…`, `--framework`, `--state-dir`) no longer make Porch treat the command as the app's name, and `porch --private -- --filter=web` passes `--filter=web` to the dev script. `porch web --name api …` is refused instead of quietly ignoring `--name`.
- On/off environment variables (`PORCH_PRIVATE`, `PORCH_HTTPS`, `PORCH_LAN`, `PORCH_CLEAN_URLS` and the rest) all accept `1`, `true`, `yes`, `on` and `0`, `false`, `no`, `off` in any case. Anything else is an error; `PORCH_PRIVATE=yes` used to mean off.
- Fields that a newer Porch adds to `state.json` and `proxy.json` are kept when this version rewrites them. Versions up to 0.5.2 drop them, which turned private apps shared and `--upstream https` aliases back into plain HTTP when an older `porch` was still running.
- `porch stop`, `--force` and `porch prune` could stop a server Porch never started. When a command exits successfully, Porch hands its route to a daemon on the app's port only if the command started it. A server that was already running and only asked to listen, such as Docker Desktop for `docker run -p` or a running PM2 daemon for `pm2 start`, is left alone. A daemon outside the command's process group is stopped on its own, never with its whole group, and a handover that can't complete no longer kills anything.
- A route left over from before a reboot, or whose process ids were reused, no longer matches someone else's processes, so `porch prune --force`, `porch doctor --fix` and relaunching the same name can't stop them.
- Processes an app leaves behind (for example a dev server behind `npm run`) get SIGTERM and up to 3 seconds to clean up before SIGKILL, instead of SIGKILL a moment later.
- Background app logs: replacing an app with `--force` no longer fills the new log with NUL bytes or the old app's output. The previous run's log is kept as `NAME.log.1`, logs over 10 MB move to `NAME.log.1`, and logs of apps stopped more than 14 days ago are deleted. `porch logs --lines N` reads only the end of the log, and a log with invalid UTF-8 shows with `�` instead of failing with "no log for NAME". `proxy.log` is capped the same way.
- With HTTPS on, an app's existing `NODE_EXTRA_CA_CERTS` (a corporate CA, say) is kept: Porch points the app at a bundle of that file and the local CA instead of replacing it.
- Two commands starting together after an update no longer both restart the proxy. A command waiting for another one that is asking for your password (sudo or CA trust) waits for it instead of failing with "another proxy startup did not finish", and an exited proxy's error shows the last 20 lines of `proxy.log` rather than all of it.
- `porch setup` defaults "Start Porch automatically at login?" to whether the startup service is installed. After `porch discovery autostart enable`, `porch setup --yes` no longer tries to install the system service with sudo, and keeping the default no longer removes sharing's login item.
- Requests to an app whose port is reserved but which hasn't started yet (while a sharing tunnel starts, say) show the "waiting for the app" page at once instead of hanging, and `porch run --detach` waits until the app has actually started.
- On Linux, a running app that saw the proxy stop after `porch self-update` couldn't restart it. It now uses the updated binary, and says so if the restart fails.
- `porch service install` can be run again: it replaces and restarts the service with the new options instead of failing because the old one is loaded. `porch service uninstall` and `porch clean` no longer stop when the service isn't loaded, and always remove its files.
- macOS and Windows trust prompts are no longer cancelled after 30 seconds; Porch waits up to 10 minutes for your password, Touch ID or confirmation.
- `porch clean` and `porch trust` no longer fail when the CA was already removed by hand (in Keychain Access or the Windows certificate manager), or when Porch's copy of it is gone. Removing system-wide trust on macOS now also removes its trust setting, not only the certificate.
- Updating the hosts file can no longer leave it empty or cut short if Porch is interrupted or the disk is full, and an unchanged file is never rewritten. On Windows, `porch hosts sync` and `porch hosts clean` failed with a sharing violation and now work.
- Changing `--tld` back and forth no longer makes a new CA, and a new trust prompt, every time: a new CA keeps the domains of the one it replaces. An interrupted CA creation no longer stops the proxy from ever starting with a bare "No such file"; Porch makes a new CA, or says how to recover a damaged one.
- A proxy that runs for a year or more renews its certificates before they expire instead of serving expired ones until restarted. Issuing a certificate no longer re-reads the CA each time.
- Linux system-wide trust installs the CA readable by every user, so tools that read the system certificate directory as a normal user can use it.
- In the foreground, `porch compose up` checks Docker every two seconds instead of twice a second, no longer rewrites Porch's state when nothing changed, and responds to Ctrl-C at once even when Docker is slow. Ctrl-C now gives Compose time to stop containers gracefully (Porch used to kill it after 5 seconds, leaving slow containers running), and a second Ctrl-C forces it.
- `porch compose down` releases the project's ports and deletes the resolved Compose model Porch kept, which could contain values from `.env` files. Projects whose directory is gone are forgotten.
- One unreadable sharing record no longer stops `porch prune` and `porch clean`, and a missing `tailscale` command no longer blocks them. Sharing records are written atomically.
- Ending a shared app gives ngrok and cloudflared a moment to close their tunnels before they are killed, without blocking the app's other work.
- LAN mode keeps announcing your apps when one route can't be announced or the mDNS service hiccups, instead of stopping until the proxy restarts.
- `porch-login enable` no longer turns off an already enabled menu bar login item when launchd is slow to unload the old one.

### Improved
- The proxy no longer reads the process table every 5 seconds when no app is running, nor connects to every app each time.
- Stopping, replacing and pruning apps check processes without running `ps` for each check, and look for a free app port without blocking other porch commands on `lsof`.
- Launching in a workspace looks up the Git worktree and the workspace's packages once instead of once per package, and `porch run --detach` doesn't repeat it in the background launcher. A command that exits without handing its port to a daemon no longer runs `lsof` for a second.

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
