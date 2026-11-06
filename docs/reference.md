# Porch reference

Compatibility baseline: [Portless `49586cfa566ffce8dcc4368f0b74fce560dced49`](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49), frozen on 2026-11-06. The executable is `porch`; configuration uses `porch.json` and the `package.json` `porch` key. Legacy Portless names remain compatibility fallbacks.

## Launching apps

| Invocation | Behavior |
| --- | --- |
| `porch` | Run the configured package script, default `dev`; discover workspace packages at a workspace root |
| `porch --script start` | Select a package script |
| `porch run COMMAND ARGS...` | Infer an app name from package.json, Git root, or directory |
| `porch run --name NAME COMMAND...` | Override the base name while retaining worktree naming |
| `porch NAME COMMAND ARGS...` | Launch an explicitly named app |
| `porch --name NAME COMMAND...` | Permit names that collide with subcommands |
| `porch NAME --app-port 4500 -- COMMAND...` | Choose a fixed app port and terminate Porch option parsing |
| `porch NAME --force COMMAND...` | Take over an existing route and terminate its owned app group |
| `porch setup [--yes] [--port N] [--https\|--no-https]` | First-run walkthrough; idempotent, current values are the defaults |
| `porch doctor [--fix] [--yes]` | Diagnose; `--fix` applies safe fixes |

Launch options include `--name`, `--script`, `--prefix`, `--app-port`, `--force`, `--lan`, `--ip`, `--tailscale`, `--funnel`, `--ngrok`, and `--cloudflare`. Options following the child command belong to that child. `--prefix ''` suppresses worktree naming. `--help`, `-h`, `--version`, `-v`, and `-V` provide CLI information.

The proxy starts automatically and launch waits for listener readiness before spawning the app. After spawning, Porch waits up to `PORCH_LISTEN_TIMEOUT` seconds (default 15, `0` disables) for something to accept TCP on the assigned port. If nothing does, it prints a warning and the app keeps running. When a process in the app's tree listens on a different port, the warning names it (`app is listening on 4321 instead`) and suggests `porch run --app-port 4321 COMMAND`. Porch never reroutes automatically. Its last saved configuration survives stop/restart. App ports are reserved under the shared route lock and selected from 4000–4999 when unspecified, excluding browser-blocked ports 4045 and 4190. A fixed port must be available and unreserved.

Child exit codes are preserved. Unix children lead their own process groups; signal forwarding and cleanup cover descendants. Persistent ownership records include process start identity so stale routes can be distinguished from a reused PID. `prune --force` removes orphan routes and terminates their owned children. Successful commands that leave an owned listening daemon transfer their route to that process. Windows code uses native process identity and kill-on-close Job Objects for tree termination; native Windows runtime acceptance is a separate verification requirement.

## Proxy and routes

```sh
porch proxy start                         # HTTP, port 7462, no sudo (default)
porch proxy start --port 8080             # HTTP, custom port, no sudo
porch proxy start --https                 # opt-in HTTPS on 443 (sudo + local CA trust)
porch proxy start --https --port 8443     # HTTPS on a high port, no sudo (run `porch trust` for browser trust)
porch proxy start --foreground
porch proxy start --tld localhost --tld test --wildcard
porch proxy start --cert ./cert.pem --key ./key.pem
porch proxy start --lan --ip 192.168.1.42
porch proxy stop
porch proxy status
```

Proxy options are `--port`/`-p`, `--https`, `--no-tls`, `--foreground`, `--skip-trust`, `--unprivileged`, `--lan`, `--ip`, repeatable/comma-separated `--tld`, `--wildcard`, paired `--cert`/`--key`, and `--state-dir`. Porch also provides explicit `--bind` and `--redirect-port` overrides. A custom certificate implies TLS.

Port and privilege mode are one setting. Ports ≥1024 imply unprivileged mode: no sudo, no hosts-file synchronization, no automatic trust changes. Browsers resolve `*.localhost` without hosts entries. Set `PORCH_SYNC_HOSTS=1` only if you explicitly want hosts entries on a high port. Ports below 1024 (HTTPS on 443) need administrator rights. Interactive startup runs `sudo -v` before detaching. Unattended startup uses cached sudo authorization or fails with `porch proxy start --no-tls --port 7462` as the no-sudo alternative. `--unprivileged` still forces no-sudo mode on a low port where the OS allows wildcard binding.

