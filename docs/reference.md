# Reference

This is an upstream compatibility inventory, **not a runnable Porch CLI reference**. Baseline: [49586cfa566ffce8dcc4368f0b74fce560dced49](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49). Porch source has landed; parity remains unverified. In particular, replacing `PORTLESS_` with `PORCH_` is not supported by evidence.

The sole currently documented Porch environment contract is `PORCH_STATE_DIR`, selecting the shared state directory. State contains routes with `name`, `target`, and optional `pid`; the implementation owner must establish persistence format and validation.

## Upstream commands

`portless [--script SCRIPT]`, `run [--name NAME] [--prefix LABEL] [--app-port PORT] [--force] [--] COMMAND...`, and `NAME COMMAND...` start apps. `--name NAME` permits reserved command names. `PORTLESS=0` bypasses routing. Default app ports are selected from 4000–4999.

`get NAME [--no-worktree]` resolves a URL; `list` inspects routes. `alias NAME PORT [--force] [--tld TLD]` adds static routes; `alias --remove NAME` removes them. `compose up` and `compose down` wrap Docker Compose and reconcile routes; Compose global flags precede the action.

`proxy start` supports port, TLS, certificate/key, TLD, wildcard, LAN/IP, foreground, and unprivileged settings; `proxy stop` stops it. Defaults are HTTPS/443, HTTP/80 with TLS disabled. Unprivileged mode has distinct binding and trust/hosts behavior; consult the pinned implementation rather than assuming standard loopback mode. `service install/status/uninstall` manages OS startup.

`hosts sync/clean`, `trust`, `doctor`, `prune [--force]`, and `clean` manage host integration, diagnosis, orphan processes and teardown. These can have machine-wide effects. `mcp [--tools PROFILES]` provides stdio MCP; default profile is core, all expands tools. Sharing flags are --tailscale, --funnel, --ngrok, --cloudflare; they require external CLIs and may expose applications publicly.

## Configuration mapping

Upstream `portless.json` supports `name`, `script`, `appPort`, `proxy`, `apps`, `turbo`, and `worktree`. Per-app entries support name/script/appPort/proxy/worktree. Worktree fields are `prefix` and `hostnameTemplate`. The package.json `portless` key accepts a name string or app settings object. CLI overrides package settings, which override app config and defaults. Workspace discovery and merged worktree settings have additional context-dependent rules in config.ts and workspace.ts. No Porch config equivalence is established yet.

## Environment inventory

Every identifier below occurs in non-test upstream source. This includes internal and output variables, so presence is not a promise of a public input. `PORTLESS_URL` and provider URL variables are child outputs; `PORT` and `HOST` configure child listening, and `NODE_EXTRA_CA_CERTS` carries Node trust. Source links resolve each variable's exact parsing and precedence.

