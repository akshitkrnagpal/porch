# Apps across machines

Porch 0.2 adds an optional native macOS menu bar / Linux tray companion and authenticated discovery agents. The headless CLI remains standalone. Both use `porch list --all --json`, a versioned snapshot of local apps, paired machines, their apps, discovery hints, and connection errors.

## Install and start

Build the CLI alone with `cargo build --release --bin porch`. Build the desktop package with `cargo build --release --features tray --bins`, then follow [packaging instructions](../packaging/README.md). macOS Apple Silicon and Linux x86_64 are supported; prebuilt Linux GNU artifacts require glibc 2.39+ (build from source on older distributions). Windows is deferred. Private CI artifacts contain the CLI and tray; no public release is created.

Run `porch tray`, or open the installed `Porch.app` on macOS. The tray has **Pair a device…** and lists local apps and paired machines, status and errors, Open / Copy URL, Refresh / reconnect, and proxy Start / Stop. It runs the existing CLI for lifecycle actions. The icon shows state: solid dots under the roof while the proxy runs, dimmed dots when it is stopped, and a small top-right badge while a paired machine is reachable. Quit closes only the tray. Apps, proxy, the independently enabled discovery agent, and browser relays continue running. Stop them explicitly using their commands. Login startup is optional and requires an explicit packaging-script action. Installation and launch do not enable it or network sharing; only pairing does.

Linux desktop builds need GTK3 and an Ayatana AppIndicator implementation. GNOME usually needs the AppIndicator shell extension; a missing status-notifier host can hide the icon even when the process is running. No display produces an actionable CLI error. The ordinary `porch` binary has no GTK/AppIndicator runtime dependency. Use `porch list --all`, `porch open`, and `porch copy` without the tray.

## Connect two machines

On one machine:

```sh
porch pair
# Pairing code: 482913
```

On the other machine:

```sh
porch pair 482913
# Paired with mac-mini
```

That's it. Both machines can now list and open each other's apps:

```sh
porch list --all                      # every app on this machine and paired machines
porch open web --peer mac-mini        # opens http://web.mac-mini.localhost:7462
```

Remote apps get readable, stable URLs of the form `http://APP.MACHINE.localhost:PROXY_PORT`, served by your local Porch proxy (`porch proxy start` if it is stopped). `MACHINE` is a short name derived from the paired machine's hostname (`Akshits-Mac-mini.local` becomes `akshits-mac-mini`). It is assigned when you pair and never changes, so bookmarks survive restarts. If two paired machines have the same name, the second becomes `mac-mini-2`. A local app always wins over a machine name. If your proxy uses HTTPS or a custom TLD, the URL uses that scheme and TLD. WebSockets and streaming work through these URLs. Apps whose names contain a path (`web/docs`) cannot be put in a hostname, so `porch open` gives them a dedicated loopback relay URL.

What `porch pair` does:

- **Showing a code** (`porch pair`): this starts this machine's discovery agent on port 7463 if it is not already running, and turns on app sharing. The agent binds this machine's Tailscale `100.x` address if it has one, otherwise its primary private LAN address. It never binds a public or wildcard address. The command prints a 6-digit code and waits until a machine pairs, the code expires (5 minutes) or you press Ctrl-C.
- **Entering a code** (`porch pair CODE`): this finds machines that are showing a code, using LAN mDNS and `tailscale status`. It sends the code, pins the other machine's certificate and stores the pairing. It also starts its own agent (choosing the interface that reaches the other machine) and hands over credentials, so access works in both directions after this one exchange.
- If discovery finds nothing (multicast blocked, a different subnet, or Tailscale CLI missing), name the machine yourself: `porch pair 482913 --host mac-mini` or `--host 100.101.102.103`. `--host` accepts an IP, `IP:PORT`, a DNS/MagicDNS name or a discovered machine name.
- `--bind IP` and `--port PORT` override the automatic interface and port. `porch pair --no-wait --json` prints the code and returns, for scripts. The tray uses it.

