# Apps across machines

Porch 0.2 adds an optional native macOS menu bar / Linux tray companion and authenticated discovery agents. The headless CLI remains standalone. Both use `porch list --all --json`, a versioned snapshot of local apps, paired machines, their apps, discovery hints, and connection errors.

## Install and start

Build the CLI alone with `cargo build --release --bin porch`. Build the desktop package with `cargo build --release --features tray --bins`, then follow [packaging instructions](../packaging/README.md). macOS Apple Silicon and Linux x86_64 are supported; Windows is deferred. Private CI artifacts contain the CLI and tray; no public release is created.

Run `porch tray`, or open the installed `Porch.app` on macOS. The tray lists local apps and paired machines, status and errors, Open / Copy URL, Refresh / reconnect, and proxy Start / Stop. It runs the existing CLI for lifecycle actions. Quit closes only the tray. Apps, proxy, the independently enabled discovery agent, and browser relays continue running. Stop them explicitly using their commands. Login startup is optional and requires an explicit packaging-script action; installation and launch do not enable it or network sharing.

Linux desktop builds need GTK3 and an Ayatana AppIndicator implementation. GNOME usually needs the AppIndicator shell extension; a missing status-notifier host can hide the icon even when the process is running. No display produces an actionable CLI error. The ordinary `porch` binary has no GTK/AppIndicator runtime dependency. Use `porch list --all`, `porch open`, and `porch copy` without the tray.

## Connect two machines

On machine B, run your development apps normally. Select the private interface on which paired peers may contact this machine. For LAN use its LAN address; for Tailscale use its `100.x` or tailnet IPv6 address. Porch refuses public and wildcard binds. This command starts a separate user-level discovery agent; it does not change the app proxy's bind, TLS, port, hosts, or trust configuration:

```sh
# On B: use B's actual private interface IP.
porch discovery enable --bind 192.168.1.20 --share-apps
porch discovery fingerprint
porch discovery pair-token
```

`--share-apps` explicitly grants enrolled peers browser access to every registered HTTP/WS development app, including apps behind a loopback-only proxy. Omit it to share only the authenticated catalog; these apps appear as local-only. This is an independent route transport, not a claim that remote `.localhost` or `.local` URLs resolve on A. Do not grant access to peers you would not trust to use your development apps.

On A:

```sh
porch peers refresh
# Compare B's fingerprint through a trusted channel. Discovery never establishes trust.
porch peers add https://192.168.1.20:1356 --fingerprint SHA256_FROM_B
# Paste B's short-lived, single-use token on standard input, then Enter.
porch list --all
porch open web --peer MACHINE_ID_FROM_LIST
porch copy web --peer MACHINE_ID_FROM_LIST
# Headless / automation: print a usable relay URL instead of launching a browser.
porch open web --peer MACHINE_ID_FROM_LIST --url-only
```

Use a private token file with `--token-file PATH` for automation; do not put tokens in shell arguments/history. Machine names are accepted by Open / Copy only when unambiguous. Pair each direction if both machines should read/open each other's apps. Explicit enrollment works when multicast is blocked. A custom agent port must be supplied in the explicit endpoint; automatic Tailscale hints use port 1356.

LAN discovery browses real `_porch._tcp.local.` service records. Advertisements contain only an opaque machine ID, protocol version and agent endpoint, not app/project names or secrets. Candidate hints expire after 90 seconds and goodbye events remove them during browsing. Refresh/reconnect starts a new bounded browse and queries enrolled agents. Tailscale discovery executes `tailscale status --json`, filters self/offline peers, and proposes their assigned tailnet addresses. It does not depend on multicast traveling over Tailscale. A candidate is not a catalog: the agent must be installed, listening on the selected interface, allowed by the tailnet ACL, and explicitly paired. Missing Tailscale does not block local or LAN use.

One agent binds one chosen interface. Use the LAN address for LAN peers or the tailnet address for tailnet peers; change exposure explicitly with Disable then Enable. Do not assume a LAN-bound agent is reachable through the tailnet address. Porch never configures Tailscale Serve or Funnel for discovery, never changes an unrelated registration, and never enables a public tunnel.

## Trust and URL behavior

Enrollment requires both the exact SHA-256 fingerprint of the agent's self-signed certificate and its 256-bit invitation token. The invitation expires after ten minutes and is consumed once. Later requests use a per-client random bearer; only its hash is stored by the server. Client bearer material is kept in private local state (0700 directory / 0600 files), excluded from snapshots and normal output. The TLS pin validates the exact certificate and TLS signatures; it does not add a remote CA to any global trust store. A changed certificate requires explicit enrollment after comparing the new fingerprint.

