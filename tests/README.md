# Tests

Run `cargo test --all-targets`. Each test uses its own temporary state directory and loopback listeners. Tests that start real processes need Node.js with npm, Python 3 and Git.

| File | What it covers | Platforms |
| --- | --- | --- |
| `cli.rs` | Aliases and URLs, human and JSON `list`, bad flags, concurrent route writers, forwarding to a real app over HTTP and verified HTTPS, proxy auto-start, concurrent launches, `NODE_EXTRA_CA_CERTS`, handing a route to a daemon, named and inferred launches, exit codes, `PORCH=0`, saved proxy settings, bind failures, orphan cleanup, `--private` and other launch options in every position, strict on/off variables, LAN mode that isn't saved by an app launch, fields from a newer Porch kept on rewrite, background logs across `--force` and with invalid UTF-8 | All; some cases macOS and Linux only |
| `network.rs` | IPv4 and IPv6 sockets, HTTP, HTTPS and HTTP/2, the generated CA, WebSocket over HTTP and HTTPS, extended CONNECT, streamed responses, routing, TLDs, wildcards and aliases, redirects, error pages, proxy loops, authenticated hosts-file sync | All |
| `discovery.rs` | Two Porch instances pairing by code and by token, opening each other's apps (including WebSockets), wrong codes, rogue hosts, refusing the old pairing exchange, cancelling a shown code, discovery autostart | All; autostart on macOS and Linux only |
| `relay_load.rs` | A Vite-like page through a paired machine's URL over HTTP and HTTPS: a burst of HTTP/2 module requests shares one agent connection, listing and opening apps warm that connection, and the proxy's peer list needs its local token. An ignored benchmark compares page loads with the local proxy at a simulated round-trip time | All |
| `config.rs` | Configuration precedence, duplicate names, a real linked Git worktree, frameworks and package runners, which `node_modules/.bin` directories reach the child's PATH, real npm and shell command shapes, Expo LAN, Turbo at the root, launching two workspace packages, the Node preload, one worktree lookup per workspace launch | macOS, Linux |
| `first_run.rs` | Default port and no-sudo mode, busy and explicit ports, the listen warning, stale route cleanup, proxy upgrades, `porch setup` and `porch doctor --fix`, `porch setup` leaving discovery autostart alone | macOS, Linux |
| `lifecycle.rs` | Force takeover of a fixed port (including apps that ignore signals), orphaned grandchildren, refusing a blocked port, recovering stale reservations, removing scoped aliases, never adopting a server that was running before the command, finding a free app port without `lsof` | macOS, Linux |
| `pty.rs` | Raw mode on a controlling terminal, Ctrl-C and TERM forwarding, descendant cleanup, the real Turbo 2 TUI with two live routes | macOS, Linux |
| `integrations.rs` | Docker Compose with a fake `docker`, launchd, systemd and Task Scheduler entries, the hosts file, Tailscale, ngrok and Cloudflare, installing and removing trust, `porch clean` ending pairing and removing logs and leftovers | macOS, Linux |

Unit tests in `src/` cover configuration validation, name shortening, route scoring, headers and error pages, PEM handling and certificate issuance, hosts-file authentication, service files, and MCP framing, profiles and arguments.

## External tools

Tests for Docker, Tailscale, ngrok, Cloudflare, service managers and trust stores put fake executables on a private PATH and point hosts, service and trust paths at temporary files. They check the arguments and files Porch produces and that it cleans up the processes it starts. They don't show that a real system, Docker Engine, provider account or remote machine accepted the change.

Start at boot, real system and browser trust, live Docker, provider sign-up, LAN multicast and network interface changes all need checking on a real machine. CI must never change real hosts, trust or startup settings, or open public tunnels.

## Writing tests

Tests run in parallel threads, and several test processes may run at once. Every rule here removes a way one test can change another's outcome. The helpers are in `tests/common/mod.rs`, which the unit tests in `src/` share.

- **Ports.** Get any port you hand to Porch (proxy, discovery agent or `--app-port`) from `common::free_port()`. Don't bind `127.0.0.1:0` and drop the listener: the OS can give that port to another test, which your probe then reaches. Each `free_port()` result is the start of a private 256-port slot below the OS's ephemeral range, so Porch's search for the next free port never reaches another test's ports. A test's first call claims all its slots at once (eight at most) with lock files shared by every test process, after checking that the ports tests bind or hand to Porch are free; the claim ends when the test's thread does. A test waiting for slots holds none, so concurrent tests cannot deadlock.
- **App ports.** Set `PORCH_APP_PORTS` to `common::app_ports(proxy_port)` in every sandbox that launches apps. Otherwise Porch picks app ports from the machine-wide 4000–4999 pool, and a parallel test's Porch can take a port your app was given but has not bound yet, or ignores.
- **Ports nothing listens on.** Use `common::refused_port()`, a socket bound but not listening, rather than a port you freed.
- **Listener ownership and release.** A socket this multi-threaded process opens is copied into any child another test forks at that moment and stays open there until the child execs. If a test asserts that a port is free after closing a listener, or which process owns one, hold the listener in a child process with `common::ChildListener`. Check availability with `common::bindable`, which binds from a child process.
- **Own your accept loops.** A fixture that counts or asserts on every connection it accepts must only be reachable by its own client. Prefer in-memory streams (`tokio::io::duplex`) where no real socket is needed.
- **Names.** Name temporary directories with the process id and a per-process counter, never the clock: macOS clocks have microsecond resolution, so parallel tests would share a directory.
- **Waiting.** Wait for something you can observe (a route, a log line, a file written by rename) with a deadline that only bounds a broken run. Don't rely on a fixture running for a fixed time or on how fast something happens, and don't retry an operation whose result you assert. Publish files that a test parses atomically (write a temporary file, then rename). Clean up even when the test fails.

## Turbo and sudo

CI installs Turbo 2.6.3 and sets `PORCH_TEST_TURBO_REQUIRED=1`, so the real Turbo TUI test must run. Locally, set `PORCH_TEST_TURBO=/absolute/path/to/turbo` if `turbo` isn't on your PATH. Without Turbo 2, the other terminal tests still run and the Turbo test reports a skip.

Tests never run real sudo or bind privileged ports; the sudo flow uses a fake prompt attached to a terminal. Port-collision tests hold wildcard IPv4 and dual-stack IPv6 listeners, the accept-error test lowers the file descriptor limit of its own proxy only, and hosts-file permission tests use temporary read-only files.

## Real frameworks (`tests/e2e.rs`)

`python3 -m http.server`, which ignores `PORT`, always runs through Porch, which adds its port and bind address. With `PORCH_E2E=1` (set in CI on Linux and macOS), the suite also installs Next.js 15 (page and HMR socket), Astro 5 (which needs the port flags Porch adds) and Vite 6 from npm, runs it with `porch run`, and checks through the proxy that the page and Vite's client module load under the Porch name, that Vite's HMR WebSocket connects with its token, and that editing a module sends an HMR update over that socket. Without the variable the test is skipped, so offline runs stay green.
