# Porch reference

The executable is `porch`. Configuration lives in `porch.json` or the `porch` key in `package.json`. Portless names (`portless.json`, the `portless` key and `PORTLESS_*` variables) still work as fallbacks. Compatibility is measured against [Portless at `49586cf`](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49), as of 2026-11-06.

## Launching apps

| Command | What it does |
| --- | --- |
| `porch` | Runs the package's configured script (default `dev`). At a workspace root, starts every workspace package |
| `porch --script start` | Runs a different package script |
| `porch run COMMAND ARGS...` | Runs a command, naming the app from `package.json`, the Git root or the directory |
| `porch run --name NAME COMMAND...` | Overrides the base name but keeps worktree naming |
| `porch NAME COMMAND ARGS...` | Runs a command under an explicit name |
| `porch --name NAME COMMAND...` | Same, for names that clash with a subcommand |
| `porch NAME --app-port 4500 -- COMMAND...` | Uses a fixed app port; `--` ends Porch's options |
| `porch NAME --force COMMAND...` | Takes over an existing route and stops the app that owned it |
| `porch setup [--yes] [--port N] [--https\|--no-https]` | Guided first run; safe to repeat, and current values are the defaults |
| `porch doctor [--fix] [--yes]` | Diagnoses problems; `--fix` applies the safe fixes |

Launch options are `--name`, `--script`, `--prefix`, `--app-port`, `--force`, `--private`, `--lan`, `--ip`, `--tailscale`, `--funnel`, `--ngrok` and `--cloudflare`. `--private` (or `PORCH_PRIVATE=1`) keeps an app off paired machines even when sharing is on; `porch alias --private` does the same for an alias. Anything after the child command belongs to the child. `--prefix ''` turns off worktree naming. `--help`/`-h` and `--version`/`-v`/`-V` work as usual.

A launch starts the proxy if needed and waits for it to accept connections before starting the app. It then waits up to `PORCH_LISTEN_TIMEOUT` seconds (default 15, `0` disables) for something to accept TCP on the assigned port. If nothing does, Porch prints a warning and leaves the app running. If a process in the app's tree is listening on another port, the warning names it (`app is listening on 4321 instead`) and suggests `porch run --app-port 4321 COMMAND`. Porch never reroutes on its own. Unless you pick one, app ports come from 4000–4999 (set `PORCH_APP_PORTS=START-END` for another range), starting at a random port and skipping 4045 and 4190, which browsers block. A fixed port must be free and not reserved by another route.

Porch exits with the child's exit code. On Unix, each child leads its own process group, so signals and cleanup reach its descendants. Route ownership records store each process's start time as well as its PID, so a reused PID isn't mistaken for the original app. When a command exits successfully but leaves a daemon it started listening, the route moves to that daemon. `porch prune --force` removes orphaned routes and stops the processes they own.

On Windows, each child gets its own console process group and receives Ctrl+Break from its launcher. Porch's own processes (proxy, discovery agent, launchers) stop when asked through a named event and are force-ended only as a fallback. Job Objects make sure a whole process tree ends together.

## Proxy and routes

```sh
porch proxy start                         # HTTP on port 80 where no sudo is needed, else 7462 (default)
porch proxy start --port 8080             # HTTP on another port, no sudo
porch proxy start --https                 # HTTPS with HTTP/2 on the same port, per-user CA trust, no sudo
porch proxy start --https --port 443      # HTTPS without a port in URLs (binding 443 needs sudo)
porch proxy start --foreground
porch proxy start --tld localhost --tld test --wildcard
porch proxy start --cert ./cert.pem --key ./key.pem
porch proxy start --lan --ip 192.168.1.42
porch proxy stop
porch proxy status
```

Proxy options: `--port`/`-p`, `--https`, `--no-tls`, `--foreground`, `--skip-trust`, `--unprivileged`, `--lan`, `--ip`, `--tld` (repeatable or comma-separated), `--wildcard`, `--cert` with `--key`, `--state-dir`, `--bind` and `--redirect-port`. Passing a certificate turns on TLS.

