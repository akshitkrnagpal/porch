# Apps across machines

Porch has an optional native macOS menu bar / Linux tray companion and authenticated discovery agents. The headless CLI remains standalone. Both use `porch list --all --json`, a versioned snapshot of local apps, paired machines, their apps, discovery hints, and connection errors.

## Install and start

Build the CLI alone with `cargo build --release --bin porch`. Build the desktop package with `cargo build --release --features tray --bins`, then follow [packaging instructions](../packaging/README.md). macOS Apple Silicon and Linux x86_64 are supported; prebuilt Linux GNU artifacts require glibc 2.39+ (build from source on older distributions). Windows x86_64 is a preview: discovery, pairing and start at login (a per-user scheduled task) are implemented but not yet verified on real machines. [GitHub Releases](https://github.com/akshitkrnagpal/porch/releases) include the CLI and tray.

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
- **Entering a code** (`porch pair CODE`): this finds machines that are showing a code, using LAN mDNS and `tailscale status`. It runs a key exchange keyed by the code (the code itself is never sent) and checks that the other machine proved it knows the code. Only then does it start its own agent (choosing the interface that reaches the other machine), turn on sharing, hand over credentials and pin the other machine's certificate, so access works in both directions after this one exchange.
- If discovery finds nothing (multicast blocked, a different subnet, or Tailscale CLI missing), name the machine yourself: `porch pair 482913 --host mac-mini` or `--host 100.101.102.103`. `--host` accepts an IP, `IP:PORT`, a DNS/MagicDNS name or a discovered machine name.
- `--bind IP` and `--port PORT` override the automatic interface and port. `porch pair --no-wait --json` prints the code and returns, for scripts. The tray uses it.

Pairing is consent: paired machines can open every registered HTTP/WS development app on this machine, including apps behind a loopback-only proxy, and see their names and health. Pair only machines you would trust to use your development apps. To stop sharing with one machine, run `porch unpair mac-mini`. This forgets the pairing on both machines when they can reach each other, and always revokes the other machine's access here. `porch peers list` shows paired machines.

The tray's **Pair a device…** item shows a code in the menu. Enter it on the other machine with `porch pair CODE`. Paired machines and their apps appear in the menu as before.

### Pairing security

- **Codes.** A code is 6 random digits. It works once, expires after 5 minutes, and is cancelled after 5 wrong guesses, so an attacker pairing with the machine showing the code has at most a 5-in-a-million chance per code you show. A machine impersonating the one showing the code gets one guess each time you run `porch pair CODE` against it (the joining machine cannot count those for the real host), and a wrong guess only makes the join fail. Each source address is also limited to 10 pairing attempts per minute. Only a salted hash of the code is stored.
- **Password-authenticated key exchange.** The code never crosses the network. Both machines run SPAKE2 (Ed25519) with the code as the password and derive a shared key. Without the code, a machine on either end of the exchange gets one guess and cannot test other codes offline. The joining machine proves the key first and the showing machine counts a failed proof as a wrong guess. The showing machine then proves the key back. Only after that proof does the joining machine turn on sharing, start its agent, mint a credential for the other machine and send it. A machine that only claims to be showing a code (a rogue mDNS or tailnet advertiser, or anything answering at `--host`) learns neither the code nor a credential, and the join fails with "did not prove it knows the pairing code".
- **Certificate binding.** The joining machine pins the certificate it saw while probing, and both proofs cover that certificate's fingerprint. A TLS-terminating interceptor that relays the exchange presents a different certificate, so both proofs fail. The credentials sent afterwards are also authenticated with the shared key.
- **After pairing.** Each machine stores the other's pinned certificate plus a random 256-bit bearer secret (0700 directory, 0600 files; the server stores only a hash). A changed certificate breaks the connection until you pair again.
- **Upgrades.** Both machines need this version of Porch. Earlier versions sent the code and a working credential to whichever machine answered. A current machine refuses that exchange in both directions and asks you to update the other machine. It never falls back to it.
- **Limits.** The security of the pairing is the secrecy of the code during its 5-minute window: anyone who sees the code can pair in your place. Discovery hints (mDNS, Tailscale) never confer trust; pairing requires the code.

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
porch discovery autostart enable|disable|status  # keep sharing across restarts
```

Disable keeps the machine identity, certificate and pairings, so re-enabling does not silently change trust. Revoke rejects later requests and new WebSocket handshakes. Already-started streams can continue until they close. Disable the agent to terminate active transports as well, and Revoke before re-enabling if access must stay revoked. The discovery agent is a separate user process. By default it stops at logout or reboot. `porch discovery autostart enable` keeps sharing on across restarts: it installs a per-user LaunchAgent (`~/Library/LaunchAgents/dev.porch.discovery.plist`) on macOS or a systemd user unit (`~/.config/systemd/user/porch-discovery.service`) on Linux, with no sudo, and hands the running agent to it. If the bound address is not up yet at login (a Tailscale IP, for example), launchd or systemd retries every 30 seconds. It requires sharing to be on, and `porch discovery disable` removes it. `autostart disable` removes only login startup and leaves sharing running. `porch setup` turns it on when you choose both start at login and sharing. Changing network interfaces requires reconfiguring the bound address.

`porch peers` lists paired machines once. A machine you are already paired with is not repeated under "Discovered machines", even when Tailscale reports it at the default port and you paired on another port or address of the same node.

## Wire protocol and evidence

All control APIs use HTTPS, JSON and `version: 1`. Code pairing (exchange version 2) uses unauthenticated `GET /v1/pairing` (`{version, open, pairing, salt}`), then three requests pinned to the probed certificate: `POST /v1/pair-code/start` `{pairing, salt, msg}` returns `{session, msg}` (SPAKE2 messages); `POST /v1/pair-code/confirm` `{session, confirm}` returns the host's `{confirm}` (HMAC-SHA-256 key confirmations over the pinned host certificate fingerprint); `POST /v1/pair-code/finish` `{session, machine, endpoint, fingerprint, token, mac}` sends the joiner's identity, agent endpoint, certificate fingerprint and a bearer the host will use to call back, and returns `{version, machine, token, mac}`. Errors from these steps are JSON `{error:{code, message}}`. Sessions expire after 60 seconds and are single-use. The version 1 `POST /v1/pair-code` answers `426 Upgrade Required`. Authenticated `POST /v1/unpair` revokes the caller's credential and forgets the caller. The explicit flow's `POST /v1/pair` accepts `{ "token": "..." }` and returns `{ "version": 1, "machine": {"id", "name"}, "token": "..." }`. `GET /v1/catalog` requires `Authorization: Bearer ...` and returns `Catalog` (`machine`, stable per-machine app IDs, names, machine-local URL, remote-access permission/reason, health, proxy status, generation timestamp). Authenticated `/v1/apps/APP_ID/proxy/PATH` carries the explicitly granted HTTP/WS route transport. Internal `x-porch-app-*` fields carry original browser headers; the relay strips caller-supplied versions and the server never forwards its bearer to apps.

`porch list --json` retains its previous local format. `porch list --all --json` is the stable versioned external discovery format: `version`, `local`, `peers`, `candidates`, `warnings`. A peer has identity, endpoint, sources, last-seen time, optional catalog and error. No bearer/pairing secrets appear. Health is conservative: a live registered process and proxy do not prove application readiness, so an unprobed app can be `unknown`; stopped ownership is reported explicitly. An unavailable peer has no usable catalog rather than stale apps presented as reachable. Sources aggregate authenticated identity across explicit/LAN/Tailscale hints.

Automated acceptance uses real TLS/HTTP/WebSocket sockets, two isolated state directories, the actual CLI, same-named apps targeting distinct upstreams, wrong pins, token replay/revocation, malformed/oversized responses, route/redirect translation, DNS-rebinding rejection, expiry/removal/deduplication, and a fake Tailscale status adapter. Native menu-model tests exercise actionable and disabled states and generated CLI actions. Loopback two-node acceptance is not physical multi-machine evidence. Physical LAN multicast/interface transitions, a real enrolled Tailscale peer, Linux desktop/indicator display and macOS rendered menu interaction require separate checks. Headless and compile checks do not establish a rendered UI.
