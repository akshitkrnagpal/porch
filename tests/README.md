# Acceptance tests

Run `cargo test --all-targets`. Tests use unique temporary state and loopback listeners. Real process fixtures require Node.js/npm, Python 3, and Git. POSIX shell/process-group fixtures are gated to Unix where applicable.

| File | Observable coverage |
| --- | --- |
| `cli.rs` | Static aliases and browser URLs, human/JSON list, malformed flags, concurrent route writers, actual child HTTP/verified HTTPS forwarding with proxy auto-start/readiness, concurrent launches, Node CA environment, self-daemon route transfer, bare/inferred/named launch, exit codes, bypass, persisted daemon settings, bind failure, orphan process cleanup |
| `pty.rs` | Controlling-PTY stty raw, Node raw mode, Ctrl-C/TERM forwarding, descendant cleanup, real Turbo 2 TUI and two live HTTP routes |
| `lifecycle.rs` | Delayed/ignored-signal fixed-port force takeover, leaderless orphan grandchildren, blocked-port refusal, stale reservation recovery, scoped alias removal over real HTTP |
| `config.rs` | Package/workspace/config precedence, duplicate hostnames, actual linked Git worktree, framework/runners/server-only flags, 22 real npm/shell argv cases, Expo LAN behavior, root Turbo command selection, real CLI workspace runner and two package processes, Node preload and wrapper recursion handling |
| `network.rs` | Actual IPv4/IPv6 sockets, HTTP/HTTPS/h2, generated CA verification, WS/WSS, extended CONNECT WebSocket frames, delayed streamed events, routing/TLD/wildcard/alias semantics, redirects, structured errors, recursive loop detection, authenticated hosts sync |
| `integrations.rs` | Fake Docker Compose model and published-port reconciliation, project scoping, all three OS service definitions, temporary hosts safety, Tailscale/ngrok/Cloudflare readiness/URL/process ownership, recorded trust installation/removal |

Unit tests additionally exercise configuration validation, hostname truncation, routing scores, headers/errors, TLS PEM/certificate issuance, host authentication proofs, service serialization, and MCP framing/profiles/arguments.

External integration tests place fake executables on an isolated PATH and point hosts/service/trust paths at temporary fixtures. These tests inspect generated arguments/files and owned subprocess cleanup; they do not assert that a real OS administrator operation, Docker Engine, provider account, or remote machine accepted the operation.

Native startup after boot, system/browser trust installation, live Docker, authenticated provider registration, physical LAN multicast resolution, and real network-interface transitions need separate platform/service verification. Ordinary CI must not mutate real hosts, trust, or startup resources or create public tunnels. New integration tests should retain deadline-based readiness checks and cleanup on failure.


CI installs Turbo 2.6.3 and sets `PORCH_TEST_TURBO_REQUIRED=1` for mandatory real TUI coverage. Locally set `PORCH_TEST_TURBO=/absolute/path/to/turbo` if it is not on PATH; without Turbo 2, the core PTY cases run and the Turbo case reports a skip. Fresh sudo acceptance uses a prompt shim attached to a PTY and never invokes real sudo or privileged ports. Collision tests hold wildcard IPv4 and dual-stack IPv6 listeners; the accept-error fixture lowers only its child daemon's descriptor limit. Hosts permission fixtures are temporary read-only files.
