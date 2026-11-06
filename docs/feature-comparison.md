# Feature comparison

Baseline: [vercel-labs/portless at `49586cfa566ffce8dcc4368f0b74fce560dced49`](https://github.com/vercel-labs/portless/tree/49586cfa566ffce8dcc4368f0b74fce560dced49), frozen on 2026-11-06. Re-check against this commit, not a moving main branch. Source checkout: `/tmp/porch-portless-reference`.

Status is intentionally conservative: Porch CLI source and manifest arrived during this documentation pass. Every row below is **pending parity verification**. Basic alias persistence is covered by integration tests; see the current Porch snapshot in reference.md. The interface contract describes intended modules, not proven behavior. Neither matching command names nor successful compilation establishes parity.

| Surface | Frozen Portless behavior | Upstream source | Porch |
| --- | --- | --- | --- |
| App lifecycle | Bare invocation; run; explicit names; --name; --script; --prefix; --app-port; --force; -- separator; child exit status and signal cleanup | cli.ts, auto.ts | Pending |
| Naming and configuration | package.json inference; directory/git fallback; linked worktree labels and hostname templates; portless.json; package.json portless key; workspace apps; pnpm/npm/yarn/bun; Turborepo | config.ts, workspace.ts, turbo.ts | Pending |
| Proxy | Daemon and foreground; auto-start and remembered settings; loopback IPv4/IPv6; HTTP/1.1; HTTP/2; WebSocket Upgrade and extended CONNECT; streaming; forwarding headers; loop detection; unknown-route pages | proxy.ts, cli.ts, pages.ts | Pending |
| Routing | Strict host matching; optional most-specific parent wildcard; multiple and overlapping TLDs; static aliases; TLD-scoped aliases; get and list | routes.ts, cli.ts | Pending |
| TLS | Local CA generation and trust; custom certificate/key pair; HTTP redirect; Node extra CA injection; Linux/macOS/Windows/WSL trust integration | certs.ts, windows-ca.ts | Pending |
| Host integration | Hosts sync/clean and authenticated auto-sync; startup services via launchd/systemd/Task Scheduler; unprivileged mode | hosts.ts, hosts-sync-auth.ts, service.ts | Pending |
| LAN | mDNS .local; explicit or discovered address; network change following; LAN binding and certificate behavior | mdns.ts, lan-ip.ts | Pending |
| Sharing | Tailscale Serve; public Funnel; ngrok; Cloudflare quick tunnels; tunnel cleanup and child URL variables | tailscale.ts, tunnel.ts, cloudflare.ts | Pending |
| Compose | compose up/down; Docker Compose flag forwarding; published port discovery; static route reconciliation | compose.ts, cli.ts | Pending |
| Framework adaptation | PORT/HOST injection; Vite/VitePlus, Astro, React Router, Angular, Expo, React Native, Wrangler; package-manager script resolution | cli-utils.ts | Pending |
| Diagnostics and cleanup | doctor; trust; prune and --force; clean; route ownership and orphan cleanup | cli.ts, clean-utils.ts | Pending |
| MCP | stdio JSON-RPC; tool profiles; route discovery and alias management; optional proxy/diagnostic tools; no dev-server spawning | mcp.ts | Pending |

Parity acceptance requires observable integration coverage: route creation/removal, actual HTTP forwarding, streaming and WebSocket traffic, TLS trust behavior, child environment and exit propagation, stale route recovery, concurrent state writes, and platform-specific service behavior. Privileged host operations and public tunnel creation must be tested separately from ordinary CI.

The installed Portless skill predates this baseline: it omits Cloudflare, Compose, MCP, worktree prefix/template configuration, and unprivileged mode. The pinned source takes precedence.

## Final integration verification

Porch is a working partial implementation, not full Portless parity. Unit and CLI tests cover config, TLS issuance, MCP framing/profiles, route selection, headers, hosts transformation, and persistent aliases. The network integration test exercises actual HTTP and HTTPS forwarding, WebSocket echo over HTTP, and SSE content through both transports in isolated state. It does not prove delayed streaming latency, WebSockets over TLS, extended CONNECT, or system trust.

Implemented: locked atomic route writes, launch ownership collision rejection, PORT/HOST injection, explicit framework port flags for vite/next/astro/nuxt, app-port, package dev fallback, inferred run names and worktree templates, workspace listing, aliases, dynamic proxy routing, local CA SNI issuance, daemon commands, stale PID pruning, Compose published-port registration, and mDNS HTTP service announcement.

Remaining gaps: automatic proxy startup; full option/environment parity; lossless child exit-code and process-group signal forwarding; PID reuse-resistant ownership; strict daemon startup readiness; simultaneous IPv4/IPv6 listeners; loop detection; TLS HTTP redirects and Node CA injection; extended CONNECT; full framework detection; workspace multi-process orchestration; Turborepo graph support; authenticated hosts auto-sync; Windows startup services; network-change mDNS reannouncement and route-specific DNS records; tunnel cleanup; Compose stale-route removal/project-scoped reconciliation and global Compose option discovery. Compose/Docker and LAN multicast have not been exercised against live external services. Privileged operations and public tunnels are untested.

Final local validation: 16 tests passed (12 unit, 3 CLI integration, 1 network integration). Concurrent CLI test preserves 24 independent writers. cargo fmt --check, cargo clippy --all-targets -- -D warnings, cargo test, cargo build --release, and release-binary isolated alias smoke passed on macOS. Origin privacy verified before pushing.