**URLs without a port.** On a fresh install, Porch checks whether this user can bind the standard port (80, or 443 with HTTPS) without sudo. macOS 10.14+ and Windows allow it; Linux does when `net.ipv4.ip_unprivileged_port_start` is 80 or lower. If so, and the port is free, Porch uses it, so URLs read `http://web.localhost`. The proxy then binds the wildcard address (the only way macOS allows it) and refuses every non-loopback client unless LAN mode is on. Otherwise Porch uses port 7462. If something takes the standard port later, Porch moves to 7462 and says so. `PORCH_CLEAN_URLS=0` turns this off, and `porch setup` asks.

**Ports and sudo.** The port decides whether Porch needs administrator rights. On ports 1024 and up, Porch never uses sudo and doesn't edit the hosts file, since browsers resolve `*.localhost` on their own. Set `PORCH_SYNC_HOSTS=1` if you want hosts entries anyway. Turning on HTTPS keeps the current port, except that a proxy on port 80 moves to 7462. Trusting the local CA is per user and needs no sudo, so an interactive HTTPS start offers to do it on any port. An unattended start skips the prompt and prints a `porch trust` hint. Ports below 1024 (such as HTTPS on 443) need administrator rights: an interactive start runs `sudo -v` before detaching, and an unattended start uses cached sudo credentials or fails and suggests `porch proxy start --no-tls --port 7462`. `--unprivileged` forces no-sudo mode on a low port where the OS allows it.

**Listeners.** The proxy normally binds the IPv4 and IPv6 loopback addresses. LAN mode, and unprivileged low ports, bind the wildcard addresses instead, and the proxy turns away non-loopback clients unless LAN mode is on. Proxy listeners and app port reservations use exclusive binds.

**Busy ports.** When the proxy port is taken, Porch names the process holding it (via `lsof` on macOS, `ss` then `lsof` on Linux; processes owned by other users may be hidden without sudo). A default or saved port moves to the next free port, skipping 7463, which is reserved for discovery, and Porch saves the new port and prints one line about the move. A port you chose with `--port` or `PORCH_PORT` never moves: startup fails, names the owner and suggests `porch proxy start --port N`. An HTTPS port below 1024 doesn't move either; Porch suggests the no-sudo HTTP default instead. If the port is held only by recently closed connections (usually for about 30 seconds after the proxy stops), Porch waits up to 60 seconds for it rather than moving.

**Upgrades.** The proxy writes its version to `proxy.version`. When a newer `porch` finds an older proxy running, it restarts the proxy with the same settings. Routes are kept in state, so they survive the restart.

**Protocols.** The proxy speaks HTTP/1.1 and, over TLS, HTTP/2. It streams request and response bodies, forwards WebSocket upgrades (including RFC 8441 extended CONNECT), adds forwarding headers and detects proxy loops. Plain HTTP requests to a TLS port are redirected to HTTPS, and `--redirect-port` adds a separate redirect listener. Requests for an unknown name under a configured TLD get a 404 page; requests for unrelated domains get a 421. Error pages come in HTML or JSON and include an error code.

**Matching.** TLDs must be valid DNS labels. When configured TLDs overlap, the longest match wins. Routing is strict by default: with `--wildcard`, a request for an unregistered subdomain goes to the closest registered parent. An exact route always wins. An alias registered with `--tld` applies only to that TLD.

```sh
porch alias database 5432
porch alias web 3000 --force --tld test
porch alias --remove web --tld test
porch get web                            # the app's URL, even before it is registered
porch get web --no-worktree
porch list                               # routes with health and sharing URLs
porch list --json                        # routes with health, proxy settings and version
```

Aliases can also point at an HTTP or HTTPS URL. For a target that isn't on this machine (`porch alias api https://staging.example.com`), Porch sends the target's own name in `Host` and TLS SNI, verifies its certificate against the system trust store, and rewrites the target's redirects to itself so they stay on the Porch URL. Targets can't carry a query string or fragment. `remove`/`rm` and `routes` are shorthands for removing and listing.