| Upstream identifier | Defining/consuming modules | Porch mapping |
| --- | --- | --- |
| `PORTLESS` | [mcp.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/mcp.ts), [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_APP_PORT` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_CLOUDFLARE` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_CLOUDFLARE_URL` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_FUNNEL` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_HEADER` | [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts), [proxy.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/proxy.ts) | Unverified |
| `PORTLESS_HOPS_HEADER` | [proxy.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/proxy.ts) | Unverified |
| `PORTLESS_HTTPS` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_INTERNAL_LAN_IP` | [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts) | Unverified |
| `PORTLESS_INTERNAL_SERVICE_ELEVATED` | [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_LAN` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_LAN_IP` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_NGROK` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_NGROK_URL` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_PORT` | [mcp.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/mcp.ts), [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_PREFIX` | [auto.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/auto.ts), [mcp.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/mcp.ts), [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_STATE_DIR` | [clean-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/clean-utils.ts), [mcp.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/mcp.ts), [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_STATE_FILES` | [clean-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/clean-utils.ts) | Unverified |
| `PORTLESS_SYNC_HOSTS` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [hosts.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/hosts.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_TAILSCALE` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_TAILSCALE_URL` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts) | Unverified |
| `PORTLESS_TLD` | [mcp.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/mcp.ts), [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_UNPRIVILEGED` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |
| `PORTLESS_URL` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [turbo.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/turbo.ts) | Unverified |
| `PORTLESS_WILDCARD` | [cli.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli.ts), [cli-utils.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/cli-utils.ts), [service.ts](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49/packages/portless/src/service.ts) | Unverified |

## Option inventory

Source-derived tokens include internal flags and forwarded framework/Compose options. They are not all top-level Portless options. The module column identifies the parser to consult. No equivalent Porch flags are claimed.

| Token | Source modules |
| --- | --- |
| `--abbrev-ref` | auto.ts |
| `--accent` | pages.ts |
| `--all` | cli.ts |
| `--allow-scripts` | cli-utils.ts |
| `--ansi` | compose.ts |
| `--app-port` | cli.ts |
| `--assets` | cli-utils.ts |
| `--base` | cli-utils.ts |
| `--bg` | tailscale.ts, pages.ts |
| `--border` | pages.ts |
| `--bun` | cli.ts, cli-utils.ts |
| `--call` | cli-utils.ts |
| `--cert` | cli.ts, cli-utils.ts, service.ts |
| `--cloudflare` | cli.ts |
| `--config` | cli-utils.ts, cloudflare.ts |
| `--config-loader` | cli-utils.ts |
| `--debug` | cli-utils.ts |
| `--define` | cli-utils.ts |
| `--detach` | compose.ts |
| `--dist-path` | cli-utils.ts |
| `--env-dir` | cli-utils.ts |
| `--env-file` | cli.ts, compose.ts |
| `--env-mode` | cli-utils.ts |
| `--environment` | cli-utils.ts |
| `--fg` | pages.ts |
| `--file` | compose.ts |
| `--filter` | cli-utils.ts |
| `--flag` | compose.ts |
| `--font-mono` | pages.ts |
| `--font-sans` | pages.ts |
| `--force` | routes.ts, mcp.ts, cli.ts |
| `--foreground` | mcp.ts, cli.ts, cli-utils.ts, service.ts |
| `--format` | cli.ts, compose.ts |
| `--funnel` | cli.ts |
| `--git-common-dir` | auto.ts |
| `--git-dir` | auto.ts |
| `--grace-period` | cloudflare.ts |
| `--help` | mcp.ts, cli.ts, mdns.ts, service.ts |
| `--host` | cli.ts, cli-utils.ts |
| `--host-header` | tunnel.ts |
| `--http-host-header` | cloudflare.ts |
| `--https` | tailscale.ts, cli.ts, cli-utils.ts, service.ts |
| `--id` | certs.ts |
| `--ip` | cli.ts, cli-utils.ts, mdns.ts, service.ts |
| `--json` | tailscale.ts |
| `--key` | cli.ts, cli-utils.ts, service.ts |
| `--lan` | mcp.ts, cli.ts, cli-utils.ts, service.ts |
| `--lan-ip-auto` | cli-utils.ts |
| `--localhost` | cli.ts, cli-utils.ts |
| `--log` | cli-utils.ts, tunnel.ts |
| `--log-format` | tunnel.ts |
| `--log-level` | cli-utils.ts |
| `--manifest` | cli-utils.ts |
| `--minify` | cli-utils.ts |
| `--mode` | cli-utils.ts |
| `--name` | mcp.ts, cli.ts, cli-utils.ts |
| `--ngrok` | cli.ts |
| `--no-autoupdate` | cloudflare.ts |
| `--no-tls` | mcp.ts, cli.ts, cli-utils.ts, service.ts |
| `--no-worktree` | mcp.ts, cli.ts |
| `--now` | service.ts |
| `--open` | cli-utils.ts |
| `--out` | cli-utils.ts |
| `--output` | cli-utils.ts |
| `--package` | cli-utils.ts |
| `--parallel` | compose.ts |
| `--porcelain` | auto.ts |
| `--port` | cli.ts, cli-utils.ts, service.ts |
| `--prefix` | cli.ts, cli-utils.ts |
| `--profile` | cli.ts, compose.ts |
| `--progress` | compose.ts |
| `--project-directory` | cli.ts, compose.ts |
| `--project-name` | compose.ts |
| `--remove` | mcp.ts, cli.ts |
| `--require` | turbo.ts |
| `--root` | cli-utils.ts |
| `--script` | cli.ts |
| `--show-toplevel` | auto.ts |
| `--site` | cli-utils.ts |
| `--skip-trust` | mcp.ts, cli.ts, cli-utils.ts, service.ts |
| `--sourcemap` | cli-utils.ts |
| `--ssr` | cli-utils.ts |
| `--state-dir` | cli.ts, service.ts |
| `--strict` | cli-utils.ts |
| `--surface` | pages.ts |
| `--tag` | cli-utils.ts |
| `--tailscale` | cli.ts |
| `--target` | cli-utils.ts |
| `--text-` | pages.ts |
| `--tld` | mcp.ts, cli.ts, cli-utils.ts, service.ts |
| `--tools` | mcp.ts, cli.ts |
| `--tunnel` | cli.ts, cli-utils.ts |
| `--unprivileged` | mcp.ts, cli.ts, cli-utils.ts, service.ts |
| `--url` | cloudflare.ts |
| `--version` | mcp.ts, cli.ts, cloudflare.ts |
| `--wait` | compose.ts |
| `--wildcard` | mcp.ts, cli.ts, cli-utils.ts, service.ts |
| `--workspace` | cli-utils.ts |
| `--yes` | tailscale.ts |

## CI and release matrix

CI targets Ubuntu, macOS, and Windows native runners with stable Rust: formatting, compilation, unit and integration tests. Bootstrap runs explicitly report missing Cargo.toml. Once a manifest exists, failures are blocking.

Release workflow is manual and builds native x86_64 Linux, ARM64 macOS, and x86_64 Windows binaries. It uploads workflow artifacts only; it does not publish a GitHub Release, sign binaries, notarize macOS builds, or promise support for cross targets, musl, or older operating systems. Each artifact follows successful tests on its runner. A complete release needs version/tag policy and licensing review from the project owner.

## Current Porch CLI snapshot

Source inspection: proxy defaults to HTTP on 127.0.0.1:1355; --https opts into TLS. Start it separately. Named launch allocates an ephemeral child port and sets PORT. Use `NAME -- COMMAND ARGS...`. --framework supports explicit port injection for vite/next/astro/nuxt. Bare invocation prints help. alias takes a URL rather than a port; remove deletes routes; get prints the target URL; list/routes prints state JSON. clean/prune currently only remove dead process-owned routes. MCP takes a positional profile. These differences prevent drop-in Portless compatibility.