Normal listeners bind IPv4 and IPv6 loopback. LAN mode, and unprivileged low ports, bind both wildcard addresses; the proxy rejects non-loopback peers unless LAN is enabled. Listeners and app reservations use exclusive non-reuse binds across IPv4/IPv6 wildcard interfaces.

Port conflicts never dead-end. When the proxy port is busy, Porch identifies the owner (pid and process name via `lsof` on macOS, `ss` then `lsof` on Linux; best effort, since other users' processes may be invisible without sudo). A default or saved port moves to the next free port, skipping 7463 (reserved for Porch discovery). The new port is saved, and one line reports the move. An explicit `--port` or `PORCH_PORT` never moves silently; startup fails with the owner and a suggested `porch proxy start --port N`. An HTTPS port below 1024 does not move; Porch suggests the no-sudo HTTP default instead. A port held only by recently closed connections (no listener, typically for about 30 seconds after a proxy stop) is waited out, up to 60 seconds, rather than moved.

The proxy records its version in `proxy.version`. When a newer `porch` binary finds an older proxy running (or one that predates the record), it restarts the proxy with the same saved settings. Routes live in state and survive the restart.

HTTPS supports HTTP/1.1 and HTTP/2. The proxy streams request/response bodies, forwards WebSocket Upgrade and RFC 8441 extended CONNECT, preserves forwarding metadata, and detects recursive proxy loops. Plain HTTP requests on a TLS listener redirect to HTTPS; an additional redirect listener can be configured. Unknown configured hosts receive a structured 404; unrelated domains receive 421. HTML and JSON error responses include a useful error code.

TLD labels obey DNS rules. Multi-segment and overlapping TLDs match the longest configured suffix. Strict routing is the default; `--wildcard` falls back to the most specific registered parent. Exact routes win. Explicit TLD aliases remain scoped to the selected configured domains.

```sh
porch alias database 5432
porch alias web 3000 --force --tld test
porch alias --remove web --tld test
porch get web                            # public URL, even before registration
porch get web --no-worktree
porch list                               # routes with health, plus sharing URLs
porch list --json                         # structured routes (with health), proxy settings and version
```

Aliases also accept HTTP/HTTPS upstream URLs. `remove`/`rm` and `routes` are additional aliases for removal and listing.

`list` health values: `ok`; `not listening` (nothing accepts on the target); `stopped` (an owned process is job-control stopped, state T, e.g. frozen by SIGTTOU/SIGTTIN; JSON includes `stopped_seconds`); `dead` (launcher exited while app processes remain; `porch prune --force`). Routes whose launcher and app are both gone are removed automatically on `list`, on launch, on proxy start, and every 5 seconds by the running proxy.

## Configuration and workspaces

Every caller selects one source per directory: `porch.json`, then `package.json` `porch`, then `portless.json`, then `package.json` `portless`. A malformed selected source fails explicitly; lower-priority sources are fallbacks, not merged overrides.

| Field | Meaning |
| --- | --- |
| `name` | Base hostname; inferred when absent |
| `script` | Package script, default `dev` |
| `appPort` | Fixed app port from 1–65535 |
| `proxy` | Whether this app/task gets a route and assigned port |
| `apps` | Workspace overrides keyed by relative package directory |
| `turbo` | Use the root Turbo runner unless explicitly false |
| `worktree.prefix` | `last-segment` (default) or `branch` |
| `worktree.hostnameTemplate` | Template containing `{worktree}`, optionally `{name}` |

A package's `porch` value can be a name string or app settings object. CLI selections take priority; leaf package settings override workspace app entries. Worktree fields merge from root to leaf. Direct leaf launches inherit root worktree settings and matching `apps` entries, but not root name/appPort/script. A root name supplies the project domain, so packages infer `web.shop` and `api.shop`. At the workspace root, the root script remains the coordinator default. Long generated labels receive a stable hash suffix. Linked worktrees on feature branches get a prefix; the main checkout and default branches do not.

Workspace discovery reads `pnpm-workspace.yaml` or array/object `package.json` workspaces and supports single-segment star patterns and exclusions. Unlisted apps infer `<package>.<project>` using the common npm scope, then root name/directory fallback. Known build-only commands such as `tsc` and `tsup` run as tasks without routes. Duplicate generated app names fail before launch.

Package managers are resolved from `packageManager` and ancestor lockfiles: npm, pnpm, yarn, and bun. Package scripts execute through the package manager, preserving shell semantics and lifecycle hooks. Local and ancestor `node_modules/.bin` directories are added to PATH.

With `turbo.json`/`turbo.jsonc`, Porch reserves package routes and runs the root task command once. A per-launch Node preload supplies each package's environment without replacing the Turbo graph. It also supplies required framework flags to Node framework CLIs. Package wrappers using `dev: porch` with `porch.script: dev:app` inherit the reserved environment and avoid a second route owner. Use `porch -- --filter=web` to pass arguments to the root Turbo runner. Setting `turbo: false` uses direct concurrent spawning. Explicit named Turbo graph commands (including root `pnpm run dev` scripts that invoke Turbo) are rejected at multi-package roots: use bare `porch` or coordinator filtering to retain separate ports.

Framework detection covers Vite, VitePress, VitePlus (`vp`), React Router, Rsbuild, Astro, Angular (`ng`), React Native, Expo, and Wrangler. It recognizes package runners and one level of package-script indirection. Existing port/host choices are preserved. Build/non-server commands receive no server flags. Compound scripts, trailing comments, and framework `--` boundaries are deliberately left to the package manager rather than appending flags to the wrong command. Expo in LAN mode receives neither an injected HOST variable nor a forced localhost host flag.

## Environment

Public `PORCH_*` settings take precedence over the corresponding `PORTLESS_*` compatibility setting.

| Suffix | Input behavior |
| --- | --- |
| `STATE_DIR` | State directory, default `~/.porch` |
| `PORT` | Proxy port (default 7462); explicit, so a busy port fails rather than moves |
| `APP_PORT` | App port; explicit CLI selection wins |
| `HTTPS` | `1` opts in to HTTPS (port 443 unless `PORT` is set); plain HTTP by default |
| `LAN`, `LAN_IP` | LAN mode and optional pinned address |
| `TLD` | Comma-separated configured domains |
| `PREFIX` | Worktree prefix; empty disables |
| `WILDCARD` | Parent wildcard fallback |
| `UNPRIVILEGED` | No elevation or automatic hosts/trust changes; implied for ports ≥1024 |
| `SYNC_HOSTS` | `0` disables automatic hosts reconciliation; `1` requests it even on a high port |
| `LISTEN_TIMEOUT` | Seconds to wait for a launched app to listen before warning (default 15, `0` disables) |
| `TAILSCALE`, `FUNNEL`, `NGROK`, `CLOUDFLARE` | Sharing provider selection |
| `SKIP_TRUST` | Skip interactive CA trust setup |

`PORCH=0` or `PORTLESS=0` bypasses routing; `false` and `skip` are also accepted. The child keeps the normal package command and its inherited environment. It receives no newly allocated port or proxy URL.

Child outputs include `PORT`, `HOST`, `PORCH_URL`, `NODE_EXTRA_CA_CERTS` for the generated CA, `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` for configured domains, and provider variables `PORCH_TAILSCALE_URL`, `PORCH_NGROK_URL`, and `PORCH_CLOUDFLARE_URL`. The launcher records sharing URLs for list output. Expo LAN intentionally omits HOST. Internal active/manifest/authentication variables are implementation details, not configuration inputs.

## Host, service, sharing and Compose integration

`hosts sync` and `hosts clean` reconcile only Porch's managed hosts block, preserving other entries and validating file reads/writes. Automatic elevated sync uses a private token and loopback challenge/proof authentication; browser-origin requests are rejected. Tests override the hosts path and never touch the real file.

`trust` installs the generated CA through macOS security, Linux CA tooling, Windows certutil, or WSL's combined stores. `clean` removes owned trust/service/hosts resources and known state, retaining certificate material when trust removal cannot be completed safely. CA generation and HTTPS client verification are exercised without adding test CAs to the host trust store. In unattended sessions, an untrusted CA produces a descriptive error rather than a prompt; `--skip-trust` or `PORCH_SKIP_TRUST=1` supports clients that explicitly load the generated CA.

`service install`, `service status`, and `service uninstall` generate/manage launchd, systemd, or Windows Task Scheduler startup definitions. Install accepts the proxy configuration flags and state directory. Generated definitions persist explicit settings and logs. Their foreground daemon skips interactive trust setup so boot is unattended; the app launcher or `porch trust` installs browser trust. Native privileged startup/trust installation has not been exercised by ordinary acceptance tests.

`--tailscale` and `--funnel` check tailnet/HTTPS prerequisites and allocate an independent HTTPS port for each registration. `--ngrok` and `--cloudflare` wait for a usable public URL before app spawn. Sharing sessions own their external processes/registrations and clean them up on exit. Live provider credentials and public network access are not needed by the fake-executable tests.

`compose [GLOBAL FLAGS] up [ARGS...]` resolves the Compose model, allocates stable loopback ports for HTTP services, registers service/port names, and leaves non-HTTP published ports intact. Global flags such as `-f`, `-p`, `--profile`, `--env-file`, and `--project-directory` retain their meaning. Repeated runs reuse route ports; down/stale reconciliation removes only that project's routes. Docker interaction is fixture-tested; a live Docker Engine check remains external verification.

LAN advertisement publishes route-specific `.local` names and refreshes route/IP changes every two seconds. A pinned IP remains fixed. Multicast reachability and physical network transitions need a real multi-device LAN check.

`doctor` reports proxy/state/routes, hostname resolution, trust, and external-tool prerequisites, then lists issues as `problem → fix applied` or `problem → command to run`. `doctor --fix` applies safe fixes. It moves a stopped proxy off a busy port, prunes dead routes, restarts an outdated proxy, and switches legacy high-port settings to no-sudo mode. Terminating stopped or orphaned launches asks first; `--yes` accepts. Non-interactive sessions without `--yes` never terminate anything. `prune [--force]` terminates stale owned children; `clean` performs explicit teardown.

## First run: `porch setup`

`porch setup` walks through four choices and is safe to re-run. Each prompt defaults to the current value.

1. HTTPS opt-in (default no): HTTPS on 443 needs sudo once and adds a local CA to the trust store.
2. Proxy port (default 7462): a busy port is reported with its owner, and the next free port is offered.
3. Start at login (default: current): installs or uninstalls the `porch service` startup definition, which asks for sudo.
4. Share with your other machines (default: current): enables or disables discovery on this machine's private address through the discovery API, with app sharing granted. Peers still pair explicitly.

Then it saves settings and starts (or restarts) the proxy. It prints a three-line summary and the next command. `--yes`, or stdin that is not a terminal, accepts every default without prompting. `--port`, `--https` and `--no-https` preselect answers.

## Updates

`porch self-update` upgrades Porch. When the running binary lives in a Homebrew Cellar it prints and runs `brew upgrade akshitkrnagpal/tap/porch`. Otherwise it reads the latest GitHub Release, downloads `porch-VERSION-TARGET.tar.gz`, verifies it against the release `SHA256SUMS`, and atomically replaces the running executable (temp file beside it, fsync, rename, `porch.old` kept until `porch --version` reports the new version). `porch-tray`/`porch-login` beside it and, on macOS, an installed `~/Applications/Porch.app` are replaced too. Running apps, the proxy, and the tray keep running; a proxy started by the previous version is restarted on next use. Binaries inside a Cargo `target/` directory are refused unless `--force`.

`--check` prints current and latest versions and exits 0 (up to date), 1 (update available), or 2 (error). `--dry-run` prints the plan without downloading or running brew. `--force` reinstalls the latest release.

| Variable | Behavior |
| --- | --- |
| `PORCH_UPDATE_REPO` | Release repository, default `akshitkrnagpal/porch` |
| `PORCH_UPDATE_API` | GitHub API base URL, default `https://api.github.com` |
| `GITHUB_TOKEN`, `GH_TOKEN` | Token for private repositories; `porch self-update` also tries `gh auth token` |
| `PORCH_NO_UPDATE_CHECK=1` | Disable the passive update notice |
| `PORCH_APP_DIR` | Directory holding the installed `Porch.app`, default `~/Applications` |

Interactive commands check for a newer release in a background thread (2 s timeout) at most once per 24 hours, caching the result in `STATE_DIR/update-check.json`, and print at most one dim line per day: `porch 0.3.1 is available — run porch self-update`. The check is skipped when stderr is not a terminal, for `--json` output, `mcp`, `tray`, `self-update`, under `CI`, and when running as root. The passive check never invokes `gh`.

## MCP

`porch mcp [--tools core,proxy,diagnostics,all]` serves newline-delimited stdio JSON-RPC. `default` aliases `core`; `full` aliases `all`. Core exposes list/get/static alias management and profile discovery; proxy adds start/stop; diagnostics adds doctor/prune/version. The server does not spawn development servers. Tool arguments include `cwd`, `extraArgs`, and `timeoutMs`, with typed route/proxy options where applicable. Foreground commands are rejected. Stdout is reserved for protocol responses.

## Build verification

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

CI definitions target Ubuntu and macOS; Windows is deferred. A workflow definition is not evidence that those runners have passed; current local acceptance was performed on macOS. The manual artifact workflow builds native Linux x86_64 and macOS ARM64 binaries and optional native desktop packages without creating a public GitHub Release, signing, or notarizing them.

Configuration lookup prefers `porch.json`, then the package `porch` key, then `portless.json`, then the package `portless` key. A native package key can override app fields from the file, consistent with leaf package overrides. A malformed selected native configuration is an error, never a reason to fall back to legacy settings. Legacy child environment names are also emitted for migration.


## Certificate issuance and hosts capability

Automatic TLS issuance accepts configured TLDs, registered route names, and `.local` in LAN mode; unrelated SNI is rejected before certificate creation, including previously cached names after removal. Leaves have hostname CN/SAN, serverAuth EKU and at most 365 days validity. New local CAs have at most 3650 days validity and pathLen=0. Persisted roots/keys are retained byte-for-byte to preserve installed trust, with profile-v2 leaves taking effect after daemon restart and a one-time legacy-root hint. DNS NameConstraints are not fixed on the root because domains can change; the running resolver enforces the issuance boundary. Replacing a legacy root requires explicit cleanup and renewed trust setup. Real trusted-CA Safari/Chrome/Firefox behavior remains unverified.

Automatic hosts updates use the daemon's recorded write capability to avoid requesting writes from an incapable proxy. Custom-domain permission errors retain setup guidance; authentication failures remain separate 403 responses, while write failures return 409. Browser `.localhost` resolution needs no hosts entries, so failure yields one hint per state directory. Other clients may still require explicit `sudo porch hosts sync`. The loopback challenge/proof, token and Origin protections apply unchanged.


## Desktop and discovery (0.2)

`porch tray` launches the optional `porch-tray` companion. The CLI remains headless with no desktop runtime dependencies. `porch list --all [--json]` and `porch peers [list|refresh] [--json]` return the shared versioned local/remote catalog. `porch peers add ENDPOINT --fingerprint SHA256 [--token-file PATH]` enrolls using a one-time token on stdin or a private file; `peers trust` aliases Add. `peers remove ID` forgets local trust. `porch open APP [--peer ID] [--url-only]` and `porch copy APP [--peer ID]` use supported state and a local authenticated relay for remote apps.

`porch discovery enable --bind PRIVATE_IP [--port 1356] [--share-apps] [--no-advertise]` starts an independent TLS agent. App transport is a separate explicit grant. `discovery serve` runs the saved configuration in the foreground; `status [--json]`, `fingerprint`, `pair-token`, `revoke`, `disconnect`, and `disable` manage local discovery only. Nothing automatically enables network sharing, login startup, trust, Tailscale Serve/Funnel, or public tunnels. See the [complete two-machine setup, security model and wire protocol](discovery.md).
