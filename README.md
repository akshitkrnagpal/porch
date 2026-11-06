# Porch

Porch gives local development servers stable names such as `https://web.localhost`, assigns free app ports, and manages the shared reverse proxy and child processes. It is a Rust implementation of the [Portless](https://github.com/vercel-labs/portless) development workflow.

**Supported release targets:** macOS Apple Silicon and Linux x86_64. Windows is deferred and is not included in CI or release artifacts.

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

The default proxy uses HTTPS on port 443. For an isolated, unprivileged high-port setup:

```sh
PORCH_STATE_DIR=/tmp/my-porch PORCH_SYNC_HOSTS=0 \
  porch proxy start --port 1355 --no-tls
PORCH_STATE_DIR=/tmp/my-porch porch api node server.js
# http://api.localhost:1355
PORCH_STATE_DIR=/tmp/my-porch porch proxy stop
```

Apps receive `PORT`, `HOST`, `PORTLESS_URL`, and `PORCH_URL`. Supported frameworks get their required port/host flags automatically. HTTPS children also receive `NODE_EXTRA_CA_CERTS` pointing at the local CA. Child arguments remain intact after the first command token or an explicit `--` separator.

Use `portless.json` or the `package.json` `portless` key for app configuration. Bare invocation at a workspace root runs discovered packages; Turbo projects retain their root task graph and receive per-package ports and URLs through a Node preload.

```json
{
  "apps": {
    "apps/web": { "name": "web.shop" },
    "apps/api": { "name": "api.shop", "appPort": 4500 }
  }
}
```

Porch supports multiple TLDs, strict or parent-wildcard routing, IPv4/IPv6 listeners, HTTP/2, WebSockets including extended CONNECT, streamed responses, LAN discovery, static aliases, Docker Compose, sharing providers, startup services, and stdio MCP. See the [command and configuration reference](docs/reference.md) and [evidence-based compatibility matrix](docs/feature-comparison.md).

`trust`, `hosts`, `service`, and `clean` manage the corresponding OS resources when explicitly invoked. Automated tests use isolated state, temporary hosts files, and fake external CLIs. Real OS trust/startup installation, authenticated public tunnels, and physical LAN discovery still require platform/service verification; service definitions and command flows have been tested with fixtures; privileged installation remains unverified.

Run `cargo test --all-targets` for acceptance coverage. The test suite uses Node.js/npm, Python 3, and Git for real child-process fixtures. No public release is published by the build workflow.