Pairing is consent: paired machines can open every registered HTTP/WS development app on this machine, including apps behind a loopback-only proxy, and see their names and health. Pair only machines you would trust to use your development apps. To stop sharing with one machine, run `porch unpair mac-mini`. This forgets the pairing on both machines when they can reach each other, and always revokes the other machine's access here. `porch peers list` shows paired machines.

The tray's **Pair a device…** item shows a code in the menu. Enter it on the other machine with `porch pair CODE`. Paired machines and their apps appear in the menu as before.

### Pairing security

- **Codes.** A code is 6 random digits. It works once, expires after 5 minutes, and is cancelled after 5 wrong guesses, so a network attacker has at most a 5-in-a-million chance per code you show. Each source address is also limited to 10 pairing attempts per minute. Codes are compared in constant time, and only a salted hash is stored.
- **Certificate pinning.** The joining machine pins the certificate it saw while probing (trust on first use). It sends the code only over a connection pinned to that certificate, together with that certificate's fingerprint. The host rejects the request if the fingerprint is not its own, which defeats a TLS-terminating interceptor that simply relays.
- **After pairing.** Each machine stores the other's pinned certificate plus a random 256-bit bearer secret (same storage as before: 0700 directory, 0600 files; the server stores only a hash). A changed certificate breaks the connection until you pair again.
- **Limits.** A 6-digit code is not a password-authenticated key exchange. An attacker who can actively intercept traffic between the two machines during the 5-minute window, and answer before the real host, could pair in its place. On a tailnet, that requires a compromised tailnet device, because WireGuard authenticates the peers. On a shared LAN, show the code only on networks you trust, or use the fingerprint flow below. Discovery hints (mDNS, Tailscale) never confer trust: pairing requires the code.

### Advanced: explicit fingerprint enrollment

For automation, or where you want to compare certificate fingerprints out of band, the explicit flow still works:

```sh
# On B
porch discovery enable --bind 192.168.1.20 --share-apps
porch discovery fingerprint
porch discovery pair-token          # 256-bit, single-use, valid for ten minutes
# On A: token on stdin, or --token-file PATH; never in shell arguments
porch peers add https://192.168.1.20:7463 --fingerprint SHA256_FROM_B
```

This enrollment is one-way: repeat it in the other direction if both machines should see each other's apps. Without `--share-apps`, peers see only the catalog and the apps appear as local-only. `porch pair` turns sharing on.

### Discovery and interfaces

LAN discovery browses real `_porch._tcp.local.` service records. Advertisements contain only an opaque machine ID, protocol version and agent endpoint, not app/project names or secrets. Candidate hints expire after 90 seconds, and goodbye events remove them during browsing. Tailscale discovery executes `tailscale status --json` (read-only), filters self/offline peers, and proposes their assigned tailnet addresses on port 7463. It does not depend on multicast traveling over Tailscale. Missing Tailscale does not block local or LAN use.

One agent binds one interface. A machine sharing on its tailnet address is not reachable at its LAN address, and the reverse is also true. To change it, run `porch discovery disable`, then pair again or enable with `--bind`. Porch never configures Tailscale Serve or Funnel, never changes an unrelated registration, and never enables a public tunnel.

## Trust and URL behavior

Agent endpoints must be literal private LAN, tailnet or loopback IPs. Discovery hints cannot trigger arbitrary DNS/public-URL fetches. Requests reject redirects. Control responses, candidate counts, concurrency, connection/header/body timeouts and body sizes are bounded. Browser Origin requests are rejected on the agent API. There are no remote lifecycle, shell, proxy configuration or state mutation endpoints. Authenticated browser app requests naturally retain the development app's own behavior and permissions.

