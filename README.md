# Porch

All your dev apps, across your machines, one click away. Porch gives local development servers stable names such as `https://web.localhost`, assigns free app ports, and manages the shared reverse proxy and child processes. It is a standalone Rust CLI inspired by the [Portless](https://github.com/vercel-labs/portless) development workflow.

Porch is under active development; full Portless parity and daily-use readiness are not yet established. The [original independent review](docs/reviews/opus-2026-11-06.md) is preserved alongside the [remediation evidence](docs/remediation-2026-11-06.md).

**Supported build targets:** macOS Apple Silicon and Linux x86_64. Windows is deferred and is not included in CI or release artifacts.

The optional native menu bar / tray companion lists local apps and authenticated LAN/Tailscale peers. Start with the [two-machine and desktop guide](docs/discovery.md); sharing and pairing are explicitly opt-in.

Build and run:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
porch                         # runs the package's dev script; starts the proxy
porch run next dev            # infers the app name
porch api node server.js      # https://api.localhost
porch get api                 # prints the browser-facing URL
porch list --json
```

The default proxy uses HTTPS on port 443. When binding requires privileges, terminal sessions prompt with sudo; unattended sessions need cached authorization or explicit setup/high-port options. Porch never silently changes the requested port or security mode. For an isolated, unprivileged high-port setup:

```sh
PORCH_STATE_DIR=/tmp/my-porch PORCH_SYNC_HOSTS=0 \
  porch proxy start --unprivileged --port 1355 --no-tls
PORCH_STATE_DIR=/tmp/my-porch porch api node server.js
# http://api.localhost:1355
PORCH_STATE_DIR=/tmp/my-porch porch proxy stop
```

Apps receive `PORT`, `HOST`, and `PORCH_URL`. Supported frameworks get their required port/host flags automatically. HTTPS children also receive `NODE_EXTRA_CA_CERTS` pointing at the local CA. Child arguments remain intact after the first command token or an explicit `--` separator.

Per-directory configuration precedence is `porch.json` > `package.json` `porch` > `portless.json` > `package.json` `portless`. The selected source replaces lower-priority sources. Leaf configuration overrides matching root `apps` entries; root worktree settings are inherited. A root name forms the project domain, while root appPort/script do not become leaf defaults for direct leaf launches. Bare invocation at a workspace root runs discovered packages; Turbo projects retain their root task graph and receive per-package ports and URLs through a Node preload. Use bare `porch` (or `porch -- --filter=web`) at a Turbo workspace root; named `porch shop pnpm run dev` is rejected when it would give multiple packages one port.

```json
{
  "apps": {
    "apps/web": { "name": "web.shop" },
    "apps/api": { "name": "api.shop", "appPort": 4500 }
  }
}
```

Porch supports multiple TLDs, strict or parent-wildcard routing, IPv4/IPv6 listeners, HTTP/2, WebSockets including extended CONNECT, streamed responses, authenticated LAN/Tailscale app discovery, an optional native tray, static aliases, Docker Compose, sharing providers, startup services, and stdio MCP. See the [command and configuration reference](docs/reference.md) and [evidence-based compatibility matrix](docs/feature-comparison.md).

`trust`, `hosts`, `service`, and `clean` manage the corresponding OS resources when explicitly invoked. Automated tests use isolated state, temporary hosts files, and fake external CLIs. Real OS trust/startup installation, authenticated public tunnels, and physical LAN/Tailscale discovery and rendered desktop behavior still require platform/service verification; service definitions and command flows have been tested with fixtures; privileged installation remains unverified.

Run `cargo test --all-targets` for acceptance coverage. For mandatory real Turbo TUI coverage, install Turbo 2.6.3 and set `PORCH_TEST_TURBO_REQUIRED=1`; CI does this on macOS and Linux. The test suite uses Node.js/npm, Python 3, and Git for real child-process fixtures. No public release is published by the build workflow.

Legacy `portless.json`, the `package.json` `portless` key, and `PORTLESS_*` variables remain compatibility fallbacks. Native Porch configuration takes precedence.