`porch list` health values:

- `ok`: the target accepts connections.
- `not listening`: nothing accepts connections on the target.
- `stopped`: a process the route owns is suspended by job control (state T, for example after SIGTTOU or SIGTTIN). JSON output adds `stopped_seconds`.
- `dead`: the launcher has exited but app processes remain. Clean up with `porch prune --force`.

Routes whose launcher and app are both gone are removed automatically on `list`, on launch, on proxy start, and every 5 seconds while the proxy runs.

## Configuration and workspaces

Porch reads one configuration source per directory, the first it finds of: `porch.json`, the `porch` key in `package.json`, `portless.json`, the `portless` key in `package.json`. Sources are not merged. If the chosen source is malformed, Porch reports an error rather than falling back to the next one.

| Field | Meaning |
| --- | --- |
| `name` | Base hostname; inferred when absent |
| `script` | Package script to run, default `dev` |
| `appPort` | Fixed app port, 1–65535 |
| `proxy` | Whether this app or task gets a route and an assigned port |
| `apps` | Per-package overrides, keyed by the package's path relative to the workspace root |
| `turbo` | Run through the root Turbo task unless set to `false` |
| `worktree.prefix` | `last-segment` (default) or `branch` |
| `worktree.hostnameTemplate` | Template containing `{worktree}` and optionally `{name}` |

A package's `porch` value can be a name string or a settings object. Precedence, highest first: CLI options, the package's own settings, then the matching `apps` entry at the root. Worktree settings merge from the root down to the package. When you launch a package directly, it inherits the root's worktree settings and its `apps` entry, but not the root's `name`, `appPort` or `script`. A root `name` becomes the project domain, so packages are named `web.shop`, `api.shop` and so on. At the workspace root, the root's own script still runs by default. Very long generated names are shortened with a stable hash suffix.

In a linked Git worktree on a feature branch, names get a prefix from the branch. The main checkout and default branches get no prefix.

Workspaces come from `pnpm-workspace.yaml` or the `workspaces` field in `package.json` (array or object form). Patterns support `*` within a path segment, `**` for any depth, and `!` exclusions. Packages without their own name are named `<package>.<project>`, using the shared npm scope, the root name or the directory as the project. Build-only commands such as `tsc` and `tsup` run as tasks without routes. Two packages that would get the same name stop the launch before anything starts.

The package manager (npm, pnpm, Yarn or Bun) comes from `packageManager` or the nearest lockfile. Scripts run through it, so shell syntax and lifecycle hooks behave normally. Local and parent `node_modules/.bin` directories are added to PATH.

**Turborepo.** With a `turbo.json` or `turbo.jsonc`, Porch reserves a route for each package and runs the root task once. A Node preload gives each package its own `PORT`, URL and framework flags, so Turbo keeps its task graph and TUI. A package whose `dev` script is `porch`, with `porch.script` set to `dev:app`, picks up the reserved environment instead of creating a second route. Pass arguments to Turbo after `--`, for example `porch -- --filter=web`. Set `turbo: false` to start packages directly and concurrently instead. At a multi-package root, a named command that runs the Turbo graph (including a root `pnpm run dev` that calls Turbo) is rejected, because every package would share one port. Use bare `porch` or Turbo's filters instead.

**Framework flags.** Porch adds port and host flags for Vite, VitePress, VitePlus (`vp`), React Router, Rsbuild, Astro, Angular (`ng`), React Native, Expo and Wrangler. It sees through package runners and one level of package script. Port and host flags you already set are kept, and build commands get none. Compound scripts, trailing comments and framework `--` separators are left alone, because appending flags there could hit the wrong command. Expo in LAN mode gets no `HOST` variable and no forced localhost flag.

## Environment

Each `PORCH_*` setting overrides its `PORTLESS_*` equivalent.