A paired app URL is handled by the local proxy, which forwards `APP.MACHINE.<tld>` to a loopback-only gateway inside the proxy process. The gateway checks that the Host is exactly that readable name, so a rebinding DNS name cannot select a paired machine. It authenticates to the other machine over pinned TLS and routes to the registered app ID, preserving its configured host routing. Only browsers on this machine can use these names. A LAN-mode proxy never chains LAN clients through to paired machines. The remote agent validates HTTPS upstream certificates against ordinary roots plus its own Porch CA. There is no global TLS verification bypass. WebSocket upgrades and streamed HTTP use the same authenticated transport. Redirects to the app's configured origin are translated to the readable origin; other loopback redirect targets are refused. Cookie domain attributes are removed. A distinct hostname per machine/app keeps cookies from crossing apps. Browsers resolve `.localhost` to loopback; headless HTTP tools may need an explicit resolver mapping to `127.0.0.1`.

The browser-facing origin is your local proxy's, not the remote machine's. Localhost secure-context rules, application CORS/CSRF checks, OAuth callbacks, certificate-bound behavior and absolute URLs embedded in HTML/JavaScript may need an explicit development origin setting. Porch cannot safely rewrite arbitrary application code or make a machine-local URL globally reachable.

## Stop and revoke

```sh
porch unpair mac-mini       # forget one machine on both sides; revoke its access here
porch discovery status --json
porch discovery revoke      # invalidate every client credential and outstanding invitation
porch peers remove ID       # forget trust locally only
porch discovery disconnect  # stop path-app relays, leaving apps/proxy running
porch discovery disable     # stop sharing; apps/proxy keep running
```

Disable keeps the machine identity, certificate and pairings, so re-enabling does not silently change trust. Revoke rejects later requests and new WebSocket handshakes. Already-started streams can continue until they close. Disable the agent to terminate active transports as well, and Revoke before re-enabling if access must stay revoked. The discovery agent is a separate user process; it is not automatically re-enabled at login (run `porch pair` or `porch discovery enable` again after a reboot). Changing network interfaces requires reconfiguring the bound address.

## Wire protocol and evidence

All control APIs use HTTPS, JSON and `version: 1`. Code pairing uses unauthenticated `GET /v1/pairing` (`{version, open}`) and `POST /v1/pair-code` with `{version, code, host_fingerprint, machine, endpoint, fingerprint, token}`: the joiner's identity, its agent endpoint and certificate fingerprint, and a bearer the host will use to call back. The reply is a `PairResponse`. Authenticated `POST /v1/unpair` revokes the caller's credential and forgets the caller. The explicit flow's `POST /v1/pair` accepts `{ "token": "..." }` and returns `{ "version": 1, "machine": {"id", "name"}, "token": "..." }`. `GET /v1/catalog` requires `Authorization: Bearer ...` and returns `Catalog` (`machine`, stable per-machine app IDs, names, machine-local URL, remote-access permission/reason, health, proxy status, generation timestamp). Authenticated `/v1/apps/APP_ID/proxy/PATH` carries the explicitly granted HTTP/WS route transport. Internal `x-porch-app-*` fields carry original browser headers; the relay strips caller-supplied versions and the server never forwards its bearer to apps.

`porch list --json` retains its previous local format. `porch list --all --json` is the stable versioned external discovery format: `version`, `local`, `peers`, `candidates`, `warnings`. A peer has identity, endpoint, sources, last-seen time, optional catalog and error. No bearer/pairing secrets appear. Health is conservative: a live registered process and proxy do not prove application readiness, so an unprobed app can be `unknown`; stopped ownership is reported explicitly. An unavailable peer has no usable catalog rather than stale apps presented as reachable. Sources aggregate authenticated identity across explicit/LAN/Tailscale hints.

Automated acceptance uses real TLS/HTTP/WebSocket sockets, two isolated state directories, the actual CLI, same-named apps targeting distinct upstreams, wrong pins, token replay/revocation, malformed/oversized responses, route/redirect translation, DNS-rebinding rejection, expiry/removal/deduplication, and a fake Tailscale status adapter. Native menu-model tests exercise actionable and disabled states and generated CLI actions. See [verification record](discovery-verification.md) for commands and observed results. Loopback two-node acceptance is not physical multi-machine evidence. Physical LAN multicast/interface transitions, a real enrolled Tailscale peer, Linux desktop/indicator display and macOS rendered menu interaction require separate checks. Headless and compile checks do not establish a rendered UI.