Agent endpoints must be literal private LAN, tailnet or loopback IPs. Discovery hints cannot trigger arbitrary DNS/public-URL fetches. Requests reject redirects; control responses, candidate counts, concurrency, connection/header/body timeouts and body sizes are bounded. Browser Origin requests are rejected on the agent API. There are no remote lifecycle, shell, proxy configuration or state mutation endpoints; authenticated browser app requests naturally retain the development app's own behavior and permissions.

Opening a peer app starts a loopback-only browser relay on A. The browser receives `http://UNIQUE_APP_PEER.localhost:PORT`, never B's localhost URL. The relay authenticates to B over pinned TLS and routes to B's registered app ID, preserving its configured host routing. B validates HTTPS upstream certificates against ordinary roots plus B's own Porch CA. There is no global TLS verification bypass. WebSocket upgrades and streamed HTTP use the same authenticated transport. Redirects to the app's configured origin are translated to A's relay origin; other loopback redirect targets are refused. Cookie domain attributes are removed for the relay's host-only context. A distinct `.localhost` hostname per machine/app prevents cookies from crossing apps (cookies do not distinguish ports). Browsers resolve `.localhost` to loopback; headless HTTP tools may need an explicit resolver mapping to `127.0.0.1`.

The browser-facing hop is local HTTP, including when the original proxy uses HTTPS. This does not preserve the browser-visible HTTPS origin or remote-domain cookies. Localhost secure-context rules, application CORS/CSRF checks, OAuth callbacks, certificate-bound behavior and absolute URLs embedded in HTML/JavaScript may need an explicit development origin setting. Porch cannot safely rewrite arbitrary application code or make a machine-local URL globally reachable. Native remote `.local` DNS and trust are not required by this transport; using those URLs directly still requires the LAN resolver and explicit certificate trust setup.

## Stop and revoke

```sh
porch discovery status --json
porch discovery revoke       # On B: revoke all clients and outstanding invitations immediately.
porch peers remove ID       # On A: forget trust; does not revoke credentials on B.
porch discovery disconnect  # On A: stop local browser relays, leaving apps/proxy running.
porch discovery disable     # On B: stop exposure; apps/proxy keep running.
```

Disable retains the machine identity, certificate and enrollment history so re-enabling does not silently change trust. Use Revoke to invalidate credentials. The discovery agent is a separate user process; it is not automatically re-enabled at login. Changing network interfaces requires explicit reconfiguration of the bound address. The tray periodically refreshes its model and exposes connection errors and disabled/local-only apps.

## Wire protocol and evidence

All control APIs use HTTPS, JSON and `version: 1`. `POST /v1/pair` accepts `{ "token": "..." }` and returns `{ "version": 1, "machine": {"id", "name"}, "token": "..." }`. `GET /v1/catalog` requires `Authorization: Bearer ...` and returns `Catalog` (`machine`, stable per-machine app IDs, names, machine-local URL, remote-access permission/reason, health, proxy status, generation timestamp). Authenticated `/v1/apps/APP_ID/proxy/PATH` carries the explicitly granted HTTP/WS route transport. Internal `x-porch-app-*` fields carry original browser headers; the relay strips caller-supplied versions and the server never forwards its bearer to apps.

`porch list --json` retains its previous local format. `porch list --all --json` is the stable versioned external discovery format: `version`, `local`, `peers`, `candidates`, `warnings`. A peer has identity, endpoint, sources, last-seen time, optional catalog and error. No bearer/pairing secrets appear. Health is conservative: a live registered process and proxy do not prove application readiness, so an unprobed app can be `unknown`; stopped ownership is reported explicitly. An unavailable peer has no usable catalog rather than stale apps presented as reachable. Sources aggregate authenticated identity across explicit/LAN/Tailscale hints.

Automated acceptance uses real TLS/HTTP/WebSocket sockets, two isolated state directories, the actual CLI, same-named apps targeting distinct upstreams, wrong pins, token replay/revocation, malformed/oversized responses, route/redirect translation, DNS-rebinding rejection, expiry/removal/deduplication, and a fake Tailscale status adapter. Native menu-model tests exercise actionable and disabled states and generated CLI actions. See [verification record](discovery-verification.md) for commands and observed results. Loopback two-node acceptance is not physical multi-machine evidence. Physical LAN multicast/interface transitions, a real enrolled Tailscale peer, Linux desktop/indicator display and macOS rendered menu interaction require separate checks. Headless and compile checks do not establish a rendered UI.