| Variable (`PORCH_` + …) | Meaning |
| --- | --- |
| `STATE_DIR` | State directory, default `~/.porch` |
| `PORT` | Proxy port (default 80 when no sudo is needed, else 7462). Counts as an explicit choice, so a busy port fails instead of moving |
| `CLEAN_URLS` | `0` keeps new installs on port 7462 instead of the standard port |
| `PRIVATE` | `1` keeps launched apps and aliases off paired machines |
| `APP_PORT` | App port; a CLI option overrides it |
| `HTTPS` | `1` turns on HTTPS on the same port (default 7462; set `PORCH_PORT=443` for the standard port) |
| `LAN`, `LAN_IP` | LAN mode and an optional fixed address |
| `TLD` | Comma-separated list of domains to serve |
| `PREFIX` | Worktree prefix; empty turns it off |
| `WILDCARD` | Fall back to the closest registered parent |
| `UNPRIVILEGED` | Never use sudo or edit the hosts file; implied for ports 1024 and up |
| `SYNC_HOSTS` | `0` turns off hosts-file updates; `1` turns them on even on a high port |
| `LISTEN_TIMEOUT` | Seconds to wait for a launched app to listen before warning (default 15, `0` disables) |
| `TAILSCALE`, `FUNNEL`, `NGROK`, `CLOUDFLARE` | Sharing provider |
| `SKIP_TRUST` | Skip the interactive CA trust prompt |

`PORCH=0` or `PORTLESS=0` (also `false` or `skip`) turns Porch off for a run. The command runs as normal with its inherited environment and no assigned port or URL.

Porch sets these variables for the app: `PORT`, `HOST`, `PORCH_URL`, `NODE_EXTRA_CA_CERTS` (the local CA), `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` (the configured domains), and when sharing, `PORCH_TAILSCALE_URL`, `PORCH_NGROK_URL` or `PORCH_CLOUDFLARE_URL`. The matching `PORTLESS_*` names are set too, for apps that read them. Expo in LAN mode gets no `HOST`. Other internal variables are not configuration and may change.

## HTTPS and trust

`porch trust` installs the local CA for the current user, without sudo:

- macOS: the login keychain (one password or Touch ID prompt).
- Linux: the NSS database Chrome and Chromium use (`~/.pki/nssdb`, created if missing) and every Firefox profile. Needs NSS `certutil`.
- Windows: the current user's Root store. Under WSL, Porch also uses the Windows user store.

