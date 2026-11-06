# Review remediation, 2026-11-06

Baseline: `8030fdb`. The [independent Opus 5.5 report](reviews/opus-2026-11-06.md) is the original pre-fix assessment, preserved with provenance. These changes address its confirmed P0/P1 blockers; passing this acceptance suite does not establish complete Portless parity or daily-use readiness.

## Reproduction and fixes

| Finding | Red-capable command and observed failure | Resulting behavior |
| --- | --- | --- |
| P0-1 terminal job control | `cargo test --test pty -- --nocapture`: `stty raw` never reached PTY_OK; actual CLI hung beyond 10s | Unix children call setsid before exec. They retain terminal descriptors while avoiding background controlling-terminal job stops. PTY tests cover stty raw, Node setRawMode, a real Ctrl-C byte, TERM forwarding, descendant cleanup, and a real two-package Turbo 2.6.3 TUI serving both URLs. Existing non-TTY lifecycle tests remain green. |
| P0-2 fresh sudo | `cargo test --lib fresh_sudo_credentials_prompt_with_a_terminal -- --nocapture`: shim rejected cached-only `sudo -n true`, producing port 443 privilege failure | Terminal startup runs sudo -v interactively before detached launch. Non-TTY/missing-sudo cases offer terminal setup or explicit high-port HTTP/unprivileged choices. Both primary and redirect-port probes consider privilege. No silent fallback changes the requested security mode. Tests use a PTY prompt shim, never real sudo or privileged binds. |
| P1-1 hosts capability | `cargo test --test integrations hosts_permissions_are_actionable_and_localhost_hints_once -- --nocapture`: read-only fixture returned only HTTP 403 | Proxy records write capability; clients avoid doomed sync requests. Custom-domain errors retain setup guidance and response bodies. Permission/sync errors return 409, invalid tokens 403. Browser localhost failures produce one hint per state directory. Existing challenge/proof, Origin, loopback and token tests pass. |
| P1-2 port collisions | `cargo test --test cli proxy_and_app_refuse_wildcard_port_collisions -- --nocapture`: proxy silently shared an IPv4 wildcard port on macOS | One exclusive socket implementation serves probes, proxy binds and app reservations. It never enables SO_REUSEADDR; reservations cover IPv4/IPv6 wildcard addresses. Regressions hold IPv4 wildcard and dual-stack IPv6 listeners and assert proxy/app refusal. |
| P1-3 root leakage | `cargo test --test config leaf_does_not_inherit_workspace_root_app_settings`: root name/appPort/script leaked into leaf | Leaves inherit worktree settings plus matching apps entries. Project-qualified names agree between root and leaf. Two actual concurrent leaf launches verify distinct web.shop/api.shop routes and ports. |
| P1-4 precedence | `cargo test --test config all_config_callers_follow_the_same_source_precedence`: load selected start, effective selected dev | All callers select porch.json > package porch > portless.json > package portless, without merging competing per-directory sources. Matrix covers load/effective/plans/Turbo, rootapps, leaf overrides and malformed native configuration. |
| P1-5 issuance policy | `cargo test tls_policy --lib -- --nocapture`: foreign issuance accepted; parsed leaf lacked serverAuth | Configured/registered names only, plus LAN.local where enabled; foreign SNI handshake rejected and cached foreign-route cert reauthorized after removal. Parsed leaves have hostname CN/SAN, serverAuth and 365-day validity. New CAs use 3650 days and pathLen 0. |
| Unsafe explicit graph | `cargo test --test config explicit_workspace_graph_commands_reject_a_shared_app_port`: named root pnpm run dev returned one shared-port plan | Multi-package Turbo graph commands reject with bare-porch/coordinator-filter guidance. Single explicit app commands remain supported. |
| Accept errors | `cargo test --test cli proxy_recovers_after_accept_fd_exhaustion`: restoring old accept-error propagation killed the fixture daemon, resetting connections | Listener logs, backs off and continues. Child-only descriptor-limit exhaustion fixture proves recovery and subsequent HTTP response. |
| False doctor readiness | `cargo test --test integrations doctor_does_not_call_a_foreign_listener_ready`: a foreign HTTP 200 server was called reachable | Doctor checks saved native process identity/readiness plus HTTP challenge proof on both loopback families, labels foreign/unverified listeners, and displays lsof output where available. |

Additional contained changes: proxy.log is 0600 (including existing files); forwarded requests write both x-porch-hops and the legacy x-portless-hops; clean requests hosts elevation only when a managed block exists, rather than because sync defaults on.

## Integrated local verification

On macOS ARM64:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
PORCH_TEST_TURBO=/tmp/porch-turbo-pty-acceptance-20261106/node_modules/.bin/turbo \
  PORCH_TEST_TURBO_REQUIRED=1 cargo test --no-fail-fast --all-targets
cargo build --release
```

The integrated test run passed **87 tests**: 34 unit, 14 CLI, 20 config, 12 integration fixtures, 5 lifecycle, 1 comprehensive network, 1 PTY acceptance. One ignored unit helper is deliberately invoked by both sudo acceptance harnesses; it is not skipped coverage. Native CI installs pinned Turbo 2.6.3 and requires real TUI acceptance on macOS and Linux.

Release build passed. The release executable separately passed all five PTY scenarios, including real Turbo TUI/HTTP routes, and an isolated HTTPS smoke with strict OpenSSL validation, foreign SNI rejection, 0600 log mode, TERM exit143 and route cleanup. Installation uses temporary-file + fsync + rename at `~/.local/bin/porch`; no running daemon is restarted. Native CI results are recorded after the implementation push. No public release or PR is requested.

## Migration and remaining limits

Existing CA/key bytes and their original validity are retained to avoid silently invalidating installed trust. Profile-v2 leaves and SNI restrictions apply after daemon restart, with a one-time legacy-root hint. New roots meet the shorter lifetime; existing roots require explicit replacement and trust setup to change their lifetime. The root has no fixed DNS NameConstraints because configured TLDs/registered domains may change; the resolver enforces the issuance boundary. An attacker who obtains the private root key is outside that resolver boundary.

Real privileged trust installation and Safari/Chrome/Firefox system-trust behavior remain untested. The review's possible browser rejection was a hypothesis, not an established failure. Boot-time launchd/systemd operation, live Docker Compose, authenticated Tailscale/ngrok/Cloudflare, physical LAN/mDNS and interface transitions remain external smoke-test requirements. Windows remains explicitly deferred.

Lower-priority gaps retained deliberately: request-time state parsing/process checks have no cache; uppercase/underscore configured names are still rejected instead of normalized; proxy stop --port uses the saved proxy rather than separate per-port state; unattended default TLS requires prior trust or explicit skip-trust/client CA loading; shell wrappers that hide workspace graph executables are not exhaustively detected. Strict non-reuse binding may wait for OS TCP socket retirement on a recently used fixed port; Porch reports the bind error instead of silently sharing it. No performance/cache refactor was included.

The installed CLI is replaced atomically. Running daemons and already launched child processes keep their old executable/session state until the user restarts them. The user's offerkit session, real hosts/trust/startup resources and other project files were not changed.
