# Changelog

All notable changes to Porch. Versions follow [semantic versioning](https://semver.org/).

## 0.6.0 (2026-11-14)

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
- Made-up names under a paired machine (`r0.mini.localhost`, `r1.mini.localhost`, …) no longer use up that budget either. Porch doesn't ask the paired machine which apps it has, so any such name used to count as a real app and could block first-time HTTPS connections to your local apps and the dashboard. Names under paired machines now have a budget of their own.
- The Tailscale sharing lock no longer sits under a guessable name in a shared `/tmp`, where another user on a Linux machine could create it first and make `--tailscale` fail.
- Other devices (LAN mode) can no longer set any forwarding header your app might trust: every `X-Forwarded-*` header (including `X-Forwarded-Prefix` and `X-Forwarded-Ssl`), `Forwarded`, `X-Real-IP`, `X-Client-IP`, `True-Client-IP`, `X-Original-*` and `X-Rewrite-URL` are removed.
- One device on your network can no longer tie up the proxy (LAN mode). Each device gets a share of the connections, connections doing nothing are closed after a minute, a request body that stalls for a minute is cut off, and connections from devices that left the network are detected and closed. Event streams and WebSockets keep working.
- An alias to a URL with a path (`http://host/base`) could turn a backend redirect to `/base//other.site` into a redirect to another site. Such redirects now stay on the Porch URL.
- A plain-HTTP request with an `Upgrade` header other than `websocket` (such as `h2c`) no longer skips the redirect to HTTPS.
- Web pages on other sites can no longer make the dashboard's app list ask every paired machine for its apps.
- A port below 1024 without sudo is bound on loopback where the OS allows it (Linux, Windows), not on every network interface.
- `porch unpair` now revokes a machine's access when the two machines were enrolled with `porch peers add` and a pair token. Before, only code-paired machines were revoked, even though `unpair` reported success. Machines enrolled by Porch 0.5.2 or earlier aren't recorded against their credential, so `porch unpair` now says it can't tell which credential is theirs and points to `porch discovery revoke`, instead of claiming their access is revoked.
- A machine pairing with this one can no longer take over the identity, URL and browser cookies of a machine that is already paired by claiming its ID. Pairing is refused unless it's the same machine (the same certificate), with a message to run `porch unpair` first if that machine was reinstalled. The joining machine also refuses a machine that claims to be itself.
- The five-wrong-codes limit now holds when many guesses arrive at once. Before, a device on the network that started several pairing attempts in parallel could test up to sixteen codes instead of five.
- Devices on your network can no longer lock paired machines out of the discovery agent by holding its connections open. Each address gets only a few connections until it authenticates, connections that never authenticate are closed when idle, and addresses of paired machines keep their headroom. The agent also accepts more connections (256).
- Apps opened from a paired machine no longer receive `X-Forwarded-For`, `Forwarded`, `X-Real-IP`, `X-Original-*` or similar headers that the other machine supplied. The agent sets `X-Forwarded-For`, `-Host`, `-Proto` and `-Port` itself.
- Names and status messages from a paired machine can't carry terminal escape sequences or invisible right-to-left and zero-width characters, and machine and app IDs can't start with `-`.
- A paired machine can no longer rename itself to another paired machine's name. `--peer` and `porch unpair` look a machine up by its ID first, then its URL name, then its name.
- Abandoned pairing attempts can no longer fill the agent's table and block pairing: each address keeps only its newest two, and the oldest gives way when the table is full. Devices from more than 4,096 addresses no longer lock everyone out of pairing.
- `porch pair CODE` reads at most 4 KB of an error reply, so a machine answering on the network can't stream a large body into it.
- LAN discovery hints can't crowd out real machines: each advertised address and machine ID is limited to a few entries, a paired machine's hints displace others when the cache is full, and a paired machine's possible addresses are tried in parallel rather than one after another.
- The default MCP profile can no longer stop your apps. `alias_add` with `force` used to end the app holding the name, although only the `apps` profile is meant to stop processes. Without `apps`, `alias_add` now refuses a name that belongs to an app Porch started, and `force` only replaces another alias.
- Aliases an agent creates over MCP stay off paired machines unless it passes `private: false`.
- The MCP `logs` and `machines` tools describe their output as untrusted, since it comes from apps and paired machines.
- `porch self-update` and the install scripts say exactly what they checked. When the GitHub CLI can't verify the build attestation (not installed, too old, not logged in, offline), they print why, show `gh`'s error and continue, rather than skipping silently (the install scripts) or failing with "did not verify" (an installed but unusable `gh`). A real attestation mismatch still stops the install. SECURITY.md no longer says the attestation is always verified.
- The attestation check now also requires that the archive was built from the release's own tag (`--source-ref`, with gh versions that support it), and releases are published only from commits on `main`.
- `PORCH_UPDATE_VERIFY=require` is enforced in every case, including with `PORCH_UPDATE_API` set, where it used to install unverified. Values other than `require` or `auto` are rejected instead of meaning "not required".
- `GITHUB_TOKEN`/`GH_TOKEN` (and `gh auth token`) are sent only to `api.github.com`, never to a `PORCH_UPDATE_API` host or an asset URL it names, and `PORCH_UPDATE_API` must use https. The daily update check sends no token. A token GitHub rejects (expired, or for GitHub Enterprise) no longer breaks updates of a public repository: the request is retried without it.
- Release builds no longer restore caches, pin every GitHub Action to a commit, and build with `--locked`.

### Fixed
- `porch clean` with an isolated state directory (`PORCH_STATE_DIR=/tmp/x porch clean`) no longer stops your real proxy or removes its start-at-login entry. On macOS and Linux that entry is shared by every state directory, so `clean` now removes it only when it serves the state directory being cleaned, and otherwise says it left it in place. `porch service uninstall` is unchanged.
- `porch clean` with an isolated state directory also leaves the machine-wide startup service alone (the Linux system unit, or the root LaunchDaemon of Porch 0.5.2 and earlier) when it starts Porch for another state directory, instead of stopping and deleting it. A system unit that names no state directory is left in place too, with a note; `porch service uninstall` still removes it.
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
- The proxy no longer runs out of file descriptors after a few hundred connections, which showed up as "Waiting for the app to start" pages and failed requests under load. It raises its open-file limit at start and sizes its connection limit to fit, shared by all its listeners and WebSockets.
- Stopping or restarting the proxy lets requests in flight finish (for up to three seconds) instead of cutting them off.
- In LAN mode, a failure in network announcements (mDNS) no longer stops the whole proxy; announcements restart on their own.
- Browsers that send cookies over HTTP/2 in several fields now reach HTTP/1.1 apps with all their cookies in one `Cookie` header.
- "Waiting for the app to start" now appears only when an app on this computer isn't listening. A remote alias that can't be reached, a timeout, or an app that crashes mid-request gets an accurate error page that doesn't reload every two seconds, and a remote alias with an untrusted certificate says so.
- Proxy loops are detected when only one of the two hop-count headers is valid.
- Checking whether port 80 or 443 is free without sudo no longer opens a listener on it.
- The first page load after a paired machine restarts, your laptop wakes or the connection sits idle opens one connection to that machine instead of one per request. Pages with many requests no longer fail some of them with "Remote Porch app is unavailable" on that first load.
- A paused video or download from a paired machine no longer stalls every other request to it, and an app that doesn't read an upload no longer blocks other uploads to that machine (both machines need this version for uploads).
- When a paired machine is unreachable, a page's requests all report it after one attempt instead of each waiting out its own timeout.
- Paired machines' requests cost the discovery agent much less: it reuses its app list for up to a second instead of running `ps` and probing every app for each request, and rereads its state files only when they change. Revoking access, turning sharing off and starting an app still apply to the next request.
- The proxy reads the list of paired machines only when it changes, instead of on every request with no local app, including 404s.
- Long-polls, slow page renders and slow uploads through a paired machine no longer fail with 502 after 15 or 30 seconds. A response may now take up to 5 minutes from the end of the upload, then gets 504.
- Uploads over 16 MB to an app on a paired machine get a 413 that says so, instead of 502 "registered app is unavailable". A route that points to another host gets 403 instead of 500.
- Redirects and cookies containing non-ASCII characters are relayed correctly. Such redirects are now checked like any other (redirects to other loopback services were passed through), and cookies with UTF-8 values are no longer dropped.
- A paired machine whose address was stored without a trailing slash (`https://10.0.0.5:7463`) works for opening apps, not just for listing them.
- An app on a paired machine can call another app on the same machine, such as `web.mac-mini.localhost` calling `api.mac-mini.localhost`. Pages from other machines and other sites are still refused.
- `porch discovery relay` keeps running after a transient error accepting a connection.
- A connection left over from a refused WebSocket upgrade no longer holds one of the paired machine's connection slots.
- Start at login (`porch discovery autostart`) records the link you run Porch through (a Nix profile, `~/.local/bin`) rather than the versioned file behind it, so upgrades don't break it. `autostart status` reports when the recorded program is gone. On Linux a state directory containing `$` is passed correctly. On macOS, enabling again waits for the old agent to unload instead of failing; a failed enable no longer leaves a startup file that says it's on; the LaunchAgent is always written with mode 0644.
- The discovery agent kept running when accepting a connection failed (for example when it ran out of file descriptors). Before, one failed accept stopped it. It also raises its open-file limit on macOS and Linux.
- On Windows, pairing attempts in the first minute after boot could make the agent refuse every later pairing attempt until it restarted.
- The discovery agent no longer rewrites and syncs a state file each time it reads the machine's identity, which every app list did twice.
- Apps started through the MCP `run` tool get the same environment as from a terminal. MCP used to set `CI=1` and `NO_COLOR=1` for every command it ran, and the app and proxy inherited them.
- The MCP server answers `ping` and other requests while a slow call is running, honours `notifications/cancelled`, and no longer exits on a line that isn't UTF-8. Lines over 1 MiB are refused with a parse error.
- An MCP `run` that runs out of `timeoutMs` stops the app it was starting, so a retry no longer reports "already running". If the app's launcher outlives the timeout, the error says the app may still be starting and to check `list`, and the tool's description now says so too instead of promising the app is always stopped.
- MCP `logs` returns the last 200 lines unless asked for more (at most 5000), instead of the whole log.
- `extraArgs: ["--json"]` on `list`, and `--no-worktree` with `noWorktree: true` on `get`, no longer fail with a duplicate-flag error. `run` refuses an empty `command` instead of running the package's default script.
- `tools_profiles` and `porch mcp --help` list every profile, including `apps`.
- The menu bar and tray app no longer wakes several times a second while idle. It refreshes apps once a minute and looks for nearby machines every five minutes on its own, and refreshes as soon as the pointer reaches the icon.
- A click on the proxy switch could be undone if the app list refreshed at the same moment.
- Opening or copying a paired machine's app from the menu works when its id starts with "-".
- Menu messages go away after a few seconds, and a pairing code leaves the menu when it expires.
- **Pair a device…** in the menu now opens a submenu that says pairing turns on sharing, and the code needs a second click.
- Running `porch tray` again while the menu is open no longer adds a second icon.
- `porch self-update` replaces `porch` last, so an update that fails partway (the tray, `porch-login` or `Porch.app`) is redone by running it again instead of reporting "up to date". It refuses a foreign `Porch.app` before replacing anything, and notes companions the new archive doesn't include.
- A static Linux `porch` with a tray beside it (from an earlier `PORCH_TRAY=1` install) updates to the build that includes the tray, through `porch self-update` and `install.sh` alike. `PORCH_TRAY=0` switches back and removes the tray. `PORCH_TRAY=1` on ARM64 Linux says it is ignored.
- `--force` no longer installs a release older than the running `porch` (except over a source build), and says it has no effect on a Homebrew install.
- Versions such as `1.0.0-rc.10` sort after `1.0.0-rc.2`, and versions with build metadata (`1.0.0+build.1`) are understood.
- Homebrew on x86_64 Linux installs the static build, which runs on Ubuntu 22.04, Debian 12 and RHEL 9 (the previous build needed glibc 2.39). The update notice for Homebrew installs suggests `brew upgrade` and waits until the tap has the new version.
- The daily update check no longer delays commands while it loads the system's certificates. A slow answer is saved if the command is still running when it arrives, rather than discarded. The check no longer creates `~/.porch` itself, and `sudo porch self-update` no longer leaves a root-owned `~/.porch`.
- `install.sh` finds the latest release without the GitHub API's hourly limit, installs the Apple Silicon build from a Rosetta shell, refuses to run under `sudo`, checks `Porch.app` is Porch's before replacing it (and swaps it in one step), uses HTTPS only, and does nothing if the download is cut short.
- `install.ps1` fails clearly when extraction fails or `porch.exe` is missing instead of reporting success, uses Windows' own `tar`, keeps `%VAR%` entries in your PATH, works when the user PATH is empty, replaces a running `porch.exe` safely, no longer changes your PowerShell session's settings, and downloads faster on Windows PowerShell 5.1.

### Improved
- The proxy no longer reads the process table every 5 seconds when no app is running, nor connects to every app each time.
- Stopping, replacing and pruning apps check processes without running `ps` for each check, and look for a free app port without blocking other porch commands on `lsof`.
- Launching in a workspace looks up the Git worktree and the workspace's packages once instead of once per package, and `porch run --detach` doesn't repeat it in the background launcher. A command that exits without handing its port to a daemon no longer runs `lsof` for a second.
- Requests no longer wait on each other while the proxy re-reads routes or checks whether apps are alive, and route matching does less work per request.
- An app Porch launched that listens only on IPv6 loopback is reached without a blocking check on every request. An `--upstream` alias moves to IPv6 loopback only when nothing listens on IPv4, never because the app was slow.

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