`porch trust --system` installs the CA machine-wide instead (the macOS System keychain or the distribution's CA tools), so command-line tools trust it too. It needs sudo. Apps that Porch launches trust the CA through `NODE_EXTRA_CA_CERTS` either way. Porch records where it installed the CA, so `porch clean` removes per-user trust without sudo and asks for sudo only to remove system trust.

When no one is at the terminal, Porch never shows a trust prompt. A no-sudo HTTPS start carries on and prints a `porch trust` hint; a start on port 443 fails with an explanation. Clients that load the CA themselves can use `--skip-trust` or `PORCH_SKIP_TRUST=1`.

Porch issues certificates only for configured TLDs, registered route names and, in LAN mode, `.local` names. Requests for any other name are refused before a certificate is created, including names that were removed after a certificate was cached. Leaf certificates are valid for at most 365 days and the CA for at most 10 years. An existing CA and key are never rewritten, so trust you have installed keeps working. Replacing a CA made by an older version means running `porch clean` and setting up trust again.

## Hosts file, startup, sharing and Compose

**Hosts file.** `porch hosts sync` and `porch hosts clean` edit only Porch's own block in the hosts file and leave other entries alone. Automatic updates on low ports go through the proxy, which authenticates each request with a private token and a loopback challenge and rejects requests from browsers. When an update fails, Porch prints one hint per state directory, since browsers resolve `.localhost` without hosts entries. Other clients may still need `sudo porch hosts sync`.

**Start at login.** `porch service install`, `porch service status` and `porch service uninstall` manage a launchd, systemd or Windows Task Scheduler entry that starts the proxy at boot. `install` accepts the same options as `porch proxy start`, and saves them along with a log location. The service skips the interactive trust prompt; run `porch trust` or launch an app to install browser trust.

**Sharing.** `--tailscale` and `--funnel` check that Tailscale and HTTPS are ready, then give each app its own HTTPS port. `--ngrok` and `--cloudflare` wait for the public URL before starting the app. Porch stops the tunnel processes and removes the registrations it created when the app exits.

**Docker Compose.** `porch compose [GLOBAL FLAGS] up [ARGS...]` reads the Compose model, assigns each HTTP service a stable loopback port and registers a route named after the service and port. Ports for non-HTTP services are left as they are. Global flags such as `-f`, `-p`, `--profile`, `--env-file` and `--project-directory` work as they do in Docker Compose. Running it again reuses the same ports, and `down` or stale containers remove only that project's routes.

**LAN.** `--lan` publishes a `.local` name for each route over mDNS and checks for route and IP changes every two seconds. A fixed `--ip` never changes.

**Cleanup.** `porch doctor` checks the proxy, state, routes, name resolution, trust and external tools, then lists each problem with either `fix applied` or the command to run. `porch doctor --fix` moves a stopped proxy off a busy port, prunes dead routes, restarts an outdated proxy and switches old high-port settings to no-sudo mode. It asks before stopping any process; `--yes` answers for you, and without `--yes` a non-interactive run never stops anything. `porch prune [--force]` stops processes left by stale routes, and `porch clean` removes everything Porch installed: trust, services, hosts entries and state. It keeps the CA files if it can't safely remove their trust.

## First run: `porch setup`

`porch setup` asks four questions. Each defaults to your current setting, so running it again only changes what you change.

1. **HTTPS** (default no). Keeps the proxy port and adds a local CA to your user's trust store, without sudo.
2. **URLs without a port** (offered when this OS allows port 80/443 without sudo), otherwise the **proxy port** (default 7462, for HTTP and HTTPS). If the port is busy, Porch names the process using it and offers the next free port.
3. **Start at login.** Installs or removes the `porch service` entry, which asks for sudo. If sharing is also on, discovery starts at login too (`porch discovery autostart`, per user, no sudo).
4. **Share with your other machines.** Turns sharing on or off on this machine's private address. Other machines still have to pair.

Setup then saves the settings, starts or restarts the proxy, and prints a short summary and the next command to run. `--yes`, or input that isn't a terminal, accepts every default. `--port`, `--https` and `--no-https` answer those questions in advance.

## Updates

`porch self-update` installs the latest release. If `porch` was installed with Homebrew, it runs `brew upgrade akshitkrnagpal/tap/porch`. Otherwise it downloads `porch-VERSION-TARGET.tar.gz` from the latest GitHub Release, checks it against the release's `SHA256SUMS` and, when the GitHub CLI is installed, verifies its build attestation (`gh attestation verify`), which proves the Porch release workflow built it. Set `PORCH_UPDATE_VERIFY=require` to refuse updates when the attestation can't be checked. It then replaces the running binary safely: it writes a temporary file beside it, syncs and renames it, and keeps `porch.old` until the new binary reports its version. It also replaces `porch-tray` and `porch-login` if they sit beside `porch`, and on macOS the installed `~/Applications/Porch.app`. Running apps, the proxy and the tray keep running, and a proxy started by the old version restarts the next time you use Porch. Binaries inside a Cargo `target/` directory are left alone unless you pass `--force`.

`--check` prints the current and latest versions and exits with 0 (up to date), 1 (update available) or 2 (error). `--dry-run` prints what would happen without downloading anything. `--force` reinstalls the latest release.

| Variable | Meaning |
| --- | --- |
| `PORCH_UPDATE_REPO` | Release repository, default `akshitkrnagpal/porch` |
| `PORCH_UPDATE_VERIFY` | `require` refuses an update unless its build attestation verifies (needs `gh`) |
| `PORCH_UPDATE_API` | GitHub API base URL, default `https://api.github.com` |
| `GITHUB_TOKEN`, `GH_TOKEN` | Token for a private repository; `porch self-update` also tries `gh auth token` |
| `PORCH_NO_UPDATE_CHECK=1` | Turns off the update notice |
| `PORCH_APP_DIR` | Where `Porch.app` is installed, default `~/Applications` |

Interactive commands check for a new release at most once a day, in the background with a 2-second timeout, and cache the answer in `STATE_DIR/update-check.json`. When one is out, they print a single dim line such as `porch 0.5.0 is available — run porch self-update`. The check is skipped when stderr isn't a terminal, for `--json` output, for `mcp`, `tray` and `self-update`, when `CI` is set and when running as root. It never calls `gh`.

## MCP

`porch mcp [--tools core,proxy,diagnostics,readonly,all]` runs an MCP server over stdio (newline-delimited JSON-RPC). `default` is the same as `core`, and `full` the same as `all`. `core` lists and looks up routes, manages static aliases and reports which tool profiles are active; `proxy` adds start and stop; `diagnostics` adds doctor, doctor_fix, prune and version; `readonly` offers only tools that change nothing. Read-only tools reject `extraArgs` that would change state. The server can't start dev servers or run foreground commands. Tools accept `cwd`, `extraArgs` and `timeoutMs`, plus route and proxy options where they apply. Stdout carries only protocol messages.

## Machines and the tray

`porch pair` shows a 6-digit code. It starts the discovery agent on port 7463 and turns on app sharing, using a private interface chosen automatically. Run `porch pair CODE [--host NAME-OR-IP]` on another machine to pair the two in both directions. Scripts can use `--bind IP`, `--port PORT`, `--no-wait` and `--json`. `porch unpair MACHINE` removes a pairing on both machines.

Apps on a paired machine open at `http://APP.MACHINE.localhost:PROXY_PORT` through your local proxy. `porch list --all [--json]` and `porch peers [list|refresh] [--json]` show local and remote apps together. `porch open APP [--peer ID] [--url-only]` opens an app and `porch copy APP [--peer ID]` copies its URL. `porch tray` starts the optional `porch-tray` app; the `porch` CLI itself has no desktop dependencies.

`porch discovery enable --bind PRIVATE_IP [--port 7463] [--share-apps] [--no-advertise]` starts the discovery agent by hand. Sharing apps is a separate opt-in. `discovery serve` runs the saved configuration in the foreground, and `discovery autostart enable|disable|status` starts the agent at login (a LaunchAgent on macOS, a systemd user unit on Linux, a scheduled task on Windows; no sudo). `status [--json]`, `fingerprint`, `pair-token`, `revoke`, `disconnect` and `disable` manage this machine's agent. For setups that skip codes, `porch peers add ENDPOINT --fingerprint SHA256 [--token-file PATH]` enrolls a machine with a one-time token (`peers trust` is the same command), and `peers remove ID` forgets a machine on this side only.

Porch never turns on sharing, login startup, trust, Tailscale Serve or Funnel, or public tunnels by itself. [Apps across machines](discovery.md) covers setup, the security model and the wire protocol.

## Building and CI

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

CI runs these on Ubuntu x86_64 and macOS ARM64. A Windows job lints and builds the CLI and tray, runs the unit and integration tests, and checks packaging; the `config`, `first_run`, `integrations`, `lifecycle` and `pty` suites run only on macOS and Linux. Pushing a `v*` tag builds the release archives (CLI and tray, plus `Porch.app` on macOS and `.exe` files on Windows), publishes them with `SHA256SUMS` and a signed build-provenance attestation on GitHub Releases, and updates the Homebrew formula when the `HOMEBREW_TAP_TOKEN` secret is set. Static, CLI-only Linux archives (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) run on any distribution, including Alpine and ARM64 servers. Verify any archive with `gh attestation verify porch-*.tar.gz --repo akshitkrnagpal/porch`. macOS builds aren't notarized yet.
