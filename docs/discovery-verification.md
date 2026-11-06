# Discovery and native menu verification — 2026-11-06

Implementation baseline: `95af621`, version 0.2.0. This record distinguishes real local socket/process evidence from physical multi-machine and rendered-desktop evidence.

## Local acceptance on macOS ARM64

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --features tray --all-targets -- -D warnings
PORCH_TEST_TURBO=/tmp/porch-turbo-pty-acceptance-20261106/node_modules/.bin/turbo \
  PORCH_TEST_TURBO_REQUIRED=1 cargo test --no-fail-fast --all-targets
cargo test --features tray --lib
python3 scripts/test_packaging.py
PORCH_TEST_MDNS_LOOPBACK=1 cargo test --lib \
  discovery::client::tests::real_local_mdns_registration_and_browse -- --ignored --nocapture
cargo build --release --bin porch
cargo build --release --features tray --bin porch-tray
python3 scripts/package.py --target aarch64-apple-darwin
```

The headless suite passed **104 tests**, including all 87 prior regressions, actual terminal/PTY/Turbo 2.6.3 operation, HTTP/h2/WebSocket/TLS coverage, ten client tests, six agent tests, and the CLI two-machine acceptance. The tray library run passed **55 tests**, including five additional menu/worker tests (the other 50 overlap the headless unit suite). Three isolated Python packaging/install tests passed. The real loopback mDNS experiment passed separately; its ignored-by-default status avoids treating multicast availability as reliable CI infrastructure. The other ignored helper is deliberately invoked by the sudo PTY harness.

`tests/discovery.rs` runs actual CLI commands in two distinct private state directories. Both machines register `web` with distinct HTTP upstreams. It proves wrong TLS pins cannot pair, one-time tokens cannot be replayed, catalogs do not automatically grant app access, an explicitly shared app opens through A's relay and reaches B, Host spoofing is refused, revocation takes effect, renewed enrollment replaces a relay with stale credentials, peer removal clears local trust, and discovery enable/disable preserves the existing proxy process identity and settings. It does not require or alter the user's state.

Client and server socket tests independently cover authenticated/pinned TLS, malformed/oversized peers, Origin rejection, token expiry, distinct app IDs, correct Host/path/query and browser authorization separation, HTTP redirects/cookies, WebSocket frames across the pinned TLS relay, stale expiry/goodbye handling, LAN/tailnet identity aggregation and self/offline Tailscale filtering. App readiness is conservatively unknown without a health probe. Uploads are capped at 16 MiB; upstream app response headers have a 15-second deadline. Peer refresh has a six-second per-peer and twelve-second overall query budget after candidate collection.

The tray worker test invokes a real executable fixture and observes initial/periodic discovery, app refresh, remote Open arguments and independent Quit. Native menu-model tests verify disabled sharing, unavailable peers, errors and stable app/machine action IDs. Packaging tests preserve an already-open old executable inode across atomic installation, check the background `Porch.app` metadata/archive, and refuse replacing an unrelated app.

## mDNS boundary

An isolated opaque `_porch._tcp.local.` service was registered and resolved by a separate browser on `lo0` (127.0.0.1). The same local experiment on `en0` (192.168.1.95) observed the advertiser's Announce event but no resolved browser records. No project names, catalog or user state were advertised. This is real local multicast/protocol evidence, not successful physical LAN discovery. Explicit endpoint enrollment is available when multicast is unavailable. Physical LAN routing/multicast, network transitions and another actual host remain unverified.

## Installation and GUI boundary

The CLI and native tray are packaged for user-level installation with temporary-file/fsync/rename replacement. Installation/launch do not enable login startup, discovery exposure, sharing, hosts changes or trust changes. The pre-existing user route/session and proxy configuration are preserved. The proxy on port 1355 was already stopped when inspected during this task; its saved settings and the existing `porch shop pnpm run dev` session were not changed or signaled.

macOS Accessibility and Screen Recording are denied to agent-desktop. A native menu screenshot or semantic menu interaction therefore could not be observed. `~/.local/bin/porch` and `porch-tray` were installed atomically at version 0.2.0, together with `~/Applications/Porch.app`. `open ~/Applications/Porch.app` succeeded and the bundled tray process remained running. Discovery status reported `running: false`, `config: null`; `porch-login status` reported disabled. The original proxy/settings state-file hashes matched before and after installation/launch and the existing app session remained alive. Process survival does not establish that every menu item is rendered correctly. No permissions were changed or login startup enabled.

## Native CI and remaining checks

The CI workflow runs the complete headless regression suite on native macOS and Linux, plus dedicated native tray builds/clippy/menu tests on both platforms. Linux installs GTK3/Ayatana AppIndicator development dependencies and checks both no-DISPLAY tray failure and absence of desktop library linkage in the ordinary CLI. The private artifact workflow builds and packages both architectures without publishing a release. Workflow definitions alone are not passing-run evidence; observed run links/results are appended below.

Still required: a real enrolled Tailscale pair, physical two-host LAN discovery/opening, Linux desktop/indicator rendering (including GNOME extension behavior), macOS rendered menu interactions, explicit login-startup behavior, and app-specific OAuth/CORS/absolute-origin compatibility. No public tunnel was created; no unrelated Tailscale Serve/Funnel configuration was modified. Browser access uses a distinct local HTTP origin with a pinned HTTPS remote hop, rather than preserving the original browser HTTPS origin.
