# Apps across machines

Pairing lets two machines open each other's dev apps by name. Each machine runs a discovery agent that serves a list of its apps to the machines it has paired with, and forwards their requests to those apps. The `porch` CLI does everything on its own; the menu bar and tray app is an optional front end for the same commands. Both read `porch list --all --json`, a versioned snapshot of local apps, paired machines and their apps, discovered machines and connection errors.

## Install and start

[GitHub Releases](https://github.com/akshitkrnagpal/porch/releases) include the CLI and the tray app. To build them yourself, run `cargo build --release --bin porch` for the CLI or `cargo build --release --features tray --bins` for both, then see the [packaging guide](../packaging/README.md).

macOS on Apple Silicon and Linux x86_64 are supported. Prebuilt Linux binaries need glibc 2.39 or newer; build from source on older distributions. Windows x86_64 is a preview: pairing, discovery and start at login (a per-user scheduled task) work, but haven't been tried on as many real machines yet.

Run `porch tray`, or open `Porch.app` on macOS. The menu has **Pair a device…**, lists local apps and paired machines with their status and any errors, and lets you open an app, copy its URL, refresh, reconnect, and start or stop the proxy. It runs the CLI to do these things. The icon's dots are solid while the proxy runs and dimmed when it stops, and a small badge appears at the top right while a paired machine is reachable.

Quitting the tray closes only the tray. Apps, the proxy, the discovery agent and browser relays keep running until you stop them with their own commands. Installing or opening the tray never turns on login startup or sharing; pairing turns on sharing, and `porch-login enable` turns on login startup.

On Linux the tray needs GTK 3 and an AppIndicator host. GNOME usually needs the AppIndicator shell extension, and without a host the icon stays hidden even though the tray is running. Without a display the tray exits with an error that points to the CLI. The `porch` binary itself doesn't depend on GTK, so `porch list --all`, `porch open` and `porch copy` work without the tray.

## Connect two machines

On one machine:

```sh
porch pair
# Pairing code: 482913
```

On the other:

```sh
porch pair 482913
# Paired with mac-mini
```

Both machines can now list and open each other's apps:

```sh
porch list --all                      # apps on this machine and on paired machines
porch open web --peer mac-mini        # opens http://web.mac-mini.localhost:7462
```

Remote apps open at `http://APP.MACHINE.localhost:PROXY_PORT`, served by your local Porch proxy (run `porch proxy start` if it's stopped). `MACHINE` is a short name made from the other machine's hostname, so `Akshits-Mac-mini.local` becomes `akshits-mac-mini`. It's set when you pair and doesn't change, so bookmarks keep working. If two paired machines would get the same name, the second one becomes `mac-mini-2`. A local app with the same name as a machine takes priority. If your proxy uses HTTPS or a custom TLD, the URL uses them too. WebSockets and streaming work through these URLs. An app whose name contains a path, such as `web/docs`, can't be used as a hostname, so `porch open` gives it its own loopback relay URL instead.

### What `porch pair` does

- **Showing a code** (`porch pair`) starts this machine's discovery agent on port 7463 if it isn't running and turns on app sharing. The agent listens on the machine's Tailscale `100.x` address if it has one, otherwise on its main private LAN address, and never on a public or wildcard address. The command prints a 6-digit code and waits until a machine pairs, the code expires after 5 minutes, or you press Ctrl-C.
- **Entering a code** (`porch pair CODE`) looks for machines showing a code over LAN mDNS and `tailscale status`. It runs a key exchange based on the code, without sending the code itself, and checks that the other machine knows the code too. Only then does it start its own agent on the interface that reaches the other machine, turn on sharing, exchange credentials and pin the other machine's certificate. One exchange sets up access in both directions.
- **If no machine is found** (multicast is blocked, the machines are on different subnets, or the Tailscale CLI isn't installed), name the machine yourself: `porch pair 482913 --host mac-mini` or `--host 100.101.102.103`. `--host` accepts an IP, `IP:PORT`, a DNS or MagicDNS name, or the name of a discovered machine.
- **For scripts**, `--bind IP` and `--port PORT` override the interface and port, and `porch pair --no-wait --json` prints the code and returns immediately. The tray uses this.

Pairing grants access. A paired machine can open every HTTP and WebSocket dev app registered on this machine, including apps behind a loopback-only proxy, and can see their names and health. Only pair machines you'd trust with your dev apps. `porch peers list` shows paired machines. To stop sharing with one, run `porch unpair mac-mini`: this removes the pairing on both machines if they can reach each other, and always revokes the other machine's access on this one. `porch unpair` and `--peer` find a machine by its ID, then its URL name, then its name. A paired machine can't rename itself to another paired machine's name.

In the tray, **Pair a device…** opens a submenu that says pairing turns on sharing; **Show a pairing code** there shows the code in the menu until it expires. Enter it on the other machine with `porch pair CODE`.

### Pairing security

- **Codes.** A code is 6 random digits. It works once, expires after 5 minutes and is cancelled after 5 wrong guesses, so someone trying to pair with the machine showing the code has at most a 5-in-a-million chance per code. A machine pretending to be the one showing the code gets one guess each time you run `porch pair CODE` against it, and a wrong guess just makes the join fail. The limit holds even when many guesses arrive at once: each guess is counted before it is checked. Each source address is limited to 10 pairing attempts per minute and keeps at most two unfinished attempts, so abandoned attempts can't block pairing. The code itself isn't written to disk, but what is stored (a hash of the code with a public salt) is easy to reverse for anyone who can read your Porch state directory, which only your user can. Treat it like the code while it is showing.
- **Key exchange.** The code never crosses the network. Both machines run SPAKE2 (Ed25519) with the code as the password and derive a shared key. A machine that doesn't know the code gets one guess and can't test other codes offline. The joining machine proves it has the key first, and the showing machine counts a failed proof as a wrong guess. The showing machine then proves it has the key. Only after that does the joining machine turn on sharing, start its agent, and create and send a credential. A machine that only claims to be showing a code (a rogue mDNS or tailnet advertiser, or whatever answers at `--host`) learns neither the code nor a credential, and the join fails with "did not prove it knows the pairing code".
- **Certificate binding.** The joining machine pins the certificate it saw when it first connected, and both proofs cover that certificate's fingerprint. A TLS-terminating proxy in the middle presents a different certificate, so the proofs fail. The credentials sent afterwards are also authenticated with the shared key.
- **After pairing.** Each machine stores the other's pinned certificate and a random 256-bit bearer secret, in a 0700 directory with 0600 files; the receiving side stores only a hash. If a machine's certificate changes, the connection stops working until you pair again.
- **Identity.** Machine IDs are public (mDNS advertises them), so an ID alone proves nothing. Pairing with a machine that claims the ID of one already paired, but presents a different certificate, is refused on both sides (`already_paired`) and the code is cancelled. If that machine was really reinstalled, run `porch unpair` for it first, then pair again. A machine that claims to be the joining machine itself is refused too.
- **Versions.** Both machines need Porch 0.4.0 or later. Earlier versions sent the code and a working credential to whichever machine answered. Current versions refuse that exchange in both directions, ask you to update the other machine, and never fall back to it.
- **Limits.** Pairing is only as secret as the code during its 5-minute window: anyone who sees the code can pair in your place. Discovery hints from mDNS or Tailscale never grant access on their own.

### Pairing without a code

For automation, or to compare certificate fingerprints yourself, you can enroll a machine explicitly:

```sh
# On B
porch discovery enable --bind 192.168.1.20 --share-apps
porch discovery fingerprint
porch discovery pair-token          # 256-bit, single use, valid for ten minutes
# On A: pass the token on stdin or with --token-file PATH, never as an argument
porch peers add https://192.168.1.20:7463 --fingerprint SHA256_FROM_B
```

This works in one direction only. Repeat it the other way if both machines should see each other's apps. Without `--share-apps`, the other machine sees the list of apps but can't open them. `porch pair` always turns sharing on.

`porch peers add` tells the other machine its machine ID, so `porch unpair` on either side revokes the right credential. Porch 0.5.2 and earlier didn't send it. For a machine they enrolled, `porch unpair` can't tell which credential is its, says so, and suggests `porch discovery revoke`, which revokes every machine's access.

### Discovery and interfaces

LAN discovery browses `_porch._tcp.local.` mDNS records. An advertisement contains only an opaque machine ID, the protocol version and the agent's address, never app or project names or secrets. A discovered machine is forgotten after 90 seconds without an update, or as soon as it says goodbye. Tailscale discovery runs `tailscale status --json` (read-only), skips this machine and offline ones, and tries each tailnet address on port 7463, so it doesn't need multicast over Tailscale. Porch works on the LAN and locally without Tailscale installed.

An agent listens on one interface. A machine sharing on its tailnet address can't be reached on its LAN address, and the reverse is also true. To switch, run `porch discovery disable`, then pair again or run `porch discovery enable --bind`. Porch never sets up Tailscale Serve or Funnel, never changes other Tailscale settings and never opens a public tunnel.

## How remote URLs work

Agent addresses must be literal private LAN, tailnet or loopback IPs, so a discovery hint can't make Porch fetch an arbitrary DNS name or public URL. Requests to agents don't follow redirects. Response sizes, the number of discovered machines, concurrency and timeouts are all capped. Each advertised address and machine ID gets only a few discovery entries, and a paired machine's possible addresses are tried in parallel, so fake advertisements can't hide it. Names and status messages from paired machines are checked for terminal control sequences and invisible formatting characters.

The agent serves up to 256 connections. An address that hasn't authenticated gets at most 8 of them, and all such addresses share 64. A connection that never authenticates is closed after 15 seconds idle or 90 seconds in total. Addresses of paired machines, and any address that has authenticated, keep the rest. WebSockets give their connection slot back once they upgrade and count against a separate limit of 128. The agent API rejects requests from browsers (any request with an Origin header) and has no endpoints for starting or stopping anything, running commands, changing proxy settings or changing state. A request to a remote app runs with that app's own behavior and permissions.

When you open a remote app's URL, your local proxy hands `APP.MACHINE.<tld>` to a gateway inside the proxy that only listens on loopback. The gateway accepts only that exact hostname, so a DNS rebinding attack can't reach a paired machine. It connects to the other machine over pinned TLS and asks for the app by its ID, keeping the app's own host routing. Only browsers on your machine can use these names; a LAN-mode proxy never passes LAN clients through to paired machines. If the remote app is served over HTTPS, the remote agent checks its certificate against the system roots plus its own Porch CA, and never turns off certificate checks. WebSocket upgrades and streamed responses use the same authenticated connection. Redirects to the app's own origin are rewritten to the readable URL, and redirects to other loopback addresses are refused. `Domain` attributes are stripped from cookies, and since each app on each machine has its own hostname, cookies stay with their app. A request with an `Origin` header must come from the app's own URL or from another app on the same paired machine (`web.mac-mini.localhost` calling `api.mac-mini.localhost` on the same proxy port and scheme); the app then sees that app's origin on its own machine. Pages from other paired machines, other local apps and other sites are refused with 403. Browsers resolve `.localhost` to loopback on their own; command-line HTTP tools may need to be told to resolve it to `127.0.0.1`.

The browser sees your local proxy's origin, not the remote machine's. Secure-context rules for localhost, an app's CORS or CSRF checks, OAuth callbacks, certificate-bound features and absolute URLs baked into HTML or JavaScript may need the app's dev origin setting changed. Porch doesn't rewrite app code and can't make a machine-local URL reachable from elsewhere.

## Stop and revoke

```sh
porch unpair mac-mini       # remove one pairing on both sides; revoke its access here
porch discovery status --json
porch discovery revoke      # invalidate every client credential and outstanding invitation
porch peers remove ID       # forget a machine on this side only
porch discovery disconnect  # stop path-app relays; apps and proxy keep running
porch discovery disable     # stop sharing; apps and proxy keep running
porch discovery autostart enable|disable|status  # keep sharing on across restarts
```

`disable` keeps the machine's identity, certificate and pairings, so turning sharing back on doesn't quietly change who has access. `revoke` rejects later requests and new WebSocket connections, but streams that are already open continue until they close. To cut those off too, disable the agent. If access must stay revoked, revoke before you enable sharing again.

The discovery agent runs as your user and by default stops when you log out or restart. `porch discovery autostart enable` starts it at login instead, without sudo, and hands the running agent over:

- macOS: a LaunchAgent at `~/Library/LaunchAgents/dev.porch.discovery.plist`.
- Linux: a systemd user unit at `~/.config/systemd/user/porch-discovery.service`.
- Windows: a scheduled task, with a copy of its definition in `%LOCALAPPDATA%\Porch`.

If the agent's address isn't up yet at login (a Tailscale IP, for example), it retries every 30 seconds (every minute on Windows). The startup file records the path you ran Porch through (a Homebrew, Nix profile or `~/.local/bin` link), so upgrades don't break it; if the recorded program is removed anyway, `autostart status` reports `broken` and `autostart enable` fixes it. Autostart needs sharing to be on. `porch discovery disable` also removes autostart, while `autostart disable` removes only the login entry and leaves sharing running. `porch setup` turns autostart on when you choose both start at login and sharing. If the machine's network interface changes, enable discovery again with the new address.

`porch peers` lists each paired machine once. A paired machine doesn't show up again under "Discovered machines", even when Tailscale reports it on the default port and you paired on another port or address.

## Wire protocol

All control APIs use HTTPS and JSON with `version: 1`.

**Code pairing** (exchange version 2):

1. `GET /v1/pairing` (unauthenticated) returns `{version, open, pairing, salt}`.
2. `POST /v1/pair-code/start` with `{pairing, salt, msg}` returns `{session, msg}`. These are the SPAKE2 messages.
3. `POST /v1/pair-code/confirm` with `{session, confirm}` returns the host's `{confirm}`. Both are HMAC-SHA-256 key confirmations over the host certificate's fingerprint.
4. `POST /v1/pair-code/finish` with `{session, machine, endpoint, fingerprint, token, mac}` sends the joining machine's identity, agent address, certificate fingerprint and a bearer token the host will use to connect back. It returns `{version, machine, token, mac}`.

Steps 2–4 are pinned to the certificate seen in step 1. Errors are JSON `{error: {code, message}}`. Step 4 answers `409` with code `already_paired` when the joining machine claims the ID of a paired machine with a different certificate. Sessions expire after 60 seconds and can be used once. The version 1 endpoint `POST /v1/pair-code` answers `426 Upgrade Required`.

**Other endpoints:**

- `POST /v1/unpair` (authenticated) revokes the caller's credential and forgets the caller.
- `POST /v1/pair` (explicit enrollment) accepts `{"token": "...", "machine": {"id", "name"}}` and returns `{"version": 1, "machine": {"id", "name"}, "token": "..."}`. `machine` is the enrolling machine. It is optional, because Porch 0.5.2 and earlier don't send it, and a request whose `machine` is invalid is refused without spending the token.
- `GET /v1/catalog` requires `Authorization: Bearer ...` and returns the machine, and for each app a stable ID, name, machine-local URL, whether remote access is allowed (and why not), health, proxy status and a timestamp.
- `/v1/apps/APP_ID/proxy/PATH` (authenticated) carries HTTP and WebSocket traffic to an app that sharing allows. Internal `x-porch-app-*` headers carry the browser's original headers; the relay strips any the caller sends, and the agent never forwards its bearer token to apps. The agent drops every forwarding header the caller sends (`X-Forwarded-*`, `Forwarded`, `X-Real-IP`, `X-Client-IP`, `True-Client-IP`, `X-Original-*`, `X-Rewrite-URL` and similar) and sets `X-Forwarded-For` to the paired machine's address and `X-Forwarded-Host`, `-Proto` and `-Port` to what the browser used there.

The agent offers HTTP/2 and HTTP/1.1 (ALPN `h2`, `http/1.1`). A relay sends a page's requests over one HTTP/2 connection and opens HTTP/1.1 connections only for WebSocket upgrades; older agents answer everything over HTTP/1.1. The relay learns which protocol an agent speaks from its first connection, then dials one connection at a time, so a burst of requests on a cold connection waits for that one instead of each opening its own. After a failed dial, requests in the next second fail at once. Both ends allow 2 MB of unread data per request and 64 MB per connection, so a few responses or uploads nobody is reading don't stall the others.

The relay keeps a peer's catalog for 10 seconds, then serves it for up to 5 minutes while refreshing it in the background. Requests that need a fresh copy share one fetch, and a failed fetch is reported for 2 seconds without trying again. The catalog only maps app names to IDs, and the agent checks every request itself. The agent reuses the catalog it built for up to a second while its route and proxy files are unchanged, and rereads its credentials, sharing setting and routes only when those files change, so revocation and turning sharing off apply to the next request.

A relayed response may take up to 5 minutes to start after the request's last upload data, then the relay answers 504. Uploads are limited to 16 MB per request (413).

The menu bar app, `porch list --all` and `porch open --peer` ask the running proxy for paired machines (`GET /_porch/peers.json` on `localhost`, loopback only, with the proxy's private token from `~/.porch/proxy.hosts-sync-token`, and refused when a browser sends `Origin` or `Sec-Fetch-Site`). The proxy fetches each catalog over the same connection its relay uses, so the list you look at has already connected to that machine and cached its catalog when you open an app. Without a running proxy they query the machines themselves.

**CLI output.** `porch list --json` keeps its local-only format. `porch list --all --json` is the stable, versioned format for tools: `version`, `local`, `peers`, `candidates` and `warnings`. Each peer has an identity, an address, how it was found, when it was last seen, its catalog if available and any error. No bearer tokens or pairing secrets appear. Health errs on the side of caution: a registered process and a running proxy don't prove the app is ready, so an app that hasn't been checked can be `unknown`, and a suspended app is reported as stopped. An unreachable peer shows no catalog rather than stale apps that look reachable. A machine found through several sources (explicit, LAN, Tailscale) appears once, with all its sources.

## Testing

`tests/discovery.rs` runs two Porch instances with separate state directories on one machine, over real TLS, HTTP and WebSocket connections, through the actual CLI. It covers code pairing in both directions, wrong pins, wrong codes, token replay and revocation, malformed and oversized responses, redirect rewriting, DNS rebinding, expiry and de-duplication, rogue machines, and refusing the old pairing exchange, with a fake `tailscale status`. Tray tests check the menu contents and the CLI commands each item runs.

Some things these tests can't cover and need checking on real hardware: two physical machines, LAN multicast and interface changes, a real Tailscale peer, the Linux tray icon showing in a desktop session, and clicking through the rendered macOS menu.
