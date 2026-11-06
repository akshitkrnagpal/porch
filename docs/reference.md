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

Launch options include `--name`, `--script`, `--prefix`, `--app-port`, `--force`, `--lan`, `--ip`, `--tailscale`, `--funnel`, `--ngrok`, and `--cloudflare`. Options following the child command belong to that child. `--prefix ''` suppresses worktree naming. `--help`, `-h`, `--version`, `-v`, and `-V` provide CLI information.

The proxy starts automatically and launch waits for listener readiness before spawning the app. Its last saved configuration survives stop/restart. App ports are reserved under the shared route lock and selected from 4000–4999 when unspecified, excluding browser-blocked ports 4045 and 4190. A fixed port must be available and unreserved.

Child exit codes are preserved. Unix children lead their own process groups; signal forwarding and cleanup cover descendants. Persistent ownership records include process start identity so stale routes can be distinguished from a reused PID. `prune --force` removes orphan routes and terminates their owned children. Successful commands that leave an owned listening daemon transfer their route to that process. Windows code uses native process identity and kill-on-close Job Objects for tree termination; native Windows runtime acceptance is a separate verification requirement.

## Proxy and routes

```sh
porch proxy start                         # HTTPS, port 443
porch proxy start --port 1355 --no-tls     # HTTP, custom port
porch proxy start --unprivileged          # HTTP, low port without sudo where OS allows
porch proxy start --https --unprivileged  # explicit HTTPS in unprivileged mode
porch proxy start --foreground
porch proxy start --tld localhost --tld test --wildcard
porch proxy start --cert ./cert.pem --key ./key.pem
porch proxy start --lan --ip 192.168.1.42
porch proxy stop
porch proxy status
```

Proxy options are `--port`/`-p`, `--https`, `--no-tls`, `--foreground`, `--skip-trust`, `--unprivileged`, `--lan`, `--ip`, repeatable/comma-separated `--tld`, `--wildcard`, paired `--cert`/`--key`, and `--state-dir`. Porch also provides explicit `--bind` and `--redirect-port` overrides. A custom certificate implies TLS.

Normal listeners bind IPv4 and IPv6 loopback. LAN mode binds both wildcard addresses and uses `.local` names. Unprivileged mode may also require wildcard binding, but the proxy rejects non-loopback peers unless LAN is enabled. Unsupported low-port binding produces an actionable error. Where binding needs privileges, interactive startup runs `sudo -v` before detaching. Unattended startup uses cached sudo authorization or gives explicit terminal-setup and `--unprivileged --port 1355 --no-tls` choices. Missing sudo never silently changes settings. Listeners and app reservations use exclusive non-reuse binds across IPv4/IPv6 wildcard interfaces; occupied ports are refused or skipped. `doctor` verifies daemon identity and HTTP readiness and lists other listeners where lsof is available.

HTTPS supports HTTP/1.1 and HTTP/2. The proxy streams request/response bodies, forwards WebSocket Upgrade and RFC 8441 extended CONNECT, preserves forwarding metadata, and detects recursive proxy loops. Plain HTTP requests on a TLS listener redirect to HTTPS; an additional redirect listener can be configured. Unknown configured hosts receive a structured 404; unrelated domains receive 421. HTML and JSON error responses include a useful error code.

TLD labels obey DNS rules. Multi-segment and overlapping TLDs match the longest configured suffix. Strict routing is the default; `--wildcard` falls back to the most specific registered parent. Exact routes win. Explicit TLD aliases remain scoped to the selected configured domains.

```sh
porch alias database 5432
porch alias web 3000 --force --tld test
porch alias --remove web --tld test
porch get web                            # public URL, even before registration
porch get web --no-worktree
porch list                               # readable active routes and sharing URLs
porch list --json                         # structured active routes/proxy settings
```

Aliases also accept HTTP/HTTPS upstream URLs. `remove`/`rm` and `routes` are additional aliases for removal and listing.

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
| `PORT` | Proxy port |
| `APP_PORT` | App port; explicit CLI selection wins |
| `HTTPS` | TLS enabled by default; `0` disables |
| `LAN`, `LAN_IP` | LAN mode and optional pinned address |
| `TLD` | Comma-separated configured domains |
| `PREFIX` | Worktree prefix; empty disables |
| `WILDCARD` | Parent wildcard fallback |
| `UNPRIVILEGED` | No elevation or automatic hosts/trust changes |
| `SYNC_HOSTS` | `0` disables automatic hosts reconciliation |
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

`doctor` reports proxy/state/routes, hostname resolution, trust, and external-tool prerequisites. `prune [--force]` terminates stale owned children; `clean` performs explicit teardown.

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
